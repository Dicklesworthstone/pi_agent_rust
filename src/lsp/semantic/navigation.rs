//! Exact, source-bound navigation. Returned locations are metadata, not I/O
//! authority: even file URIs are never opened to render or validate a target.

use serde_json::{Value, json};

use super::{Budget, LspInput, LspTool, MAX_RESPONSE_BYTES, Result, Source, bounded_size};
use crate::lsp::client::{hover_to_text, uri_to_path};
use crate::lsp::text::{Position, Range, position_to_offset_exact};
use crate::lsp::{MAX_PAYLOAD_BYTES, display_path, resolve_tool_path, text_output, tool_err};
use crate::tools::ToolOutput;

const MAX_SERVER_LOCATIONS: usize = 4096;
const MAX_LOCATIONS: usize = 1000;
const MAX_URI_BYTES: usize = 8192;
const MAX_LSP_UINTEGER: u32 = 2_147_483_647;

fn malformed(message: &str) -> crate::error::Error {
    tool_err("LSP_NAVIGATION_MALFORMED", message)
}

fn input_file(input: &LspInput) -> Result<&str> {
    let file = input.file.as_deref().filter(|file| !file.is_empty() && file.len() <= MAX_URI_BYTES)
        .ok_or_else(|| tool_err("LSP_USAGE", "navigation requires a bounded file path"))?;
    if (input.position.is_some() && (input.symbol.is_some() || input.line.is_some()))
        || (input.position.is_none() && input.symbol.as_deref().is_none_or(str::is_empty))
        || input.symbol.as_ref().is_some_and(|symbol| symbol.len() > MAX_URI_BYTES)
        || input.line == Some(0) || input.limit == Some(0)
        || input.range.is_some() || input.query.is_some() || input.resolve.is_some()
        || input.apply.is_some() || input.new_name.is_some() || input.new_file.is_some()
        || input.action_id.is_some() || input.refactor_id.is_some()
        || input.completion_id.is_some() || input.snippet_values.is_some()
        || input.hierarchy_id.is_some() || input.only.is_some() || input.after.is_some()
        || input.format_options.is_some() || input.method.is_some() || input.payload.is_some()
    {
        return Err(tool_err("LSP_USAGE", "navigation accepts file and either exact position or symbol with optional one-based line, plus positive limit and timeout; no edit selectors"));
    }
    Ok(file)
}

fn position(raw: &Value) -> Result<Position> {
    let coordinate = |key: &str| -> Result<u32> {
        raw.get(key).and_then(Value::as_u64)
            .filter(|number| *number <= u64::from(MAX_LSP_UINTEGER))
            .and_then(|number| u32::try_from(number).ok())
            .ok_or_else(|| malformed("location positions require LSP unsigned integers"))
    };
    Ok(Position { line: coordinate("line")?, character: coordinate("character")? })
}

fn range(raw: &Value) -> Result<Range> {
    let range = Range { start: position(&raw["start"])?, end: position(&raw["end"])? };
    if range.end < range.start { return Err(malformed("location range is reversed")); }
    Ok(range)
}

fn source_range(raw: &Value, source: &str) -> Result<Range> {
    let range = range(raw)?;
    if position_to_offset_exact(source, range.start).is_none()
        || position_to_offset_exact(source, range.end).is_none()
    {
        return Err(malformed("origin range is not an exact UTF-16 source range"));
    }
    Ok(range)
}

fn uri(raw: &Value) -> Result<&str> {
    let uri = raw.as_str().filter(|uri| !uri.is_empty() && uri.len() <= MAX_URI_BYTES
        && !uri.chars().any(|ch| ch.is_control() || ch.is_whitespace()))
        .ok_or_else(|| malformed("location requires a bounded absolute URI"))?;
    url::Url::parse(uri).map_err(|_| malformed("location URI must be absolute"))?;
    Ok(uri)
}

struct Location<'a> {
    uri: &'a str,
    selection: Range,
    target: Option<Range>,
    origin: Option<Range>,
}

