//! Workspace navigation, including lazily supplied symbol locations.
//!
//! The anchor selects one server/root. Returned URIs remain opaque metadata:
//! neither local targets nor external documents are opened by this workflow.

use serde_json::{Value, json};

use super::{Budget, LspInput, LspTool, MAX_RESPONSE_BYTES, Result, Source, bounded_size, uint};
use crate::lsp::{MAX_PAYLOAD_BYTES, display_path, text_output, tool_err};
use crate::tools::ToolOutput;

const MAX_SERVER_ITEMS: usize = 4096;
const MAX_ITEMS: usize = 128;
const MAX_ITEM_BYTES: usize = 64 * 1024;
const MAX_QUERY_BYTES: usize = 1024;

fn malformed(message: &str) -> crate::error::Error {
    tool_err("LSP_SYMBOL_MALFORMED", message)
}

fn request(input: &LspInput) -> Result<(&str, &str)> {
    let query = input.query.as_deref().ok_or_else(|| {
        tool_err("LSP_USAGE", "workspace symbols require query and an anchor file")
    })?;
    let anchor = match (input.file.as_deref(), input.symbol.as_deref()) {
        (Some(file), None) | (None, Some(file)) if !file.is_empty() && file.len() <= 8192 => file,
        _ => return Err(tool_err("LSP_USAGE", "workspace symbols require one anchor file: file or symbol, not both")),
    };
    if query.len() > MAX_QUERY_BYTES || query.contains('\0') || input.limit == Some(0)
        || input.position.is_some() || input.line.is_some() || input.range.is_some()
        || input.apply.is_some() || input.new_name.is_some() || input.new_file.is_some()
        || input.action_id.is_some() || input.refactor_id.is_some()
        || input.completion_id.is_some() || input.snippet_values.is_some()
        || input.hierarchy_id.is_some() || input.method.is_some() || input.payload.is_some()
        || input.only.is_some() || input.after.is_some() || input.format_options.is_some()
    {
        return Err(tool_err("LSP_USAGE", "workspace symbols accept only anchor, query (up to 1024 bytes without NUL), positive limit, resolve and timeout"));
    }
    Ok((anchor, query))
}

fn resolve_supported(capabilities: &Value) -> Result<bool> {
    match capabilities.get("workspaceSymbolProvider") {
        // Missing metadata is not evidence of lazy resolution support. Keep
        // older servers' ordinary workspace/symbol request behavior.
        None | Some(Value::Null | Value::Bool(true)) => Ok(false),
        Some(Value::Bool(false)) => Err(tool_err("LSP_UNSUPPORTED", "server disabled workspace symbols")),
        Some(Value::Object(options)) => match options.get("resolveProvider") {
            None => Ok(false),
            Some(Value::Bool(supported)) => Ok(*supported),
            _ => Err(malformed("workspace symbol resolveProvider must be boolean")),
        },
        _ => Err(malformed("workspaceSymbolProvider must be boolean or options")),
    }
}

fn range(value: &Value) -> Result<()> {
    let position = |key: &str| -> Result<(usize, usize)> {
        let position = value.get(key).ok_or_else(|| malformed("symbol range needs start and end"))?;
        Ok((uint(&position["line"])?, uint(&position["character"])?))
    };
    if position("start")? > position("end")? {
        return Err(malformed("symbol location range is reversed"));
    }
    Ok(())
}

