//! Selected code-action lifecycle and scoped server-initiated edit permission.
//!
//! A server request is not itself permission to modify files. Only the window
//! around an explicitly selected executeCommand accepts workspace/applyEdit.

use super::edits::{ApplyOutcome, FileOp, apply_workspace_edit, parse_workspace_edit};
use super::registry::ServerEntry;
use super::{
    LspInput, LspTool, display_path, resolve_tool_path, select_code_action, text_output, tool_err,
};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

mod command_edits;
mod refactor;

const MAX_ACTIONS: usize = 128;
const MAX_ACTION_BYTES: usize = 2 * 1024 * 1024;
const MAX_APPLY_REQUESTS: usize = 32;

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct CachedAction {
    id: String,
    entry: Weak<ServerEntry>,
    path: PathBuf,
    snapshot: Arc<refactor::RefactorSnapshot>,
    action: Value,
    bytes: usize,
}

#[derive(Default)]
struct Report {
    files: Vec<PathBuf>,
    operations: Vec<String>,
    failures: Vec<String>,
    requests: usize,
    rollback_incomplete: bool,
}

impl Report {
    fn record(&mut self, outcome: ApplyOutcome) {
        for path in outcome.files_changed {
            if !self.files.contains(&path) {
                self.files.push(path);
            }
        }
        self.operations.extend(outcome.file_ops_applied);
    }

    const fn changed(&self) -> bool {
        !self.files.is_empty() || !self.operations.is_empty() || self.rollback_incomplete
    }
}

struct Grant {
    id: u64,
    entry: Weak<ServerEntry>,
    report: Report,
    edits: command_edits::CommandEdits,
    accepting: bool,
    revoked: bool,
}

#[derive(Default)]
pub(super) struct ActionState {
    cache: Mutex<VecDeque<CachedAction>>,
    active: Mutex<Option<Grant>>,
    next_grant: AtomicU64,
    admission: AtomicU64,
}

struct CommandLease {
    state: Arc<ActionState>,
    id: u64,
}

impl CommandLease {
    fn close_admission(&self) {
        let _ =
            self.state
                .admission
                .compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Acquire);
    }

    fn apply_inline(&self, entry: &Arc<ServerEntry>, edit: &Value) -> Result<()> {
        let mut active = lock(&self.state.active);
        let grant = active
            .as_mut()
            .filter(|grant| grant.id == self.id)
            .ok_or_else(|| tool_err("LSP_EDIT_REVOKED", "selected action lease ended"))?;
        let outcome = grant.edits.apply(entry, edit, || Ok(()))?;
        grant.report.record(outcome);
        drop(active);
        Ok(())
    }

    fn activate(&self, entry: &Arc<ServerEntry>, timeout: std::time::Duration) -> Result<()> {
        let mut active = lock(&self.state.active);
        let grant = active
            .as_mut()
            .filter(|grant| grant.id == self.id)
            .ok_or_else(|| tool_err("LSP_EDIT_REVOKED", "selected action lease ended"))?;
        grant.edits.activate(entry, timeout)?;
        grant.accepting = true;
        drop(active);
        self.state.admission.store(self.id, Ordering::Release);
        Ok(())
    }

    fn finish(self) -> Report {
        self.close_admission();
        let mut active = lock(&self.state.active);
        let report = if active.as_ref().is_some_and(|grant| grant.id == self.id) {
            active
                .take()
                .map_or_else(Report::default, |grant| grant.report)
        } else {
            Report::default()
        };
        drop(active);
        report
    }
}

impl Drop for CommandLease {
    fn drop(&mut self) {
        // Close admission before waiting for a callback's staging lock. That
        // callback rechecks this generation immediately before committing.
        self.close_admission();
        let mut active = lock(&self.state.active);
        if active.as_ref().is_some_and(|grant| grant.id == self.id) {
            active.take();
        }
    }
}

