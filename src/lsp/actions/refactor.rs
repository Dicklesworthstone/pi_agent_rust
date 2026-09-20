//! Refactoring requests retain their source/version evidence through apply.
//!
//! Reuse the selected-action workspace boundary and transaction implementation.
//! A server response never grants additional filesystem scope, and a rename
//! lookup never opens the executeCommand permission window.

use super::{
    AgentCx, ApplyOutcome, FileOp, HashMap, LspInput, LspTool, MAX_ACTION_BYTES, Path, PathBuf,
    Result, ServerEntry, ToolOutput, Value, apply_scoped, display_path, file_hash, inside_root,
    json, lock, parse_workspace_edit, resolve_tool_path, tool_err, verify_source,
};
use crate::lsp::client::{DocumentSnapshot, try_path_to_uri, uri_to_path};
use crate::lsp::edits::{FileEvidence, PreparedEdit, WorkspaceEditPlan};

mod formatting;
mod versions;

pub(super) struct RefactorSnapshot {
    source: PathBuf,
    source_hash: u64,
    documents: HashMap<PathBuf, DocumentSnapshot>,
}

impl RefactorSnapshot {
    pub(super) fn command_edits(&self, owner: AgentCx) -> super::command_edits::CommandEdits {
        super::command_edits::CommandEdits::new(
            self.source.clone(),
            self.source_hash,
            self.documents.clone(),
            owner,
        )
    }

    pub(super) fn capture(entry: &ServerEntry, source: &Path, hash: u64) -> Result<Self> {
        inside_root(source, entry.client.root())?;
        verify_source(source, hash)?;
        let documents = entry.client.document_snapshots();
        if documents
            .get(source)
            .is_some_and(|document| document.hash != hash)
        {
            return Err(tool_err(
                "LSP_EDIT_CONFLICT",
                "source synchronization changed before refactoring",
            ));
        }
        Ok(Self {
            source: source.to_path_buf(),
            source_hash: hash,
            documents,
        })
    }

    /// Check the original source before resolving a cached action or running a
    /// command. Keep the listing-time snapshot; refreshing it would bless drift.
    pub(super) fn verify_request_source(&self, entry: &ServerEntry) -> Result<()> {
        let empty = parse_workspace_edit(&Value::Null)?;
        self.validate(entry, &Value::Null, &empty).map(|_| ())
    }

    fn validate(
        &self,
        entry: &ServerEntry,
        raw: &Value,
        plan: &WorkspaceEditPlan,
    ) -> Result<HashMap<PathBuf, u64>> {
        verify_source(&self.source, self.source_hash)?;
        let current = entry.client.document_snapshots();
        validate_versions(raw, &self.documents, &current)?;
        let mut hashes = HashMap::new();
        for (path, snapshot) in &self.documents {
            let touched = plan.text_edits.contains_key(path)
                || path == &self.source
                || plan.file_ops.iter().any(|operation| match operation {
                    FileOp::Create { path: target, .. } | FileOp::Delete { path: target } => {
                        target == path
                    }
                    FileOp::Rename {
                        old_path, new_path, ..
                    } => old_path == path || new_path == path,
                });
            if touched {
                if current.get(path).is_none_or(|document| {
                    document.version != snapshot.version || document.hash != snapshot.hash
                }) {
                    return Err(tool_err(
                        "LSP_EDIT_CONFLICT",
                        "document synchronization changed during refactoring",
                    ));
                }
                hashes.insert(path.clone(), snapshot.hash);
            }
        }
        hashes.insert(self.source.clone(), self.source_hash);
        Ok(hashes)
    }
}

pub(super) fn validate_versions(
    raw: &Value,
    requested: &HashMap<PathBuf, DocumentSnapshot>,
    current: &HashMap<PathBuf, DocumentSnapshot>,
) -> Result<()> {
    versions::validate(raw, requested, current)
}