fn location<'a>(raw: &'a Value, source: &str, links: bool) -> Result<Location<'a>> {
    if raw.get("targetUri").is_some() {
        if !links || raw.get("uri").is_some() || raw.get("range").is_some() {
            return Err(malformed("unexpected or mixed location-link form"));
        }
        let target = range(&raw["targetRange"])?;
        let selection = range(&raw["targetSelectionRange"])?;
        if selection.start < target.start || selection.end > target.end {
            return Err(malformed("targetSelectionRange must be contained in targetRange"));
        }
        let origin = raw.get("originSelectionRange")
            .map(|range| source_range(range, source)).transpose()?;
        Ok(Location { uri: uri(&raw["targetUri"])?, selection, target: Some(target), origin })
    } else {
        if raw.get("targetRange").is_some() || raw.get("targetSelectionRange").is_some()
            || raw.get("originSelectionRange").is_some()
        {
            return Err(malformed("location mixes ordinary and link fields"));
        }
        Ok(Location { uri: uri(&raw["uri"])?, selection: range(&raw["range"])?, target: None, origin: None })
    }
}

fn location_items(raw: &Value, references: bool) -> Result<&[Value]> {
    bounded_size(raw, MAX_RESPONSE_BYTES)?;
    let items = match raw {
        Value::Null => &[][..],
        Value::Array(items) => items.as_slice(),
        Value::Object(_) if !references && raw.get("targetUri").is_none() => std::slice::from_ref(raw),
        _ => return Err(malformed("navigation requires Location, a location array or null; references requires an array or null")),
    };
    if items.len() > MAX_SERVER_LOCATIONS {
        return Err(tool_err("LSP_SEMANTIC_LIMIT", "navigation exceeds 4096 server locations"));
    }
    Ok(items)
}

impl Location<'_> {
    fn output(&self, cwd: &std::path::Path) -> Value {
        let file = uri_to_path(self.uri)
            .map_or_else(|| self.uri.to_string(), |path| display_path(&path, cwd));
        let mut value = json!({
            "file":file,"uri":self.uri,"range":self.selection,
            "line":self.selection.start.line+1,"character":self.selection.start.character+1
        });
        if let Some(target) = self.target {
            value["targetUri"] = json!(self.uri);
            value["targetRange"] = json!(target);
            value["targetSelectionRange"] = json!(self.selection);
        }
        if let Some(origin) = self.origin { value["originSelectionRange"] = json!(origin); }
        value
    }
}

fn location_output(raw: &Value, source: &str, cwd: &std::path::Path, limit: usize,
    references: bool, mut payload: Value, budget: &Budget) -> Result<Value>
{
    let all = location_items(raw, references)?;
    payload["locations"] = json!([]);
    payload["total"] = json!(all.len());
    payload["count"] = json!(0);
    payload["truncated"] = json!(false);
    payload["responseComplete"] = json!(true);
    let overhead = bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
    let mut remaining = MAX_PAYLOAD_BYTES.saturating_sub(overhead + 256);
    let mut retained = Vec::new();
    let mut full = false;
    let mut link_form = None;
    // Validate omitted items too. A bad tail must not look like a complete,
    // successful empty result, even with limit:1 or an exhausted output budget.
    for raw in all {
        budget.remaining()?;
        let location = location(raw, source, !references)?;
        let linked = location.target.is_some();
        if link_form.is_some_and(|form| form != linked) {
            return Err(malformed("location and location-link arrays must not be mixed"));
        }
        link_form = Some(linked);
        if full || retained.len() >= limit { continue; }
        let value = location.output(cwd);
        let bytes = bounded_size(&value, MAX_PAYLOAD_BYTES)? + 1;
        if bytes > remaining { full = true; continue; }
        remaining -= bytes;
        retained.push(value);
    }
    payload["count"] = json!(retained.len());
    payload["truncated"] = json!(retained.len() != all.len());
    payload["responseComplete"] = json!(retained.len() == all.len());
    payload["locations"] = json!(retained);
    bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
    Ok(payload)
}

fn marked_string(value: &Value) -> bool {
    value.is_string() || (value.get("language").is_some_and(Value::is_string)
        && value.get("value").is_some_and(Value::is_string) && value.get("kind").is_none())
}

