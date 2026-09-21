//! Agent-facing `lsp` tool: IDE-grade code intelligence over child LSP servers.
//!
//! Position-based addressing uses file + 1-based line + symbol substring.
//! Code actions support lazy resolution and edit-then-command execution;
//! server-initiated edits require an explicitly selected command window.

mod actions;
pub mod client;
mod completion;
#[cfg(test)]
mod diagnostics_tests;
pub mod edits;
mod hierarchy;
pub mod jsonrpc;
mod refactor_preview;
pub mod registry;
mod semantic;
pub mod text;
mod workspace_diagnostics;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use client::{hover_to_text, parse_locations, uri_to_path};
use registry::{LspRegistry, ServerEntry};
use text::{Position, find_occurrences, offset_to_position};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};

const MAX_PAYLOAD_BYTES: usize = 200 * 1024;
const DEFAULT_LOCATION_LIMIT: usize = 100;
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

fn resolve_tool_path(path: &str, cwd: &Path) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).map_or_else(
        |_| path.display().to_string(),
        |rel| rel.display().to_string(),
    )
}

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

/// One instance owns its language servers and cached code-action identities.
///
/// Whole workflows are serialized so another tool call cannot interleave
/// document synchronization or edits with a selected action's command window.
pub struct LspTool {
    cwd: PathBuf,
    registry: LspRegistry,
    actions: Arc<actions::ActionState>,
    hierarchies: hierarchy::HierarchyCache,
    completions: completion::CompletionCache,
    refactors: refactor_preview::RefactorCache,
    operations: Arc<asupersync::sync::Mutex<()>>,
}

