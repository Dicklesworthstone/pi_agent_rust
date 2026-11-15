//! Frozen, keep-only workspaces for retained RPC conversations. A pool shares
//! one captured parent state, but each member owns a different materialization.
//! No turn, close, cancellation, or destructor applies or deletes these files.

use crate::error::{Error, Result};
use crate::text_completion::AuxiliaryPrivacySnapshot;
use crate::worktree_iso::{IsoBaseline, IsoHandle, IsoOutcome};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(in crate::subagents) struct Capture {
    baseline: IsoBaseline,
    relative_cwd: PathBuf,
    privacy: AuxiliaryPrivacySnapshot,
}

pub(super) fn validate(input: &Value) -> Result<()> {
    // Pool assignments are strings, not ordinary task objects. Validate only
    // the common controls rather than invoking the ordinary batch validator.
    let mut controls = serde_json::Map::new();
    for name in ["cwd", "isolation", "isoApply"] {
        if let Some(value) = input.get(name) {
            controls.insert(name.to_string(), value.clone());
        }
    }
    crate::subagents::workspace::validate_request(&Value::Object(controls))?;
    if input
        .get("isoApply")
        .is_some_and(|value| value.as_str() != Some("keep"))
    {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_WORKER_ISOLATION: retained workers and pools preserve worktrees; isoApply may only be keep",
        ));
    }
    Ok(())
}

impl Capture {
    /// Call after model/schema preflight, before admitting or spawning children.
    /// This does not create a worktree or modify the parent's real HEAD/index.
    pub(super) fn prepare(
        input: &Value,
        cwd: &Path,
        privacy: &AuxiliaryPrivacySnapshot,
    ) -> Result<Option<Arc<Self>>> {
        validate(input)?;
        if input.get("isolation").and_then(Value::as_str) != Some("worktree") {
            return Ok(None);
        }
        let root = crate::worktree_iso::isolation_root(cwd)?;
        let relative_cwd = crate::worktree_iso::relative_working_directory(&root, cwd)?;
        let baseline = IsoBaseline::capture(root)?;
        Ok(Some(Arc::new(Self {
            baseline,
            relative_cwd,
            privacy: privacy.clone(),
        })))
    }

    pub(in crate::subagents) fn materialize(&self, id: &str) -> Result<IsoHandle> {
        self.baseline.isolate(id)
    }

    pub(in crate::subagents) fn working_directory(&self, handle: &IsoHandle) -> Result<PathBuf> {
        handle.working_directory(&self.relative_cwd)
    }

    /// A recovery descriptor is small and never carries a live authority.
    /// Screen before launch, including for an initially empty workpool.
    pub(in crate::subagents) fn descriptor(&self, handle: &IsoHandle) -> Result<Value> {
        let descriptor = json!({
            "schema": "pi.subagent.worker.workspace.v1",
            "mode": "worktree",
            "worktreePath": handle.path,
            "branch": handle.branch,
            "baseline": handle.baseline,
            "cwd": handle.path.join(&self.relative_cwd),
            "applyMode": "keep",
            "applied": false,
        });
        if self.privacy.project_json(&descriptor)? != descriptor {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_WORKER_ISOLATION_PRIVACY: launch policy withheld workspace identity; no child started and the worktree is preserved",
            ));
        }
        Ok(descriptor)
    }
}

/// Each accepted turn reports the cumulative patch against its fixed launch
/// baseline. It is never an incremental patch against the preceding turn.
pub(in crate::subagents) fn outcome(handle: &IsoHandle) -> Result<IsoOutcome> {
    let (patch, diff_stat) = crate::worktree_iso::collect_diff(handle)?;
    Ok(IsoOutcome {
        schema: crate::worktree_iso::ISO_SCHEMA.to_string(),
        worktree_path: handle.path.display().to_string(),
        branch: handle.branch.clone(),
        diff_stat,
        patch,
        conflicted_files: Vec::new(),
        apply_mode: "keep".to_string(),
        applied: false,
    })
}

/// Redacting a patch or its source identity can change its effects. Withhold
/// such a result rather than returning modified executable file data.
pub(super) fn preserves_result(original: &Value, projected: &Value) -> bool {
    original
        .pointer("/details/results")
        .and_then(Value::as_array)
        .is_none_or(|results| {
            results.iter().enumerate().all(|(index, result)| {
                result.get("iso").is_none_or(|iso| {
                    projected.pointer(&format!("/details/results/{index}/iso")) == Some(iso)
                })
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_workspaces_refuse_writeback_and_malformed_controls() {
        for input in [
            json!({"isolation":"worktree", "isoApply":"apply"}),
            json!({"isolation":"worktree", "isoApply":"drop"}),
            json!({"isoApply":"keep"}),
            json!({"isolation":"none", "isoApply":"keep"}),
            json!({"isolation":null}),
            json!({"isolation":"worktree", "isoApply":null}),
        ] {
            assert!(validate(&input).is_err());
        }
        for input in [
            json!({}),
            json!({"isolation":"none"}),
            json!({"isolation":"worktree"}),
            json!({"isolation":"worktree", "isoApply":"keep", "tasks":["edit"]}),
        ] {
            assert!(validate(&input).is_ok());
        }
    }

    #[test]
    fn privacy_cannot_change_or_remove_a_retained_patch() {
        let original = json!({"content":[], "details":{"results":[
            {"output":"private text", "iso":{"patch":"-old\n+new\n", "worktreePath":"/tmp/work"}}
        ]}});
        let mut projected = original.clone();
        projected["details"]["results"][0]["output"] = json!("redacted");
        assert!(preserves_result(&original, &projected));
        projected["details"]["results"][0]["iso"]["patch"] = json!("redacted");
        assert!(!preserves_result(&original, &projected));
        projected["details"]["results"][0]["iso"] = Value::Null;
        assert!(!preserves_result(&original, &projected));
        assert!(!preserves_result(&original, &Value::Null));
    }
}
