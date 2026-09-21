//! Native formatter requests with explicit preview/apply and source evidence.
//!
//! A formatting response is a TextEdit array for exactly one document, never
//! a WorkspaceEdit or a command. Range formatting may expand to a syntactic
//! construct, but it cannot select another file or grant server edit permission.

use std::io::Read as _;
use std::time::{Duration, Instant};

use super::{
    AgentCx, LspInput, LspTool, Path, RefactorSnapshot, Result, ServerEntry, ToolOutput, Value,
    check_response_size, display_path, json, parse_workspace_edit, resolve_tool_path, tool_err,
};
use crate::lsp::text::{Range, apply_text_edits, content_hash_for_drift, position_to_offset_exact};

const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_FORMAT_EDITS: usize = 32768;
const MAX_PREVIEW_BYTES: usize = 64 * 1024;

struct FormatBudget {
    owner: AgentCx,
    started: Instant,
    timeout: Duration,
}

impl FormatBudget {
    fn new(timeout: Duration) -> Self {
        Self {
            owner: AgentCx::for_current_or_request(),
            started: Instant::now(),
            timeout,
        }
    }

    fn remaining(&self) -> Result<Duration> {
        crate::lsp::refactor_preview::check_owner(&self.owner)?;
        self.timeout
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| tool_err("LSP_TIMEOUT", "format request budget expired"))
    }

    fn verify(&self, entry: &ServerEntry, snapshot: &RefactorSnapshot) -> Result<()> {
        self.remaining()?;
        if !entry.client.is_alive() {
            return Err(tool_err("LSP_TRANSPORT_CLOSED", "formatting connection closed"));
        }
        snapshot.verify_request_source(entry)?;
        self.remaining()?;
        Ok(())
    }
}

fn validate_input(input: &LspInput) -> Result<()> {
    if input.file.as_deref().is_none_or(str::is_empty)
        || input.line.is_some() || input.symbol.is_some() || input.position.is_some()
        || input.query.is_some() || input.new_name.is_some() || input.new_file.is_some()
        || input.action_id.is_some() || input.refactor_id.is_some()
        || input.completion_id.is_some() || input.snippet_values.is_some()
        || input.hierarchy_id.is_some() || input.only.is_some() || input.after.is_some()
        || input.resolve.is_some() || input.limit.is_some()
        || input.method.is_some() || input.payload.is_some()
    {
        return Err(tool_err(
            "LSP_USAGE",
            "fresh format accepts only file, range, formatOptions, apply and timeout; approve a preview using only its refactorId",
        ));
    }
    Ok(())
}

fn read_source(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(tool_err(
            "LSP_FILE_UNREADABLE",
            "format requires a regular file, not a directory or symlink",
        ));
    }
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(tool_err(
            "LSP_FILE_UNREADABLE",
            "format source is not a regular file",
        ));
    }
    if metadata.len() > MAX_SOURCE_BYTES as u64 {
        return Err(tool_err("LSP_EDIT_LIMIT", "format source exceeds 16 MiB"));
    }
    let mut source = String::new();
    file.take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_string(&mut source)?;
    if source.len() > MAX_SOURCE_BYTES {
        return Err(tool_err("LSP_EDIT_LIMIT", "format source exceeds 16 MiB"));
    }
    Ok(source)
}

fn options(raw: Option<&Value>) -> Result<Value> {
    let mut options = json!({"tabSize":4,"insertSpaces":true});
    let Some(raw) = raw else { return Ok(options) };
    let object = raw
        .as_object()
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
            "insertSpaces"
            | "trimTrailingWhitespace"
            | "insertFinalNewline"
            | "trimFinalNewlines" => value.is_boolean(),
            _ => {
                value.is_boolean()
                    || value
                        .as_i64()
                        .is_some_and(|number| i32::try_from(number).is_ok())
                    || value.as_str().is_some_and(|text| text.len() <= 512)
            }
        };
        if !valid {
            return Err(tool_err(
                "LSP_USAGE",
                format!("invalid formatting option {name:?}"),
            ));
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
        return Err(tool_err(
            "LSP_USAGE",
            "format range must use ordered, in-bounds zero-based UTF-16 positions",
        ));
    }
    Ok(())
}

