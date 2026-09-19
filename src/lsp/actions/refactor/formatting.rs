//! Native formatter requests with explicit preview/apply and source evidence.
//!
//! A formatting response is a TextEdit array for exactly one document, never
//! a WorkspaceEdit or a command. Range formatting may expand to a syntactic
//! construct, but it cannot select another file or grant server edit permission.

use std::io::Read as _;

use super::{
    AgentCx, LspInput, LspTool, Path, RefactorSnapshot, Result, ToolOutput, Value,
    check_response_size, display_path, json, parse_workspace_edit, resolve_tool_path, tool_err,
};
use crate::lsp::text::{
    Range, apply_text_edits, content_hash_for_drift, position_to_offset_exact,
};

const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_FORMAT_EDITS: usize = 32768;
const MAX_PREVIEW_BYTES: usize = 64 * 1024;

fn read_source(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(tool_err("LSP_FILE_UNREADABLE", "format requires a regular file, not a directory or symlink"));
    }
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(tool_err("LSP_FILE_UNREADABLE", "format source is not a regular file"));
    }
    if metadata.len() > MAX_SOURCE_BYTES as u64 {
        return Err(tool_err("LSP_EDIT_LIMIT", "format source exceeds 16 MiB"));
    }
    let mut source = String::new();
    file.take(MAX_SOURCE_BYTES as u64 + 1).read_to_string(&mut source)?;
    if source.len() > MAX_SOURCE_BYTES {
        return Err(tool_err("LSP_EDIT_LIMIT", "format source exceeds 16 MiB"));
    }
    Ok(source)
}

fn options(raw: Option<&Value>) -> Result<Value> {
    let mut options = json!({"tabSize":4,"insertSpaces":true});
    let Some(raw) = raw else { return Ok(options) };
    let object = raw.as_object()
        .ok_or_else(|| tool_err("LSP_USAGE", "formatOptions must be an object"))?;
    if object.len() > 64 {
        return Err(tool_err("LSP_USAGE", "formatOptions exceeds 64 fields"));
    }
    for (name, value) in object {
        if name.is_empty() || name.len() > 128 {
            return Err(tool_err("LSP_USAGE", "invalid format option name"));
        }
        let valid = match name.as_str() {
            "tabSize" => value.as_u64().is_some_and(|size| (1..=32).contains(&size)),
            "insertSpaces" | "trimTrailingWhitespace" | "insertFinalNewline"
                | "trimFinalNewlines" => value.is_boolean(),
            _ => value.is_boolean()
                || value.as_i64().is_some_and(|number| i32::try_from(number).is_ok())
                || value.as_str().is_some_and(|text| text.len() <= 512),
        };
        if !valid {
            return Err(tool_err("LSP_USAGE", format!("invalid formatting option {name:?}")));
        }
        options[name] = value.clone();
    }
    Ok(options)
}

fn validate_range(source: &str, range: Range) -> Result<()> {
    if range.end < range.start
        || position_to_offset_exact(source, range.start).is_none()
        || position_to_offset_exact(source, range.end).is_none()
    {
        return Err(tool_err("LSP_USAGE", "format range must use ordered, in-bounds zero-based UTF-16 positions"));
    }
    Ok(())
}

fn require_capability(capabilities: &Value, ranged: bool) -> Result<()> {
    let name = if ranged { "documentRangeFormattingProvider" } else { "documentFormattingProvider" };
    match capabilities.get(name) {
        Some(Value::Bool(true) | Value::Object(_)) => Ok(()),
        None | Some(Value::Bool(false)) => Err(tool_err(
            "LSP_FORMAT_UNSUPPORTED", format!("server did not advertise {name}"),
        )),
        Some(_) => Err(tool_err("LSP_FORMAT_PROTOCOL", "invalid formatting capability")),
    }
}

/// Preview only complete edits that fit. Never return an unlabelled truncated
/// TextEdit that a caller could mistake for the actual replacement payload.
fn preview(edits: &[Value]) -> Result<(Vec<Value>, bool)> {
    let mut shown = Vec::new();
    let mut bytes = 2usize;
    for edit in edits {
        let size = serde_json::to_vec(edit)?.len().saturating_add(1);
        if size > MAX_PREVIEW_BYTES.saturating_sub(bytes) {
            break;
        }
        bytes += size;
        shown.push(edit.clone());
    }
    let truncated = shown.len() != edits.len();
    Ok((shown, truncated))
}

