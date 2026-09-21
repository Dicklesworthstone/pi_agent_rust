//! Bounded, source-bound read workflows for semantic call-site information.
//!
//! Read operations never open an executeCommand or workspace/applyEdit lease.
//! Their result describes the synchronized source, not an atomic project view.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::registry::ServerEntry;
use super::{LspInput, LspTool, tool_err};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};

mod hints;
mod signatures;
mod symbols;

const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

fn malformed(message: &str) -> Error {
    tool_err("LSP_SEMANTIC_MALFORMED", message)
}

fn stale() -> Error {
    tool_err(
        "LSP_SEMANTIC_STALE",
        "source or server changed during semantic inspection; repeat the request",
    )
}

fn bounded_size(value: &impl serde::Serialize, limit: usize) -> Result<usize> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("semantic response byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(limit);
    serde_json::to_writer(&mut counter, value).map_err(|_| {
        tool_err(
            "LSP_SEMANTIC_LIMIT",
            "semantic response exceeds its byte limit; narrow the request",
        )
    })?;
    Ok(limit - counter.0)
}

fn read_source(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(tool_err(
            "LSP_FILE_UNREADABLE",
            "semantic inspection requires a regular nonsymlink file",
        ));
    }
    if metadata.len() > MAX_SOURCE_BYTES as u64 {
        return Err(tool_err(
            "LSP_SEMANTIC_LIMIT",
            "semantic source exceeds 2 MiB",
        ));
    }
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(tool_err(
            "LSP_FILE_UNREADABLE",
            "semantic source is no longer a regular file",
        ));
    }
    let mut text = String::new();
    file.take((MAX_SOURCE_BYTES + 1) as u64)
        .read_to_string(&mut text)?;
    if text.len() > MAX_SOURCE_BYTES {
        return Err(tool_err(
            "LSP_SEMANTIC_LIMIT",
            "semantic source grew beyond 2 MiB",
        ));
    }
    Ok(text)
}

struct Budget {
    owner: AgentCx,
    started: Instant,
    timeout: Duration,
}

impl Budget {
    fn new(timeout: Duration) -> Self {
        Self {
            owner: AgentCx::for_current_or_request(),
            started: Instant::now(),
            timeout,
        }
    }

    fn remaining(&self) -> Result<Duration> {
        self.owner
            .checkpoint()
            .map_err(|_| tool_err("LSP_CANCELLED", "semantic inspection cancelled"))?;
        if !self.owner.capabilities().io {
            return Err(tool_err(
                "LSP_IO_PERMISSION",
                "semantic inspection requires filesystem I/O",
            ));
        }
        self.timeout
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| tool_err("LSP_TIMEOUT", "semantic inspection budget expired"))
    }
}

struct Source {
    path: PathBuf,
    uri: String,
    text: Arc<str>,
    entry: Arc<ServerEntry>,
}

impl Source {
    async fn open(
        tool: &LspTool,
        input: &LspInput,
        budget: &Budget,
        validate: impl FnOnce(&str) -> Result<()>,
    ) -> Result<Self> {
        budget.remaining()?;
        let file = input
            .file
            .as_deref()
            .filter(|file| !file.is_empty())
            .ok_or_else(|| tool_err("LSP_USAGE", "semantic inspection requires file"))?;
        Self::open_file(tool, file, budget, validate).await
    }

    async fn open_file(
        tool: &LspTool,
        file: &str,
        budget: &Budget,
        validate: impl FnOnce(&str) -> Result<()>,
    ) -> Result<Self> {
        budget.remaining()?;
        let requested = super::resolve_tool_path(file, &tool.cwd);
        let text = read_source(&requested)?;
        validate(&text)?;
        let path = requested.canonicalize()?;
        let now = budget
            .owner
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let (uri, entry) = asupersync::time::timeout(now, budget.remaining()?, tool.synced(&path))
            .await
            .map_err(|_| {
                tool_err(
                    "LSP_TIMEOUT",
                    "semantic server startup exceeded the request budget",
                )
            })??;
        if !path.starts_with(entry.client.root()) {
            return Err(tool_err(
                "LSP_FILE_SCOPE",
                "semantic source is outside the server workspace",
            ));
        }
        if entry
            .client
            .capabilities()
            .raw
            .get("positionEncoding")
            .is_some_and(|encoding| encoding.as_str() != Some("utf-16"))
        {
            return Err(tool_err(
                "LSP_UNSUPPORTED",
                "server selected an unadvertised position encoding",
            ));
        }
        let synchronized = entry
            .client
            .synchronized_text(&uri)
            .filter(|synchronized| synchronized.as_ref() == text.as_str())
            .ok_or_else(stale)?;
        let source = Self {
            path,
            uri,
            text: synchronized,
            entry,
        };
        source.verify(budget)?;
        Ok(source)
    }

    fn verify(&self, budget: &Budget) -> Result<()> {
        budget.remaining()?;
        if read_source(&self.path)?.as_str() != self.text.as_ref() {
            return Err(stale());
        }
        if !self.entry.client.is_alive()
            || !self
                .entry
                .client
                .synchronized_text(&self.uri)
                .is_some_and(|current| Arc::ptr_eq(&current, &self.text))
        {
            return Err(stale());
        }
        budget.remaining()?;
        Ok(())
    }
}

fn read_only_input(input: &LspInput) -> Result<()> {
    if input.symbol.is_some()
        || input.line.is_some()
        || input.query.is_some()
        || input.action_id.is_some()
        || input.completion_id.is_some()
        || input.snippet_values.is_some()
        || input.hierarchy_id.is_some()
        || input.new_name.is_some()
        || input.new_file.is_some()
        || input.apply.is_some()
        || input.method.is_some()
        || input.payload.is_some()
        || input.format_options.is_some()
        || input.only.is_some()
        || input.after.is_some()
        || input.limit == Some(0)
    {
        return Err(tool_err(
            "LSP_USAGE",
            "semantic inspection is read-only; unrelated selectors and apply are not accepted",
        ));
    }
    Ok(())
}

fn documentation(value: Option<&Value>) -> Result<Value> {
    match value {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::String(text)) => Ok(Value::String(text.clone())),
        Some(value) if value.is_object() => {
            if !matches!(
                value.get("kind").and_then(Value::as_str),
                Some("markdown" | "plaintext")
            ) || value.get("value").and_then(Value::as_str).is_none()
            {
                return Err(malformed(
                    "documentation must be text or markdown/plaintext MarkupContent",
                ));
            }
            Ok(serde_json::json!({"kind":value["kind"],"value":value["value"]}))
        }
        _ => Err(malformed("documentation must be text or MarkupContent")),
    }
}

fn uint(value: &Value) -> Result<usize> {
    value
        .as_u64()
        .filter(|value| i32::try_from(*value).is_ok())
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| malformed("expected an LSP unsigned integer"))
}

fn optional_index(value: Option<&Value>) -> Result<Option<usize>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => uint(value).map(Some),
    }
}

#[cfg(test)]
mod tests;