impl ActionState {
    fn grant(
        self: &Arc<Self>,
        entry: &Arc<ServerEntry>,
        edits: command_edits::CommandEdits,
    ) -> Result<CommandLease> {
        let mut active = lock(&self.active);
        if active.is_some() {
            return Err(tool_err(
                "LSP_EDIT_BUSY",
                "another code action owns the edit window",
            ));
        }
        // The active lock serializes allocation. A distinct generation is
        // published in `admission` only after inline edits and activation.
        let id = self
            .next_grant
            .load(Ordering::Relaxed)
            .checked_add(1)
            .ok_or_else(|| tool_err("LSP_EDIT_LIMIT", "selected command generation exhausted"))?;
        self.next_grant.store(id, Ordering::Relaxed);
        *active = Some(Grant {
            id,
            entry: Arc::downgrade(entry),
            report: Report::default(),
            edits,
            accepting: false,
            revoked: false,
        });
        drop(active);
        Ok(CommandLease {
            state: Arc::clone(self),
            id,
        })
    }

    fn apply_from_server(&self, entry: &Arc<ServerEntry>, params: &Value) -> Value {
        // Do not queue a request received before command activation behind an
        // inline apply and then accidentally authorize it after activation.
        let admission = self.admission.load(Ordering::Acquire);
        if admission == 0 {
            return json!({"applied":false,"failureReason":"no selected code action authorizes server edits"});
        }
        // Keep close and apply linearized. A command cannot finish/drop its
        // permission while a callback that already acquired it is committing.
        let mut active = lock(&self.active);
        let Some(grant) = active.as_mut().filter(|grant| {
            grant.id == admission
                && grant.accepting
                && self.admission.load(Ordering::Acquire) == admission
                && Weak::ptr_eq(&grant.entry, &Arc::downgrade(entry))
        }) else {
            return json!({"applied":false,"failureReason":"no selected code action authorizes server edits"});
        };
        grant.report.requests = grant.report.requests.saturating_add(1);
        let outcome = if grant.revoked {
            Err(tool_err(
                "LSP_EDIT_REVOKED",
                "an earlier rejection revoked this command's edit permission",
            ))
        } else if grant.report.requests > MAX_APPLY_REQUESTS {
            Err(tool_err(
                "LSP_EDIT_LIMIT",
                "code action exceeded its server-edit request budget",
            ))
        } else {
            params
                .get("edit")
                .ok_or_else(|| tool_err("LSP_EDIT_MALFORMED", "missing workspace edit"))
                .and_then(|edit| {
                    grant.edits.apply(entry, edit, || {
                        if self.admission.load(Ordering::Acquire) == admission {
                            Ok(())
                        } else {
                            Err(tool_err(
                                "LSP_CANCELLED",
                                "selected command ended before committing server edits",
                            ))
                        }
                    })
                })
        };
        match outcome {
            Ok(outcome) => {
                grant.report.record(outcome);
                drop(active);
                lock(&self.cache).clear();
                json!({"applied":true})
            }
            Err(error) => {
                // Once rejected, never accept a "corrected" callback under
                // stale evidence. The caller must explicitly select again.
                if !grant.revoked {
                    entry.client.invalidate_all();
                    lock(&self.cache).clear();
                }
                grant.revoked = true;
                let reason = error.to_string();
                grant.report.rollback_incomplete |= reason.contains("[LSP_EDIT_ROLLBACK]");
                if grant.report.failures.len() < MAX_APPLY_REQUESTS {
                    grant.report.failures.push(reason.clone());
                }
                json!({"applied":false,"failureReason":reason})
            }
        }
    }

