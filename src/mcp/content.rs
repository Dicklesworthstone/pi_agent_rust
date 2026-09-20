//! Translate MCP tool results without turning binary payloads into prompt text.
//!
//! The native message model already knows how to route images and audio to
//! capable providers. Keep those blocks native, retain their order, and expose
//! structured results and embedded documents to the model instead of hiding
//! them in `ToolOutput::details`. Resource URIs are opaque labels here: this
//! module never opens a path, fetches a URL, or executes server content.
//! Explicit audience annotations are honored before rendering or decoding.
//! Blocks not addressed to the assistant remain in client details. These are
//! routing hints, not a grant of authority or trust in the server's content.

use base64::Engine as _;
use serde_json::{Value, json};

use crate::model::{ContentBlock, ImageContent, MediaContent, TextContent};
use crate::tools::ToolOutput;

// These are client admission limits, not claims about any provider's limits.
// HTTP and stdio impose their own, smaller encoded-response limits as well.
const MAX_CONTENT_BLOCKS: usize = 1024;
const MAX_BINARY_BYTES: usize = 20 * 1024 * 1024;
const MAX_AUDIO_BYTES: usize = 5 * 1024 * 1024;
const MAX_LABEL_CHARS: usize = 256;

#[derive(Clone, Copy)]
struct Limits {
    blocks: usize,
    binary_bytes: usize,
    audio_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            blocks: MAX_CONTENT_BLOCKS,
            binary_bytes: MAX_BINARY_BYTES,
            audio_bytes: MAX_AUDIO_BYTES,
        }
    }
}

struct Shaper {
    content: Vec<ContentBlock>,
    warnings: Vec<Value>,
    binary_bytes: usize,
    is_error: bool,
    limits: Limits,
}

impl Shaper {
    fn text(&mut self, text: &str) {
        // Preserve the old newline-join behavior for adjacent text blocks,
        // but never move text across an intervening image or audio block.
        if let Some(ContentBlock::Text(previous)) = self.content.last_mut() {
            previous.text.push('\n');
            previous.text.push_str(text);
        } else {
            self.content
                .push(ContentBlock::Text(TextContent::new(text)));
        }
    }

    fn warning(&mut self, index: Option<usize>, code: &str, reason: &str, invalid: bool) {
        self.is_error |= invalid;
        self.warnings
            .push(json!({"index": index, "code": code, "reason": reason}));
        let location = index.map_or_else(|| "result".to_string(), |i| format!("content block {i}"));
        self.text(&format!("[MCP {location}: {code}: {reason}]"));
    }

