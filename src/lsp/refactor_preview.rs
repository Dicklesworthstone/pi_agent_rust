//! Review and approve an exact, already-staged multi-file refactor.
//!
//! One bounded preview belongs to one tool and one live server connection.
//! Approval consumes it; no server request or re-planning occurs on approval.
//! The transaction retains original bytes/permissions even for unopened files.

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::client::try_path_to_uri;
use super::edits::PreparedEdit;
use super::{LspInput, LspTool, MAX_PAYLOAD_BYTES, Result, ServerEntry, ToolOutput, display_path, text_output, tool_err};
use crate::agent_cx::AgentCx;

const PREVIEW_AGE: Duration = Duration::from_secs(300);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn stale() -> crate::error::Error {
    tool_err("LSP_REFACTOR_STALE", "refactor plan, document or server expired; request a new preview")
}

pub(super) fn check_owner(owner: &AgentCx) -> Result<()> {
    owner.checkpoint().map_err(|_| tool_err("LSP_CANCELLED", "refactor cancelled"))?;
    if !owner.capabilities().io {
        return Err(tool_err("LSP_EDIT_PERMISSION", "refactor requires filesystem I/O authority"));
    }
    Ok(())
}

pub(super) fn validate_selection(input: &LspInput) -> Result<()> {
    let Some(id) = &input.refactor_id else { return Ok(()) };
    if !matches!(input.action.as_str(), "rename" | "rename_file" | "code_actions") || id.is_empty() || id.len() > 128
        || input.file.is_some() || input.new_name.is_some() || input.new_file.is_some()
        || input.position.is_some() || input.symbol.is_some() || input.line.is_some()
        || input.range.is_some() || input.query.is_some() || input.limit.is_some()
        || input.only.is_some() || input.after.is_some() || input.action_id.is_some()
        || input.completion_id.is_some() || input.snippet_values.is_some()
        || input.hierarchy_id.is_some() || input.resolve.is_some() || input.format_options.is_some()
        || input.method.is_some() || input.payload.is_some()
    {
        return Err(tool_err("LSP_USAGE", "refactorId requires its original action and accepts only apply and timeout; the plan cannot be overridden"));
    }
    Ok(())
}

