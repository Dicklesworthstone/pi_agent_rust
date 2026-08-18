//! Agent-facing `lsp` tool: IDE-grade code intelligence over child LSP
//! servers (bd-cv653.1.1).
//!
//! Position-based addressing follows the omp convention: `file` + `line`
//! (1-indexed) + `symbol` substring (with an optional `#N` suffix selecting
//! the Nth occurrence). Project-aware lookups (definition, references,
//! rename, code actions, ...) error without a symbol — no silent fallback.
//! `rename` applies the returned WorkspaceEdit atomically (all files or
//! none); `rename_file` goes through `workspace/willRenameFiles` when the
//! server advertises it so importers update before the file moves.

pub mod client;
pub mod edits;
pub mod jsonrpc;
pub mod registry;
pub mod text;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use client::{hover_to_text, parse_locations, path_to_uri, uri_to_path};
use edits::{WorkspaceEditPlan, apply_workspace_edit, parse_workspace_edit};
use registry::{LspRegistry, ServerEntry};
use text::{Position, find_occurrences, line_count, offset_to_position};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};

/// Cap on serialized JSON payloads in tool output (symbols/actions lists).
const MAX_PAYLOAD_BYTES: usize = 200 * 1024;
/// Default cap on locations returned by references.
const DEFAULT_LOCATION_LIMIT: usize = 100;
/// Hard cap on locations returned by references.
const HARD_LOCATION_LIMIT: usize = 1000;

fn text_output(text: String, details: Value) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error: false,
    }
}

fn usage_error(message: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(message.into()))],
        details: None,
        is_error: true,
    }
}

fn tool_err(code: &str, message: impl Into<String>) -> Error {
    Error::tool("lsp", format!("[{code}] {}", message.into()))
}

/// Resolve a user-supplied path against the tool working directory.
fn resolve_tool_path(path: &str, cwd: &Path) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// Display path relative to the working directory when possible.
fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).map_or_else(
        || path.display().to_string(),
        |rel| rel.display().to_string(),
    )
}

/// Parse a symbol selector: `name` or `name#N` (1-indexed occurrence).
fn parse_symbol_selector(raw: &str) -> (String, Option<usize>) {
    if let Some((name, nth)) = raw.rsplit_once('#')
        && !name.is_empty()
        && let Ok(nth) = nth.parse::<usize>()
        && nth >= 1
    {
        return (name.to_string(), Some(nth));
    }
    (raw.to_string(), None)
}

/// The `lsp` tool.
///
/// One instance per registry construction; owns the server registry (and
/// therefore all spawned language servers) for its lifetime.
pub struct LspTool {
    cwd: PathBuf,
    registry: LspRegistry,
}