fn require_capability(capabilities: &Value, ranged: bool) -> Result<()> {
    let name = if ranged {
        "documentRangeFormattingProvider"
    } else {
        "documentFormattingProvider"
    };
    match capabilities.get(name) {
        Some(Value::Bool(true) | Value::Object(_)) => Ok(()),
        None | Some(Value::Bool(false)) => Err(tool_err(
            "LSP_FORMAT_UNSUPPORTED",
            format!("server did not advertise {name}"),
        )),
        Some(_) => Err(tool_err(
            "LSP_FORMAT_PROTOCOL",
            "invalid formatting capability",
        )),
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
    #[allow(clippy::too_many_lines)]
    pub(in crate::lsp) async fn run_format(&self, input: &LspInput) -> Result<ToolOutput> {
        validate_input(input)?;
        let file = input
            .file
            .as_deref()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| tool_err("LSP_USAGE", "format requires file"))?;
        let options = options(input.format_options.as_ref())?;
        let budget = FormatBudget::new(self.request_timeout(input));
        budget.remaining()?;
        let requested = resolve_tool_path(file, &self.cwd);
        let source = read_source(&requested)?;
        if let Some(range) = input.range {
            validate_range(&source, range)?;
        }
        // An admitted fresh computation replaces a reviewed plan even if the
        // provider fails. Invalid selectors/options/positions do not consume it.
        self.refactors.clear();
        let path = requested.canonicalize()?;
        let hash = content_hash_for_drift(&source);
        let now = budget.owner.cx().timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let (uri, entry) = asupersync::time::timeout(now, budget.remaining()?, self.synced(&path))
            .await
            .map_err(|_| tool_err("LSP_TIMEOUT", "format server startup exceeded the request budget"))??;
        let snapshot = RefactorSnapshot::capture(&entry, &path, hash)?;
        if snapshot.source_text.as_ref() != source.as_str() {
            return Err(tool_err("LSP_EDIT_CONFLICT", "format source changed during synchronization"));
        }
        let ranged = input.range.is_some();
        require_capability(&entry.client.capabilities().raw, ranged)?;
        let method = if ranged {
            "textDocument/rangeFormatting"
        } else {
            "textDocument/formatting"
        };
        let mut params = json!({"textDocument":{"uri":uri},"options":options});
        if let Some(range) = input.range {
            params["range"] = serde_json::to_value(range)?;
        }
        let response = entry
            .client
            .call(method, params, budget.remaining()?)
            .await?;
        budget.verify(&entry, &snapshot)?;
        check_response_size(&response)?;
        let returned_null = response.is_null();
        let edits = match response {
            Value::Null => Vec::new(),
            Value::Array(edits) if edits.len() <= MAX_FORMAT_EDITS => edits,
            Value::Array(_) => return Err(tool_err("LSP_EDIT_LIMIT", "too many formatting edits")),
            _ => {
                return Err(tool_err(
                    "LSP_FORMAT_PROTOCOL",
                    "formatting result must be a TextEdit array or null",
                ));
            }
        };
        // Check the standard TextEdit shape; do not accept a command, file
        // operation or the nonstandard textEdit wrapper accepted elsewhere.
        for edit in &edits {
            if !edit.is_object()
                || edit.get("range").is_none()
                || edit.get("newText").and_then(Value::as_str).is_none()
            {
                return Err(tool_err(
                    "LSP_FORMAT_PROTOCOL",
                    "invalid formatting TextEdit",
                ));
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
        let parsed = plan.text_edits.get(&document_path).ok_or_else(|| {
            tool_err("LSP_FORMAT_PROTOCOL", "formatter document identity changed")
        })?;
        // Match the transaction's byte admission before building a preview.
        // Filesystem eligibility is still rechecked by the apply transaction.
        let admitted = parsed.iter().try_fold(source.len(), |bytes, edit| {
            bytes
                .checked_add(edit.new_text.len())
                .filter(|size| *size <= MAX_SOURCE_BYTES)
        });
        if admitted.is_none() {
            return Err(tool_err(
                "LSP_EDIT_LIMIT",
                "format source plus replacements exceeds 16 MiB",
            ));
        }
        let updated = apply_text_edits(&source, parsed)
            .map_err(|error| tool_err("LSP_EDIT_CONFLICT", error))?;
        // Both immediate application and reviewed approval commit the same
        // immutable staged images. Scope admission happens before staging.
        let prepared = self.prepare_refactor(&entry, &workspace, &snapshot)?;
        budget.verify(&entry, &snapshot)?;
        let changed = source != updated;
        let apply = input.apply.unwrap_or(false);
        if apply && changed {
            let result = prepared.commit(|| budget.verify(&entry, &snapshot));
            self.invalidate_refactor(&entry);
            result?;
        } else if apply {
            // No-op requests neither rewrite the file nor invalidate its live
            // document; they still validate every retained preimage.
            prepared.verify()?;
            budget.verify(&entry, &snapshot)?;
        } else {
            let edits = workspace["documentChanges"][0]["edits"]
                .as_array()
                .expect("constructed formatting edit array");
            let (shown, truncated) = preview(edits)?;
            let metadata = json!({
                "action":"format","file":display_path(&path,&self.cwd),"server":entry.spec_name,
                "mode":if ranged { "range" } else { "document" },"range":input.range,
                "changed":changed,"previewOnly":true,"editCount":count,"returnedNull":returned_null,
                "beforeBytes":source.len(),"afterBytes":updated.len(),"formatOptions":options,
                "edits":shown,"previewTruncated":truncated,"workspaceEditComplete":true,
                "note":"The edits overview may be shortened; workspaceEdit is the complete reviewed plan. Approve with action:format, refactorId and apply:true; supplying file computes a new plan."
            });
            let output = self.cache_refactor(
                &entry, prepared, workspace, metadata, None, &budget.owner,
            )?;
            // Caching verifies disk preimages and serializes the complete edit.
            // Do not leave an approvable handle behind if that spent the budget.
            if let Err(error) = budget.verify(&entry, &snapshot) {
                self.refactors.clear();
                return Err(error);
            }
            return Ok(output);
        }
        let payload = json!({
            "action":"format","file":display_path(&path,&self.cwd),"server":entry.spec_name,
            "mode":if ranged { "range" } else { "document" },
            "previewOnly":!apply,"applied":apply && changed,"changed":changed,
            "editCount":count,"returnedNull":returned_null,
            "beforeBytes":source.len(),"afterBytes":updated.len(),"formatOptions":options,
            "rollbackOnError":true,"atomic":false
        });
        Ok(crate::lsp::text_output(payload.to_string(), payload))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod review_tests;