    fn decode(&mut self, data: &str, audio: bool) -> Result<Vec<u8>, &'static str> {
        let remaining = self.limits.binary_bytes.saturating_sub(self.binary_bytes);
        let limit = if audio {
            remaining.min(self.limits.audio_bytes)
        } else {
            remaining
        };
        // Check the encoded bound BEFORE allocating the decoded buffer. The
        // final length check accounts for padding and non-multiple-of-3 caps.
        if data.len() > limit.div_ceil(3).saturating_mul(4) {
            return Err("binary content exceeds the per-part or aggregate byte limit");
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data))
            .map_err(|_| "binary content is not valid standard base64")?;
        if bytes.is_empty() {
            return Err("binary content is empty");
        }
        if bytes.len() > limit {
            return Err("binary content exceeds the per-part or aggregate byte limit");
        }
        self.binary_bytes += bytes.len();
        Ok(bytes)
    }

    fn binary(
        &mut self,
        block: &Value,
        data_field: &str,
        expected_major: Option<&str>,
        name: Option<&str>,
    ) -> Result<(), &'static str> {
        let raw_mime = match block.get("mimeType") {
            Some(Value::String(mime)) => mime.as_str(),
            None if expected_major.is_none() => "application/octet-stream",
            _ => return Err("binary content must have a string mimeType"),
        };
        let mime = crate::model::sanitize_image_mime_type(raw_mime);
        if mime != raw_mime.trim() || !mime.contains('/') {
            return Err("binary content has an invalid mimeType");
        }
        let mime = mime.to_ascii_lowercase();
        let (major, subtype) = mime.split_once('/').ok_or("invalid mimeType")?;
        if major.is_empty()
            || subtype.is_empty()
            || subtype.contains('/')
            || expected_major.is_some_and(|expected| expected != major)
        {
            return Err("binary mimeType does not match the content type");
        }
        let data = block
            .get(data_field)
            .and_then(Value::as_str)
            .ok_or("binary content must contain a base64 string")?;
        let bytes = self.decode(data, major == "audio")?;
        match major {
            "image" => self.content.push(ContentBlock::Image(ImageContent {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                mime_type: mime,
            })),
            "audio" | "video" => self.content.push(ContentBlock::Media(MediaContent {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                mime_type: mime,
                name: name.and_then(crate::model::sanitize_media_name),
            })),
            "text" => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| "text resource blob is not valid UTF-8")?;
                self.text(text);
            }
            _ if mime == "application/json" || mime.ends_with("+json") => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| "JSON resource blob is not valid UTF-8")?;
                self.text(text);
            }
            _ => {
                // Unknown binary resources remain available in details. Do
                // not feed megabytes of base64 to a text-only model as a
                // purported representation of a PDF, archive, or other file.
                self.text(&format!(
                    "[MCP binary resource: {mime}, {} bytes; payload retained in tool details]",
                    bytes.len()
                ));
            }
        }
        Ok(())
    }

    fn embedded_resource(&mut self, block: &Value) -> Result<(), &'static str> {
        let resource = block
            .get("resource")
            .filter(|resource| resource.is_object())
            .ok_or("embedded resource must be an object")?;
        let uri = resource
            .get("uri")
            .and_then(Value::as_str)
            .filter(|uri| !uri.is_empty())
            .ok_or("embedded resource must have a nonempty string uri")?;
        let text = resource.get("text");
        let blob = resource.get("blob");
        if text.is_some() == blob.is_some() {
            return Err("embedded resource must contain exactly one of text or blob");
        }
        // Debug quoting escapes terminal controls in the untrusted label.
        // The document itself remains exact content, not a host instruction.
        self.text(&format!("MCP resource {}:", quoted_label(uri)));
        if let Some(text) = text {
            self.text(text.as_str().ok_or("resource text must be a string")?);
        } else {
            self.binary(resource, "blob", None, Some(uri))?;
        }
        Ok(())
    }

    fn resource_link(&mut self, block: &Value) -> Result<(), &'static str> {
        let uri = block
            .get("uri")
            .and_then(Value::as_str)
            .filter(|uri| !uri.is_empty())
            .ok_or("resource link must have a nonempty string uri")?;
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .ok_or("resource link must have a string name")?;
        // Render only the public resource fields. _meta is client metadata,
        // not an instruction or a source of extra model-visible content.
        let mut link = json!({"uri": uri, "name": name});
        for field in ["title", "description", "mimeType", "size"] {
            if let Some(value) = block.get(field) {
                link[field] = value.clone();
            }
        }
        self.text("MCP resource link (reference only; content has not been fetched):");
        self.text(&json_text(&link));
        Ok(())
    }

    /// None means the original block is retained only in client details;
    /// Some records whether the normal model-content conversion succeeded.
    /// Check routing before inspecting labels or decoding binary payloads.
    fn model_block(
        &mut self,
        index: usize,
        block: &Value,
        client_only: &mut Vec<Value>,
    ) -> Option<bool> {
        let reason = match assistant_audience(block) {
            Ok(true) => return Some(self.block(index, block)),
            Ok(false) => "not_for_assistant",
            Err(reason) => {
                self.warning(Some(index), "MCP_AUDIENCE_INVALID", reason, true);
                "invalid_annotations"
            }
        };
        client_only.push(json!({"index":index, "reason":reason, "block":block}));
        None
    }

    fn block(&mut self, index: usize, block: &Value) -> bool {
        let kind = block.get("type").and_then(Value::as_str);
        let result = match kind {
            Some("text") => block
                .get("text")
                .and_then(Value::as_str)
                .ok_or("text content must have a string text field")
                .map(|text| self.text(text)),
            Some("image") => self.binary(block, "data", Some("image"), None),
            Some("audio") => self.binary(block, "data", Some("audio"), None),
            Some("resource") => self.embedded_resource(block),
            Some("resource_link") => self.resource_link(block),
            Some(kind) => {
                self.warning(
                    Some(index),
                    "MCP_CONTENT_UNSUPPORTED",
                    &format!(
                        "unsupported content type {}; original retained in tool details",
                        quoted_label(kind)
                    ),
                    false,
                );
                return false;
            }
            None => Err("content block must have a string type"),
        };
        if let Err(reason) = result {
            self.warning(Some(index), "MCP_CONTENT_INVALID", reason, true);
            return false;
        }
        true
    }
}

