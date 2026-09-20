//! Inferred type/parameter hints and optional lazy documentation resolution.
//!
//! These are observations. Embedded commands and acceptance edits are never
//! executed, and external locations are returned as opaque metadata only.

use serde_json::{Value, json};

use super::{Budget, MAX_RESPONSE_BYTES, Source, bounded_size, documentation, malformed, read_only_input};
use crate::error::Result;
use crate::lsp::text::{Position, Range, offset_to_position, position_to_offset_exact};
use crate::lsp::{LspInput, LspTool, MAX_PAYLOAD_BYTES, display_path, text_output, tool_err};
use crate::tools::ToolOutput;

const MAX_SERVER_HINTS: usize = 4096;
const MAX_HINTS: usize = 128;
const MAX_HINT_BYTES: usize = 64 * 1024;
const MAX_LABEL_BYTES: usize = 16 * 1024;
const MAX_LABEL_PARTS: usize = 64;

fn hint_range(text: &str, range: Option<Range>) -> Result<Range> {
    let range = match range {
        Some(range) => range,
        None => Range {
            start: Position { line: 0, character: 0 },
            end: offset_to_position(text, text.len())
                .ok_or_else(|| tool_err("LSP_USAGE", "cannot represent document end"))?,
        },
    };
    if range.end < range.start || position_to_offset_exact(text, range.start).is_none()
        || position_to_offset_exact(text, range.end).is_none()
    {
        return Err(tool_err("LSP_USAGE", "inlay hint range must use ordered exact UTF-16 boundaries"));
    }
    Ok(range)
}

fn bool_field(raw: &Value, key: &str) -> Result<bool> {
    match raw.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(malformed("inlay hint padding must be boolean")),
    }
}

fn location(raw: Option<&Value>) -> Result<Value> {
    let Some(raw) = raw.filter(|value| !value.is_null()) else { return Ok(Value::Null) };
    let uri = raw.get("uri").and_then(Value::as_str)
        .filter(|uri| !uri.is_empty() && uri.len() <= 4096 && !uri.chars().any(char::is_control))
        .ok_or_else(|| malformed("hint location needs a bounded URI"))?;
    let range: Range = serde_json::from_value(raw.get("range").cloned().unwrap_or(Value::Null))
        .map_err(|_| malformed("hint location needs a range"))?;
    if range.end < range.start { return Err(malformed("hint location range is reversed")); }
    // No native path conversion, read, network request or navigation occurs.
    Ok(json!({"uri":uri,"range":range}))
}

fn label(raw: &Value) -> Result<(String, Vec<Value>)> {
    match raw.get("label") {
        Some(Value::String(label)) if !label.is_empty() && label.len() <= MAX_LABEL_BYTES => {
            Ok((label.clone(), Vec::new()))
        }
        Some(Value::Array(parts)) if !parts.is_empty() && parts.len() <= MAX_LABEL_PARTS => {
            let mut label = String::new();
            let mut normalized = Vec::with_capacity(parts.len());
            for part in parts {
                let value = part.get("value").and_then(Value::as_str).filter(|value| !value.is_empty())
                    .ok_or_else(|| malformed("hint label parts must have nonempty values"))?;
                if label.len().saturating_add(value.len()) > MAX_LABEL_BYTES {
                    return Err(tool_err("LSP_SEMANTIC_LIMIT", "hint label exceeds 16 KiB"));
                }
                label.push_str(value);
                normalized.push(json!({"value":value,"tooltip":documentation(part.get("tooltip"))?,
                    "location":location(part.get("location"))?,
                    "commandPresent":part.get("command").is_some_and(|command| !command.is_null())}));
            }
            Ok((label, normalized))
        }
        _ => Err(malformed("hint label must be a bounded nonempty string or label-part array")),
    }
}

fn normalize(raw: &Value, text: &str, range: Range, index: usize, resolved: bool) -> Result<Value> {
    bounded_size(raw, MAX_HINT_BYTES)?;
    let position: Position = serde_json::from_value(raw.get("position").cloned().unwrap_or(Value::Null))
        .map_err(|_| malformed("inlay hint needs a position"))?;
    // Hints are zero-width anchors: include both selection endpoints, including
    // a zero-width selection or the exact document end. Never broaden the range.
    if position < range.start || position > range.end || position_to_offset_exact(text, position).is_none() {
        return Err(malformed("hint position is outside the requested range or not an exact UTF-16 boundary"));
    }
    let kind = match raw.get("kind") {
        None | Some(Value::Null) => "unspecified",
        Some(value) if value.as_u64() == Some(1) => "type",
        Some(value) if value.as_u64() == Some(2) => "parameter",
        _ => return Err(malformed("unknown inlay hint kind")),
    };
    let (label, parts) = label(raw)?;
    let text_edits_present = match raw.get("textEdits") {
        None | Some(Value::Null) => false,
        Some(Value::Array(edits)) => !edits.is_empty(),
        _ => return Err(malformed("hint textEdits must be an array")),
    };
    Ok(json!({"index":index,"position":position,"kind":kind,"label":label,"labelParts":parts,
        "tooltip":documentation(raw.get("tooltip"))?,"paddingLeft":bool_field(raw,"paddingLeft")?,
        "paddingRight":bool_field(raw,"paddingRight")?,"textEditsPresent":text_edits_present,"resolved":resolved}))
}

