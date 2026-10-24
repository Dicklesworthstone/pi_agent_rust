//! Recover accepted wave evidence without inventing a second workflow runtime.
//!
//! The native jobs store owns retention, privacy and terminal publication.
//! These observations are checkpoints, not a durable journal or resumable plan.

use super::{Error, Record, Result, State, ToolOutput, encoded_size};
use crate::model::{ContentBlock, TextContent};
use crate::text_completion::AuxiliaryPrivacySnapshot;
use serde::Serialize;

// Leave headroom for the ToolOutput and jobs progress/interruption wrappers.
const MAX_CHECKPOINT_BYTES: usize = 768 * 1024;

pub(crate) struct Progress {
    session: String,
    job: String,
    privacy: AuxiliaryPrivacySnapshot,
    revision: usize,
}

impl Progress {
    pub(crate) fn new(session: String, job: String, privacy: AuxiliaryPrivacySnapshot) -> Self {
        Self {
            session,
            job,
            privacy,
            revision: 0,
        }
    }

    pub(super) fn publish(&mut self, records: &[Record], wave: usize, waves: usize) -> Result<()> {
        let revision = self.revision + 1; // At most one checkpoint per graph wave (32).
        let output = match checkpoint(records, wave, waves, revision) {
            Ok(output) => output,
            Err(error) => {
                // Never retain an older checkpoint as if it were the newest
                // observation when this wave cannot be represented completely.
                let _ = crate::jobs::clear_owned_task_progress(&self.session, &self.job);
                return Err(error);
            }
        };
        // This exact projection refuses changed typed data and clears stale
        // progress on a privacy/size failure. Cancellation and session shutdown
        // fence publication in the existing jobs registry, not a local cache.
        crate::jobs::publish_owned_task_progress(
            &self.session, &self.job, output, &self.privacy,
        ).map_err(|_| Error::tool(
            "subagent",
            "PI_WORKFLOW_PROGRESS_UNAVAILABLE: checkpoint publication failed; no later wave was started; prior effects may remain",
        ))?;
        self.revision = revision;
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Checkpoint<'a> {
    schema: &'static str,
    mode: &'static str,
    complete: bool,
    checkpoint: usize,
    settled_through_wave: usize,
    waves_executed: usize,
    total_nodes: usize,
    accepted_nodes: usize,
    rejected_nodes: usize,
    skipped_nodes: usize,
    not_selected_nodes: usize,
    pending_nodes: usize,
    nodes: &'a [Record],
}

fn checkpoint(
    records: &[Record],
    wave: usize,
    waves: usize,
    revision: usize,
) -> Result<ToolOutput> {
    let count = |state| {
        records
            .iter()
            .filter(|record| record.state == state)
            .count()
    };
    let snapshot = Checkpoint {
        schema: "pi.subagent.workflow.progress.v1",
        mode: "workflow",
        complete: false,
        checkpoint: revision,
        settled_through_wave: wave,
        waves_executed: waves,
        total_nodes: records.len(),
        accepted_nodes: count(State::Accepted),
        rejected_nodes: count(State::Rejected),
        skipped_nodes: count(State::Skipped),
        not_selected_nodes: count(State::NotSelected),
        pending_nodes: count(State::Pending),
        nodes: records,
    };
    // Count borrowed records BEFORE materializing another copy of native output.
    // Truncating data or omitting failed nodes would change the evidence contract.
    if encoded_size(&snapshot, MAX_CHECKPOINT_BYTES).is_none() {
        return Err(Error::tool(
            "subagent",
            "PI_WORKFLOW_PROGRESS_LIMIT: complete checkpoint exceeds 768 KiB; no later wave was started; prior effects may remain",
        ));
    }
    let mut text = format!(
        "Workflow checkpoint {revision} after wave {wave}: {} accepted, {} rejected, {} skipped, {} not selected, {} unresolved. This is not a final result. Unresolved nodes may have started since the checkpoint; never replay them automatically.",
        snapshot.accepted_nodes,
        snapshot.rejected_nodes,
        snapshot.skipped_nodes,
        snapshot.not_selected_nodes,
        snapshot.pending_nodes,
    );
    for record in records {
        text.push_str(&format!("\n{}: {}", record.id, record.state.label()));
        if let Some(id) = &record.hub_id {
            text.push_str(&format!("; hub {}", serde_json::to_string(id)?));
        }
    }
    Ok(ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(serde_json::to_value(snapshot)?),
        // Even a checkpoint containing failed nodes is not a terminal job error.
        // Native final reporting still determines whether the graph succeeded.
        is_error: false,
    })
}

#[cfg(all(test, unix))]
mod tests;