pub(super) fn check_response_size(raw: &Value) -> Result<()> {
    // Count without allocating another potentially large response copy.
    struct Limit(usize);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("workspace edit exceeds 2 MiB"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(&mut Limit(MAX_ACTION_BYTES), raw)
        .map_err(|_| tool_err("LSP_EDIT_LIMIT", "workspace edit exceeds 2 MiB"))
}

/// Append the user-requested move AFTER server import updates. Preserve the
/// server's document order and annotations, and never combine both edit forms.
fn append_move(raw: Value, old_uri: &str, new_uri: &str) -> Result<Value> {
    check_response_size(&raw)?;
    parse_workspace_edit(&raw)?;
    let mut object = match raw {
        Value::Null => serde_json::Map::new(),
        Value::Object(object) => object,
        _ => {
            return Err(tool_err(
                "LSP_EDIT_MALFORMED",
                "workspace edit is not an object",
            ));
        }
    };
    let mut ordered = match object.remove("documentChanges") {
        Some(Value::Array(ordered)) => ordered,
        None => match object.remove("changes") {
            Some(Value::Object(changes)) => changes
                .into_iter()
                .map(
                    |(uri, edits)| json!({"textDocument":{"uri":uri,"version":null},"edits":edits}),
                )
                .collect(),
            None => Vec::new(),
            _ => return Err(tool_err("LSP_EDIT_MALFORMED", "invalid changes object")),
        },
        _ => {
            return Err(tool_err(
                "LSP_EDIT_MALFORMED",
                "invalid documentChanges array",
            ));
        }
    };
    object.remove("changes");
    ordered.push(
        json!({"kind":"rename","oldUri":old_uri,"newUri":new_uri,"options":{"overwrite":false}}),
    );
    object.insert("documentChanges".to_string(), Value::Array(ordered));
    Ok(Value::Object(object))
}

/// Static registrations apply to the original file being renamed. Globs use
/// native paths, not percent-encoded URI text. No filesystem traversal occurs.
fn registered_for_file(capabilities: &Value, operation: &str, path: &Path) -> Result<bool> {
    let Some(options) = capabilities
        .pointer("/workspace/fileOperations")
        .and_then(|operations| operations.get(operation))
    else {
        return Ok(false);
    };
    let invalid = || {
        tool_err(
            "LSP_FILE_OPERATION_OPTIONS",
            "invalid file operation registration",
        )
    };
    let filters = options
        .get("filters")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if filters.len() > 128 {
        return Err(invalid());
    }
    let mut matched = false;
    for filter in filters {
        let scheme_matches = match filter.get("scheme") {
            None => true,
            Some(Value::String(scheme)) => scheme == "file",
            _ => return Err(invalid()),
        };
        let pattern = filter.get("pattern").ok_or_else(invalid)?;
        let glob = pattern
            .get("glob")
            .and_then(Value::as_str)
            .filter(|glob| glob.len() <= 4096)
            .ok_or_else(invalid)?;
        let file_matches = match pattern.get("matches") {
            None => true,
            Some(Value::String(kind)) if kind == "file" => true,
            Some(Value::String(kind)) if kind == "folder" => false,
            _ => return Err(invalid()),
        };
        let ignore_case = match pattern.get("options") {
            None => false,
            Some(Value::Object(options)) => match options.get("ignoreCase") {
                None => false,
                Some(Value::Bool(value)) => *value,
                _ => return Err(invalid()),
            },
            _ => return Err(invalid()),
        };
        let matcher = globset::GlobBuilder::new(glob)
            .literal_separator(true)
            .backslash_escape(false)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|_| invalid())?
            .compile_matcher();
        matched |= scheme_matches && file_matches && matcher.is_match(path);
    }
    Ok(matched)
}