/// Missing audience annotations preserve the existing model-visible default.
/// An explicit audience is an allow-list: an empty list includes nobody, and
/// malformed roles do not silently become permission to expose the block.
fn assistant_audience(block: &Value) -> Result<bool, &'static str> {
    let Some(annotations) = block.get("annotations") else {
        return Ok(true);
    };
    let annotations = annotations
        .as_object()
        .ok_or("content annotations must be an object")?;
    let Some(audience) = annotations.get("audience") else {
        return Ok(true);
    };
    let roles = audience
        .as_array()
        .ok_or("audience must be an array of user or assistant roles")?;
    roles.iter().try_fold(false, |visible, role| match role.as_str() {
        Some("assistant") => Ok(true),
        Some("user") => Ok(visible),
        _ => Err("audience entries must be user or assistant roles"),
    })
}

/// Only a model-visible text block can stand in for structuredContent. In
/// particular, user-only JSON text must not suppress separately advertised
/// structured facts that have not yet reached the model.
fn structured_is_model_visible(result: &Value, structured: &Value, block_limit: usize) -> bool {
    result
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().take(block_limit).any(|block| {
                assistant_audience(block).unwrap_or(false)
                    && block.get("type").and_then(Value::as_str) == Some("text")
                    && block
                        .get("text")
                        .and_then(Value::as_str)
                        .and_then(|text| serde_json::from_str::<Value>(text).ok())
                        .as_ref()
                        == Some(structured)
            })
        })
}

fn quoted_label(label: &str) -> String {
    let bounded: String = label.chars().take(MAX_LABEL_CHARS).collect();
    format!("{bounded:?}")
}

fn json_text(value: &Value) -> String {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| "[MCP value could not be serialized]".to_string())
}

/// Keep metadata, but not a second copy of bytes already stored natively.
/// Unknown binary resources retain their payload because no native block can
/// carry it. Never remove data from the input value itself.
fn non_text_metadata(block: &Value) -> Value {
    let mut metadata = block.clone();
    match block.get("type").and_then(Value::as_str) {
        Some("image" | "audio") => {
            if let Some(object) = metadata.as_object_mut()
                && let Some(data) = object.remove("data")
            {
                object.insert(
                    "encodedBytes".to_string(),
                    json!(data.as_str().map_or(0, str::len)),
                );
                object.insert("dataInNativeContent".to_string(), Value::Bool(true));
            }
        }
        Some("resource") => {
            if let Some(resource) = metadata.get_mut("resource").and_then(Value::as_object_mut) {
                let mime = block
                    .pointer("/resource/mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                if resource.get("text").is_some_and(Value::is_string) {
                    resource.remove("text");
                    resource.insert("textInNativeContent".to_string(), Value::Bool(true));
                }
                if (mime.starts_with("image/")
                    || mime.starts_with("audio/")
                    || mime.starts_with("video/")
                    || mime.starts_with("text/")
                    || mime == "application/json"
                    || mime.ends_with("+json"))
                    && let Some(data) = resource.remove("blob")
                {
                    resource.insert(
                        "encodedBytes".to_string(),
                        json!(data.as_str().map_or(0, str::len)),
                    );
                    resource.insert("dataInNativeContent".to_string(), Value::Bool(true));
                }
            }
        }
        _ => {}
    }
    metadata
}

pub(super) fn tool_output(result: &Value) -> ToolOutput {
    tool_output_with_limits(result, Limits::default())
}

