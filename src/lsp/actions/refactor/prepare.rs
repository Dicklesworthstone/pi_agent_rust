//! Server-confirmed symbol targeting before a rename is computed.
//!
//! Preparation is an observation, not an approval handle. Actual renames
//! repeat the check under their own source snapshot and request budget.

use std::io::Read as _;
use std::time::{Duration, Instant};

use super::{
    AgentCx, Arc, LspInput, LspTool, Path, RefactorSnapshot, Result, ServerEntry, ToolOutput,
    Value, check_response_size, display_path, json, resolve_tool_path, tool_err,
};
use crate::lsp::refactor_preview::check_owner;
use crate::lsp::text::{Position, Range, content_hash_for_drift, position_to_offset_exact};

const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TARGET_BYTES: usize = 16 * 1024;

fn malformed(message: &str) -> crate::error::Error {
    tool_err("LSP_RENAME_MALFORMED", message)
}

pub(super) struct Budget {
    pub(super) owner: AgentCx,
    started: Instant,
    timeout: Duration,
}

impl Budget {
    fn new(timeout: Duration) -> Self {
        Self { owner: AgentCx::for_current_or_request(), started: Instant::now(), timeout }
    }

    pub(super) fn remaining(&self) -> Result<Duration> {
        check_owner(&self.owner)?;
        self.timeout.checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| tool_err("LSP_TIMEOUT", "symbol rename request budget expired"))
    }
}

/// Never guess a language's identifier rules. Only concrete ranges are
/// negotiated; defaultBehavior is deliberately not advertised to the server.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Target {
    range: Range,
    placeholder: String,
    source_text: String,
}

fn parse(raw: &Value, text: &str, position: Position) -> Result<Option<Target>> {
    check_response_size(raw)?;
    if raw.is_null() {
        return Ok(None);
    }
    let object = raw.as_object().ok_or_else(|| malformed("prepareRename needs a range or null"))?;
    if object.contains_key("defaultBehavior") {
        return Err(malformed("prepareRename defaultBehavior was not negotiated; a concrete range is required"));
    }
    let (range, placeholder) = if let Some(range) = object.get("range") {
        if object.contains_key("start") || object.contains_key("end") {
            return Err(malformed("prepareRename mixes range response forms"));
        }
        let placeholder = object.get("placeholder").and_then(Value::as_str)
            .ok_or_else(|| malformed("prepareRename range wrapper requires a string placeholder"))?;
        (range, Some(placeholder))
    } else {
        if object.contains_key("placeholder") {
            return Err(malformed("prepareRename placeholder has no range"));
        }
        (raw, None)
    };
    let range: Range = serde_json::from_value(range.clone())
        .map_err(|_| malformed("prepareRename returned an invalid range"))?;
    let start = position_to_offset_exact(text, range.start)
        .ok_or_else(|| malformed("prepareRename start is not an exact UTF-16 boundary"))?;
    let end = position_to_offset_exact(text, range.end)
        .ok_or_else(|| malformed("prepareRename end is not an exact UTF-16 boundary"))?;
    // A cursor immediately after a token is a valid rename target. Do not
    // replace that cursor with the returned range start in the rename request.
    if start >= end || position < range.start || position > range.end {
        return Err(malformed("prepareRename range must be nonempty and contain the requested cursor"));
    }
    if end - start > MAX_TARGET_BYTES || placeholder.is_some_and(|value| value.len() > MAX_TARGET_BYTES) {
        return Err(tool_err("LSP_RENAME_LIMIT", "rename target or placeholder exceeds 16 KiB"));
    }
    let source_text = text[start..end].to_string();
    Ok(Some(Target {
        range,
        placeholder: placeholder.unwrap_or(&source_text).to_string(),
        source_text,
    }))
}

fn supports_prepare(caps: &Value) -> Result<bool> {
    if caps.get("positionEncoding").is_some_and(|value| value.as_str() != Some("utf-16")) {
        return Err(tool_err("LSP_UNSUPPORTED", "rename requires the advertised UTF-16 position encoding"));
    }
    match caps.get("renameProvider") {
        None | Some(Value::Null | Value::Bool(true)) => Ok(false),
        Some(Value::Bool(false)) => Err(tool_err("LSP_UNSUPPORTED", "server disabled symbol rename")),
        Some(Value::Object(options)) => match options.get("prepareProvider") {
            None => Ok(false),
            Some(Value::Bool(prepare)) => Ok(*prepare),
            _ => Err(malformed("rename prepareProvider must be boolean")),
        },
        _ => Err(malformed("renameProvider must be boolean or options")),
    }
}

