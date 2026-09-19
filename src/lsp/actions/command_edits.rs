//! Evidence and ownership for an explicitly selected command's edit batches.
//!
//! Inline edits and callbacks share one lineage. Only the first observation of
//! an unsynchronized file uses a transaction-time snapshot; thereafter receipts
//! pin its expected content hash or absence. This does not sandbox the
//! language-server process or roll back already acknowledged earlier batches.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{AgentCx, ApplyOutcome, FileOp, Result, ServerEntry, Value, inside_root, parse_workspace_edit, refactor, tool_err};
use crate::lsp::client::{DocumentSnapshot, try_path_to_uri};
use crate::lsp::edits::{FileEvidence, apply_checked};

const MAX_COMMAND_FILES: usize = 1024;

pub(super) struct CommandEdits {
    owner: AgentCx,
    states: HashMap<PathBuf, FileEvidence>,
    documents: HashMap<PathBuf, DocumentSnapshot>,
    // The selected source and every path already observed by accepted batches
    // remain guards, even when the next batch only edits another file.
    anchors: HashSet<PathBuf>,
    deadline: Option<(Instant, Duration)>,
}

impl CommandEdits {
    pub(super) fn new(
        source: PathBuf,
        source_hash: u64,
        documents: HashMap<PathBuf, DocumentSnapshot>,
        owner: AgentCx,
    ) -> Self {
        let mut states: HashMap<_, _> = documents.iter().map(|(path, document)| {
            (path.clone(), FileEvidence::Text(document.hash))
        }).collect();
        states.insert(source.clone(), FileEvidence::Text(source_hash));
        Self { owner, states, documents, anchors: HashSet::from([source]), deadline: None }
    }

    pub(super) fn activate(&mut self, entry: &ServerEntry, timeout: Duration) -> Result<()> {
        self.deadline = Some((Instant::now(), timeout));
        // Recheck the selected source after inline edits but before authorizing
        // callbacks. An empty batch writes nothing and retains existing docs.
        self.apply(entry, &Value::Null, || Ok(())).map(|_| ())
    }

    fn checkpoint(&self, entry: &ServerEntry) -> Result<()> {
        self.owner.checkpoint().map_err(|_| tool_err("LSP_CANCELLED", "selected command was cancelled"))?;
        if !self.owner.capabilities().io {
            return Err(tool_err("LSP_EDIT_PERMISSION", "selected command owner does not permit filesystem I/O"));
        }
        if self.deadline.is_some_and(|(started, timeout)| started.elapsed() >= timeout) {
            return Err(tool_err("LSP_TIMEOUT", "selected command edit window expired"));
        }
        if !entry.client.is_alive() {
            return Err(tool_err("LSP_TRANSPORT_CLOSED", "selected command connection closed"));
        }
        Ok(())
    }

    pub(super) fn apply(
        &mut self,
        entry: &ServerEntry,
        raw: &Value,
        permission: impl Fn() -> Result<()>,
    ) -> Result<ApplyOutcome> {
        permission()?;
        self.checkpoint(entry)?;
        refactor::check_response_size(raw)?;
        let plan = parse_workspace_edit(raw)?;
        let mut touched: HashSet<_> = plan.text_edits.keys().cloned().collect();
        for operation in &plan.file_ops {
            match operation {
                FileOp::Create { path, .. } | FileOp::Delete { path } => { touched.insert(path.clone()); }
                FileOp::Rename { old_path, new_path, .. } => {
                    touched.insert(old_path.clone());
                    touched.insert(new_path.clone());
                }
            }
        }
        let new_paths = touched.iter().filter(|path| !self.states.contains_key(*path)).count();
        if self.states.len().saturating_add(new_paths) > MAX_COMMAND_FILES {
            return Err(tool_err("LSP_EDIT_LIMIT", "selected command exceeds 1024 observed files"));
        }
        let root = entry.client.root();
        for path in touched.iter().chain(&self.anchors) {
            inside_root(path, root)?;
        }
        let current = entry.client.document_snapshots();
        refactor::validate_versions(raw, &self.documents, &current)?;
        let mut expected = HashMap::new();
        for path in touched.iter().chain(&self.anchors) {
            if let Some(before) = self.documents.get(path)
                && current.get(path).is_none_or(|now| now.version != before.version || now.hash != before.hash)
            {
                return Err(tool_err("LSP_EDIT_CONFLICT", "document synchronization changed during the selected command"));
            }
            if let Some(state) = self.states.get(path) {
                expected.insert(path.clone(), *state);
            }
        }
        // Do every fallible URI conversion before commit. Once the write has
        // succeeded, notification failure must not erase its apply receipt.
        let invalidations: Vec<_> = touched.iter().map(|path| try_path_to_uri(path)).collect::<Result<_>>()?;
        let checked = match apply_checked(&plan, &expected, || {
            permission()?;
            self.checkpoint(entry)
        }) {
            Ok(checked) => checked,
            Err(error) => {
                entry.client.invalidate_all();
                return Err(error);
            }
        };
        self.anchors.extend(checked.states.keys().cloned());
        self.states.extend(checked.states);
        for path in &touched {
            // Closing changed buffers invalidates their old wire versions.
            // Subsequent callbacks may use null versions plus receipt guards;
            // they cannot reuse a version from before an accepted edit.
            self.documents.remove(path);
        }
        for uri in invalidations {
            entry.client.invalidate(&uri);
        }
        Ok(checked.outcome)
    }
}