impl LspTool {
    fn prepare_refactor(
        entry: &ServerEntry,
        raw: &Value,
        snapshot: &RefactorSnapshot,
    ) -> Result<PreparedEdit> {
        check_response_size(raw)?;
        let plan = parse_workspace_edit(raw)?;
        let hashes = snapshot.validate(entry, raw, &plan)?;
        // Admission precedes staging: even a preview must not read arbitrary
        // server-selected files. This is the same boundary as apply_scoped.
        let root = entry.client.root();
        for path in plan.text_edits.keys().chain(hashes.keys()) {
            inside_root(path, root)?;
        }
        for operation in &plan.file_ops {
            match operation {
                FileOp::Create { path, .. } | FileOp::Delete { path } => inside_root(path, root)?,
                FileOp::Rename {
                    old_path, new_path, ..
                } => {
                    inside_root(old_path, root)?;
                    inside_root(new_path, root)?;
                }
            }
        }
        let expected = hashes
            .into_iter()
            .map(|(path, hash)| (path, FileEvidence::Text(hash)))
            .collect();
        PreparedEdit::new(&plan, &expected)
    }

    pub(in crate::lsp) fn invalidate_refactor(&self, entry: &ServerEntry) {
        entry.client.invalidate_all();
        lock(&self.actions.cache).clear();
        self.hierarchies.clear();
        self.completions.clear();
        self.refactors.clear();
    }

    pub(super) fn apply_refactor(
        &self,
        entry: &ServerEntry,
        raw: &Value,
        snapshot: &RefactorSnapshot,
        owner: &AgentCx,
    ) -> Result<ApplyOutcome> {
        check_response_size(raw)?;
        let plan = parse_workspace_edit(raw)?;
        let hashes = snapshot.validate(entry, raw, &plan)?;
        owner
            .checkpoint()
            .map_err(|_| tool_err("LSP_CANCELLED", "refactoring cancelled before applying"))?;
        if !entry.client.is_alive() {
            return Err(tool_err(
                "LSP_TRANSPORT_CLOSED",
                "refactoring connection closed before applying",
            ));
        }
        let result = apply_scoped(entry, raw, Some(&hashes));
        // Also discard stale handles after an incomplete rollback. Do not let
        // cached diagnostics or selected actions claim the old files still exist.
        self.invalidate_refactor(entry);
        result
    }

