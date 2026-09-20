//! Call-site signatures, overload choice and normalized parameter labels.

use serde_json::{Value, json};

use super::{Budget, MAX_RESPONSE_BYTES, Source, bounded_size, documentation, malformed, optional_index, read_only_input, uint};
use crate::error::Result;
use crate::lsp::text::{Position, position_to_offset_exact};
use crate::lsp::{LspInput, LspTool, MAX_PAYLOAD_BYTES, display_path, text_output, tool_err};
use crate::tools::ToolOutput;

const MAX_SIGNATURES: usize = 128;
const MAX_PARAMETERS: usize = 128;
const MAX_LABEL_BYTES: usize = 16 * 1024;

/// Signature offsets index the entire label's UTF-16 representation, not a
/// document line. Never clamp an out-of-range or surrogate-interior offset.
fn label_offset(label: &str, offset: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, ch) in label.char_indices() {
        if units == offset { return Some(byte); }
        units += ch.len_utf16();
        if units > offset { return None; }
    }
    (units == offset).then_some(label.len())
}

fn parameter(raw: &Value, label: &str, index: usize) -> Result<Value> {
    let (text, offsets) = match raw.get("label") {
        Some(Value::String(text)) if text.len() <= MAX_LABEL_BYTES => (text.clone(), Value::Null),
        Some(Value::Array(pair)) if pair.len() == 2 => {
            let start = uint(&pair[0])?;
            let end = uint(&pair[1])?;
            if end < start { return Err(malformed("parameter label offsets are reversed")); }
            let (Some(start_byte), Some(end_byte)) = (label_offset(label, start), label_offset(label, end)) else {
                return Err(malformed("parameter label offsets are not exact UTF-16 boundaries"));
            };
            (label[start_byte..end_byte].to_string(), json!([start,end]))
        }
        _ => return Err(malformed("parameter needs a string label or a UTF-16 offset pair")),
    };
    Ok(json!({"index":index,"label":text,"labelOffsets":offsets,"documentation":documentation(raw.get("documentation"))?}))
}

fn parse(raw: &Value, limit: usize) -> Result<Value> {
    bounded_size(raw, MAX_RESPONSE_BYTES)?;
    if raw.is_null() {
        return Ok(json!({"signatures":[],"count":0,"total":0,"activeSignature":null,"activeParameter":null,"truncated":false}));
    }
    let signatures = raw.get("signatures").and_then(Value::as_array)
        .ok_or_else(|| malformed("signature help needs a signatures array or null"))?;
    if signatures.len() > MAX_SIGNATURES {
        return Err(tool_err("LSP_SEMANTIC_LIMIT", "server returned more than 128 signatures"));
    }
    let active = optional_index(raw.get("activeSignature"))?.filter(|index| *index < signatures.len()).unwrap_or(0);
    let global_parameter = optional_index(raw.get("activeParameter"))?;
    let mut normalized = Vec::with_capacity(signatures.len());
    for (index, raw) in signatures.iter().enumerate() {
        let label = raw.get("label").and_then(Value::as_str).filter(|label| label.len() <= MAX_LABEL_BYTES)
            .ok_or_else(|| malformed("signature needs a label of at most 16 KiB"))?;
        let parameters = match raw.get("parameters") {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(parameters)) if parameters.len() <= MAX_PARAMETERS => parameters.as_slice(),
            _ => return Err(malformed("signature parameters must be a bounded array")),
        };
        let chosen = optional_index(raw.get("activeParameter"))?
            .or_else(|| (index == active).then_some(global_parameter).flatten());
        // The 3.17 protocol defaults an omitted/out-of-range active parameter
        // to zero when parameters exist. A zero-argument signature has none.
        let chosen = if parameters.is_empty() { None } else if index == active || chosen.is_some() {
            Some(chosen.filter(|index| *index < parameters.len()).unwrap_or(0))
        } else { None };
        let parameters: Vec<_> = parameters.iter().enumerate()
            .map(|(index, raw)| parameter(raw, label, index)).collect::<Result<_>>()?;
        normalized.push(json!({"index":index,"label":label,"documentation":documentation(raw.get("documentation"))?,
            "parameters":parameters,"activeParameter":chosen}));
    }
    let active_parameter = normalized.get(active).map_or(Value::Null, |item| item["activeParameter"].clone());
    let total = normalized.len();
    let limit = limit.clamp(1, MAX_SIGNATURES);
    // Keep server indices, ordering and the active overload, even when it lies
    // beyond the first page. Never silently point to a discarded overload.
    let active_item = (active >= limit).then(|| normalized[active].clone());
    normalized.truncate(limit);
    if let Some(active_item) = active_item {
        if let Some(last) = normalized.last_mut() { *last = active_item; }
    }
    Ok(json!({"count":normalized.len(),"total":total,"signatures":normalized,"activeSignature":(!signatures.is_empty()).then_some(active),
        "activeParameter":active_parameter,"truncated":total > limit}))
}

impl LspTool {
    pub(in crate::lsp) async fn run_signature_help(&self, input: &LspInput) -> Result<ToolOutput> {
        read_only_input(input)?;
        if input.range.is_some() {
            return Err(tool_err("LSP_USAGE", "signature_help takes position, not a range"));
        }
        let position: Position = input.position.ok_or_else(|| tool_err("LSP_USAGE", "signature_help requires file and exact position inside the call"))?;
        let budget = Budget::new(self.request_timeout(input));
        let source = Source::open(self, input, &budget, |text| {
            position_to_offset_exact(text, position).map(|_| ())
                .ok_or_else(|| tool_err("LSP_USAGE", "signature position must be an exact zero-based UTF-16 boundary"))
        }).await?;
        if !source.entry.client.capabilities().raw.get("signatureHelpProvider").is_some_and(Value::is_object) {
            return Err(tool_err("LSP_UNSUPPORTED", "server did not advertise signatureHelpProvider"));
        }
        let raw = source.entry.client.call("textDocument/signatureHelp", json!({
            "textDocument":{"uri":source.uri},"position":position,
            "context":{"triggerKind":1,"isRetrigger":false}
        }), budget.remaining()?).await?;
        source.verify(&budget)?;
        let mut payload = parse(&raw, input.limit.unwrap_or(16))?;
        payload["action"] = json!("signature_help");
        payload["file"] = json!(display_path(&source.path, &self.cwd));
        payload["server"] = json!(source.entry.spec_name);
        payload["position"] = json!(position);
        bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
        source.verify(&budget)?;
        Ok(text_output(payload.to_string(), payload))
    }
}

#[cfg(test)]
mod tests;
