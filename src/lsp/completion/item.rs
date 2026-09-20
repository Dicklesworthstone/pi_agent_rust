//! Completion wire defaults and edit semantics, without guessing word boundaries.

use serde_json::{Value, json};

use super::{MAX_ITEM_BYTES, bounded_size, malformed};
use crate::error::Result;
use crate::lsp::text::{Position, Range, position_to_offset_exact};
use crate::lsp::tool_err;

pub(super) fn label(item: &Value) -> Result<&str> {
    item.get("label").and_then(Value::as_str)
        .filter(|label| !label.is_empty() && label.len() <= 2048)
        .ok_or_else(|| malformed("completion label must be a nonempty string of at most 2048 bytes"))
}

/// Defaults are materialized before caching and resolving, including opaque
/// data. Explicit item values win. We do not negotiate 3.18 merge applyKinds.
pub(super) fn materialize(raw: &Value, defaults: Option<&Value>) -> Result<Value> {
    label(raw)?;
    bounded_size(raw, MAX_ITEM_BYTES)?;
    let mut item = raw.clone();
    if let Some(defaults) = defaults {
        let defaults = defaults.as_object().ok_or_else(|| malformed("itemDefaults must be an object"))?;
        for (key, value) in defaults {
            if !matches!(key.as_str(), "editRange" | "insertTextFormat" | "insertTextMode" | "data") {
                return Err(malformed("server returned an unnegotiated completion default"));
            }
            if key != "editRange" && item.get(key).is_none_or(Value::is_null) {
                item[key] = value.clone();
            }
        }
        if item.get("textEdit").is_none_or(Value::is_null)
            && let Some(range) = defaults.get("editRange")
        {
            // textEditText, not insertText, supplies the default-range text.
            let text = item.get("textEditText").filter(|value| !value.is_null())
                .unwrap_or(&item["label"]).as_str()
                .ok_or_else(|| malformed("textEditText must be a string"))?;
            item["textEdit"] = if range.get("insert").is_some() || range.get("replace").is_some() {
                json!({"newText":text,"insert":range.get("insert"),"replace":range.get("replace")})
            } else {
                json!({"newText":text,"range":range})
            };
        }
    }
    bounded_size(&item, MAX_ITEM_BYTES)?;
    Ok(item)
}

pub(super) fn parsed_range(raw: &Value, source: &str) -> Result<Range> {
    let range: Range = serde_json::from_value(raw.clone())
        .map_err(|_| malformed("invalid completion edit range"))?;
    if range.end < range.start
        || position_to_offset_exact(source, range.start).is_none()
        || position_to_offset_exact(source, range.end).is_none()
    {
        return Err(malformed("completion edit must use exact, ordered UTF-16 boundaries"));
    }
    Ok(range)
}

pub(super) fn primary_range(range: Range, position: Position) -> Result<()> {
    if range.start.line != range.end.line || position < range.start || position > range.end {
        return Err(malformed("primary completion range must be single-line and contain the requested cursor"));
    }
    Ok(())
}

fn plain_text_mode(item: &Value) -> Result<()> {
    for property in ["insertTextFormat", "insertTextMode"] {
        if item.get(property).filter(|value| !value.is_null()).is_some_and(|value| value.as_u64() != Some(1)) {
            return Err(tool_err("LSP_COMPLETION_UNSUPPORTED", "snippets and indentation-adjusting completions are not supported"));
        }
    }
    if item.get("command").is_some_and(|value| !value.is_null()) {
        return Err(tool_err("LSP_COMPLETION_UNSUPPORTED", "command-backed completions cannot be applied; no command or partial insertion was performed"));
    }
    Ok(())
}