    pub(in crate::lsp) async fn run_rename(&self, input: &LspInput) -> Result<ToolOutput> {
        let new_name = input
            .new_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| tool_err("LSP_USAGE", "lsp rename requires a nonempty newName"))?;
        let file = input
            .file
            .as_deref()
            .ok_or_else(|| tool_err("LSP_USAGE", "lsp rename requires file"))?;
        let symbol = input
            .symbol
            .as_deref()
            .ok_or_else(|| tool_err("LSP_USAGE", "lsp rename requires symbol"))?;
        let owner = AgentCx::for_current_or_request();
        let path = resolve_tool_path(file, &self.cwd).canonicalize()?;
        if !path.metadata()?.is_file() {
            return Err(tool_err(
                "LSP_FILE_UNREADABLE",
                "rename source is not a regular file",
            ));
        }
        let hash = file_hash(&path)?;
        let position = Self::resolve_position(&path, input.line, symbol)?;
        let (uri, entry) = self.synced(&path).await?;
        let snapshot = RefactorSnapshot::capture(&entry, &path, hash)?;
        let raw = entry
            .client
            .call(
                "textDocument/rename",
                json!({"textDocument":{"uri":uri},"position":position,"newName":new_name}),
                self.request_timeout(input),
            )
            .await?;
        if input.apply == Some(false) {
            crate::lsp::refactor_preview::check_owner(&owner)?;
            let prepared = Self::prepare_refactor(&entry, &raw, &snapshot)?;
            return self.cache_refactor(
                &entry,
                prepared,
                raw,
                json!({"action":"rename","newName":new_name}),
                None,
                &owner,
            );
        }
        let outcome = self.apply_refactor(&entry, &raw, &snapshot, &owner)?;
        let files: Vec<_> = outcome
            .files_changed
            .iter()
            .map(|path| display_path(path, &self.cwd))
            .collect();
        let payload = json!({
            "action":"rename","newName":new_name,"filesChanged":files,
            "fileOps":outcome.file_ops_applied,"atomic":false,"rollbackOnError":true,
            "note":"Scoped regular-file transaction with rollback; not cross-file atomic visibility."
        });
        Ok(super::text_output(payload.to_string(), payload))
    }

    pub(in crate::lsp) async fn run_rename_file(&self, input: &LspInput) -> Result<ToolOutput> {
        let file = input
            .file
            .as_deref()
            .ok_or_else(|| tool_err("LSP_USAGE", "rename_file requires file"))?;
        let new_file = input
            .new_file
            .as_deref()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| tool_err("LSP_USAGE", "rename_file requires a nonempty newFile"))?;
        let owner = AgentCx::for_current_or_request();
        let requested = resolve_tool_path(file, &self.cwd);
        let metadata = std::fs::symlink_metadata(&requested)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(tool_err(
                "LSP_FILE_UNREADABLE",
                "rename_file requires a regular file, not a directory or symlink",
            ));
        }
        let old_path = requested.canonicalize()?;
        let hash = file_hash(&old_path)?;
        let (old_uri, entry) = self.synced(&old_path).await?;
        let snapshot = RefactorSnapshot::capture(&entry, &old_path, hash)?;
        // Use a canonical base without creating parents. Scope validation
        // rejects traversal and symlink components, including dangling targets.
        let new_path = resolve_tool_path(new_file, &self.cwd.canonicalize()?);
        inside_root(&new_path, entry.client.root())?;
        if new_path.try_exists()? {
            return Err(tool_err(
                "LSP_EDIT_CONFLICT",
                "rename destination already exists",
            ));
        }
        let new_uri = try_path_to_uri(&new_path)?;
        let capabilities = entry.client.capabilities().raw;
        // These are capability keys, not protocol method names with 'Files'.
        let will = registered_for_file(&capabilities, "willRename", &old_path)?;
        let did = registered_for_file(&capabilities, "didRename", &old_path)?;
        let params = json!({"files":[{"oldUri":old_uri,"newUri":new_uri}]});
        let edit = if will {
            entry
                .client
                .call(
                    "workspace/willRenameFiles",
                    params.clone(),
                    self.request_timeout(input),
                )
                .await?
        } else {
            Value::Null
        };
        let combined = append_move(edit, &old_uri, &new_uri)?;
        if input.apply == Some(false) {
            crate::lsp::refactor_preview::check_owner(&owner)?;
            let prepared = Self::prepare_refactor(&entry, &combined, &snapshot)?;
            return self.cache_refactor(
                &entry,
                prepared,
                combined,
                json!({"action":"rename_file","from":display_path(&old_path,&self.cwd),
                    "to":display_path(&new_path,&self.cwd),"willRenameFiles":will}),
                did.then_some(params),
                &owner,
            );
        }
        // One transaction computes import updates AND the move before any
        // target changes. A late destination conflict cannot strand imports.
        let outcome = self.apply_refactor(&entry, &combined, &snapshot, &owner)?;
        let warning = Self::notify_refactor_move(&entry, did.then_some(params));
        let updates: Vec<_> = outcome
            .files_changed
            .iter()
            .map(|path| display_path(path, &self.cwd))
            .collect();
        let payload = json!({
            "action":"rename_file","from":display_path(&old_path,&self.cwd),"to":display_path(&new_path,&self.cwd),
            "applied":true,"importUpdates":updates,"fileOps":outcome.file_ops_applied,
            "willRenameFiles":will,"notificationRequested":did,"notificationWritten":did && warning.is_none(),
            "warning":warning,"atomic":false,"rollbackOnError":true
        });
        Ok(super::text_output(payload.to_string(), payload))
    }
}

#[cfg(test)]
mod tests;