impl LspTool {
    #[must_use]
    pub fn new(cwd: &Path, config: Option<&Config>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            registry: LspRegistry::new(cwd, config),
            actions: Arc::new(actions::ActionState::default()),
            hierarchies: hierarchy::HierarchyCache::default(),
            completions: completion::CompletionCache::default(),
            refactors: refactor_preview::RefactorCache::default(),
            operations: Arc::new(asupersync::sync::Mutex::new(())),
        }
    }

    fn language_id_for(path: &Path, spec: &registry::ServerSpec) -> String {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
            .and_then(|dotted| registry::language_id_for_extension(&dotted).map(str::to_string))
            .or_else(|| spec.languages.first().cloned())
            .unwrap_or_else(|| "plaintext".to_string())
    }

    async fn client_for(&self, path: &Path) -> Result<Arc<ServerEntry>> {
        let entry = self.registry.client_for(path).await?;
        actions::install_apply_edit_handler(&entry, Arc::clone(&self.actions));
        Ok(entry)
    }

    async fn synced(&self, path: &Path) -> Result<(String, Arc<ServerEntry>)> {
        let entry = self.client_for(path).await?;
        let spec = self.registry.spec_for_file(path).ok_or_else(|| {
            tool_err("LSP_NO_SERVER", format!("no server for {}", path.display()))
        })?;
        let language_id = Self::language_id_for(path, spec);
        let uri = entry.client.ensure_synced(path, &language_id)?;
        Ok((uri, entry))
    }

    fn resolve_position(path: &Path, line: Option<u32>, symbol: &str) -> Result<Position> {
        let content = std::fs::read_to_string(path).map_err(|err| {
            tool_err(
                "LSP_FILE_UNREADABLE",
                format!("cannot read {}: {err}", path.display()),
            )
        })?;
        Self::resolve_position_in(path, &content, line, symbol)
    }

    fn resolve_position_in(
        path: &Path,
        content: &str,
        line: Option<u32>,
        symbol: &str,
    ) -> Result<Position> {
        let (needle, nth) = parse_symbol_selector(symbol);
        if needle.is_empty() {
            return Err(tool_err("LSP_NO_SYMBOL", "symbol must not be empty"));
        }
        let occurrences = find_occurrences(content, &needle, line.map(|l| l.saturating_sub(1)));
        if occurrences.is_empty() {
            let scope = line.map_or_else(|| "file".to_string(), |l| format!("line {l}"));
            return Err(tool_err(
                "LSP_NO_SYMBOL",
                format!(
                    "no occurrence of {needle:?} in {scope} of {}",
                    path.display()
                ),
            ));
        }
        let selected = match (nth, occurrences.len()) {
            (Some(n), len) if n <= len => occurrences[n - 1],
            (Some(n), len) => {
                return Err(tool_err(
                    "LSP_SYMBOL_AMBIGUOUS",
                    format!(
                        "selector asked for occurrence #{n} but only {len} match(es) of {needle:?} exist"
                    ),
                ));
            }
            (None, 1) => occurrences[0],
            (None, len) => {
                if line.is_some() {
                    return Err(tool_err(
                        "LSP_SYMBOL_AMBIGUOUS",
                        format!(
                            "{len} matches of {needle:?} on that line; disambiguate with {needle}#N"
                        ),
                    ));
                }
                return Err(tool_err(
                    "LSP_SYMBOL_AMBIGUOUS",
                    format!(
                        "{len} matches of {needle:?} in file; narrow with `line` or {needle}#N"
                    ),
                ));
            }
        };
        offset_to_position(content, selected.0).ok_or_else(|| {
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

    fn locations_output(
        &self,
        action: &str,
        locations: &[(String, text::Range)],
        limit: usize,
    ) -> ToolOutput {
        let mut entries = Vec::new();
        for (uri, range) in locations.iter().take(limit) {
            let path =
                uri_to_path(uri).map_or_else(|| uri.clone(), |p| display_path(&p, &self.cwd));
            entries.push(
                json!({"file":path,"line":range.start.line+1,"character":range.start.character+1}),
            );
        }
        let payload = json!({"action":action,"count":entries.len(),"truncated":locations.len()>limit,"locations":entries});
        text_output(payload.to_string(), payload)
    }

    async fn run_diagnostics(&self, input: &LspInput) -> Result<ToolOutput> {
        let Some(file) = input.file.as_deref() else {
            return Ok(usage_error(
                "lsp diagnostics requires `file` (a path or glob like src/**/*.rs)",
            ));
        };
        if file.contains(['*', '[', '?']) {
            let override_filter = build_glob_override(&self.cwd, file)?;
            let mut matched = Vec::new();
            for status in self.registry.status() {
                if let Some(entry) = self.registry.entry_for_root(&status.name, &status.root) {
                    for (uri, diags) in entry.client.diagnostics_snapshot() {
                        if let Some(path) = uri_to_path(&uri) {
                            let rel = path.strip_prefix(&self.cwd).unwrap_or(&path);
                            if override_filter.matched(rel, false).is_ignore() {
                                matched.push(json!({"file":display_path(&path,&self.cwd),"server":status.name,"diagnostics":diags}));
                            }
                        }
                    }
                }
            }
            let payload = json!({
                "action":"diagnostics","glob":file,"files":matched.len(),"entries":matched,
                "cachedOnly":true,"complete":false,
                "note":"Cached reports only; files absent from this view have not been checked by this request."
            });
            return Ok(text_output(payload.to_string(), payload));
        }
        let path = resolve_tool_path(file, &self.cwd);
        let (uri, entry) = self.synced(&path).await?;
        let wait = input
            .timeout
            .filter(|secs| *secs > 0)
            .map_or(client::DEFAULT_DIAGNOSTICS_WAIT, |secs| {
                Duration::from_secs(secs).min(Duration::from_secs(60))
            });
        let diags = entry.client.document_diagnostics(&uri, wait).await?;
        let payload = json!({"action":"diagnostics","file":display_path(&path,&self.cwd),"server":entry.spec_name,"count":diags.len(),"diagnostics":diags});
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
        let mut params = json!({"textDocument":{"uri":uri},"position":position});
        if let (Some(dst), Some(src)) = (params.as_object_mut(), extra_params.as_object()) {
            for (key, value) in src {
                dst.insert(key.clone(), value.clone());
            }
        }
        let result = entry
            .client
            .call(method, params, self.request_timeout(input))
            .await?;
        if action == "hover" {
            let text = hover_to_text(&result).unwrap_or_else(|| "no hover information".to_string());
            let payload = json!({"action":"hover","file":display_path(&path,&self.cwd),"line":position.line+1,"hover":text});
            return Ok(text_output(payload.to_string(), payload));
        }
        let locations = parse_locations(&result);
        let limit = input
            .limit
            .unwrap_or(DEFAULT_LOCATION_LIMIT)
            .min(HARD_LOCATION_LIMIT);
        if locations.is_empty() {
            let payload = json!({"action":action,"file":display_path(&path,&self.cwd),"line":position.line+1,
                "count":0,"locations":[],"note":format!("no {action} found at that position")});
            return Ok(text_output(payload.to_string(), payload));
        }
        Ok(self.locations_output(action, &locations, limit))
    }

    fn require_position(&self, input: &LspInput) -> Result<(PathBuf, Position)> {
        let file = input.file.as_deref().ok_or_else(|| {
            tool_err("LSP_USAGE", format!("lsp {} requires `file`", input.action))
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
        let position = Self::resolve_position(&path, input.line, symbol)?;
        Ok((path, position))
    }

    async fn run_symbols(&self, input: &LspInput) -> Result<ToolOutput> {
        if input.query.is_some() {
            return self.run_workspace_symbols(input).await;
        }
        if input.resolve.is_some() {
            return Err(tool_err("LSP_USAGE", "symbol resolution requires a workspace query"));
        }
        match (input.file.as_deref(), input.query.as_deref()) {
            (Some(file), _) => {
                let path = resolve_tool_path(file, &self.cwd);
                let (uri, entry) = self.synced(&path).await?;
                let result = entry
                    .client
                    .call(
                        "textDocument/documentSymbol",
                        json!({"textDocument":{"uri":uri}}),
                        self.request_timeout(input),
                    )
                    .await?;
                let (payload, truncated) = cap_payload(
                    json!({"action":"symbols","file":display_path(&path,&self.cwd),"server":entry.spec_name,"symbols":result}),
                );
                Ok(text_output(
                    payload.to_string(),
                    json!({"truncated":truncated,"payload":payload}),
                ))
            }
            (None, _) => Ok(usage_error(
                "lsp symbols requires `file` (document symbols) or `query` (workspace symbols)",
            )),
        }
    }

    fn code_action_range(input: &LspInput, path: &Path) -> Result<Value> {
        if input.range.is_some() && (input.symbol.is_some() || input.line.is_some()) {
            return Err(tool_err(
                "LSP_USAGE",
                "code_actions range cannot be combined with symbol or line",
            ));
        }
        if let Some(symbol) = input.symbol.as_deref() {
            let position = Self::resolve_position(path, input.line, symbol)?;
            return Ok(json!({"start":position,"end":position}));
        }
        if input.line.is_some() {
            return Err(tool_err(
                "LSP_USAGE",
                "code_actions line requires symbol; use range to select text",
            ));
        }
        let content = std::fs::read_to_string(path).map_err(|err| {
            tool_err(
                "LSP_FILE_UNREADABLE",
                format!("cannot read {}: {err}", path.display()),
            )
        })?;
        if let Some(range) = input.range {
            if range.end < range.start
                || text::position_to_offset_exact(&content, range.start).is_none()
                || text::position_to_offset_exact(&content, range.end).is_none()
            {
                return Err(tool_err(
                    "LSP_USAGE",
                    "code_actions range must be ordered and use exact zero-based UTF-16 boundaries in the document",
                ));
            }
            return Ok(json!(range));
        }
        let end = offset_to_position(&content, content.len()).ok_or_else(|| {
            tool_err(
                "LSP_USAGE",
                "cannot represent the document end as an LSP position",
            )
        })?;
        Ok(json!({"start":Position { line:0, character:0 },"end":end}))
    }

    async fn run_status(&self) -> Result<ToolOutput> {
        let statuses = self.registry.status();
        let servers: Vec<_> = statuses.iter().map(|s| json!({
            "name":s.name,"serverName":s.server_name,"root":s.root.display().to_string(),"alive":s.alive,
            "idleSecs":s.idle_secs,"openDocuments":s.open_documents,"droppedNotifications":s.dropped_notifications
        })).collect();
        let configured: Vec<_> = self
            .registry
            .configured_servers()
            .iter()
            .map(|spec| {
                json!({
                    "name":spec.name,"command":spec.command,"extensions":spec.extensions
                })
            })
            .collect();
        let payload = json!({"action":"status","cwd":self.cwd.display().to_string(),"live":servers,"configured":configured});
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_reload(&self, input: &LspInput) -> Result<ToolOutput> {
        let path = input
            .file
            .as_deref()
            .map(|f| resolve_tool_path(f, &self.cwd));
        let killed = self.registry.kill_matching(path.as_deref()).await;
        self.hierarchies.clear();
        self.completions.clear();
        self.refactors.clear();
        let payload =
            json!({"action":"reload","killed":killed,"note":"servers respawn lazily on next use"});
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
        let payload = json!({"action":"capabilities","server":entry.spec_name,"serverName":caps.server_name,
            "willRenameFiles":caps.raw.pointer("/workspace/fileOperations/willRename").is_some_and(Value::is_object),"textDocumentSyncKind":caps.sync_kind,"capabilities":caps.raw});
        Ok(text_output(payload.to_string(), payload))
    }

    async fn run_raw_request(&self, input: &LspInput) -> Result<ToolOutput> {
        let (Some(method), Some(file)) = (input.method.as_deref(), input.file.as_deref()) else {
            return Ok(usage_error(
                "lsp request requires `method` and `file` (its extension picks the server)",
            ));
        };
        if method == "workspace/executeCommand" {
            return Err(tool_err(
                "LSP_USAGE",
                "select a code_actions result to execute a server command with scoped edit permission",
            ));
        }
        let path = resolve_tool_path(file, &self.cwd);
        let entry = self.client_for(&path).await?;
        let result = entry
            .client
            .call(
                method,
                input.payload.clone().unwrap_or(Value::Null),
                self.request_timeout(input),
            )
            .await?;
        let (payload, truncated) = cap_payload(
            json!({"action":"request","method":method,"server":entry.spec_name,"result":result}),
        );
        Ok(text_output(
            payload.to_string(),
            json!({"truncated":truncated,"payload":payload}),
        ))
    }
}

fn select_code_action(actions: &[Value], query: &str) -> Result<Value> {
    if let Ok(index) = query.parse::<usize>() {
        return actions
            .get(index.saturating_sub(1))
            .filter(|_| index >= 1)
            .cloned()
            .ok_or_else(|| {
                tool_err(
                    "LSP_USAGE",
                    format!(
                        "code action index {index} out of range ({} actions)",
                        actions.len()
                    ),
                )
            });
    }
    let needle = query.to_ascii_lowercase();
    let matches: Vec<_> = actions
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

fn cap_payload(payload: Value) -> (Value, bool) {
    let serialized = payload.to_string();
    if serialized.len() <= MAX_PAYLOAD_BYTES {
        return (payload, false);
    }
    let mut end = MAX_PAYLOAD_BYTES;
    while !serialized.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = serialized;
    truncated.truncate(end);
    truncated.push_str("...[TRUNCATED]");
    (Value::String(truncated), true)
}

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
    position: Option<Position>,
    resolve: Option<bool>,
    completion_id: Option<String>,
    snippet_values: Option<std::collections::BTreeMap<String, String>>,
    file: Option<String>,
    line: Option<u32>,
    symbol: Option<String>,
    query: Option<String>,
    action_id: Option<String>,
    refactor_id: Option<String>,
    new_name: Option<String>,
    new_file: Option<String>,
    apply: Option<bool>,
    timeout: Option<u64>,
    method: Option<String>,
    payload: Option<Value>,
    limit: Option<usize>,
    after: Option<String>,
    range: Option<text::Range>,
    only: Option<Vec<String>>,
    format_options: Option<Value>,
    hierarchy_id: Option<String>,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound, clippy::too_many_lines)]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }
    fn label(&self) -> &str {
        "lsp"
    }
    fn description(&self) -> &str {
        concat!(
            "IDE-grade code intelligence via language servers: diagnostics, definition, references, hover, symbols, incoming_calls, outgoing_calls, supertypes, subtypes, rename, rename_file, code_actions, format, type_definition, implementation, status, reload, capabilities, request, workspace_diagnostics and completion. completion lists semantic suggestions at file + exact position; query optionally filters by case-sensitive prefix. Select completionId to resolve and preview, then apply:true to insert with auto-import edits. Numeric snippet placeholders accept literal snippetValues; repeat the values when applying. Commands, snippet variables and transforms are unsupported. Completion range is an explicit replacement fallback for servers omitting textEdit. workspace_diagnostics actively checks a workspace-relative file glob, lazily starting servers; inspect complete and all per-file errors. diagnostics globs remain a server-free cached view. Call/type hierarchy queries start at file + symbol, then follow returned hierarchyId handles within the same hierarchy kind. code_actions accepts a selected range and only kinds such as refactor.extract, refactor.inline or source.organizeImports. List first, then select actionId or a fresh title/index query to resolve and preview an edit-only action; approve the returned refactorId with apply:true. Direct apply:true with actionId or query retains edit-then-command execution; command-backed actions cannot be statically previewed. Cached actionId already identifies its selection; do not combine it with range, only, symbol, line or query. format previews document or range formatting; apply:true writes the changes. Position addressing uses file + 1-indexed line + symbol substring; symbol#N selects an occurrence. All range positions are zero-based UTF-16.",
            " signature_help inspects callable overloads and the active parameter at file + exact position. inlay_hints inspects inferred types and argument labels over file + optional exact range; resolve:true obtains lazy tooltips and label locations. Both actions are read-only and never accept hint edits or run commands.",
            " rename and rename_file with apply:false stage a reviewable workspace edit without writes. Then use the same action with refactorId and apply:true to commit that exact plan without another server request. Omitting apply on a fresh rename preserves immediate application.",
            " prepare_rename inspects the server-confirmed symbol range and placeholder without computing edits. It and rename accept file + exact position instead of symbol/line. Fresh rename automatically prepares when supported; a refusal stops before computing edits. Preparation is not an approval handle.",
            " symbols with query searches the workspace selected by an anchor file (file or symbol, not both); file without query lists document symbols. resolve:true fills missing symbol location ranges when supported. Inspect truncated and unresolvedIndices; returned URIs are metadata, never opened or executed."
        )
    }
    fn parameters(&self) -> Value {
        json!({
            "type":"object","required":["action"],
            "properties": {
                "action":{"type":"string","enum":["diagnostics","definition","references","hover","symbols","incoming_calls","outgoing_calls","supertypes","subtypes","rename","rename_file","code_actions","format","type_definition","implementation","status","reload","capabilities","request","workspace_diagnostics","completion","signature_help","inlay_hints","prepare_rename"]},
                "resolve":{"type":"boolean","description":"inlay_hints: resolve retained hints for tooltips and label locations. symbols with query: resolve retained symbols missing location ranges. Requires server resolve support; omit or false for inline results. Uses the same request budget and never opens returned URIs, applies edits or executes commands."},
                "position":{"type":"object","description":"Exact zero-based UTF-16 cursor for completion, signature_help, prepare_rename or rename; requires file. For rename targeting, use instead of symbol/line. Put signature_help's cursor inside the call. Completion may also use an explicit replacement range.","required":["line","character"],"properties":{"line":{"type":"integer","minimum":0},"character":{"type":"integer","minimum":0}}},
                "completionId":{"type":"string","description":"Opaque completion from the latest listing. Select without apply to resolve and preview, or apply:true to insert it with its auto-import edits. Do not combine with other selectors. Expires on source changes, server replacement, reload or another completion listing."},
                "snippetValues":{"type":"object","maxProperties":64,"additionalProperties":{"type":"string","maxLength":16384},"description":"Selected snippet completion only: literal replacements keyed by canonical numeric placeholder index (0..65535), e.g. {\"1\":\"argument\"}. Values are not evaluated or reparsed. Defaults and first choices apply otherwise; unbound positive tabstops require a value. Repeat the map with apply:true; preview substitutions are not cached."},
                "file":{"type":"string","description":"Path relative to cwd or absolute; diagnostics globs inspect cached reports. workspace_diagnostics uses a positive workspace-relative glob to actively check matching nonignored regular files; it may start language servers."},
                "line":{"type":"integer","minimum":1,"description":"1-indexed line narrowing symbol search"},
                "symbol":{"type":"string","description":"Symbol substring; append #N for the Nth occurrence"},
                "query":{"type":"string","description":"symbols: workspace query (up to 1024 UTF-8 bytes; empty requests all symbols), with file or symbol as its anchor file. Also fresh nonempty code-action title/index selection (preview unless apply:true), or case-sensitive completion filterText/label prefix (at most 128 bytes)."},
                "actionId":{"type":"string","maxLength":128,"description":"Opaque ID from a prior code_actions listing. Without apply:true, consumes the action to resolve and stage an edit-only preview with refactorId. With apply:true, executes it directly, including its command. Cannot be combined with query, range, only, symbol or line."},
                "refactorId":{"type":"string","maxLength":128,"description":"Opaque reviewed plan from rename, rename_file or a selected edit-only code_actions result. Use the same action and apply:true to consume and commit it; omit apply to inspect it again without any server request. No other selectors are accepted. Expires after five minutes, reload or another fresh refactor request. Every read preimage must remain unchanged."},
                "hierarchyId":{"type":"string","description":"Opaque hierarchy item from a previous result; use instead of file/line/symbol to traverse one more level. Call and type handles are not interchangeable. Handles expire with their source or server."},
                "newName":{"type":"string","description":"New symbol name for rename"},
                "newFile":{"type":"string","description":"Destination path for rename_file"},
                "apply":{"type":"boolean","description":"Apply a selected code action/completion or formatting. Selected code actions preview by default; true executes directly. Fresh rename/rename_file apply immediately unless false, which stages a preview. A selected refactorId writes only with true."},
                "range":{"type":"object","description":"Optional code_actions, format or inlay_hints selection, with exact zero-based lines and UTF-16 character offsets. Hints default to the whole file. For completion, this is a single-line replacement fallback containing position, used only when the server omits textEdit. For code_actions, cannot be combined with symbol or line. Refactors may also edit outside the selection.","required":["start","end"],"properties":{
                    "start":{"type":"object","required":["line","character"],"properties":{"line":{"type":"integer","minimum":0},"character":{"type":"integer","minimum":0}}},
                    "end":{"type":"object","required":["line","character"],"properties":{"line":{"type":"integer","minimum":0},"character":{"type":"integer","minimum":0}}}
                }},
                "only":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1,"maxLength":128},"description":"code_actions kinds, matching the named kind and its dot-separated descendants, e.g. [refactor.extract] or [source.organizeImports]. Nonmatching or unclassified server results are excluded before indexing and selection. Omit for all kinds."},
                "formatOptions":{"type":"object","maxProperties":64,"description":"Formatting options; defaults to tabSize 4 and insertSpaces true. Additional server options must be boolean, 32-bit integer or bounded string values.","properties":{
                    "tabSize":{"type":"integer","minimum":1,"maximum":32,"default":4},
                    "insertSpaces":{"type":"boolean","default":true},
                    "trimTrailingWhitespace":{"type":"boolean"},
                    "insertFinalNewline":{"type":"boolean"},
                    "trimFinalNewlines":{"type":"boolean"}
                }},
                "timeout":{"type":"integer","description":"Per-request timeout in seconds (0 = registry default). workspace_diagnostics instead budgets the whole scan (default 30 seconds, capped at 120); synchronous filesystem operations are not preemptible."},
                "method":{"type":"string","description":"Raw LSP method; executeCommand requires code_actions"},
                "payload":{"description":"Raw JSON params for request"},
                "limit":{"type":"integer","description":"Max returned locations, capped at 1000; hierarchy items are capped at 128. completion, inlay_hints and workspace symbols default to 50, capped at 128. signature_help defaults to 16, capped at 128, retaining the active overload. workspace_diagnostics checks at most this many files (default 100, capped at 256)."},
                "after":{"type":"string","maxLength":4096,"description":"workspace_diagnostics only: resume strictly after the workspace-relative path returned as nextAfter. Use the same glob. This is a stateless path cursor, not a snapshot; complete is false for continuation pages. Inspect pageComplete, hasMore and all per-file failures."}
            }
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
        if input.line == Some(0) {
            return Err(tool_err("LSP_USAGE", "line must be 1-indexed"));
        }
        refactor_preview::validate_selection(&input)?;
        if input.resolve.is_some() && !matches!(input.action.as_str(), "inlay_hints" | "symbols") {
            return Err(tool_err(
                "LSP_USAGE",
                "resolve is supported only by inlay_hints and workspace symbols",
            ));
        }
        if input.action != "completion"
            && (input.completion_id.is_some() || input.snippet_values.is_some())
        {
            return Err(tool_err(
                "LSP_USAGE",
                "completionId and snippetValues require completion",
            ));
        }
        if input.position.is_some()
            && !matches!(
                input.action.as_str(),
                "completion" | "signature_help" | "prepare_rename" | "rename"
            )
        {
            return Err(tool_err(
                "LSP_USAGE",
                "position requires completion, signature_help, prepare_rename or rename",
            ));
        }
        if input.only.is_some() && input.action != "code_actions" {
            return Err(tool_err("LSP_USAGE", "only is supported by code_actions"));
        }
        if input.after.is_some() && input.action != "workspace_diagnostics" {
            return Err(tool_err(
                "LSP_USAGE",
                "after requires workspace_diagnostics",
            ));
        }
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        let _operation =
            asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&self.operations), owner.cx())
                .await
                .map_err(|_| tool_err("LSP_CANCELLED", "LSP workflow cancelled while queued"))?;
        if input.refactor_id.is_some() {
            return self.select_refactor(&input, &owner);
        }
        if matches!(input.action.as_str(), "rename" | "rename_file") {
            refactor_preview::check_owner(&owner)?;
            self.refactors.clear();
        }
        match input.action.as_str() {
            "completion" => self.run_completion(&input).await,
            "signature_help" => self.run_signature_help(&input).await,
            "inlay_hints" => self.run_inlay_hints(&input).await,
            "diagnostics" => self.run_diagnostics(&input).await,
            "workspace_diagnostics" => {
                let pattern = input.file.as_deref().ok_or_else(|| {
                    tool_err(
                        "LSP_USAGE",
                        "workspace_diagnostics requires file (a workspace-relative glob)",
                    )
                })?;
                self.run_workspace_diagnostics(&input, pattern).await
            }
            "definition" => {
                self.run_position_request(
                    &input,
                    "definition",
                    "textDocument/definition",
                    json!({}),
                )
                .await
            }
            "references" => {
                self.run_position_request(
                    &input,
                    "references",
                    "textDocument/references",
                    json!({"context":{"includeDeclaration":true}}),
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
            "incoming_calls" | "outgoing_calls" | "supertypes" | "subtypes" => {
                self.run_hierarchy(&input).await
            }
            "rename" => self.run_rename(&input).await,
            "prepare_rename" => self.run_prepare_rename(&input).await,
            "rename_file" => self.run_rename_file(&input).await,
            "code_actions" => self.run_code_actions(&input).await,
            "format" => self.run_format(&input).await,
            "status" => self.run_status().await,
            "reload" => self.run_reload(&input).await,
            "capabilities" => self.run_capabilities(&input).await,
            "request" => self.run_raw_request(&input).await,
            other => Ok(usage_error(format!(
                "unknown lsp action {other:?}; expected diagnostics|definition|references|hover|symbols|incoming_calls|outgoing_calls|supertypes|subtypes|rename|rename_file|prepare_rename|code_actions|format|type_definition|implementation|status|reload|capabilities|request|workspace_diagnostics|completion|signature_help|inlay_hints"
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
        assert_eq!(parse_symbol_selector("c#"), ("c#".to_string(), None));
        assert_eq!(parse_symbol_selector("x#0"), ("x#0".to_string(), None));
    }

    #[test]
    fn position_resolution_picks_and_disambiguates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = temp.path().join("a.rs");
        std::fs::write(&file, "fn alpha() {}\nfn beta() { alpha(); }\n").expect("file");
        assert_eq!(
            LspTool::resolve_position(&file, Some(1), "alpha").expect("line 1"),
            Position {
                line: 0,
                character: 3
            }
        );
        let err = LspTool::resolve_position(&file, None, "alpha").expect_err("ambiguous");
        assert!(err.to_string().contains("LSP_SYMBOL_AMBIGUOUS"), "{err}");
        assert_eq!(
            LspTool::resolve_position(&file, None, "alpha#2")
                .expect("second occurrence")
                .line,
            1
        );
        assert!(LspTool::resolve_position(&file, None, "alpha#9").is_err());
        let err = LspTool::resolve_position(&file, None, "gamma").expect_err("missing");
        assert!(err.to_string().contains("LSP_NO_SYMBOL"), "{err}");
    }

    #[test]
    fn cap_payload_truncates() {
        let small = json!({"a":1});
        let (payload, truncated) = cap_payload(small.clone());
        assert!(!truncated);
        assert_eq!(payload, small);
        assert!(cap_payload(json!({"data":"x".repeat(MAX_PAYLOAD_BYTES+100)})).1);
        assert!(cap_payload(json!({"data":"界".repeat(MAX_PAYLOAD_BYTES)})).1);
    }

    #[test]
    fn select_code_action_by_index_and_title() {
        let actions = vec![
            json!({"title":"Add missing import"}),
            json!({"title":"Extract function"}),
        ];
        assert_eq!(
            select_code_action(&actions, "2").unwrap()["title"],
            "Extract function"
        );
        assert_eq!(
            select_code_action(&actions, "missing").unwrap()["title"],
            "Add missing import"
        );
        assert!(select_code_action(&actions, "9").is_err());
        assert!(select_code_action(&actions, "nope").is_err());
        assert!(
            select_code_action(
                &[json!({"title":"Fix all"}), json!({"title":"Fix this"})],
                "fix"
            )
            .is_err()
        );
    }

    #[test]
    fn code_action_range_includes_the_last_line_and_utf16_columns() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.rs");
        std::fs::write(&path, "first\n😀last").unwrap();
        let input: LspInput = serde_json::from_value(json!({"action":"code_actions"})).unwrap();
        assert_eq!(
            LspTool::code_action_range(&input, &path).unwrap()["end"],
            json!({"line":1,"character":6})
        );
    }
}