/// Only the three advertised resolve properties may change. Never let resolve
/// substitute a different insertion, filter identity, command, or opaque data.
pub(super) fn resolved(before: &Value, after: &Value) -> Result<Value> {
    label(after)?;
    bounded_size(after, MAX_ITEM_BYTES)?;
    let before_object = before.as_object().ok_or_else(|| malformed("completion item must be an object"))?;
    let after_object = after.as_object().ok_or_else(|| malformed("resolved completion must be an object"))?;
    for key in before_object.keys().chain(after_object.keys()) {
        if !matches!(key.as_str(), "detail" | "documentation" | "additionalTextEdits")
            && before.get(key) != after.get(key)
        {
            return Err(malformed("completion resolve changed a non-resolvable property"));
        }
    }
    Ok(after.clone())
}

/// The completion primary edit and auto-imports all address the original
/// source image. Return plain TextEdits for the shared workspace transaction.
pub(super) fn edits(item: &Value, source: &str, cursor: Position, fallback: Option<Range>) -> Result<Vec<Value>> {
    plain_text_mode(item)?;
    let main = if let Some(edit) = item.get("textEdit").filter(|value| !value.is_null()) {
        let object = edit.as_object().ok_or_else(|| malformed("completion textEdit must be an object"))?;
        if object.keys().any(|key| !matches!(key.as_str(), "range" | "insert" | "replace" | "newText")) {
            return Err(malformed("completion textEdit contains unsupported fields"));
        }
        let text = edit.get("newText").and_then(Value::as_str)
            .ok_or_else(|| malformed("completion textEdit needs newText"))?;
        let range = if let Some(range) = edit.get("range") {
            if edit.get("insert").is_some() || edit.get("replace").is_some() {
                return Err(malformed("completion textEdit mixes range and insert/replace"));
            }
            parsed_range(range, source)?
        } else {
            let insert = parsed_range(&edit["insert"], source)?;
            let replace = parsed_range(&edit["replace"], source)?;
            primary_range(insert, cursor)?;
            if insert.start != replace.start || insert.end > replace.end {
                return Err(malformed("completion insert range must be a prefix of its replace range"));
            }
            // Explicitly use replacement semantics; preview shows this range.
            replace
        };
        primary_range(range, cursor)?;
        json!({"range":range,"newText":text})
    } else {
        let range = fallback.ok_or_else(|| tool_err(
            "LSP_COMPLETION_RANGE_REQUIRED",
            "server omitted textEdit; list again with an explicit replacement range instead of guessing language-specific word boundaries",
        ))?;
        parsed_range(&json!(range), source)?;
        primary_range(range, cursor)?;
        let text = item.get("insertText").filter(|value| !value.is_null()).unwrap_or(&item["label"])
            .as_str().ok_or_else(|| malformed("insertText must be a string"))?;
        json!({"range":range,"newText":text})
    };
    let mut edits = vec![main];
    if let Some(additional) = item.get("additionalTextEdits").filter(|value| !value.is_null()) {
        let additional = additional.as_array().ok_or_else(|| malformed("additionalTextEdits must be an array"))?;
        if additional.len() > 128 {
            return Err(tool_err("LSP_COMPLETION_LIMIT", "completion has more than 128 additional edits"));
        }
        for edit in additional {
            // No annotated edits, URI overrides, resource operations or wrappers
            // are accepted in this TextEdit-only surface.
            let object = edit.as_object().ok_or_else(|| malformed("additional edit must be an object"))?;
            if object.keys().any(|key| key != "range" && key != "newText") {
                return Err(malformed("additional edit contains unsupported fields"));
            }
            let range = parsed_range(&edit["range"], source)?;
            let text = edit.get("newText").and_then(Value::as_str)
                .ok_or_else(|| malformed("additional edit needs newText"))?;
            edits.push(json!({"range":range,"newText":text}));
        }
    }
    let mut spans = edits.iter().map(|edit| parsed_range(&edit["range"], source)).collect::<Result<Vec<_>>>()?;
    spans.sort_by_key(|range| (range.start, range.end));
    for pair in spans.windows(2) {
        if pair[0].end > pair[1].start
            || (pair[0].start == pair[1].start)
        {
            return Err(malformed("completion edits overlap, including a shared insertion position"));
        }
    }
    Ok(edits)
}

#[cfg(test)]
mod tests;