/// Validate every item, including items beyond the caller's output limit.
/// Unknown numeric kinds/tags are preserved rather than treated as known ones.
fn item(value: &Value) -> Result<bool> {
    bounded_size(value, MAX_ITEM_BYTES)?;
    if value.get("name").and_then(Value::as_str).is_none_or(str::is_empty)
        || uint(&value["kind"])? == 0
    {
        return Err(malformed("workspace symbol needs a name and positive kind"));
    }
    if let Some(container) = value.get("containerName").filter(|v| !v.is_null())
        && !container.is_string()
    {
        return Err(malformed("symbol containerName must be a string"));
    }
    if let Some(deprecated) = value.get("deprecated").filter(|v| !v.is_null())
        && !deprecated.is_boolean()
    {
        return Err(malformed("symbol deprecated must be boolean"));
    }
    if let Some(tags) = value.get("tags").filter(|v| !v.is_null()) {
        for tag in tags.as_array().ok_or_else(|| malformed("symbol tags must be an array"))? {
            uint(tag)?;
        }
    }
    let location = value.get("location").and_then(Value::as_object)
        .ok_or_else(|| malformed("workspace symbol needs a location"))?;
    let uri = location.get("uri").and_then(Value::as_str)
        .filter(|uri| !uri.is_empty() && uri.len() <= 8192 && !uri.chars().any(char::is_control))
        .ok_or_else(|| malformed("symbol location needs a bounded URI"))?;
    url::Url::parse(uri).map_err(|_| malformed("symbol location URI must be absolute"))?;
    if let Some(value) = location.get("range") {
        range(value)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn items(raw: &Value) -> Result<&[Value]> {
    bounded_size(raw, MAX_RESPONSE_BYTES)?;
    let items = match raw {
        Value::Null => &[][..],
        Value::Array(items) => items.as_slice(),
        _ => return Err(malformed("workspace/symbol must return an array or null")),
    };
    if items.len() > MAX_SERVER_ITEMS {
        return Err(tool_err("LSP_SEMANTIC_LIMIT", "server returned more than 4096 workspace symbols"));
    }
    for value in items { item(value)?; }
    Ok(items)
}

/// Resolution may add only location.range, as negotiated. It must not move a
/// result to a different URI or replace the symbol/name/kind/opaque data.
fn resolved(original: &Value, result: Value) -> Result<Value> {
    if !item(&result)? {
        return Err(malformed("resolved symbol still has no location range"));
    }
    let mut identity = result.clone();
    identity["location"].as_object_mut().ok_or_else(|| malformed("missing location"))?.remove("range");
    if &identity != original {
        return Err(tool_err("LSP_SYMBOL_CHANGED", "workspaceSymbol/resolve changed symbol identity or unnegotiated properties"));
    }
    Ok(result)
}

impl LspTool {
    pub(in crate::lsp) async fn run_workspace_symbols(&self, input: &LspInput) -> Result<ToolOutput> {
        let (anchor, query) = request(input)?;
        let budget = Budget::new(self.request_timeout(input));
        let source = Source::open_file(self, anchor, &budget, |_| Ok(())).await?;
        let can_resolve = resolve_supported(&source.entry.client.capabilities().raw)?;
        let resolve = input.resolve == Some(true);
        if resolve && !can_resolve {
            return Err(tool_err("LSP_UNSUPPORTED", "server does not advertise workspace symbol resolution; omit resolve for inline locations"));
        }
        source.verify(&budget)?;
        let raw = source.entry.client.call("workspace/symbol", json!({"query":query}), budget.remaining()?).await?;
        source.verify(&budget)?;
        let all = items(&raw)?;
        let limit = input.limit.unwrap_or(50).min(MAX_ITEMS);
        let mut payload = json!({
            "action":"symbols","scope":"workspace","query":query,
            "anchor":display_path(&source.path,&self.cwd),"server":source.entry.spec_name,
            "workspace":display_path(source.entry.client.root(),&self.cwd),
            "count":0,"total":all.len(),"truncated":false,"responseComplete":true,
            "resolveRequested":resolve,"resolveSupported":can_resolve,"resolvedCount":0,
            "locationsComplete":true,"unresolvedIndices":[],"symbols":[],
            "note":"One server/root response, not an exhaustive or atomic project snapshot. Target URIs are metadata only."
        });
        // Reserve final counts, brackets, commas and at most 128 unresolved
        // indices before cloning output. No text-truncation of JSON objects.
        let overhead = bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
        let mut remaining = MAX_PAYLOAD_BYTES.saturating_sub(overhead + 2048);
        let mut retained = Vec::new();
        let mut unresolved = Vec::new();
        let mut resolved_count = 0;
        for original in all.iter().take(limit) {
            budget.remaining()?;
            let original_size = bounded_size(original, MAX_ITEM_BYTES)?;
            if original_size + 1 > remaining { break; }
            let has_location = original["location"].get("range").is_some();
            let value = if resolve && !has_location {
                source.verify(&budget)?;
                let result = source.entry.client.call(
                    "workspaceSymbol/resolve", original.clone(), budget.remaining()?,
                ).await?;
                source.verify(&budget)?;
                let result = resolved(original, result)?;
                resolved_count += 1;
                result
            } else { original.clone() };
            let bytes = bounded_size(&value, MAX_ITEM_BYTES)?;
            if bytes + 1 > remaining {
                return Err(tool_err("LSP_SEMANTIC_LIMIT", "resolved symbol output exceeds its byte budget; request fewer results"));
            }
            remaining -= bytes + 1;
            if !has_location && !resolve { unresolved.push(retained.len() + 1); }
            retained.push(value);
        }
        let truncated = retained.len() != all.len();
        payload["count"] = json!(retained.len());
        payload["truncated"] = json!(truncated);
        payload["responseComplete"] = json!(!truncated);
        payload["resolvedCount"] = json!(resolved_count);
        payload["locationsComplete"] = json!(unresolved.is_empty());
        payload["unresolvedIndices"] = json!(unresolved);
        payload["symbols"] = json!(retained);
        bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
        source.verify(&budget)?;
        let text = payload.to_string();
        budget.remaining()?;
        // Retain the existing symbols details envelope for both query forms.
        Ok(text_output(text, json!({"truncated":truncated,"payload":payload})))
    }
}

#[cfg(test)]
mod tests;