fn hover_output(raw: &Value, source: &str, mut payload: Value) -> Result<Value> {
    bounded_size(raw, MAX_RESPONSE_BYTES)?;
    if !raw.is_null() {
        let contents = raw.get("contents").ok_or_else(|| malformed("hover has no contents"))?;
        let valid = match contents {
            Value::Array(items) => items.iter().all(marked_string),
            value if value.get("kind").is_some() => {
                matches!(value["kind"].as_str(), Some("markdown" | "plaintext"))
                    && value["value"].is_string() && value.get("language").is_none()
            }
            value => marked_string(value),
        };
        if !valid { return Err(malformed("invalid hover content or marked-string member")); }
        if let Some(range) = raw.get("range") { source_range(range, source)?; }
    }
    payload["hover"] = json!(hover_to_text(raw).unwrap_or_else(|| "no hover information".into()));
    payload["contents"] = raw.get("contents").cloned().unwrap_or(Value::Null);
    payload["range"] = raw.get("range").cloned().unwrap_or(Value::Null);
    payload["returnedNull"] = json!(raw.is_null());
    payload["truncated"] = json!(false);
    bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
    Ok(payload)
}

fn capability(caps: &Value, action: &str) -> Result<()> {
    let key = match action {
        "definition" => "definitionProvider",
        "type_definition" => "typeDefinitionProvider",
        "implementation" => "implementationProvider",
        "references" => "referencesProvider",
        "hover" => "hoverProvider",
        _ => return Err(tool_err("LSP_USAGE", "unknown navigation action")),
    };
    match caps.get(key) {
        // Missing metadata preserves older servers' request behavior; it does
        // not certify support. An explicit refusal must not be ignored.
        None | Some(Value::Null | Value::Bool(true) | Value::Object(_)) => Ok(()),
        Some(Value::Bool(false)) => Err(tool_err("LSP_UNSUPPORTED", "server disabled this navigation operation")),
        _ => Err(malformed("invalid navigation provider capability")),
    }
}

impl LspTool {
    pub(in crate::lsp) async fn run_position_request(
        &self, input: &LspInput, action: &str, method: &str, extra_params: Value,
    ) -> Result<ToolOutput> {
        let file = input_file(input)?;
        let budget = Budget::new(self.request_timeout(input));
        let path = resolve_tool_path(file, &self.cwd);
        let mut selected = None;
        let source = Source::open(self, input, &budget, |text| {
            let position = match input.position {
                Some(position) => position,
                None => Self::resolve_position_in(&path, text, input.line, input.symbol.as_deref().unwrap_or_default())?,
            };
            if position.line > MAX_LSP_UINTEGER || position.character > MAX_LSP_UINTEGER
                || position_to_offset_exact(text, position).is_none()
            {
                return Err(tool_err("LSP_USAGE", "navigation position must be an exact zero-based UTF-16 source boundary"));
            }
            selected = Some(position);
            Ok(())
        }).await?;
        let position = selected.ok_or_else(|| tool_err("LSP_USAGE", "missing navigation position"))?;
        capability(&source.entry.client.capabilities().raw, action)?;
        let mut params = json!({"textDocument":{"uri":source.uri},"position":position});
        if let Some(extra) = extra_params.as_object() {
            for (key, value) in extra { params[key] = value.clone(); }
        }
        source.verify(&budget)?;
        let raw = source.entry.client.call(method, params, budget.remaining()?).await?;
        source.verify(&budget)?;
        let payload = json!({
            "action":action,"file":display_path(&source.path,&self.cwd),
            "position":position,"line":position.line+1,"server":source.entry.spec_name,
            "note":"Source-bound server result, not an exhaustive or atomic project snapshot. Target URIs are metadata only."
        });
        let payload = if action == "hover" {
            hover_output(&raw, &source.text, payload)?
        } else {
            location_output(&raw, &source.text, &self.cwd,
                input.limit.unwrap_or(100).min(MAX_LOCATIONS), action == "references", payload, &budget)?
        };
        source.verify(&budget)?;
        let text = payload.to_string();
        budget.remaining()?;
        Ok(text_output(text, payload))
    }
}

#[cfg(test)]
mod tests;