    fn remember(
        &self,
        entry: &Arc<ServerEntry>,
        path: &Path,
        snapshot: Arc<refactor::RefactorSnapshot>,
        action: Value,
    ) -> Result<String> {
        let bytes = serde_json::to_vec(&action)?.len();
        if bytes > MAX_ACTION_BYTES {
            return Err(tool_err(
                "LSP_ACTION_LIMIT",
                "one code action exceeds 2 MiB",
            ));
        }
        let mut cache = lock(&self.cache);
        let mut retained: usize = cache.iter().map(|item| item.bytes).sum();
        while cache.len() >= MAX_ACTIONS || retained + bytes > MAX_ACTION_BYTES {
            if let Some(old) = cache.pop_front() {
                retained -= old.bytes;
            } else {
                break;
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        cache.push_back(CachedAction {
            id: id.clone(),
            entry: Arc::downgrade(entry),
            path: path.to_path_buf(),
            snapshot,
            action,
            bytes,
        });
        drop(cache);
        Ok(id)
    }

    fn take(&self, id: &str) -> Result<CachedAction> {
        let mut cache = lock(&self.cache);
        let index = cache.iter().position(|item| item.id == id).ok_or_else(|| {
            tool_err(
                "LSP_ACTION_EXPIRED",
                "unknown or expired actionId; list code actions again",
            )
        })?;
        cache
            .remove(index)
            .ok_or_else(|| tool_err("LSP_ACTION_EXPIRED", "code action expired"))
    }
}

pub(super) fn install_apply_edit_handler(entry: &Arc<ServerEntry>, state: Arc<ActionState>) {
    // Install before publishing the marker. Tool operations are serialized, so
    // another operation cannot observe an entry with a not-yet-installed hook.
    if entry.handler_installed.load(Ordering::Acquire) {
        return;
    }
    let weak = Arc::downgrade(entry);
    entry
        .client
        .set_server_request_handler(Arc::new(move |method, params| {
            if method != "workspace/applyEdit" {
                return None;
            }
            Some(weak.upgrade().map_or_else(
                || json!({"applied":false,"failureReason":"language server session ended"}),
                |entry| state.apply_from_server(&entry, params),
            ))
        }));
    entry.handler_installed.store(true, Ordering::Release);
}

fn file_hash(path: &Path) -> Result<u64> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(tool_err(
            "LSP_FILE_UNREADABLE",
            "code action source is not a regular file",
        ));
    }
    let mut content = String::new();
    file.take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut content)?;
    if content.len() > 16 * 1024 * 1024 {
        return Err(tool_err(
            "LSP_ACTION_LIMIT",
            "code action source exceeds 16 MiB",
        ));
    }
    Ok(super::text::content_hash_for_drift(&content))
}

fn verify_source(path: &Path, expected: u64) -> Result<()> {
    if file_hash(path)? != expected {
        return Err(tool_err(
            "LSP_EDIT_CONFLICT",
            "source changed since code actions were requested; list again",
        ));
    }
    Ok(())
}

