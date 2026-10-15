//! Connection and document evidence for one coordinated refactor.
//!
//! Every contributor remains part of approval, including a server returning no
//! edits. One server's survival never stands in for another server's evidence.

use std::sync::{Arc, Weak};

use serde_json::{Value, json};

use super::{LspTool, PreparedEdit, Result, ServerEntry, stale, tool_err, try_path_to_uri};

pub(in crate::lsp) const MAX_REFACTOR_SERVERS: usize = 8;

struct Participant {
    entry: Weak<ServerEntry>,
    documents: Vec<(String, Arc<str>)>,
    notification: Option<Value>,
}

pub(in crate::lsp) struct RefactorGroup(Vec<Participant>);

impl RefactorGroup {
    pub(in crate::lsp) fn capture(
        sources: Vec<(Arc<ServerEntry>, Option<Value>)>,
        prepared: &PreparedEdit,
    ) -> Result<Self> {
        if sources.is_empty() || sources.len() > MAX_REFACTOR_SERVERS {
            return Err(tool_err("LSP_EDIT_LIMIT", "refactor requires 1..8 server connections"));
        }
        let mut participants = Vec::with_capacity(sources.len());
        for (index, (entry, notification)) in sources.iter().enumerate() {
            if sources[..index].iter().any(|(other, _)| Arc::ptr_eq(entry, other)) {
                return Err(tool_err("LSP_EDIT_CONFLICT", "duplicate refactor server connection"));
            }
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
            participants.push(Participant {
                entry: Arc::downgrade(entry),
                documents,
                notification: notification.clone(),
            });
        }
        let group = Self(participants);
        group.verify()?;
        Ok(group)
    }

    pub(in crate::lsp) fn requested(&self) -> bool {
        self.0.iter().any(|participant| participant.notification.is_some())
    }

    /// Pin all original connections through the final commit/notification path.
    pub(in crate::lsp) fn verify(&self) -> Result<Vec<Arc<ServerEntry>>> {
        self.0.iter().map(|participant| {
            let entry = participant.entry.upgrade().ok_or_else(stale)?;
            if !entry.client.is_alive() {
                return Err(stale());
            }
            for (uri, before) in &participant.documents {
                if !entry.client.synchronized_text(uri)
                    .is_some_and(|now| Arc::ptr_eq(before, &now))
                {
                    return Err(stale());
                }
            }
            Ok(entry)
        }).collect()
    }

    /// Publish all requested notifications after a successful filesystem commit.
    /// A failed notification never stops delivery to the remaining participants
    /// or changes an applied result into an error inviting a second move.
    pub(in crate::lsp) fn notify(
        self,
        entries: &[Arc<ServerEntry>],
        metadata: &mut Value,
    ) {
        debug_assert_eq!(self.0.len(), entries.len());
        let requested = self.requested();
        let mut warnings = Vec::new();
        let mut reports = Vec::with_capacity(entries.len());
        for (participant, entry) in self.0.into_iter().zip(entries) {
            debug_assert!(Weak::ptr_eq(&participant.entry, &Arc::downgrade(entry)));
            let wanted = participant.notification.is_some();
            let warning = LspTool::notify_refactor_move(entry, participant.notification);
            reports.push(json!({
                "server":entry.spec_name,"requested":wanted,
                "written":wanted && warning.is_none(),"warning":warning
            }));
            if let Some(warning) = warning {
                warnings.push(format!("{}: {warning}", entry.spec_name));
            }
        }
        metadata["notificationRequested"] = json!(requested);
        metadata["notificationWritten"] = json!(requested && warnings.is_empty());
        metadata["warning"] = json!(if warnings.is_empty() { None } else { Some(warnings.join("; ")) });
        metadata["serverNotifications"] = json!(reports);
    }
}
