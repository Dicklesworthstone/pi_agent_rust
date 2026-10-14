//! Directory-aware rename_file through the existing LSP/refactor transaction.
//!
//! Select one unambiguous server workspace, retain the entire source tree, ask
//! for import edits, then preview or commit the exact edit-plus-move plan.

use std::time::Instant;

use super::{
    AgentCx, HashMap, LspInput, LspTool, Path, PathBuf, PreparedEdit, RefactorSnapshot,
    Result, ToolOutput, Value, append_move, check_response_size, display_path,
    file_hash, file_operation_filters, inside_root, json, parse_workspace_edit,
    resolve_tool_path, tool_err, try_path_to_uri,
};
use crate::lsp::refactor_preview::check_owner;

fn validate_input(input: &LspInput) -> Result<()> {
    if input.position.is_some() || input.line.is_some() || input.symbol.is_some()
        || input.query.is_some() || input.range.is_some() || input.limit.is_some()
        || input.resolve.is_some() || input.action_id.is_some() || input.refactor_id.is_some()
        || input.completion_id.is_some() || input.snippet_values.is_some()
        || input.hierarchy_id.is_some() || input.only.is_some() || input.after.is_some()
        || input.new_name.is_some() || input.format_options.is_some()
        || input.method.is_some() || input.payload.is_some()
    {
        return Err(tool_err(
            "LSP_USAGE",
            "directory rename_file accepts only file, newFile, apply and timeout",
        ));
    }
    Ok(())
}

/// Prefer the root folder event when registered. Otherwise enumerate only
/// matching regular-file events, as needed by extension-scoped servers. Do not
/// send overlapping root and descendant events in the same request.
fn notification_params(
    capabilities: &Value,
    operation: &str,
    source: &Path,
    destination: &Path,
    before: &PreparedEdit,
) -> Result<Option<Value>> {
    let filters = file_operation_filters(capabilities, operation)?;
    let mut files = Vec::new();
    if filters.iter().any(|filter| filter.matches(source, true)) {
        files.push(json!({
            "oldUri":try_path_to_uri(source)?,
            "newUri":try_path_to_uri(destination)?
        }));
    } else {
        for path in before.paths() {
            if filters.iter().any(|filter| filter.matches(path, false)) {
                let relative = path.strip_prefix(source).map_err(|_| {
                    tool_err("LSP_EDIT_SCOPE", "directory snapshot escaped its source")
                })?;
                files.push(json!({
                    "oldUri":try_path_to_uri(path)?,
                    "newUri":try_path_to_uri(&destination.join(relative))?
                }));
            }
        }
    }
    if files.is_empty() {
        return Ok(None);
    }
    let params = json!({"files":files});
    check_response_size(&params)?;
    Ok(Some(params))
}

impl LspTool {
    fn directory_anchor(&self, source: &Path, before: &PreparedEdit) -> Result<PathBuf> {
        let mut selected: Option<(String, PathBuf, PathBuf)> = None;
        for path in before.paths() {
            let Some(spec) = self.registry.spec_for_file(path) else { continue };
            let root = self.registry.workspace_root(path, spec).canonicalize()?;
            inside_root(source, &root)?;
            if source == root.as_path() {
                return Err(tool_err(
                    "LSP_EDIT_SCOPE",
                    "cannot rename the language-server workspace root",
                ));
            }
            if let Some((name, previous_root, _)) = &selected {
                if name != &spec.name || previous_root != &root {
                    return Err(tool_err(
                        "LSP_RENAME_MULTIPLE_SERVERS",
                        "directory spans multiple language-server workspaces; rename a smaller subtree",
                    ));
                }
            } else {
                selected = Some((spec.name.clone(), root, path.clone()));
            }
        }
        selected.map(|(_, _, anchor)| anchor).ok_or_else(|| {
            tool_err(
                "LSP_NO_SERVER",
                "directory needs at least one file handled by a configured language server",
            )
        })
    }