impl LspTool {
    pub(in crate::lsp) async fn run_format(&self, input: &LspInput) -> Result<ToolOutput> {
        let file = input.file.as_deref().filter(|path| !path.is_empty())
            .ok_or_else(|| tool_err("LSP_USAGE", "format requires file"))?;
        if input.line.is_some() || input.symbol.is_some() {
            return Err(tool_err("LSP_USAGE", "format uses range, not line or symbol"));
        }
        let options = options(input.format_options.as_ref())?;
        let owner = AgentCx::for_current_or_request();
        owner.checkpoint().map_err(|_| tool_err("LSP_CANCELLED", "format cancelled before reading"))?;
        let requested = resolve_tool_path(file, &self.cwd);
        let source = read_source(&requested)?;
        if let Some(range) = input.range {
            validate_range(&source, range)?;
        }
        let path = requested.canonicalize()?;
        let hash = content_hash_for_drift(&source);
        let (uri, entry) = self.synced(&path).await?;
        let snapshot = RefactorSnapshot::capture(&entry, &path, hash)?;
        let ranged = input.range.is_some();
        require_capability(&entry.client.capabilities().raw, ranged)?;
        let method = if ranged { "textDocument/rangeFormatting" } else { "textDocument/formatting" };
        let mut params = json!({"textDocument":{"uri":uri},"options":options});
        if let Some(range) = input.range {
            params["range"] = serde_json::to_value(range)?;
        }
        let response = entry.client.call(method, params, self.request_timeout(input)).await?;
        check_response_size(&response)?;
        let returned_null = response.is_null();
        let edits = match response {
            Value::Null => Vec::new(),
            Value::Array(edits) if edits.len() <= MAX_FORMAT_EDITS => edits,
            Value::Array(_) => return Err(tool_err("LSP_EDIT_LIMIT", "too many formatting edits")),
            _ => return Err(tool_err("LSP_FORMAT_PROTOCOL", "formatting result must be a TextEdit array or null")),
        };
        // Check the standard TextEdit shape; do not accept a command, file
        // operation or the nonstandard textEdit wrapper accepted elsewhere.
        for edit in &edits {
            if !edit.is_object() || edit.get("range").is_none()
                || edit.get("newText").and_then(Value::as_str).is_none()
            {
                return Err(tool_err("LSP_FORMAT_PROTOCOL", "invalid formatting TextEdit"));
            }
        }
        let count = edits.len();
        let workspace = json!({"documentChanges":[{
            "textDocument":{"uri":uri,"version":null},"edits":edits
        }]});
        check_response_size(&workspace)?;
        let plan = parse_workspace_edit(&workspace)?;
        // A canonical Windows path may carry a verbatim prefix that is not
        // part of the equivalent native path decoded from its file URI.
        let document_path = crate::lsp::client::uri_to_path(&uri)
            .ok_or_else(|| tool_err("LSP_FORMAT_PROTOCOL", "invalid formatting document URI"))?;
        let parsed = plan.text_edits.get(&document_path)
            .ok_or_else(|| tool_err("LSP_FORMAT_PROTOCOL", "formatter document identity changed"))?;
        // Match the transaction's byte admission before building a preview.
        // Filesystem eligibility is still rechecked by the apply transaction.
        let admitted = parsed.iter().try_fold(source.len(), |bytes, edit| {
            bytes.checked_add(edit.new_text.len()).filter(|size| *size <= MAX_SOURCE_BYTES)
        });
        if admitted.is_none() {
            return Err(tool_err("LSP_EDIT_LIMIT", "format source plus replacements exceeds 16 MiB"));
        }
        let updated = apply_text_edits(&source, parsed)
            .map_err(|error| tool_err("LSP_EDIT_CONFLICT", error))?;
        snapshot.validate(&entry, &workspace, &plan)?;
        owner.checkpoint().map_err(|_| tool_err("LSP_CANCELLED", "format cancelled before delivery"))?;
        if !entry.client.is_alive() {
            return Err(tool_err("LSP_TRANSPORT_CLOSED", "formatting connection closed"));
        }
        let changed = source != updated;
        let apply = input.apply.unwrap_or(false);
        if apply && changed {
            self.apply_refactor(&entry, &workspace, &snapshot, &owner)?;
        }
        let mut payload = json!({
            "action":"format","file":display_path(&path,&self.cwd),"server":entry.spec_name,
            "mode":if ranged { "range" } else { "document" },
            "previewOnly":!apply,"applied":apply && changed,"changed":changed,
            "editCount":count,"returnedNull":returned_null,
            "beforeBytes":source.len(),"afterBytes":updated.len(),"formatOptions":options,
            "rollbackOnError":true
        });
        if !apply {
            let edits = workspace["documentChanges"][0]["edits"].as_array()
                .expect("constructed formatting edit array");
            let (shown, truncated) = preview(edits)?;
            payload["edits"] = json!(shown);
            payload["previewTruncated"] = json!(truncated);
            payload["note"] = json!("Preview only. apply:true requests formatting again and rechecks source freshness; it does not replay this preview.");
        }
        Ok(crate::lsp::text_output(payload.to_string(), payload))
    }
}

#[cfg(test)]
mod tests;
