//! Coordinated rename_file through the existing LSP/refactor transaction.
//!
//! Coordinate all selected language servers in one workspace. Retain each
//! request's evidence, reconcile independent imports, and move the tree once.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use super::{
    AgentCx, HashMap, LspInput, LspTool, Path, PathBuf, PreparedEdit, RefactorSnapshot, Result,
    ServerEntry, ToolOutput, Value, append_move, check_response_size, display_path, file_hash,
    file_operation_filters, inside_root, json, parse_workspace_edit, resolve_tool_path, tool_err,
    try_path_to_uri,
};
use crate::lsp::edits::{FileEvidence, WorkspaceEditPlan};
use crate::lsp::refactor_preview::{MAX_REFACTOR_SERVERS, RefactorGroup, check_owner};
use crate::lsp::registry::ServerSpec;

mod merge;

fn validate_input(input: &LspInput) -> Result<()> {
    if input.position.is_some()
        || input.line.is_some()
        || input.symbol.is_some()
        || input.query.is_some()
        || input.range.is_some()
        || input.limit.is_some()
        || input.resolve.is_some()
        || input.action_id.is_some()
        || input.refactor_id.is_some()
        || input.completion_id.is_some()
        || input.snippet_values.is_some()
        || input.hierarchy_id.is_some()
        || input.only.is_some()
        || input.after.is_some()
        || input.new_name.is_some()
        || input.format_options.is_some()
        || input.method.is_some()
        || input.payload.is_some()
    {
        return Err(tool_err(
            "LSP_USAGE",
            "coordinated rename_file accepts only file, newFile, apply and timeout",
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
    directory: bool,
) -> Result<Option<Value>> {
    let filters = file_operation_filters(capabilities, operation)?;
    let mut files = Vec::new();
    if filters
        .iter()
        .any(|filter| filter.matches(source, directory))
    {
        files.push(json!({
            "oldUri":try_path_to_uri(source)?,
            "newUri":try_path_to_uri(destination)?
        }));
    } else if directory {
        for path in before.paths().filter(|path| path.starts_with(source)) {
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

struct Contributor {
    entry: Arc<ServerEntry>,
    snapshot: RefactorSnapshot,
    will: Option<Value>,
    did: Option<Value>,
}

struct RenameAnchor {
    path: PathBuf,
    spec: ServerSpec,
}

fn pair_count(params: Option<&Value>) -> usize {
    params.map_or(0, |params| params["files"].as_array().map_or(0, Vec::len))
}

/// Validate each response's wire versions against THAT connection, while the
/// complete affected path set guards every contributor's open documents.
fn evidence(
    contributors: &[Contributor],
    responses: &[Value],
    plan: &WorkspaceEditPlan,
) -> Result<HashMap<PathBuf, FileEvidence>> {
    if contributors.len() != responses.len() {
        return Err(tool_err(
            "LSP_RENAME_CONFLICT",
            "incomplete server response set",
        ));
    }
    let mut expected = HashMap::new();
    for (contributor, raw) in contributors.iter().zip(responses) {
        if !contributor.entry.client.is_alive() {
            return Err(tool_err(
                "LSP_TRANSPORT_CLOSED",
                "a rename contributor disconnected",
            ));
        }
        for (path, hash) in contributor
            .snapshot
            .validate(&contributor.entry, raw, plan)?
        {
            let state = FileEvidence::Text(hash);
            if expected
                .insert(path, state)
                .is_some_and(|previous| previous != state)
            {
                return Err(tool_err(
                    "LSP_RENAME_CONFLICT",
                    "language servers disagree about original document contents",
                ));
            }
        }
    }
    Ok(expected)
}

impl LspTool {
    fn rename_anchors(
        &self,
        source: &Path,
        before: &PreparedEdit,
    ) -> Result<(PathBuf, Vec<RenameAnchor>)> {
        let mut workspace: Option<PathBuf> = None;
        let mut selected = BTreeMap::new();
        // A regular-file transaction also retains the absent destination.
        // Only original source paths may select participants or workspace roots.
        for path in before.paths().filter(|path| path.starts_with(source)) {
            for spec in self.registry.specs_for_file(path) {
                let root = self.registry.workspace_root(path, spec).canonicalize()?;
                inside_root(source, &root)?;
                if workspace.as_ref().is_some_and(|previous| previous != &root) {
                    return Err(tool_err(
                        "LSP_RENAME_MULTIPLE_WORKSPACES",
                        "rename participants resolve to different language-server roots",
                    ));
                }
                workspace = Some(root);
                selected
                    .entry(spec.name.clone())
                    .or_insert_with(|| RenameAnchor {
                        path: path.clone(),
                        spec: spec.clone(),
                    });
                if selected.len() > MAX_REFACTOR_SERVERS {
                    return Err(tool_err(
                        "LSP_EDIT_LIMIT",
                        "rename exceeds 8 language servers",
                    ));
                }
            }
        }
        let root = workspace.ok_or_else(|| {
            tool_err(
                "LSP_NO_SERVER",
                "rename needs at least one file handled by a configured language server",
            )
        })?;
        // Stable server-name order; file traversal and HashMap order never
        // decide which response or annotation receives the first identity.
        Ok((root, selected.into_values().collect()))
    }

    #[allow(clippy::too_many_lines)] // one admission, merge and commit sequence across servers
    pub(super) async fn run_rename_group(
        &self,
        input: &LspInput,
        requested: &Path,
        new_file: &str,
        directory: bool,
    ) -> Result<ToolOutput> {
        validate_input(input)?;
        let owner = AgentCx::for_current_or_request();
        let started = Instant::now();
        let timeout = self.request_timeout(input);
        let remaining = || {
            check_owner(&owner)?;
            timeout
                .checked_sub(started.elapsed())
                .filter(|left| !left.is_zero())
                .ok_or_else(|| tool_err("LSP_TIMEOUT", "rename request budget expired"))
        };
        remaining()?;
        let cwd = self.cwd.canonicalize()?;
        inside_root(requested, &cwd)?;
        let source = requested.canonicalize()?;
        let destination = resolve_tool_path(new_file, &cwd);
        inside_root(&destination, &cwd)?;
        if source == cwd {
            return Err(tool_err(
                "LSP_EDIT_SCOPE",
                "cannot rename the tool working directory",
            ));
        }
        let old_uri = try_path_to_uri(&source)?;
        let new_uri = try_path_to_uri(&destination)?;
        let move_only = append_move(Value::Null, &old_uri, &new_uri)?;
        // Snapshot before starting ANY server, including unknown/binary files.
        let before = PreparedEdit::new(&parse_workspace_edit(&move_only)?, &HashMap::new())?;
        remaining()?;
        let (root, anchors) = self.rename_anchors(&source, &before)?;
        inside_root(&destination, &root)?;
        let mut contributors = Vec::with_capacity(anchors.len());
        // Synchronize all participants before asking any of them for edits.
        // A failed startup never turns into a partial-success filesystem move.
        for RenameAnchor { path: anchor, spec } in anchors {
            remaining()?;
            let anchor_hash = file_hash(&anchor)?;
            let now = owner
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            let (_, entry) = asupersync::time::timeout(
                now,
                remaining()?,
                self.synced_for_spec_with_timeout(&anchor, &spec, remaining()?),
            )
            .await
            .map_err(|_| tool_err("LSP_TIMEOUT", "rename server startup timed out"))??;
            entry
                .client
                .ensure_open_documents_synced(remaining()?)
                .await?;
            remaining()?;
            before.verify()?;
            if entry.client.root() != root.as_path() {
                return Err(tool_err(
                    "LSP_EDIT_SCOPE",
                    "language-server workspace changed during startup",
                ));
            }
            let snapshot = RefactorSnapshot::capture(&entry, &anchor, anchor_hash)?;
            let capabilities = entry.client.capabilities().raw;
            let will = notification_params(
                &capabilities,
                "willRename",
                &source,
                &destination,
                &before,
                directory,
            )?;
            let did = notification_params(
                &capabilities,
                "didRename",
                &source,
                &destination,
                &before,
                directory,
            )?;
            contributors.push(Contributor {
                entry,
                snapshot,
                will,
                did,
            });
        }
        let mut responses = Vec::with_capacity(contributors.len());
        for contributor in &contributors {
            remaining()?;
            contributor
                .snapshot
                .verify_request_source(&contributor.entry)?;
            let raw = if let Some(params) = &contributor.will {
                contributor
                    .entry
                    .client
                    .call("workspace/willRenameFiles", params.clone(), remaining()?)
                    .await?
            } else {
                Value::Null
            };
            responses.push(raw);
            // Enforce the aggregate budget as results arrive, not after eight
            // full-size responses and their cloned merge payloads accumulate.
            merge::admit(&responses)?;
            remaining()?;
            before.verify()?;
        }
        let merged = if responses.len() == 1 {
            // Single-server ordered edits keep their original coordinate and
            // annotation semantics; reconciliation is only for independent sets.
            merge::Merged {
                edit: responses[0].clone(),
                duplicates: 0,
            }
        } else {
            // Validate wire versions before normalization removes their
            // connection-local numbers from the merged WorkspaceEdit.
            for (contributor, raw) in contributors.iter().zip(&responses) {
                contributor.snapshot.validate(
                    &contributor.entry,
                    raw,
                    &parse_workspace_edit(raw)?,
                )?;
            }
            merge::combine(&responses, &root)?
        };
        let combined = append_move(merged.edit, &old_uri, &new_uri)?;
        let plan = parse_workspace_edit(&combined)?;
        for path in plan.text_edits.keys() {
            inside_root(path, &root)?;
        }
        // Additional resource operations are rejected by the shared directory
        // transaction; never let server-selected paths bypass scope admission.
        for operation in &plan.file_ops {
            match operation {
                super::FileOp::Create { path, .. } | super::FileOp::Delete { path } => {
                    inside_root(path, &root)?;
                }
                super::FileOp::Rename {
                    old_path, new_path, ..
                } => {
                    inside_root(old_path, &root)?;
                    inside_root(new_path, &root)?;
                }
            }
        }
        let expected = evidence(&contributors, &responses, &plan)?;
        let prepared = PreparedEdit::new(&plan, &expected)?;
        before.verify()?;
        prepared.verify()?;
        remaining()?;
        let group = RefactorGroup::capture(
            contributors
                .iter()
                .map(|source| (Arc::clone(&source.entry), source.did.clone()))
                .collect(),
            &prepared,
        )?;
        // Do not bless a resynchronized incarnation while assembling the group.
        evidence(&contributors, &responses, &plan)?;
        let servers: Vec<_> = contributors.iter().map(|contributor| json!({
            "server":contributor.entry.spec_name,
            "willRenamePairs":pair_count(contributor.will.as_ref()),"didRenamePairs":pair_count(contributor.did.as_ref())
        })).collect();
        let will_count: usize = contributors
            .iter()
            .map(|source| pair_count(source.will.as_ref()))
            .sum();
        let did_count: usize = contributors
            .iter()
            .map(|source| pair_count(source.did.as_ref()))
            .sum();
        let mut metadata = json!({
            "action":"rename_file","directory":directory,
            "from":display_path(&source, &cwd),"to":display_path(&destination, &cwd),
            "server":if contributors.len() == 1 { Some(contributors[0].entry.spec_name.as_str()) } else { None },
            "servers":servers,"serverCount":contributors.len(),
            "filesMoved":before.paths().filter(|path| path.starts_with(&source)).count(),
            "deduplicatedEdits":merged.duplicates,
            "willRenameFiles":will_count > 0,"willRenamePairs":will_count,"didRenamePairs":did_count,
            "atomic":false,"rollbackOnError":true,
            "note":"One workspace with all selected language servers. Import coverage depends on each registration and returned edits; no conflicting response is silently preferred."
        });
        remaining()?;
        if input.apply == Some(false) {
            return self.cache_refactor_group(group, prepared, combined, metadata, &owner);
        }
        let entries = group.verify()?;
        let result = prepared.commit(|| {
            remaining()?;
            evidence(&contributors, &responses, &plan)?;
            before.verify()?;
            group.verify()?;
            remaining()?;
            Ok(())
        });
        for entry in &entries {
            self.invalidate_refactor(entry);
        }
        let outcome = result?;
        group.notify(&entries, &mut metadata);
        metadata["applied"] = json!(true);
        metadata["preview"] = json!(false);
        metadata["importUpdates"] = json!(
            outcome
                .files_changed
                .iter()
                .map(|path| display_path(path, &cwd))
                .collect::<Vec<_>>()
        );
        metadata["fileOps"] = json!(outcome.file_ops_applied);
        Ok(super::super::text_output(metadata.to_string(), metadata))
    }
}

#[cfg(test)]
mod tests;
