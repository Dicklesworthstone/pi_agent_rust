//! Carry trusted content/existence evidence between acknowledged edit batches.
//!
//! Receipts are derived from staged final images, never by reading the disk
//! again after commit. A concurrent writer therefore cannot silently become
//! the trusted baseline for the next workspace/applyEdit callback.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use super::{ApplyOutcome, Image, Result, Transaction, WorkspaceEditPlan, conflict};
use crate::lsp::text::content_hash_for_drift;

/// Absence is evidence too: a deleted or moved-away path must not be silently
/// recreated by another writer between callbacks. Text hashes match the
/// existing request-time DocumentSnapshot hash; binary resources remain
/// supported without lossy UTF-8 conversion or conflating them with absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::lsp) enum FileEvidence {
    Absent,
    Text(u64),
    Binary(u64),
}

impl FileEvidence {
    fn from_image(image: Option<&Image>) -> Self {
        let Some(image) = image else { return Self::Absent };
        if let Ok(text) = std::str::from_utf8(&image.bytes) {
            Self::Text(content_hash_for_drift(text))
        } else {
            let mut hash = DefaultHasher::new();
            image.bytes.hash(&mut hash);
            Self::Binary(hash.finish())
        }
    }
}

#[derive(Debug)]
pub(in crate::lsp) struct CheckedApply {
    pub(in crate::lsp) outcome: ApplyOutcome,
    pub(in crate::lsp) states: HashMap<PathBuf, FileEvidence>,
}

fn prepare_checked(
    plan: &WorkspaceEditPlan,
    expected: &HashMap<PathBuf, FileEvidence>,
) -> Result<Transaction> {
    let mut transaction = super::super::sequence::stage(plan)?;
    for (path, expected) in expected {
        // Guard-only files participate in the commit's existing preimage
        // recheck even when the server edits only a sibling document.
        let normalized = transaction.load(path)?;
        let actual = FileEvidence::from_image(transaction.files[&normalized].before.as_ref());
        if actual != *expected {
            return Err(conflict(format!(
                "{} changed on disk since the selected action or last accepted edit; re-run the request",
                path.display()
            )));
        }
    }
    Ok(transaction)
}

/// Apply one rollback-safe batch and return staged content/existence evidence.
/// Callers own workspace scoping, document-version checks and the bounded
/// lifetime of the receipt. The callback rechecks ownership/cancellation after
/// expensive staging, immediately before the existing commit implementation.
/// Accepted batches are not one cross-request or crash-atomic transaction.
#[allow(clippy::implicit_hasher)] // Receipts and request snapshots use one concrete map type.
pub(in crate::lsp) fn apply_checked(
    plan: &WorkspaceEditPlan,
    expected: &HashMap<PathBuf, FileEvidence>,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<CheckedApply> {
    let transaction = prepare_checked(plan, expected)?;
    let states = transaction.files.iter().map(|(path, staged)| {
        (path.clone(), FileEvidence::from_image(staged.after.as_ref()))
    }).collect();
    before_commit()?;
    let outcome = transaction.commit()?;
    Ok(CheckedApply { outcome, states })
}

#[cfg(test)]
mod tests;