fn inside_root(path: &Path, root: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return Err(tool_err(
            "LSP_EDIT_SCOPE",
            "server edits require absolute local file paths",
        ));
    }
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(tool_err(
                    "LSP_EDIT_SCOPE",
                    "symlink components are not allowed in server edit paths",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    // The final destination may not exist. Resolve the existing prefix without
    // dropping unresolved '..' components. This is a scope check, not an OS
    // sandbox against concurrent directory replacement by another process.
    let mut prefix = path;
    let mut suffix = Vec::new();
    while !prefix.try_exists()? {
        suffix.push(
            prefix
                .file_name()
                .ok_or_else(|| tool_err("LSP_EDIT_SCOPE", "invalid edit path"))?,
        );
        prefix = prefix
            .parent()
            .ok_or_else(|| tool_err("LSP_EDIT_SCOPE", "edit path has no existing parent"))?;
    }
    let mut resolved = prefix.canonicalize()?;
    for part in suffix.iter().rev() {
        resolved.push(part);
    }
    if !resolved.starts_with(root) || resolved == root {
        return Err(tool_err(
            "LSP_EDIT_SCOPE",
            "server edit escapes the language server workspace",
        ));
    }
    Ok(())
}

fn apply_scoped(
    entry: &ServerEntry,
    edit: &Value,
    hashes: Option<&HashMap<PathBuf, u64>>,
) -> Result<ApplyOutcome> {
    if serde_json::to_vec(edit)?.len() > MAX_ACTION_BYTES {
        return Err(tool_err("LSP_EDIT_LIMIT", "workspace edit exceeds 2 MiB"));
    }
    let plan = parse_workspace_edit(edit)?;
    let root = entry.client.root().canonicalize()?;
    for path in plan.text_edits.keys() {
        inside_root(path, &root)?;
    }
    for operation in &plan.file_ops {
        match operation {
            FileOp::Create { path, .. } | FileOp::Delete { path } => inside_root(path, &root)?,
            FileOp::Rename {
                old_path, new_path, ..
            } => {
                inside_root(old_path, &root)?;
                inside_root(new_path, &root)?;
            }
        }
    }
    apply_workspace_edit(&plan, hashes)
}

fn enabled(action: &Value) -> Result<()> {
    if !action.is_object() || action["title"].as_str().is_none_or(str::is_empty) {
        return Err(tool_err(
            "LSP_ACTION_MALFORMED",
            "code action needs a nonempty title",
        ));
    }
    if let Some(disabled) = action.get("disabled").filter(|value| !value.is_null()) {
        return Err(tool_err(
            "LSP_ACTION_DISABLED",
            disabled["reason"]
                .as_str()
                .unwrap_or("server disabled this code action"),
        ));
    }
    Ok(())
}

fn command(action: &Value) -> Result<Option<Value>> {
    let Some(command) = action.get("command").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let (name, arguments) = if let Some(name) = command.as_str() {
        (name, action.get("arguments"))
    } else {
        (
            command["command"].as_str().ok_or_else(|| {
                tool_err("LSP_ACTION_MALFORMED", "command object has no command name")
            })?,
            command.get("arguments"),
        )
    };
    if name.is_empty() || name.len() > 1024 || name.contains('\0') {
        return Err(tool_err(
            "LSP_ACTION_MALFORMED",
            "invalid code action command name",
        ));
    }
    let mut params = json!({"command":name});
    if let Some(arguments) = arguments.filter(|value| !value.is_null()) {
        if !arguments.is_array() {
            return Err(tool_err(
                "LSP_ACTION_MALFORMED",
                "command arguments must be an array",
            ));
        }
        params["arguments"] = arguments.clone();
    }
    Ok(Some(params))
}

fn validate_resolution(original: &Value, resolved: Value) -> Result<Value> {
    enabled(&resolved)?;
    // LSP resolution adds lazily computed properties; it must not swap the
    // selected title, existing edit, command or identity data under the user.
    for (key, value) in original
        .as_object()
        .ok_or_else(|| tool_err("LSP_ACTION_MALFORMED", "invalid selected action"))?
    {
        if resolved.get(key) != Some(value) {
            return Err(tool_err(
                "LSP_ACTION_CHANGED",
                format!("codeAction/resolve changed existing property {key}"),
            ));
        }
    }
    Ok(resolved)
}

fn validate_action_request(input: &LspInput) -> Result<()> {
    if input.action_id.is_some()
        && (!input.apply.unwrap_or(false)
            || input.query.is_some()
            || input.range.is_some()
            || input.only.is_some()
            || input.symbol.is_some()
            || input.line.is_some())
    {
        return Err(tool_err(
            "LSP_USAGE",
            "actionId requires apply:true and cannot be combined with query, range, only, symbol or line",
        ));
    }
    if input.action_id.is_none()
        && input.apply.unwrap_or(false)
        && input
            .query
            .as_deref()
            .is_none_or(|query| query.trim().is_empty())
    {
        return Err(tool_err(
            "LSP_USAGE",
            "apply:true requires actionId or a nonempty query",
        ));
    }
    if let Some(only) = &input.only
        && (only.is_empty()
            || only.len() > 16
            || only.iter().any(|kind| {
                kind.is_empty()
                    || kind.len() > 128
                    || kind
                        .chars()
                        .any(|character| character.is_control() || character.is_whitespace())
                    || kind.split('.').any(str::is_empty)
            }))
    {
        return Err(tool_err(
            "LSP_USAGE",
            "only requires 1..16 nonempty action kinds, at most 128 bytes each, with nonempty dot-separated segments and no whitespace or controls",
        ));
    }
    Ok(())
}

fn matches_action_kind(action: &Value, only: &[String]) -> bool {
    action
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            only.iter().any(|requested| {
                kind == requested.as_str()
                    || kind
                        .strip_prefix(requested.as_str())
                        .is_some_and(|suffix| suffix.starts_with('.'))
            })
        })
}