    pub(super) async fn run_rename_directory(
        &self,
        input: &LspInput,
        requested: &Path,
        new_file: &str,
    ) -> Result<ToolOutput> {
        validate_input(input)?;
        let owner = AgentCx::for_current_or_request();
        let started = Instant::now();
        let timeout = self.request_timeout(input);
        let remaining = || {
            check_owner(&owner)?;
            timeout.checked_sub(started.elapsed()).filter(|left| !left.is_zero())
                .ok_or_else(|| tool_err("LSP_TIMEOUT", "directory rename request budget expired"))
        };
        remaining()?;
        let cwd = self.cwd.canonicalize()?;
        inside_root(requested, &cwd)?;
        let source = requested.canonicalize()?;
        let destination = resolve_tool_path(new_file, &cwd);
        inside_root(&destination, &cwd)?;
        if source == cwd {
            return Err(tool_err("LSP_EDIT_SCOPE", "cannot rename the tool working directory"));
        }
        let old_uri = try_path_to_uri(&source)?;
        let new_uri = try_path_to_uri(&destination)?;
        let move_only = append_move(Value::Null, &old_uri, &new_uri)?;
        // Stage before server startup. This reads only the admitted workspace
        // tree; it creates nothing and catches unsupported resources early.
        let before = PreparedEdit::new(&parse_workspace_edit(&move_only)?, &HashMap::new())?;
        remaining()?;
        let anchor = self.directory_anchor(&source, &before)?;
        let anchor_hash = file_hash(&anchor)?;
        let now = owner.cx().timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let (_, entry) = asupersync::time::timeout(
            now, remaining()?, self.synced_with_timeout(&anchor, remaining()?),
        ).await.map_err(|_| tool_err("LSP_TIMEOUT", "directory rename server startup timed out"))??;
        remaining()?;
        before.verify()?;
        inside_root(&source, entry.client.root())?;
        inside_root(&destination, entry.client.root())?;
        let snapshot = RefactorSnapshot::capture(&entry, &anchor, anchor_hash)?;
        let capabilities = entry.client.capabilities().raw;
        let will = notification_params(&capabilities, "willRename", &source, &destination, &before)?;
        let did = notification_params(&capabilities, "didRename", &source, &destination, &before)?;
        let will_count = will.as_ref().map_or(0, |params| params["files"].as_array().map_or(0, Vec::len));
        let did_count = did.as_ref().map_or(0, |params| params["files"].as_array().map_or(0, Vec::len));
        let raw = if let Some(params) = will {
            entry.client.call("workspace/willRenameFiles", params, remaining()?).await?
        } else {
            Value::Null
        };
        remaining()?;
        before.verify()?;
        let combined = append_move(raw, &old_uri, &new_uri)?;
        let prepared = self.prepare_refactor(&entry, &combined, &snapshot)?;
        // Both immutable views must still agree with the live original tree.
        // Neither a server callback nor re-staging may bless request-time drift.
        before.verify()?;
        prepared.verify()?;
        remaining()?;
        let mut metadata = json!({
            "action":"rename_file","directory":true,
            "from":display_path(&source, &cwd),"to":display_path(&destination, &cwd),
            "server":entry.spec_name,"filesMoved":before.paths().count(),
            "willRenameFiles":will_count > 0,"willRenamePairs":will_count,"didRenamePairs":did_count,
            "atomic":false,"rollbackOnError":true,
            "note":"One language-server workspace. Import coverage depends on server registration and returned edits; unrecognized files are moved unchanged."
        });
        if input.apply == Some(false) {
            return self.cache_refactor(&entry, prepared, combined, metadata, did, &owner);
        }
        let plan = parse_workspace_edit(&combined)?;
        let result = prepared.commit(|| {
            remaining()?;
            if !entry.client.is_alive() {
                return Err(tool_err("LSP_TRANSPORT_CLOSED", "directory rename connection closed"));
            }
            snapshot.validate(&entry, &combined, &plan)?;
            before.verify()?;
            remaining()?;
            Ok(())
        });
        self.invalidate_refactor(&entry);
        let outcome = result?;
        let notification_requested = did.is_some();
        let warning = Self::notify_refactor_move(&entry, did);
        metadata["applied"] = json!(true);
        metadata["preview"] = json!(false);
        metadata["importUpdates"] = json!(outcome.files_changed.iter()
            .map(|path| display_path(path, &cwd)).collect::<Vec<_>>());
        metadata["fileOps"] = json!(outcome.file_ops_applied);
        metadata["notificationRequested"] = json!(notification_requested);
        metadata["notificationWritten"] = json!(notification_requested && warning.is_none());
        metadata["warning"] = json!(warning);
        Ok(super::super::text_output(metadata.to_string(), metadata))
    }
}

#[cfg(test)]
mod tests;