/// Only the advertised detail fields may change during resolve. Retain exact
/// raw data, labels, positions, order, edits, commands and unknown extensions.
fn identity(raw: &Value) -> Value {
    let mut value = raw.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("tooltip");
        if let Some(Value::Array(parts)) = object.get_mut("label") {
            for part in parts {
                if let Some(part) = part.as_object_mut() {
                    part.remove("tooltip");
                    part.remove("location");
                }
            }
        }
    }
    value
}

fn resolve_supported(caps: &Value) -> Result<bool> {
    match caps.get("inlayHintProvider") {
        Some(Value::Bool(true)) => Ok(false),
        Some(Value::Object(options)) => match options.get("resolveProvider") {
            None | Some(Value::Bool(false)) => Ok(false),
            Some(Value::Bool(true)) => Ok(true),
            _ => Err(malformed("inlay hint resolveProvider must be boolean")),
        },
        _ => Err(tool_err("LSP_UNSUPPORTED", "server did not advertise inlayHintProvider")),
    }
}

impl LspTool {
    pub(in crate::lsp) async fn run_inlay_hints(&self, input: &LspInput) -> Result<ToolOutput> {
        read_only_input(input)?;
        if input.position.is_some() {
            return Err(tool_err("LSP_USAGE", "inlay_hints takes an optional range, not position"));
        }
        let budget = Budget::new(self.request_timeout(input));
        let source = Source::open(self, input, &budget, |text| hint_range(text,input.range).map(|_| ())).await?;
        let range = hint_range(&source.text,input.range)?;
        let can_resolve = resolve_supported(&source.entry.client.capabilities().raw)?;
        let resolve = input.resolve == Some(true);
        if resolve && !can_resolve {
            return Err(tool_err("LSP_UNSUPPORTED", "server does not support inlay hint resolution; omit resolve to inspect inline hints"));
        }
        let raw = source.entry.client.call("textDocument/inlayHint", json!({"textDocument":{"uri":source.uri},"range":range}), budget.remaining()?).await?;
        source.verify(&budget)?;
        bounded_size(&raw, MAX_RESPONSE_BYTES)?;
        let hints = match &raw {
            Value::Null => &[][..],
            Value::Array(hints) if hints.len() <= MAX_SERVER_HINTS => hints.as_slice(),
            Value::Array(_) => return Err(tool_err("LSP_SEMANTIC_LIMIT", "server returned more than 4096 hints; narrow the range")),
            _ => return Err(malformed("inlay hints must be an array or null")),
        };
        let limit = input.limit.unwrap_or(50).min(MAX_HINTS);
        let mut items = Vec::with_capacity(hints.len().min(limit));
        // Validate the complete response before resolving any item. Invalid
        // omitted hints cannot be laundered by a small limit.
        for (index, hint) in hints.iter().enumerate() {
            budget.remaining()?;
            let item = normalize(hint, &source.text, range, index, false)?;
            if index < limit { items.push(item); }
        }
        let mut output_bytes = bounded_size(&items, MAX_PAYLOAD_BYTES)?;
        if resolve {
            for (index, item) in items.iter_mut().enumerate() {
                source.verify(&budget)?;
                let resolved = source.entry.client.call("inlayHint/resolve", hints[index].clone(), budget.remaining()?).await?;
                source.verify(&budget)?;
                bounded_size(&resolved, MAX_HINT_BYTES)?;
                if identity(&resolved) != identity(&hints[index]) {
                    return Err(malformed("resolved hint changed fields outside the negotiated detail properties"));
                }
                let next = normalize(&resolved, &source.text, range, index, true)?;
                output_bytes = output_bytes.saturating_sub(bounded_size(item,MAX_PAYLOAD_BYTES)?)
                    .saturating_add(bounded_size(&next,MAX_PAYLOAD_BYTES)?);
                if output_bytes > MAX_PAYLOAD_BYTES {
                    return Err(tool_err("LSP_SEMANTIC_LIMIT", "resolved hints exceed the output budget; narrow the range or limit"));
                }
                *item = next;
            }
        }
        let payload = json!({"action":"inlay_hints","file":display_path(&source.path,&self.cwd),
            "server":source.entry.spec_name,"range":range,"count":items.len(),"total":hints.len(),
            "truncated":hints.len() > limit,"resolveSupported":can_resolve,"resolveRequested":resolve,
            "hints":items,"readOnly":true});
        bounded_size(&payload,MAX_PAYLOAD_BYTES)?;
        source.verify(&budget)?;
        Ok(text_output(payload.to_string(),payload))
    }
}

#[cfg(test)]
mod tests;