/// Refuse oversized previews, rather than issuing an approval handle for a
/// truncated edit. Admission counts bytes without allocating a serialized copy.
fn output(payload: Value) -> Result<ToolOutput> {
    struct Limit(usize);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| std::io::Error::other("refactor preview limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    serde_json::to_writer(&mut Limit(MAX_PAYLOAD_BYTES), &payload)
        .map_err(|_| tool_err("LSP_EDIT_LIMIT", "refactor preview exceeds 200 KiB; no approval handle was issued"))?;
    Ok(text_output(payload.to_string(), payload))
}

struct Preview {
    id: String,
    entry: Weak<ServerEntry>,
    prepared: PreparedEdit,
    documents: Vec<(String, Arc<str>)>,
    metadata: Value,
    payload: Value,
    notification: Option<Value>,
    created: Instant,
}

fn verify_documents(entry: &ServerEntry, documents: &[(String, Arc<str>)]) -> Result<()> {
    if !entry.client.is_alive() {
        return Err(stale());
    }
    for (uri, before) in documents {
        if !entry.client.synchronized_text(uri).is_some_and(|now| Arc::ptr_eq(before, &now)) {
            return Err(stale());
        }
    }
    Ok(())
}

impl Preview {
    fn verify(&self, owner: &AgentCx) -> Result<Arc<ServerEntry>> {
        check_owner(owner)?;
        if self.created.elapsed() >= PREVIEW_AGE {
            return Err(stale());
        }
        let entry = self.entry.upgrade().ok_or_else(stale)?;
        verify_documents(&entry, &self.documents)?;
        self.prepared.verify()?;
        check_owner(owner)?;
        if self.created.elapsed() >= PREVIEW_AGE { return Err(stale()); }
        verify_documents(&entry, &self.documents)?;
        Ok(entry)
    }
}

#[derive(Default)]
pub(super) struct RefactorCache(Mutex<Option<Preview>>);

impl RefactorCache {
    pub(super) fn clear(&self) {
        *lock(&self.0) = None;
    }
}

impl LspTool {
    /// The caller has already validated workspace scope and request-time
    /// versions before staging. All subsequently read paths are immutable.
    pub(super) fn cache_refactor(
        &self,
        entry: &Arc<ServerEntry>,
        prepared: PreparedEdit,
        edit: Value,
        metadata: Value,
        notification: Option<Value>,
        owner: &AgentCx,
    ) -> Result<ToolOutput> {
        check_owner(owner)?;
        let mut documents = Vec::new();
        for path in prepared.paths() {
            let uri = try_path_to_uri(path)?;
            if let Some(text) = entry.client.synchronized_text(&uri) {
                if !prepared.matches_document(path, &text) {
                    return Err(stale());
                }
                documents.push((uri, text));
            }
        }
        let id = format!("refactor-{}", uuid::Uuid::new_v4().simple());
        let mut payload = metadata.clone();
        payload["refactorId"] = json!(id);
        payload["applied"] = json!(false);
        payload["preview"] = json!(true);
        payload["expiresInSecs"] = json!(PREVIEW_AGE.as_secs());
        payload["files"] = json!(prepared.summary(&self.cwd));
        payload["workspaceEdit"] = edit;
        payload["notificationRequested"] = json!(notification.is_some());
        payload["notificationWritten"] = json!(false);
        payload["atomic"] = json!(false);
        payload["rollbackOnError"] = json!(true);
        let preview = Preview {
            id, entry: Arc::downgrade(entry), prepared, documents, metadata,
            payload, notification, created: Instant::now(),
        };
        preview.verify(owner)?;
        let result = output(preview.payload.clone())?;
        *lock(&self.refactors.0) = Some(preview);
        Ok(result)
    }

    pub(super) fn select_refactor(&self, input: &LspInput, owner: &AgentCx) -> Result<ToolOutput> {
        validate_selection(input)?;
        check_owner(owner)?;
        let started = Instant::now();
        let timeout = self.request_timeout(input);
        let preview = {
            let mut cache = lock(&self.refactors.0);
            let preview = cache.as_ref()
                .filter(|preview| Some(preview.id.as_str()) == input.refactor_id.as_deref())
                .ok_or_else(stale)?;
            if preview.metadata["action"].as_str() != Some(input.action.as_str()) {
                return Err(tool_err("LSP_USAGE", "refactorId belongs to a different action"));
            }
            // Consume before validation or any write. Failed/stale applications
            // cannot be replayed, including failures with incomplete rollback.
            cache.take().ok_or_else(stale)?
        };
        let entry = preview.verify(owner)?;
        if started.elapsed() >= timeout {
            return Err(tool_err("LSP_TIMEOUT", "refactor approval timed out before delivery"));
        }
        if input.apply != Some(true) {
            let mut payload = preview.payload.clone();
            payload["expiresInSecs"] = json!(PREVIEW_AGE.saturating_sub(preview.created.elapsed()).as_secs());
            let result = output(payload)?;
            *lock(&self.refactors.0) = Some(preview);
            return Ok(result);
        }
        let Preview { id, prepared, documents, mut metadata, notification, created, .. } = preview;
        let result = prepared.commit(|| {
            check_owner(owner)?;
            if started.elapsed() >= timeout {
                return Err(tool_err("LSP_TIMEOUT", "refactor approval timed out before commit"));
            }
            if created.elapsed() >= PREVIEW_AGE { return Err(stale()); }
            verify_documents(&entry, &documents)
        });
        self.invalidate_refactor(&entry);
        let outcome = result?;
        let requested = notification.is_some();
        let warning = Self::notify_refactor_move(&entry, notification);
        let files: Vec<_> = outcome.files_changed.iter().map(|path| display_path(path, &self.cwd)).collect();
        metadata["refactorId"] = json!(id);
        metadata["applied"] = json!(true);
        metadata["preview"] = json!(false);
        metadata["filesChanged"] = json!(files);
        if input.action == "rename_file" { metadata["importUpdates"] = json!(files); }
        metadata["fileOps"] = json!(outcome.file_ops_applied);
        metadata["notificationRequested"] = json!(requested);
        metadata["notificationWritten"] = json!(requested && warning.is_none());
        metadata["warning"] = json!(warning);
        metadata["atomic"] = json!(false);
        metadata["rollbackOnError"] = json!(true);
        // The complete preview was bounded before approval. A notification
        // failure is reported as applied-with-warning, never as safe to retry.
        Ok(text_output(metadata.to_string(), metadata))
    }

    pub(super) fn notify_refactor_move(entry: &ServerEntry, params: Option<Value>) -> Option<String> {
        params.and_then(|params| {
            entry.client.call_no_wait_notify("workspace/didRenameFiles", params).err().map(|error| {
                entry.client.kill();
                format!("Files were moved, but the server notification failed: {}. Do not repeat the move; reload the server.", error.message())
            })
        })
    }
}

#[cfg(test)]
mod tests;