impl LspTool {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn run_code_actions(&self, input: &LspInput) -> Result<ToolOutput> {
        // Validate before consuming a handle or starting a language server.
        validate_action_request(input)?;
        let (entry, snapshot, selected) = if let Some(id) = input.action_id.as_deref() {
            let cached = self.actions.take(id)?;
            let entry = cached
                .entry
                .upgrade()
                .filter(|entry| entry.client.is_alive())
                .ok_or_else(|| {
                    tool_err(
                        "LSP_ACTION_EXPIRED",
                        "language server restarted; list code actions again",
                    )
                })?;
            if let Some(file) = input.file.as_deref()
                && resolve_tool_path(file, &self.cwd).canonicalize()? != cached.path
            {
                return Err(tool_err("LSP_USAGE", "actionId belongs to another file"));
            }
            (entry, cached.snapshot, cached.action)
        } else {
            let file = input.file.as_deref().ok_or_else(|| {
                tool_err(
                    "LSP_USAGE",
                    "code_actions requires file or a cached actionId",
                )
            })?;
            let path = resolve_tool_path(file, &self.cwd).canonicalize()?;
            let hash = file_hash(&path)?;
            let range = Self::code_action_range(input, &path)?;
            let (uri, entry) = self.synced(&path).await?;
            let snapshot = Arc::new(refactor::RefactorSnapshot::capture(&entry, &path, hash)?);
            let diagnostics = entry
                .client
                .diagnostics_snapshot()
                .get(&uri)
                .cloned()
                .unwrap_or_default();
            let mut context = json!({"diagnostics":diagnostics,"triggerKind":1});
            if let Some(only) = &input.only {
                context["only"] = json!(only);
            }
            let result = entry
                .client
                .call(
                    "textDocument/codeAction",
                    json!({
                        "textDocument":{"uri":uri},"range":range,
                        "context":context
                    }),
                    self.request_timeout(input),
                )
                .await?;
            snapshot.verify_request_source(&entry)?;
            let mut actions = if result.is_null() {
                Vec::new()
            } else {
                result.as_array().cloned().ok_or_else(|| {
                    tool_err(
                        "LSP_ACTION_MALFORMED",
                        "codeAction result must be an array or null",
                    )
                })?
            };
            if actions.len() > MAX_ACTIONS || serde_json::to_vec(&actions)?.len() > MAX_ACTION_BYTES
            {
                return Err(tool_err(
                    "LSP_ACTION_LIMIT",
                    "code action response exceeds 128 actions or 2 MiB",
                ));
            }
            // Treat the requested kind as a selection boundary even when a
            // server ignores context.only. Indices refer to this filtered list.
            let received = actions.len();
            if let Some(only) = &input.only {
                actions.retain(|action| matches_action_kind(action, only));
            }
            if !input.apply.unwrap_or(false) {
                let mut summaries = Vec::new();
                // Old handles are intentionally replaced by the new listing.
                lock(&self.actions.cache).clear();
                for (index, action) in actions.iter().enumerate() {
                    let title = action["title"]
                        .as_str()
                        .filter(|title| !title.is_empty())
                        .ok_or_else(|| {
                            tool_err("LSP_ACTION_MALFORMED", "code action has no title")
                        })?;
                    let id = self.actions.remember(
                        &entry,
                        &path,
                        Arc::clone(&snapshot),
                        action.clone(),
                    )?;
                    let preview: String = title.chars().take(1000).collect();
                    summaries.push(json!({
                        "index":index+1,"actionId":id,"title":preview,"titleTruncated":preview.len()!=title.len(),"kind":action.get("kind"),
                        "isPreferred":action["isPreferred"] == true,
                        "disabled":action.get("disabled").is_some_and(|value| !value.is_null()),
                        "disabledReason":action.pointer("/disabled/reason"),
                        "hasEdit":action.get("edit").is_some_and(|value| !value.is_null()),
                        "hasCommand":action.get("command").is_some_and(|value| !value.is_null())
                    }));
                }
                let payload = json!({
                    "action":"code_actions","file":display_path(&path,&self.cwd),
                    "range":range,"only":input.only,"filteredOut":received-actions.len(),
                    "count":summaries.len(),"actions":summaries
                });
                return Ok(text_output(payload.to_string(), payload));
            }
            let query = input
                .query
                .as_deref()
                .filter(|query| !query.trim().is_empty())
                .ok_or_else(|| {
                    tool_err(
                        "LSP_USAGE",
                        "apply:true requires actionId or a nonempty query",
                    )
                })?;
            let selected = select_code_action(&actions, query)?;
            (entry, snapshot, selected)
        };
        enabled(&selected)?;
        snapshot.verify_request_source(&entry)?;
        let selected = if selected["command"].is_string() {
            // Legacy Command literals are not CodeAction objects.
            selected
        } else if entry
            .client
            .capabilities()
            .raw
            .pointer("/codeActionProvider/resolveProvider")
            == Some(&Value::Bool(true))
            && selected.get("edit").is_none_or(Value::is_null)
        {
            let resolved = entry
                .client
                .call(
                    "codeAction/resolve",
                    selected.clone(),
                    self.request_timeout(input),
                )
                .await?;
            validate_resolution(&selected, resolved)?
        } else {
            selected
        };
        enabled(&selected)?;
        let command = command(&selected)?; // Validate before applying any edit.
        let edit = selected.get("edit").filter(|value| !value.is_null());
        if edit.is_none() && command.is_none() {
            return Err(tool_err(
                "LSP_ACTION_NO_EDIT",
                "resolved code action contains neither an edit nor a command",
            ));
        }
        snapshot.verify_request_source(&entry)?;
        let owner = AgentCx::for_current_or_request();
        owner
            .checkpoint()
            .map_err(|_| tool_err("LSP_CANCELLED", "code action cancelled before applying"))?;
        lock(&self.actions.cache).clear();
        let mut report = Report::default();
        let mut failure = None;
        let mut command_started = false;
        if let Some(params) = &command {
            // Reserve the lineage before inline writes, but do not authorize
            // server callbacks until those writes and activation checks pass.
            let lease = self.actions.grant(&entry, snapshot.command_edits(owner))?;
            if let Some(edit) = edit {
                lease.apply_inline(&entry, edit)?;
            }
            let timeout = self.request_timeout(input);
            match lease.activate(&entry, timeout) {
                Ok(()) => {
                    command_started = true;
                    if let Err(error) = entry
                        .client
                        .call("workspace/executeCommand", params.clone(), timeout)
                        .await
                    {
                        failure = Some(error.message());
                    }
                }
                Err(error) => failure = Some(error.to_string()),
            }
            report = lease.finish();
        } else if let Some(edit) = edit {
            report.record(self.apply_refactor(&entry, edit, &snapshot, &owner)?);
        }
        if failure.is_none() && !report.failures.is_empty() {
            failure = Some("server edit request was rejected during the command".to_string());
        }
        let failed = failure.is_some();
        let partial = failed && report.changed();
        let files: Vec<_> = report
            .files
            .iter()
            .map(|path| display_path(path, &self.cwd))
            .collect();
        let payload = json!({
            "action":"code_actions","title":selected["title"],"applied":!failed,
            "filesChanged":files,"fileOps":report.operations,
            "executedCommand":command.as_ref().filter(|_| command_started).and_then(|params| params["command"].as_str()),
            "serverEditRequests":report.requests,"serverEditFailures":report.failures,
            "partial":partial,"rollbackIncomplete":report.rollback_incomplete,"error":failure,
            "note":if failed { "Previously accepted edits or command effects are not rolled back; inspect before retrying." } else { "Edit applied before command; no automatic command retry." }
        });
        let mut output = text_output(payload.to_string(), payload);
        output.is_error = failed;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_and_changed_resolved_actions_are_rejected() {
        assert!(enabled(&json!({"title":"Fix","disabled":{"reason":"not available"}})).is_err());
        let action = json!({"title":"Fix","data":{"id":7}});
        assert!(
            validate_resolution(&action, json!({"title":"Other","data":{"id":7},"edit":{}}))
                .is_err()
        );
        assert!(
            validate_resolution(&action, json!({"title":"Fix","data":{"id":7},"edit":{}})).is_ok()
        );
    }