fn tool_output_with_limits(result: &Value, limits: Limits) -> ToolOutput {
    let mut shaper = Shaper {
        content: Vec::new(),
        warnings: Vec::new(),
        binary_bytes: 0,
        is_error: result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        limits,
    };
    let mut details = json!({"mcp": true, "nonTextBlocks": 0});
    let mut non_text = Vec::new();
    let mut client_only = Vec::new();
    if result.is_object() {
        if result.get("isError").is_some_and(|flag| !flag.is_boolean()) {
            shaper.warning(
                None,
                "MCP_RESULT_INVALID",
                "isError must be a boolean",
                true,
            );
        }
        match result.get("content") {
            Some(Value::Array(blocks)) => {
                details["nonTextBlocks"] = json!(
                    blocks
                        .iter()
                        .filter(|block| block.get("type").and_then(Value::as_str) != Some("text"))
                        .count()
                );
                for (index, block) in blocks.iter().take(limits.blocks).enumerate() {
                    let Some(represented) = shaper.model_block(index, block, &mut client_only) else {
                        continue;
                    };
                    if block.get("type").and_then(Value::as_str) != Some("text") {
                        non_text.push(if represented {
                            non_text_metadata(block)
                        } else {
                            block.clone()
                        });
                    }
                }
                if blocks.len() > limits.blocks {
                    details["omittedContentBlocks"] = json!(blocks.len() - limits.blocks);
                    shaper.warning(
                        None,
                        "MCP_CONTENT_LIMIT",
                        "result exceeds the content-block limit",
                        true,
                    );
                }
            }
            None if result.get("structuredContent").is_some() => {}
            _ => shaper.warning(None, "MCP_RESULT_INVALID", "content must be an array", true),
        }
        if let Some(structured) = result.get("structuredContent") {
            details["structuredContent"] = structured.clone();
            // Servers commonly supply the exact same JSON in a text block
            // for older clients. Avoid duplicating it, but a human summary
            // such as "Done" must not hide structured facts from the model.
            if !structured_is_model_visible(result, structured, limits.blocks) {
                shaper.text("MCP structured result:");
                shaper.text(&json_text(structured));
            }
        }
        if let Some(metadata) = result.get("_meta") {
            details["_meta"] = metadata.clone();
        }
    } else {
        shaper.warning(
            None,
            "MCP_RESULT_INVALID",
            "tool result must be an object",
            true,
        );
    }
    if shaper.content.is_empty() {
        shaper.text(if client_only.is_empty() {
            "[MCP tool returned no content]"
        } else {
            "[MCP tool returned only content not addressed to the assistant; retained in client details]"
        });
    }
    if !client_only.is_empty() {
        details["audienceFilteredBlocks"] = json!(client_only.len());
        details["audienceFilteredContent"] = Value::Array(client_only);
    }
    if !non_text.is_empty() {
        details["nonText"] = Value::Array(non_text);
    }
    if !shaper.warnings.is_empty() {
        details["contentWarnings"] = Value::Array(shaper.warnings);
    }
    details["decodedBinaryBytes"] = json!(shaper.binary_bytes);
    ToolOutput {
        content: shaper.content,
        details: Some(details),
        is_error: shaper.is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn text(output: &ToolOutput) -> String {
        output
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn limits(bytes: usize) -> Limits {
        Limits {
            blocks: 16,
            binary_bytes: bytes,
            audio_bytes: bytes,
        }
    }

    #[test]
    fn user_only_text_is_retained_in_details_not_model_content() {
        let hidden = json!({
            "type":"text", "text":"private-user-text",
            "annotations":{"audience":["user"]}, "_meta":{"token":"private-user-meta"}
        });
        let input = json!({"content":[
            {"type":"text", "text":"before"},
            hidden,
            {"type":"text", "text":"after"}
        ]});
        let before = input.clone();
        let output = tool_output(&input);
        assert!(!output.is_error);
        assert_eq!(text(&output), "before\nafter");
        assert!(!text(&output).contains("private-user"));
        let details = output.details.expect("details");
        assert_eq!(details["audienceFilteredBlocks"], 1);
        assert_eq!(details["audienceFilteredContent"][0]["index"], 1);
        assert_eq!(details["audienceFilteredContent"][0]["reason"], "not_for_assistant");
        assert_eq!(details["audienceFilteredContent"][0]["block"], hidden);
        assert_eq!(input, before, "routing must not mutate the original result");
    }

    #[test]
    fn user_only_binary_and_resource_blocks_are_not_decoded_or_rendered() {
        let mut blocks = vec![
            json!({"type":"image", "mimeType":"image/png", "data":"private-user-image"}),
            json!({"type":"audio", "mimeType":"audio/wav", "data":"private-user-audio"}),
            json!({"type":"resource", "resource":{
                "uri":"file:///private-user-resource", "text":"private-user-document"
            }}),
            json!({"type":"resource_link", "uri":"file:///private-user-link", "name":"private-user-name"}),
        ];
        for block in &mut blocks {
            block["annotations"] = json!({"audience":["user"]});
        }
        let output = tool_output_with_limits(&json!({"content":blocks}), limits(0));
        assert!(!output.is_error, "unaddressed payloads must not enter binary admission");
        assert!(!text(&output).contains("private-user"));
        assert!(text(&output).contains("retained in client details"));
        assert!(output.content.iter().all(|block| matches!(block, ContentBlock::Text(_))));
        let details = output.details.expect("details");
        assert_eq!(details["decodedBinaryBytes"], 0);
        assert_eq!(details["audienceFilteredBlocks"], 4);
        assert!(details.get("nonText").is_none(), "do not duplicate omitted binary payloads");
        assert!(details.get("contentWarnings").is_none());
        for (index, block) in blocks.iter().enumerate() {
            assert_eq!(details["audienceFilteredContent"][index]["block"], *block);
        }
    }

    #[test]
    fn assistant_and_shared_audiences_preserve_native_content_and_order() {
        let image = encoded(b"shared image");
        let output = tool_output(&json!({"content":[
            {"type":"text", "text":"default", "annotations":{"priority":0.5}},
            {"type":"text", "text":"assistant", "annotations":{"audience":["assistant"]}},
            {"type":"image", "mimeType":"image/png", "data":image,
                "annotations":{"audience":["user","assistant"]}},
            {"type":"text", "text":"after", "annotations":{"audience":["assistant","user"]}}
        ]}));
        assert!(!output.is_error);
        assert_eq!(output.content.len(), 3);
        assert!(matches!(&output.content[0], ContentBlock::Text(t) if t.text == "default\nassistant"));
        assert!(matches!(&output.content[1], ContentBlock::Image(i) if i.data == image));
        assert!(matches!(&output.content[2], ContentBlock::Text(t) if t.text == "after"));
        assert!(output.details.expect("details").get("audienceFilteredContent").is_none());
    }

    #[test]
    fn invalid_audience_annotations_fail_closed_without_echoing_payloads() {
        for annotations in [
            Value::Null,
            json!(true),
            json!([]),
            json!("private-annotation"),
            json!({"audience":"assistant"}),
            json!({"audience":null}),
            json!({"audience":[1]}),
            json!({"audience":["assistant","private-unknown-role"]}),
        ] {
            let block = json!({"type":"text", "text":"private-payload", "annotations":annotations});
            let output = tool_output(&json!({"content":[block]}));
            assert!(output.is_error);
            let visible = text(&output);
            assert!(visible.contains("MCP_AUDIENCE_INVALID"));
            assert!(!visible.contains("private-"));
            let details = output.details.expect("details");
            assert_eq!(details["audienceFilteredContent"][0]["reason"], "invalid_annotations");
            assert_eq!(details["audienceFilteredContent"][0]["block"], block);
        }
    }

    #[test]
    fn an_empty_audience_has_no_model_visible_content() {
        let output = tool_output(&json!({"content":[{
            "type":"text", "text":"unaddressed-private-content", "annotations":{"audience":[]}
        }]}));
        assert!(!output.is_error);
        assert!(!text(&output).contains("unaddressed-private-content"));
        assert!(text(&output).contains("not addressed to the assistant"));
        assert_eq!(output.details.expect("details")["audienceFilteredBlocks"], 1);
    }

    #[test]
    fn filtered_json_text_does_not_hide_separately_advertised_structured_content() {
        let structured = json!({"answer":42});
        for audience in [json!(["user"]), json!([]), json!("invalid")] {
            let output = tool_output(&json!({
                "content":[{"type":"text", "text":structured.to_string(),
                    "annotations":{"audience":audience}}],
                "structuredContent":structured
            }));
            assert!(text(&output).contains("MCP structured result"));
            assert!(text(&output).contains("\"answer\": 42"));
        }
    }

    #[test]
    fn audience_filtering_does_not_extend_the_content_block_budget() {
        let output = tool_output_with_limits(
            &json!({"content":[
                {"type":"text", "text":"private-first", "annotations":{"audience":["user"]}},
                {"type":"text", "text":"late-visible-content"}
            ]}),
            Limits { blocks: 1, binary_bytes: 0, audio_bytes: 0 },
        );
        assert!(output.is_error);
        assert!(text(&output).contains("MCP_CONTENT_LIMIT"));
        assert!(!text(&output).contains("private-first"));
        assert!(!text(&output).contains("late-visible-content"));
        let details = output.details.expect("details");
        assert_eq!(details["audienceFilteredBlocks"], 1);
        assert_eq!(details["omittedContentBlocks"], 1);
    }

    #[test]
    fn filtering_preserves_remote_execution_error_status() {
        let output = tool_output(&json!({"isError":true, "content":[{
            "type":"text", "text":"private-error-detail", "annotations":{"audience":["user"]}
        }]}));
        assert!(output.is_error);
        assert!(!text(&output).contains("private-error-detail"));
        assert_eq!(output.details.expect("details")["audienceFilteredBlocks"], 1);
    }

    #[test]
    fn image_results_stay_native_and_preserve_mixed_block_order() {
        let image = encoded(b"image payload");
        let input = json!({"content": [
            {"type":"text", "text":"before"},
            {"type":"image", "mimeType":"image/png", "data":image, "annotations":{"priority":0.9}},
            {"type":"text", "text":"after"},
            {"type":"text", "text":"caption"}
        ]});
        let output = tool_output(&input);
        assert!(!output.is_error);
        assert_eq!(output.content.len(), 3);
        assert!(matches!(&output.content[0], ContentBlock::Text(t) if t.text == "before"));
        assert!(
            matches!(&output.content[1], ContentBlock::Image(i) if i.data == image && i.mime_type == "image/png")
        );
        assert!(matches!(&output.content[2], ContentBlock::Text(t) if t.text == "after\ncaption"));
        assert!(!text(&output).contains(&image));
        let details = output.details.expect("details");
        assert_eq!(details["nonTextBlocks"], 1);
        assert_eq!(details["nonText"][0]["annotations"]["priority"], 0.9);
        assert!(details["nonText"][0].get("data").is_none());
        assert_eq!(input["content"][1]["data"], image, "source was not mutated");
    }

    #[test]
    fn audio_only_results_reach_the_native_media_pipeline() {
        let audio = encoded(b"audio bytes");
        let output = tool_output(&json!({"content":[{
            "type":"audio", "mimeType":"audio/wav", "data":audio
        }]}));
        assert!(!output.is_error);
        assert!(
            matches!(output.content.as_slice(), [ContentBlock::Media(media)]
            if media.data == audio && media.mime_type == "audio/wav")
        );
        assert!(
            text(&output).is_empty(),
            "audio must not become base64 text"
        );
    }

    #[test]
    fn unpadded_base64_is_canonicalized_without_changing_bytes() {
        let output = tool_output(&json!({"content":[{
            "type":"image", "mimeType":"IMAGE/PNG", "data":"aGk"
        }]}));
        assert!(
            matches!(output.content.as_slice(), [ContentBlock::Image(image)]
            if image.data == "aGk=" && image.mime_type == "image/png")
        );
    }

    #[test]
    fn malformed_media_is_reported_without_echoing_its_payload() {
        for block in [
            json!({"type":"image", "mimeType":"image/png", "data":"not-base64-secret"}),
            json!({"type":"image", "mimeType":"text/plain", "data":"not-base64-secret"}),
            json!({"type":"audio", "mimeType":"audio/wav\r\nInjected: yes", "data":"not-base64-secret"}),
            json!({"type":"image", "data":"not-base64-secret"}),
            json!({"type":"image", "mimeType":"image/png", "data":""}),
        ] {
            let output = tool_output(&json!({"content":[block]}));
            assert!(output.is_error);
            assert!(text(&output).contains("MCP_CONTENT_INVALID"));
            assert!(!text(&output).contains("not-base64-secret"));
            assert!(
                !output
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image(_) | ContentBlock::Media(_)))
            );
        }
    }

    #[test]
    fn binary_budget_is_aggregate_and_keeps_valid_siblings() {
        let output = tool_output_with_limits(
            &json!({"content":[
                {"type":"image", "mimeType":"image/png", "data":encoded(b"1234")},
                {"type":"image", "mimeType":"image/png", "data":encoded(b"5678")},
                {"type":"text", "text":"still available"}
            ]}),
            limits(6),
        );
        assert!(output.is_error);
        assert_eq!(
            output
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::Image(_)))
                .count(),
            1
        );
        assert!(text(&output).contains("still available"));
        assert_eq!(output.details.expect("details")["decodedBinaryBytes"], 4);
    }

    #[test]
    fn decoded_size_check_catches_padding_boundary() {
        let output = tool_output_with_limits(
            &json!({"content":[{
                "type":"image", "mimeType":"image/png", "data":encoded(b"abc")
            }]}),
            limits(2),
        );
        assert!(
            output.is_error,
            "four encoded characters can decode to three, not two, bytes"
        );
        assert!(
            !output
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
        );
    }

    #[test]
    fn audio_has_its_own_smaller_admission_limit() {
        let cap = Limits {
            blocks: 4,
            binary_bytes: 16,
            audio_bytes: 2,
        };
        let output = tool_output_with_limits(
            &json!({"content":[
                {"type":"audio", "mimeType":"audio/wav", "data":encoded(b"abc")},
                {"type":"image", "mimeType":"image/png", "data":encoded(b"abc")}
            ]}),
            cap,
        );
        assert!(output.is_error);
        assert!(
            output
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
        );
        assert!(
            !output
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Media(_)))
        );
    }

    #[test]
    fn embedded_text_is_model_visible_not_only_details() {
        let document = "fn main() {\n    println!(\"hello\");\n}\n";
        let output = tool_output(&json!({"content":[{
            "type":"resource", "resource":{
                "uri":"file:///remote/main.rs", "mimeType":"text/x-rust", "text":document
            }
        }]}));
        assert!(!output.is_error);
        assert!(text(&output).contains(document));
        assert!(text(&output).contains("file:///remote/main.rs"));
        assert!(
            output.details.expect("details")["nonText"][0]["resource"]
                .get("text")
                .is_none()
        );
    }

    #[test]
    fn embedded_binary_images_audio_video_and_text_use_native_content() {
        for (mime, expected) in [
            ("image/png", "image"),
            ("audio/wav", "media"),
            ("video/mp4", "media"),
            ("text/plain", "text"),
            ("application/problem+json", "text"),
        ] {
            let output = tool_output(&json!({"content":[{
                "type":"resource", "resource":{
                    "uri":"remote://attachment", "mimeType":mime, "blob":encoded(b"payload")
                }
            }]}));
            assert!(!output.is_error, "{mime}");
            assert!(
                output.content.iter().any(|block| match block {
                    ContentBlock::Image(image) =>
                        expected == "image" && image.data == encoded(b"payload"),
                    ContentBlock::Media(media) =>
                        expected == "media" && media.data == encoded(b"payload"),
                    ContentBlock::Text(text) => expected == "text" && text.text.contains("payload"),
                    _ => false,
                }),
                "{mime}"
            );
        }
    }

    #[test]
    fn unknown_binary_resources_are_described_without_base64_in_prompt() {
        let blob = encoded(b"opaque binary document");
        let output = tool_output(&json!({"content":[{
            "type":"resource", "resource":{
                "uri":"remote://document", "mimeType":"application/pdf", "blob":blob
            }
        }]}));
        assert!(!output.is_error);
        assert!(text(&output).contains("application/pdf"));
        assert!(!text(&output).contains(&blob));
        assert_eq!(
            output.details.expect("details")["nonText"][0]["resource"]["blob"],
            blob
        );
    }

    #[test]
    fn a_binary_resource_may_omit_mime_type() {
        let blob = encoded(b"opaque bytes");
        let output = tool_output(&json!({"content":[{
            "type":"resource", "resource":{"uri":"remote://unknown", "blob":blob}
        }]}));
        assert!(!output.is_error);
        assert!(text(&output).contains("application/octet-stream"));
        assert_eq!(
            output.details.expect("details")["nonText"][0]["resource"]["blob"],
            blob
        );
    }

    #[test]
    fn rejected_media_does_not_claim_its_data_was_stored_natively() {
        let block = json!({"type":"image", "mimeType":"image/png", "data":"invalid"});
        let output = tool_output(&json!({"content":[block]}));
        assert!(output.is_error);
        assert_eq!(output.details.expect("details")["nonText"][0], block);
    }

    #[test]
    fn invalid_resource_shapes_do_not_discard_their_valid_siblings() {
        for resource in [
            json!({"uri":"file:///remote", "text":"x", "blob":"eA=="}),
            json!({"uri":"file:///remote"}),
            json!({"text":"x"}),
            json!({"uri":"file:///remote", "text":42}),
            json!({"uri":"file:///remote", "mimeType":"text/plain", "blob":encoded(&[255])}),
        ] {
            let output = tool_output(&json!({"content":[
                {"type":"resource", "resource":resource}, {"type":"text", "text":"valid sibling"}
            ]}));
            assert!(output.is_error);
            assert!(text(&output).contains("valid sibling"));
        }
    }

    #[test]
    fn links_are_references_and_private_metadata_stays_out_of_prompt() {
        let output = tool_output(&json!({
            "content":[{"type":"resource_link", "uri":"file:///must-not-open",
                "name":"report", "description":"remote report", "_meta":{"token":"private-link-meta"}}],
            "_meta":{"secret":"private-result-meta"}
        }));
        assert!(!output.is_error);
        let visible = text(&output);
        assert!(visible.contains("file:///must-not-open"));
        assert!(visible.contains("has not been fetched"));
        assert!(!visible.contains("private-link-meta"));
        assert!(!visible.contains("private-result-meta"));
        assert_eq!(
            output.details.expect("details")["_meta"]["secret"],
            "private-result-meta"
        );
    }

    #[test]
    fn structured_data_is_visible_even_when_server_supplies_human_summary() {
        let structured = json!({"answer":42, "items":["a", "b"]});
        let output = tool_output(&json!({"content":[{"type":"text", "text":"Done"}],
            "structuredContent":structured}));
        assert!(!output.is_error);
        assert!(text(&output).contains("Done"));
        assert!(text(&output).contains("\"answer\": 42"));
        assert_eq!(
            output.details.expect("details")["structuredContent"],
            structured
        );
    }

    #[test]
    fn serialized_structured_fallback_is_not_repeated() {
        let structured = json!({"answer":42});
        let raw = structured.to_string();
        let output = tool_output(&json!({"content":[{"type":"text", "text":raw}],
            "structuredContent":structured}));
        assert_eq!(text(&output), raw);
        assert!(!text(&output).contains("MCP structured result"));
    }

    #[test]
    fn structured_only_and_empty_success_results_have_usable_output() {
        let output = tool_output(&json!({"structuredContent":{"answer":42}}));
        assert!(!output.is_error);
        assert!(text(&output).contains("\"answer\": 42"));
        let empty = tool_output(&json!({"content":[]}));
        assert!(!empty.is_error);
        assert_eq!(text(&empty), "[MCP tool returned no content]");
    }

    #[test]
    fn result_and_block_validation_keep_remote_error_flag() {
        for result in [
            json!(null),
            json!("text"),
            json!({}),
            json!({"content":null}),
            json!({"content":[], "isError":"false"}),
            json!({"content":[null]}),
        ] {
            assert!(tool_output(&result).is_error, "{result}");
        }
        let output =
            tool_output(&json!({"isError":true,"content":[{"type":"text","text":"remote error"}]}));
        assert!(output.is_error);
        assert_eq!(text(&output), "remote error");
    }

    #[test]
    fn unsupported_kinds_are_explicit_and_not_silently_lost() {
        let unknown = json!({"type":"future-content", "payload":{"important":42}});
        let output = tool_output(&json!({"content":[unknown]}));
        assert!(!output.is_error);
        assert!(text(&output).contains("MCP_CONTENT_UNSUPPORTED"));
        assert_eq!(output.details.expect("details")["nonText"][0], unknown);
    }

    #[test]
    fn too_many_blocks_is_a_bounded_partial_result_not_silent_success() {
        let output = tool_output_with_limits(
            &json!({"content":[
                {"type":"text","text":"first"}, {"type":"text","text":"second"}
            ]}),
            Limits {
                blocks: 1,
                binary_bytes: 4,
                audio_bytes: 4,
            },
        );
        assert!(output.is_error);
        assert!(text(&output).contains("first"));
        assert!(!text(&output).contains("second"));
        assert_eq!(output.details.expect("details")["omittedContentBlocks"], 1);
    }
}