fn validate_input(input: &LspInput) -> Result<()> {
    if input.file.as_deref().is_none_or(str::is_empty)
        || (input.position.is_some() && (input.symbol.is_some() || input.line.is_some()))
        || (input.position.is_none() && input.symbol.as_deref().is_none_or(str::is_empty))
        || input.line == Some(0)
    {
        return Err(tool_err("LSP_USAGE", "rename targeting requires file and either exact position or symbol with optional one-based line"));
    }
    if input.range.is_some() || input.query.is_some() || input.new_file.is_some()
        || input.action_id.is_some() || input.refactor_id.is_some() || input.completion_id.is_some()
        || input.snippet_values.is_some() || input.hierarchy_id.is_some() || input.only.is_some()
        || input.after.is_some() || input.resolve.is_some() || input.format_options.is_some()
        || input.limit.is_some() || input.method.is_some() || input.payload.is_some()
    {
        return Err(tool_err("LSP_USAGE", "unrelated selectors are not accepted for symbol rename targeting"));
    }
    if input.action == "prepare_rename" {
        if input.new_name.is_some() || input.apply.is_some() {
            return Err(tool_err("LSP_USAGE", "prepare_rename only inspects a target; use rename to compute or apply edits"));
        }
    } else if input.new_name.as_deref().is_none_or(|name| name.is_empty() || name.len() > MAX_TARGET_BYTES || name.contains('\0')) {
        return Err(tool_err("LSP_USAGE", "rename requires nonempty newName up to 16 KiB without NUL"));
    }
    Ok(())
}

fn read_source(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(tool_err("LSP_FILE_UNREADABLE", "rename requires a regular, nonsymlink source"));
    }
    if metadata.len() > MAX_SOURCE_BYTES as u64 {
        return Err(tool_err("LSP_RENAME_LIMIT", "rename source exceeds 16 MiB"));
    }
    let mut text = String::new();
    std::fs::File::open(path)?.take((MAX_SOURCE_BYTES + 1) as u64).read_to_string(&mut text)?;
    if text.len() > MAX_SOURCE_BYTES {
        return Err(tool_err("LSP_RENAME_LIMIT", "rename source grew beyond 16 MiB"));
    }
    Ok(text)
}

pub(super) struct Request {
    pub(super) entry: Arc<ServerEntry>,
    pub(super) snapshot: RefactorSnapshot,
    pub(super) position: Position,
    pub(super) uri: String,
    pub(super) budget: Budget,
    pub(super) prepare_supported: bool,
}

impl Request {
    pub(super) fn verify(&self) -> Result<()> {
        self.budget.remaining()?;
        if !self.entry.client.is_alive() {
            return Err(tool_err("LSP_TRANSPORT_CLOSED", "rename connection closed"));
        }
        self.snapshot.verify_request_source(&self.entry)?;
        self.budget.remaining()?;
        Ok(())
    }

    pub(super) async fn prepare(&self) -> Result<Option<Target>> {
        if !self.prepare_supported {
            return Err(tool_err("LSP_UNSUPPORTED", "server does not advertise rename preparation"));
        }
        self.verify()?;
        let raw = self.entry.client.call(
            "textDocument/prepareRename",
            json!({"textDocument":{"uri":self.uri},"position":self.position}),
            self.budget.remaining()?,
        ).await?;
        self.verify()?;
        parse(&raw, &self.snapshot.source_text, self.position)
    }
}

impl LspTool {
    pub(in crate::lsp::actions::refactor) async fn rename_request(&self, input: &LspInput) -> Result<Request> {
        validate_input(input)?;
        let budget = Budget::new(self.request_timeout(input));
        budget.remaining()?;
        let requested = resolve_tool_path(input.file.as_deref().unwrap_or_default(), &self.cwd);
        let text = read_source(&requested)?;
        let path = requested.canonicalize()?;
        let position = match input.position {
            Some(position) => position,
            None => Self::resolve_position_in(&path, &text, input.line, input.symbol.as_deref().unwrap_or_default())?,
        };
        if position_to_offset_exact(&text, position).is_none() {
            return Err(tool_err("LSP_USAGE", "rename position is not an exact zero-based UTF-16 boundary"));
        }
        let now = budget.owner.cx().timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let (uri, entry) = asupersync::time::timeout(now, budget.remaining()?, self.synced(&path))
            .await.map_err(|_| tool_err("LSP_TIMEOUT", "rename server startup exceeded the request budget"))??;
        let snapshot = RefactorSnapshot::capture(&entry, &path, content_hash_for_drift(&text))?;
        if snapshot.source_text.as_ref() != text.as_str() {
            return Err(tool_err("LSP_EDIT_CONFLICT", "rename source changed during synchronization"));
        }
        let prepare_supported = supports_prepare(&entry.client.capabilities().raw)?;
        let request = Request { entry, snapshot, position, uri, budget, prepare_supported };
        request.verify()?;
        Ok(request)
    }

    pub(in crate::lsp) async fn run_prepare_rename(&self, input: &LspInput) -> Result<ToolOutput> {
        let request = self.rename_request(input).await?;
        let target = request.prepare().await?;
        let payload = json!({
            "action":"prepare_rename","file":display_path(&request.snapshot.source,&self.cwd),
            "position":request.position,"canRename":target.is_some(),"target":target,
            "applied":false,"note":"Target inspection only; rename validates again before computing edits."
        });
        request.verify()?;
        Ok(super::super::text_output(payload.to_string(), payload))
    }
}

#[cfg(test)]
mod tests;