impl LspTool {
    #[must_use]
    pub fn new(cwd: &Path, config: Option<&Config>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            registry: LspRegistry::new(cwd, config),
        }
    }

    /// Language id for a file under a given server spec.
    fn language_id_for(&self, path: &Path, spec: &registry::ServerSpec) -> String {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
            .and_then(|dotted| {
                registry::language_id_for_extension(&dotted).map(str::to_string)
            })
            .or_else(|| spec.languages.first().cloned())
            .unwrap_or_else(|| "plaintext".to_string())
    }

    /// Resolve + spawn the server for `path`, install the applyEdit hook on
    /// first use, and return the entry.
    async fn client_for(&self, path: &Path) -> Result<Arc<ServerEntry>> {
        let entry = self.registry.client_for(path).await?;
        install_apply_edit_handler(&entry);
        Ok(entry)
    }

    /// Sync the document and return `(uri, server entry)`.
    async fn synced(&self, path: &Path) -> Result<(String, Arc<ServerEntry>)> {
        let entry = self.client_for(path).await?;
        let spec = self
            .registry
            .spec_for_file(path)
            .ok_or_else(|| tool_err("LSP_NO_SERVER", format!("no server for {}", path.display())))?;
        let language_id = self.language_id_for(path, spec);
        let uri = entry.client.ensure_synced(path, &language_id)?;
        Ok((uri, entry))
    }

    /// Resolve a position from file + line + symbol selector.
    fn resolve_position(
        &self,
        path: &Path,
        line: Option<u32>,
        symbol: &str,
    ) -> Result<Position> {
        let content = std::fs::read_to_string(path).map_err(|err| {
            tool_err(
                "LSP_FILE_UNREADABLE",
                format!("cannot read {}: {err}", path.display()),
            )
        })?;
        let (needle, nth) = parse_symbol_selector(symbol);
        if needle.is_empty() {
            return Err(tool_err("LSP_NO_SYMBOL", "symbol must not be empty"));
        }
        let occurrences = find_occurrences(&content, &needle, line.map(|l| l.saturating_sub(1)));
        if occurrences.is_empty() {
            let scope = line.map_or_else(|| "file".to_string(), |l| format!("line {l}"));
            return Err(tool_err(
                "LSP_NO_SYMBOL",
                format!("no occurrence of {needle:?} in {scope} of {}", path.display()),
            ));
        }
        let selected = match (nth, occurrences.len()) {
            (Some(n), len) if n <= len => occurrences[n - 1],
            (Some(n), len) => {
                return Err(tool_err(
                    "LSP_SYMBOL_AMBIGUOUS",
                    format!("selector asked for occurrence #{n} but only {len} match(es) of {needle:?} exist"),
                ));
            }
            (None, 1) => occurrences[0],
            (None, len) => {
                if line.is_some() {
                    // Within one line, multiple matches: require #N.
                    return Err(tool_err(
                        "LSP_SYMBOL_AMBIGUOUS",
                        format!("{len} matches of {needle:?} on that line; disambiguate with {needle}#N"),
                    ));
                }
                return Err(tool_err(
                    "LSP_SYMBOL_AMBIGUOUS",
                    format!("{len} matches of {needle:?} in file; narrow with `line` or {needle}#N"),
                ));
            }
        };
        offset_to_position(&content, selected.0).ok_or_else(|| {
            tool_err(
                "LSP_NO_SYMBOL",
                format!("occurrence of {needle:?} does not map to an LSP position"),
            )
        })
    }

    fn request_timeout(&self, input: &LspInput) -> Duration {
        input
            .timeout
            .filter(|secs| *secs > 0)
            .map_or_else(|| self.registry.request_timeout(), Duration::from_secs)
    }

    /// Render a location list as JSON payload + details.
    fn locations_output(
        &self,
        action: &str,
        locations: &[(String, text::Range)],
        limit: usize,
    ) -> ToolOutput {
        let mut entries = Vec::new();
        for (uri, range) in locations.iter().take(limit) {
            let path = uri_to_path(uri)
                .map(|p| display_path(&p, &self.cwd))
                .unwrap_or_else(|| uri.clone());
            entries.push(json!({
                "file": path,
                "line": range.start.line + 1,
                "character": range.start.character + 1,
            }));
        }
        let truncated = locations.len() > limit;
        let payload = json!({
            "action": action,
            "count": entries.len(),
            "truncated": truncated,
            "locations": entries,
        });
        text_output(payload.to_string(), payload)
    }

    async fn run_diagnostics(&self, input: &LspInput) -> Result<ToolOutput> {
        let Some(file) = input.file.as_deref() else {
            return Ok(usage_error(
                "lsp diagnostics requires `file` (a path or glob like src/**/*.rs)",
            ));
        };
        let is_glob = file.contains(['*', '[', '?']);
        if is_glob {
            // Glob mode: answer from the diagnostics caches of live servers
            // without spawning anything new.
            let override_filter = build_glob_override(&self.cwd, file)?;
            let mut matched = Vec::new();
            for status in self.registry.status() {
                if let Some(entry) = self.registry.entry_for_root(&status.name, &status.root) {
                    for (uri, diags) in entry.client.diagnostics_snapshot() {
                        if let Some(path) = uri_to_path(&uri) {
                            let rel = path.strip_prefix(&self.cwd).unwrap_or(&path);
                            if override_filter.matched(rel, false).is_match() {
                                matched.push(json!({
                                    "file": display_path(&path, &self.cwd),
                                    "server": status.name,
                                    "diagnostics": diags,
                                }));
                            }
                        }
                    }
                }
            }
            let payload = json!({
                "action": "diagnostics",
                "glob": file,
                "files": matched.len(),
                "entries": matched,
            });
            return Ok(text_output(payload.to_string(), payload));
        }

        let path = resolve_tool_path(file, &self.cwd);
        let (uri, entry) = self.synced(&path).await?;
        // Give the server a bounded window to publish fresh diagnostics.
        entry
            .client
            .wait_for_diagnostics(&uri, client::DEFAULT_DIAGNOSTICS_WAIT)
            .await;
        let snapshot = entry.client.diagnostics_snapshot();
        let diags = snapshot.get(&uri).cloned().unwrap_or_default();
        let payload = json!({
            "action": "diagnostics",
            "file": display_path(&path, &self.cwd),
            "server": entry.spec_name,
            "count": diags.len(),
            "diagnostics": diags,
        });
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_position_request(
        &self,
        input: &LspInput,
        action: &str,
        method: &str,
        extra_params: Value,
    ) -> Result<ToolOutput> {
        let (path, position) = self.require_position(input)?;
        let (uri, entry) = self.synced(&path).await?;
        let mut params = json!({
            "textDocument": { "uri": uri },
            "position": position,
        });
        if let (Some(dst), Some(src)) = (params.as_object_mut(), extra_params.as_object()) {
            for (key, value) in src {
                dst.insert(key.clone(), value.clone());
            }
        }
        let result = entry
            .client
            .call(method, params, self.request_timeout(input))
            .await
            .map_err(Error::from)?;
        match action {
            "hover" => {
                let text = hover_to_text(&result)
                    .unwrap_or_else(|| "no hover information".to_string());
                let payload = json!({
                    "action": "hover",
                    "file": display_path(&path, &self.cwd),
                    "line": position.line + 1,
                    "hover": text,
                });
                Ok(text_output(payload.to_string(), payload))
            }
            _ => {
                let locations = parse_locations(&result);
                let limit = input
                    .limit
                    .unwrap_or(DEFAULT_LOCATION_LIMIT)
                    .min(HARD_LOCATION_LIMIT);
                if locations.is_empty() {
                    let payload = json!({
                        "action": action,
                        "file": display_path(&path, &self.cwd),
                        "line": position.line + 1,
                        "count": 0,
                        "locations": [],
                        "note": format!("no {action} found at that position"),
                    });
                    return Ok(text_output(payload.to_string(), payload));
                }
                Ok(self.locations_output(action, &locations, limit))
            }
        }
    }

    fn require_position(&self, input: &LspInput) -> Result<(PathBuf, Position)> {
        let file = input.file.as_deref().ok_or_else(|| {
            tool_err(
                "LSP_USAGE",
                format!("lsp {} requires `file`", input.action),
            )
        })?;
        let symbol = input.symbol.as_deref().ok_or_else(|| {
            tool_err(
                "LSP_USAGE",
                format!(
                    "lsp {} requires `symbol` (project-aware lookups never guess a position)",
                    input.action
                ),
            )
        })?;
        let path = resolve_tool_path(file, &self.cwd);
        let position = self.resolve_position(&path, input.line, symbol)?;
        Ok((path, position))
    }

    async fn run_symbols(&self, input: &LspInput) -> Result<ToolOutput> {
        match (input.file.as_deref(), input.query.as_deref()) {
            (Some(file), _) => {
                let path = resolve_tool_path(file, &self.cwd);
                let (uri, entry) = self.synced(&path).await?;
                let result = entry
                    .client
                    .call(
                        "textDocument/documentSymbol",
                        json!({ "textDocument": { "uri": uri } }),
                        self.request_timeout(input),
                    )
                    .await
                    .map_err(Error::from)?;
                let (payload, truncated) = cap_payload(json!({
                    "action": "symbols",
                    "file": display_path(&path, &self.cwd),
                    "server": entry.spec_name,
                    "symbols": result,
                }));
                Ok(text_output(payload.to_string(), json!({"truncated": truncated, "payload": payload})))
            }
            (None, Some(query)) => {
                // Workspace symbols need *a* server; route via the cwd so the
                // user can pick the server by extension of any anchor file.
                let anchor = input
                    .symbol
                    .as_deref()
                    .map(|s| resolve_tool_path(s, &self.cwd));
                let Some(anchor) = anchor else {
                    return Ok(usage_error(
                        "lsp symbols with `query` also needs `symbol` set to an anchor file path (its extension picks the server)",
                    ));
                };
                let entry = self.client_for(&anchor).await?;
                let result = entry
                    .client
                    .call(
                        "workspace/symbol",
                        json!({ "query": query }),
                        self.request_timeout(input),
                    )
                    .await
                    .map_err(Error::from)?;
                let (payload, truncated) = cap_payload(json!({
                    "action": "symbols",
                    "query": query,
                    "server": entry.spec_name,
                    "symbols": result,
                }));
                Ok(text_output(payload.to_string(), json!({"truncated": truncated, "payload": payload})))
            }
            (None, None) => Ok(usage_error(
                "lsp symbols requires `file` (document symbols) or `query` (workspace symbols)",
            )),
        }
    }

    async fn run_rename(&self, input: &LspInput) -> Result<ToolOutput> {
        let new_name = input.new_name.as_deref().ok_or_else(|| {
            tool_err("LSP_USAGE", "lsp rename requires `newName`")
        })?;
        if new_name.is_empty() {
            return Ok(usage_error("lsp rename requires a non-empty `newName`"));
        }
        let (path, position) = self.require_position(input)?;
        let (uri, entry) = self.synced(&path).await?;
        let result = entry
            .client
            .call(
                "textDocument/rename",
                json!({
                    "textDocument": { "uri": uri },
                    "position": position,
                    "newName": new_name,
                }),
                self.request_timeout(input),
            )
            .await
            .map_err(Error::from)?;
        let plan = parse_workspace_edit(&result)?;
        let outcome = apply_workspace_edit(&plan, None)?;
        // External mutation: forget affected docs so the next sync reopens
        // them from disk.
        for changed in &outcome.files_changed {
            entry.client.invalidate(&path_to_uri(changed));
        }
        let files: Vec<String> = outcome
            .files_changed
            .iter()
            .map(|p| display_path(p, &self.cwd))
            .collect();
        let payload = json!({
            "action": "rename",
            "newName": new_name,
            "filesChanged": files,
            "fileOps": outcome.file_ops_applied,
            "atomic": true,
        });
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_rename_file(&self, input: &LspInput) -> Result<ToolOutput> {
        let (Some(file), Some(new_file)) = (input.file.as_deref(), input.new_file.as_deref())
        else {
            return Ok(usage_error(
                "lsp rename_file requires `file` and `newFile`",
            ));
        };
        let old_path = resolve_tool_path(file, &self.cwd);
        let new_path = resolve_tool_path(new_file, &self.cwd);
        if !old_path.exists() {
            return Err(tool_err(
                "LSP_FILE_UNREADABLE",
                format!("rename source does not exist: {}", old_path.display()),
            ));
        }
        if new_path.exists() {
            return Err(tool_err(
                "LSP_EDIT_CONFLICT",
                format!("rename target already exists: {}", new_path.display()),
            ));
        }
        let (old_uri, entry) = self.synced(&old_path).await?;
        // canonicalize() fails on not-yet-existing paths; canonicalize the
        // parent and re-attach the file name instead.
        let canonical_new = new_path
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .and_then(|parent| {
                new_path
                    .file_name()
                    .map(|name| parent.join(name))
            })
            .unwrap_or_else(|| new_path.clone());
        let new_uri = path_to_uri(&canonical_new);
        let mut edits_applied: Vec<String> = Vec::new();

        // Stage 1: willRenameFiles (when advertised) lets the server compute
        // import updates BEFORE the move.
        if entry.client.capabilities().will_rename_files {
            let result = entry
                .client
                .call(
                    "workspace/willRenameFiles",
                    json!({
                        "files": [{ "oldUri": old_uri, "newUri": new_uri }]
                    }),
                    self.request_timeout(input),
                    )
                .await;
            match result {
                Ok(edit) if !edit.is_null() => {
                    let plan = parse_workspace_edit(&edit)?;
                    let outcome = apply_workspace_edit(&plan, None)?;
                    for changed in &outcome.files_changed {
                        entry.client.invalidate(&path_to_uri(changed));
                        edits_applied.push(display_path(changed, &self.cwd));
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    return Err(tool_err(
                        "LSP_SERVER_ERROR",
                        format!(
                            "willRenameFiles failed; file NOT moved (fail-closed): {}",
                            err.message()
                        ),
                    ));
                }
            }
        }

        // Stage 2: the move itself.
        if let Some(parent) = new_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                tool_err(
                    "LSP_EDIT_APPLY",
                    format!("cannot create {}: {err}", parent.display()),
                )
            })?;
        }
        std::fs::rename(&old_path, &new_path).map_err(|err| {
            tool_err(
                "LSP_EDIT_APPLY",
                format!(
                    "cannot rename {} -> {}: {err}",
                    old_path.display(),
                    new_path.display()
                ),
            )
        })?;

        // Stage 3: notify + forget both endpoints.
        let _ = entry.client.call_no_wait_notify(
            "workspace/didRenameFiles",
            json!({ "files": [{ "oldUri": old_uri, "newUri": new_uri }] }),
        );
        entry.client.invalidate(&old_uri);
        entry.client.invalidate(&new_uri);

        let payload = json!({
            "action": "rename_file",
            "from": display_path(&old_path, &self.cwd),
            "to": display_path(&new_path, &self.cwd),
            "importUpdates": edits_applied,
            "willRenameFiles": entry.client.capabilities().will_rename_files,
        });
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_code_actions(&self, input: &LspInput) -> Result<ToolOutput> {
        let file = input.file.as_deref().ok_or_else(|| {
            tool_err("LSP_USAGE", "lsp code_actions requires `file`")
        })?;
        let path = resolve_tool_path(file, &self.cwd);
        let (uri, entry) = self.synced(&path).await?;

        // Range: symbol+line resolves a point; otherwise the whole document.
        let range = match input.symbol.as_deref() {
            Some(symbol) => {
                let position = self.resolve_position(&path, input.line, symbol)?;
                json!({ "start": position, "end": position })
            }
            None => {
                let content = std::fs::read_to_string(&path).map_err(|err| {
                    tool_err(
                        "LSP_FILE_UNREADABLE",
                        format!("cannot read {}: {err}", path.display()),
                    )
                })?;
                let last_line = line_count(&content).saturating_sub(1);
                json!({
                    "start": Position { line: 0, character: 0 },
                    "end": Position { line: last_line, character: 0 },
                })
            }
        };
        let diagnostics = entry
            .client
            .diagnostics_snapshot()
            .get(&uri)
            .cloned()
            .unwrap_or_default();
        let result = entry
            .client
            .call(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": uri },
                    "range": range,
                    "context": { "diagnostics": diagnostics },
                }),
                self.request_timeout(input),
            )
            .await
            .map_err(Error::from)?;
        let actions = result.as_array().cloned().unwrap_or_default();
        let summaries: Vec<Value> = actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                json!({
                    "index": index + 1,
                    "title": action.get("title").and_then(Value::as_str).unwrap_or("<untitled>"),
                    "kind": action.get("kind").and_then(Value::as_str),
                    "isPreferred": action.get("isPreferred").and_then(Value::as_bool).unwrap_or(false),
                    "disabled": action.get("disabled").is_some(),
                    "hasEdit": action.get("edit").is_some(),
                    "hasCommand": action.get("command").is_some(),
                })
            })
            .collect();

        if !input.apply.unwrap_or(false) {
            let payload = json!({
                "action": "code_actions",
                "file": display_path(&path, &self.cwd),
                "count": summaries.len(),
                "actions": summaries,
            });
            return Ok(text_output(payload.to_string(), payload));
        }

        let query = input.query.as_deref().ok_or_else(|| {
            tool_err(
                "LSP_USAGE",
                "lsp code_actions with `apply: true` requires `query` (action title substring or 1-based index)",
            )
        })?;
        let selected = select_code_action(&actions, query)?;
        let title = selected
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("<untitled>")
            .to_string();

        if let Some(edit) = selected.get("edit") {
            let plan: WorkspaceEditPlan = parse_workspace_edit(edit)?;
            let outcome = apply_workspace_edit(&plan, None)?;
            for changed in &outcome.files_changed {
                entry.client.invalidate(&path_to_uri(changed));
            }
            let files: Vec<String> = outcome
                .files_changed
                .iter()
                .map(|p| display_path(p, &self.cwd))
                .collect();
            let payload = json!({
                "action": "code_actions",
                "applied": title,
                "filesChanged": files,
                "fileOps": outcome.file_ops_applied,
            });
            return Ok(text_output(payload.to_string(), payload));
        }

        if let Some(command) = selected.get("command") {
            let (command_name, arguments) = if let Some(name) = command.as_str() {
                (name.to_string(), selected.get("arguments").cloned().unwrap_or(Value::Null))
            } else {
                let name = command
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or_else(|| tool_err("LSP_EDIT_MALFORMED", "command object missing name"))?
                    .to_string();
                let arguments = command.get("arguments").cloned().unwrap_or(Value::Null);
                (name, arguments)
            };
            // workspace/applyEdit requests issued by the server during this
            // call are applied by the installed handler.
            entry
                .client
                .call(
                    "workspace/executeCommand",
                    json!({ "command": command_name, "arguments": arguments }),
                    self.request_timeout(input),
                )
                .await
                .map_err(Error::from)?;
            let payload = json!({
                "action": "code_actions",
                "applied": title,
                "executedCommand": command_name,
            });
            return Ok(text_output(payload.to_string(), payload));
        }

        Err(tool_err(
            "LSP_ACTION_NO_EDIT",
            format!("code action {title:?} carries neither an edit nor a command"),
        ))
    }

    async fn run_status(&self) -> Result<ToolOutput> {
        let statuses = self.registry.status();
        let servers: Vec<Value> = statuses
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "serverName": s.server_name,
                    "root": s.root.display().to_string(),
                    "alive": s.alive,
                    "idleSecs": s.idle_secs,
                    "openDocuments": s.open_documents,
                    "droppedNotifications": s.dropped_notifications,
                })
            })
            .collect();
        let configured: Vec<Value> = self
            .registry
            .configured_servers()
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "command": spec.command,
                    "extensions": spec.extensions,
                })
            })
            .collect();
        let payload = json!({
            "action": "status",
            "cwd": self.cwd.display().to_string(),
            "live": servers,
            "configured": configured,
        });
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_reload(&self, input: &LspInput) -> Result<ToolOutput> {
        let path = input.file.as_deref().map(|f| resolve_tool_path(f, &self.cwd));
        let killed = self.registry.kill_matching(path.as_deref()).await;
        let payload = json!({
            "action": "reload",
            "killed": killed,
            "note": "servers respawn lazily on next use",
        });
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_capabilities(&self, input: &LspInput) -> Result<ToolOutput> {
        let Some(file) = input.file.as_deref() else {
            return Ok(usage_error(
                "lsp capabilities requires `file` (its extension picks the server)",
            ));
        };
        let path = resolve_tool_path(file, &self.cwd);
        let entry = self.client_for(&path).await?;
        let caps = entry.client.capabilities();
        let payload = json!({
            "action": "capabilities",
            "server": entry.spec_name,
            "serverName": caps.server_name,
            "willRenameFiles": caps.will_rename_files,
            "textDocumentSyncKind": caps.sync_kind,
            "capabilities": caps.raw,
        });
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_raw_request(&self, input: &LspInput) -> Result<ToolOutput> {
        let (Some(method), Some(file)) = (input.method.as_deref(), input.file.as_deref()) else {
            return Ok(usage_error(
                "lsp request requires `method` and `file` (its extension picks the server)",
            ));
        };
        let path = resolve_tool_path(file, &self.cwd);
        let entry = self.client_for(&path).await?;
        let result = entry
            .client
            .call(
                method,
                input.payload.clone().unwrap_or(Value::Null),
                self.request_timeout(input),
            )
            .await
            .map_err(Error::from)?;
        let (payload, truncated) = cap_payload(json!({
            "action": "request",
            "method": method,
            "server": entry.spec_name,
            "result": result,
        }));
        Ok(text_output(payload.to_string(), json!({"truncated": truncated, "payload": payload})))
    }
}

/// Install the `workspace/applyEdit` server-request handler once per entry.
fn install_apply_edit_handler(entry: &Arc<ServerEntry>) {
    if entry.handler_installed.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(entry);
    entry
        .client
        .set_server_request_handler(Arc::new(move |method, params| {
            if method != "workspace/applyEdit" {
                return None;
            }
            let Some(entry) = weak.upgrade() else {
                return Some(json!({ "applied": false, "failureReason": "client dropped" }));
            };
            let Some(edit) = params.get("edit") else {
                return Some(json!({ "applied": false, "failureReason": "missing edit" }));
            };
            let outcome = parse_workspace_edit(edit)
                .and_then(|plan| apply_workspace_edit(&plan, None));
            match outcome {
                Ok(outcome) => {
                    for changed in &outcome.files_changed {
                        entry.client.invalidate(&path_to_uri(changed));
                    }
                    Some(json!({ "applied": true }))
                }
                Err(err) => Some(json!({
                    "applied": false,
                    "failureReason": err.to_string(),
                })),
            }
        }));
}

/// Pick one code action by 1-based index or case-insensitive title substring.
fn select_code_action(actions: &[Value], query: &str) -> Result<Value> {
    if let Ok(index) = query.parse::<usize>() {
        return actions
            .get(index.saturating_sub(1))
            .filter(|_| index >= 1)
            .cloned()
            .ok_or_else(|| {
                tool_err(
                    "LSP_USAGE",
                    format!("code action index {index} out of range ({} actions)", actions.len()),
                )
            });
    }
    let needle = query.to_ascii_lowercase();
    let matches: Vec<&Value> = actions
        .iter()
        .filter(|action| {
            action
                .get("title")
                .and_then(Value::as_str)
                .is_some_and(|title| title.to_ascii_lowercase().contains(&needle))
        })
        .collect();
    match matches.len() {
        0 => Err(tool_err(
            "LSP_USAGE",
            format!("no code action title contains {query:?}"),
        )),
        1 => Ok(matches[0].clone()),
        n => Err(tool_err(
            "LSP_SYMBOL_AMBIGUOUS",
            format!("{n} code actions match {query:?}; narrow the query or use a 1-based index"),
        )),
    }
}

/// Cap a JSON payload's serialized size; returns (payload, truncated).
fn cap_payload(payload: Value) -> (Value, bool) {
    let serialized = payload.to_string();
    if serialized.len() <= MAX_PAYLOAD_BYTES {
        return (payload, false);
    }
    let mut truncated = serialized;
    truncated.truncate(MAX_PAYLOAD_BYTES);
    truncated.push_str("...[TRUNCATED]");
    (Value::String(truncated), true)
}

/// Build a gitignore-style glob override anchored at `cwd`.
fn build_glob_override(cwd: &Path, glob: &str) -> Result<ignore::overrides::Override> {
    ignore::overrides::OverrideBuilder::new(cwd)
        .add(glob)
        .and_then(|builder| builder.build())
        .map_err(|err| tool_err("LSP_USAGE", format!("invalid glob {glob:?}: {err}")))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LspInput {
    action: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    new_name: Option<String>,
    #[serde(default)]
    new_file: Option<String>,
    #[serde(default)]
    apply: Option<bool>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn label(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "IDE-grade code intelligence (definition, references, rename) via language servers. \
         Actions: diagnostics (file or glob), definition, references, hover, symbols (file = document, \
         query = workspace), rename (symbol-aware, atomic multi-file apply), rename_file (moves a file \
         and updates importers via willRenameFiles), code_actions (list; apply with apply:true + query), \
         type_definition, implementation, status, reload, capabilities, request (raw LSP method). \
         Position addressing: file + line (1-indexed) + symbol substring; use symbol#N for the Nth \
         occurrence. Project-aware lookups error without `symbol` — they never guess."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "diagnostics", "definition", "references", "hover", "symbols",
                        "rename", "rename_file", "code_actions", "type_definition",
                        "implementation", "status", "reload", "capabilities", "request"
                    ],
                    "description": "The LSP operation to run"
                },
                "file": {
                    "type": "string",
                    "description": "Target file (path relative to cwd or absolute); diagnostics also accepts a glob"
                },
                "line": {
                    "type": "integer",
                    "description": "1-indexed line narrowing the symbol search"
                },
                "symbol": {
                    "type": "string",
                    "description": "Symbol substring; append #N to pick the Nth occurrence"
                },
                "query": {
                    "type": "string",
                    "description": "Workspace-symbol query, or code-action title substring/index when applying"
                },
                "newName": {
                    "type": "string",
                    "description": "New symbol name for rename"
                },
                "newFile": {
                    "type": "string",
                    "description": "Destination path for rename_file"
                },
                "apply": {
                    "type": "boolean",
                    "description": "code_actions: apply the action selected by query instead of listing"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Per-request timeout in seconds (0 = registry default)"
                },
                "method": {
                    "type": "string",
                    "description": "Raw LSP method name for the request action"
                },
                "payload": {
                    "description": "Raw JSON params for the request action"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max locations returned (references/definition), capped at 1000"
                }
            },
            "required": ["action"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
            .union(ToolEffects::write())
            .union(ToolEffects::process())
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: LspInput = serde_json::from_value(input)
            .map_err(|err| tool_err("LSP_USAGE", format!("invalid input: {err}")))?;
        match input.action.as_str() {
            "diagnostics" => self.run_diagnostics(&input).await,
            "definition" => {
                self.run_position_request(&input, "definition", "textDocument/definition", json!({}))
                    .await
            }
            "references" => {
                self.run_position_request(
                    &input,
                    "references",
                    "textDocument/references",
                    json!({ "context": { "includeDeclaration": true } }),
                )
                .await
            }
            "hover" => {
                self.run_position_request(&input, "hover", "textDocument/hover", json!({}))
                    .await
            }
            "type_definition" => {
                self.run_position_request(
                    &input,
                    "type_definition",
                    "textDocument/typeDefinition",
                    json!({}),
                )
                .await
            }
            "implementation" => {
                self.run_position_request(
                    &input,
                    "implementation",
                    "textDocument/implementation",
                    json!({}),
                )
                .await
            }
            "symbols" => self.run_symbols(&input).await,
            "rename" => self.run_rename(&input).await,
            "rename_file" => self.run_rename_file(&input).await,
            "code_actions" => self.run_code_actions(&input).await,
            "status" => self.run_status().await,
            "reload" => self.run_reload(&input).await,
            "capabilities" => self.run_capabilities(&input).await,
            "request" => self.run_raw_request(&input).await,
            other => Ok(usage_error(format!(
                "unknown lsp action {other:?}; expected diagnostics|definition|references|hover|symbols|rename|rename_file|code_actions|type_definition|implementation|status|reload|capabilities|request"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_selector_parses_nth() {
        assert_eq!(
            parse_symbol_selector("render#2"),
            ("render".to_string(), Some(2))
        );
        assert_eq!(
            parse_symbol_selector("render"),
            ("render".to_string(), None)
        );
        // Trailing # without digits is part of the name.
        assert_eq!(
            parse_symbol_selector("c#"),
            ("c#".to_string(), None)
        );
        // #0 is invalid as an index and stays part of the name.
        assert_eq!(
            parse_symbol_selector("x#0"),
            ("x#0".to_string(), None)
        );
    }

    #[test]
    fn position_resolution_picks_and_disambiguates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = temp.path().join("a.rs");
        std::fs::write(&file, "fn alpha() {}\nfn beta() { alpha(); }\n").expect("file");
        let tool = LspTool::new(temp.path(), None);

        // Unique on a line.
        let pos = tool
            .resolve_position(&file, Some(1), "alpha")
            .expect("line 1");
        assert_eq!(pos, Position { line: 0, character: 3 });

        // Ambiguous across file without line.
        let err = tool
            .resolve_position(&file, None, "alpha")
            .expect_err("ambiguous");
        assert!(err.to_string().contains("LSP_SYMBOL_AMBIGUOUS"), "{err}");

        // #N selects.
        let pos = tool
            .resolve_position(&file, None, "alpha#2")
            .expect("second occurrence");
        assert_eq!(pos.line, 1);

        // Out-of-range #N errors.
        assert!(
            tool.resolve_position(&file, None, "alpha#9").is_err()
        );

        // Missing symbol errors.
        let err = tool
            .resolve_position(&file, None, "gamma")
            .expect_err("missing");
        assert!(err.to_string().contains("LSP_NO_SYMBOL"), "{err}");
    }

    #[test]
    fn cap_payload_truncates() {
        let small = json!({"a": 1});
        let (payload, truncated) = cap_payload(small.clone());
        assert!(!truncated);
        assert_eq!(payload, small);

        let big = json!({"data": "x".repeat(MAX_PAYLOAD_BYTES + 100)});
        let (_, truncated) = cap_payload(big);
        assert!(truncated);
    }

    #[test]
    fn select_code_action_by_index_and_title() {
        let actions = vec![
            json!({"title": "Add missing import"}),
            json!({"title": "Extract function"}),
        ];
        assert_eq!(
            select_code_action(&actions, "2").expect("index")["title"],
            "Extract function"
        );
        assert_eq!(
            select_code_action(&actions, "missing").expect("title")["title"],
            "Add missing import"
        );
        assert!(select_code_action(&actions, "9").is_err());
        assert!(select_code_action(&actions, "nope").is_err());
        let both = vec![
            json!({"title": "Fix all"}),
            json!({"title": "Fix this"}),
        ];
        assert!(select_code_action(&both, "fix").is_err());
    }
}