    #[test]
    fn commands_preserve_literal_arguments_and_reject_malformed_shapes() {
        assert_eq!(
            command(&json!({"title":"Fix","command":"fix","arguments":["$(literal)"]})).unwrap(),
            Some(json!({"command":"fix","arguments":["$(literal)"]}))
        );
        assert_eq!(
            command(&json!({"title":"Fix","command":{"command":"fix","arguments":[7]}})).unwrap(),
            Some(json!({"command":"fix","arguments":[7]}))
        );
        assert!(
            command(&json!({"title":"Fix","command":{"command":"fix","arguments":{}}})).is_err()
        );
    }

    #[test]
    fn changed_source_cannot_reuse_a_listed_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "before").unwrap();
        let hash = file_hash(&path).unwrap();
        std::fs::write(&path, "after").unwrap();
        assert!(verify_source(&path, hash).is_err());
    }

    #[test]
    fn scope_checks_include_new_destinations_and_escape_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(inside_root(&root.join("new/sub/file.rs"), &root).is_ok());
        assert!(inside_root(&root.join("../outside.rs"), &root).is_err());
        assert!(inside_root(&root, &root).is_err());
        assert!(inside_root(Path::new("relative.rs"), &root).is_err());
    }

    include!("actions/protocol_tests.rs");
    include!("actions/selection_tests.rs");
    include!("actions/command_tests.rs");
}
