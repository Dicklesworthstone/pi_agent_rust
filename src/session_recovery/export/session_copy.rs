//! Explicit, create-only text recovery forks. Inspection stays read-only and
//! normal session loading never silently substitutes this lossy operation.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize as _;
use serde_json::{Value, json};

use super::{
    AgentCx, CheckedWriter, MAX_EXPORT_BYTES, RecoveryReport, Result, RetainedBudget, check_owner,
    graph, invalid, publish_new_file,
};
use crate::session::{Session, SessionEntry, SessionHeader};

const PROVENANCE: &str = "pi.session_text_recovery.fork.v1";
const NOTICE: &str = "This is an explicitly requested text-only recovery copy, not a repaired original. \
    Native image, audio and video attachments were omitted without inspection. \
    Earlier descriptions or summaries of those attachments are historical claims, not newly verified evidence. \
    Reattach the original media before relying on it. No historical tool was executed by recovery.";

impl Session {
    /// Create a NEW version-3 JSONL session from a strictly recovered transcript.
    ///
    /// This is an explicit lossy fork, never a fallback in `open` or `save`.
    /// Every original branch and session-wide record is retained, with the
    /// recovery reader's visible media omissions. The copy has a new session
    /// ID and a provenance/warning message on its initially selected branch.
    /// `selected_leaf = None` uses the saved selection; `Some(id)` chooses an
    /// exact source entry, and `Some("")` starts at the root. Other branches
    /// are not pruned. The returned string is the NEW session ID.
    ///
    /// An unfinished or mismatched tool-call batch on the selected path is
    /// refused rather than replayed or filled with fabricated results. Select
    /// an earlier completed point when recovering an interrupted tool turn.
    /// The other branches remain historical records, not execution authority.
    ///
    /// The destination must be a new `.jsonl` file in an existing directory,
    /// without existing attachment, V2 or lock sidecars. Source files and
    /// sidecars are never changed, and source attachments are never opened.
    /// Publication shares inspection export's private, synced, no-clobber
    /// staging. Directory durability and hostile-directory confinement are not
    /// guaranteed. Normal resume still enforces workspace, credentials and
    /// provider policy; this operation neither loads extensions nor calls a
    /// provider. History is private and is NOT secret-redacted.
    pub fn fork_recovered_text(
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        selected_leaf: Option<&str>,
    ) -> Result<String> {
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner)?;
        require_jsonl(destination.as_ref())?;
        let report = super::super::recover_session_text(source)?;
        let prepared = PreparedCopy::new(&report, selected_leaf, &owner)?;
        prepared.publish(&report, destination.as_ref(), &owner)?;
        Ok(prepared.session_id)
    }
}

fn require_jsonl(path: &Path) -> Result<()> {
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
    {
        return Err(invalid(
            "a recovered session destination must end in .jsonl",
        ));
    }
    Ok(())
}

fn require_unused_sidecars(path: &Path) -> Result<()> {
    require_jsonl(path)?;
    let mut sidecars = vec![
        crate::session::attachments::sidecar_path(path),
        crate::session_store_v2::v2_sidecar_path(path),
    ];
    for suffix in [".lock", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        sidecars.push(PathBuf::from(name));
    }
    for sidecar in sidecars {
        match std::fs::symlink_metadata(sidecar) {
            Ok(_) => {
                return Err(invalid(
                    "recovered session destination already has a sidecar; choose a new filename",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Preserve all roots, branches and global metadata, but publish parents before
/// children so native readers need not infer a new natural leaf from file order.
/// Original row position breaks ties; an explicit header selection is mandatory.
fn ordered_indices(report: &RecoveryReport, owner: &AgentCx) -> Result<Vec<usize>> {
    let positions: HashMap<_, _> = report
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id.as_str(), index))
        .collect();
    let mut children = vec![Vec::new(); report.entries.len()];
    let mut ready = BinaryHeap::new();
    for (index, entry) in report.entries.iter().enumerate() {
        check_owner(owner)?;
        if let Some(parent) = entry.parent_id.as_deref() {
            let parent = positions
                .get(parent)
                .ok_or_else(|| invalid("missing copy parent"))?;
            children[*parent].push(index);
        } else {
            ready.push(Reverse(index));
        }
    }
    let mut order = Vec::with_capacity(report.entries.len());
    while let Some(Reverse(index)) = ready.pop() {
        check_owner(owner)?;
        order.push(index);
        for child in &children[index] {
            ready.push(Reverse(*child));
        }
    }
    if order.len() != report.entries.len() {
        return Err(invalid("recovered copy graph cannot be ordered"));
    }
    Ok(order)
}

fn require_completed_tools(report: &RecoveryReport, path: &[usize], owner: &AgentCx) -> Result<()> {
    let mut pending = HashSet::new();
    for index in path {
        check_owner(owner)?;
        let record = &report.entries[*index].record;
        let Some(message) = record.get("message") else {
            if record["type"] == "compaction" && !pending.is_empty() {
                return Err(invalid(
                    "selected recovery path compacts an unfinished tool batch",
                ));
            }
            continue;
        };
        if message["role"] == "toolResult" {
            let id = message
                .get("toolCallId")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("selected recovery path has an invalid tool result"))?;
            if !pending.remove(id) {
                return Err(invalid(
                    "selected recovery path has an unmatched tool result",
                ));
            }
            continue;
        }
        if matches!(message["role"].as_str(), Some("user" | "assistant")) && !pending.is_empty() {
            return Err(invalid(
                "selected recovery path contains an unfinished tool batch; select an earlier entry",
            ));
        }
        if message["role"] == "assistant"
            && let Some(blocks) = message.get("content").and_then(Value::as_array)
        {
            for block in blocks {
                check_owner(owner)?;
                if block["type"] == "toolCall" {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            invalid("selected recovery path has an invalid tool call")
                        })?;
                    if !pending.insert(id) {
                        return Err(invalid(
                            "selected recovery path has duplicate tool-call IDs",
                        ));
                    }
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(invalid(
            "selected recovery leaf has unfinished tool calls; select an earlier entry",
        ));
    }
    Ok(())
}

struct PreparedCopy {
    session_id: String,
    header: Value,
    notice: Value,
    order: Vec<usize>,
}

impl PreparedCopy {
    fn new(report: &RecoveryReport, selected: Option<&str>, owner: &AgentCx) -> Result<Self> {
        check_owner(owner)?;
        let leaf = selected
            .or(report.selected_leaf.as_deref())
            .filter(|id| !id.is_empty());
        let path = graph(&report.entries, leaf, owner)?;
        require_completed_tools(report, &path, owner)?;
        let order = ordered_indices(report, owner)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let notice_id = format!("text-recovery-{}", uuid::Uuid::new_v4());
        if report.entries.iter().any(|entry| entry.id == notice_id) {
            return Err(invalid("recovery notice identity collision"));
        }
        let now = chrono::Utc::now();
        let timestamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut header = report.header.clone();
        header["id"] = json!(session_id);
        header["timestamp"] = json!(timestamp);
        header["currentLeaf"] = json!(notice_id);
        // A copied grandparent path would misidentify this fork's origin.
        // Use immutable source identity/digest provenance, not a borrowed path.
        header
            .as_object_mut()
            .ok_or_else(|| invalid("invalid copy header"))?
            .remove("parentSession");
        let notice = json!({
            "type":"message", "id":notice_id, "parentId":leaf, "timestamp":timestamp,
            "message":{
                "role":"custom", "customType":PROVENANCE, "content":NOTICE,
                "display":true, "timestamp":now.timestamp_millis(),
                "details":{
                    "schema":PROVENANCE, "sourceSessionId":report.header["id"],
                    "sourceSha256":report.source_sha256, "sourceBytes":report.source_bytes,
                    "sourceLeafId":leaf, "sourceEntryCount":report.entries.len(),
                    "omittedAttachments":report.omissions.len(), "allBranchesPreserved":true
                }
            }
        });
        // Check the exact native schema before creating any destination. The
        // inspection envelope itself is never written as a session transcript.
        let _ = SessionHeader::deserialize(&header)?;
        let _ = SessionEntry::deserialize(&notice)?;
        let prepared = Self {
            session_id,
            header,
            notice,
            order,
        };
        prepared.write(report, &mut RetainedBudget(MAX_EXPORT_BYTES), owner)?;
        Ok(prepared)
    }

    fn write(
        &self,
        report: &RecoveryReport,
        writer: &mut impl Write,
        owner: &AgentCx,
    ) -> Result<()> {
        let mut writer = CheckedWriter {
            inner: writer,
            owner,
            remaining: MAX_EXPORT_BYTES,
        };
        serde_json::to_writer(&mut writer, &self.header)?;
        writer.write_all(b"\n")?;
        for index in &self.order {
            check_owner(owner)?;
            serde_json::to_writer(&mut writer, &report.entries[*index].record)?;
            writer.write_all(b"\n")?;
        }
        serde_json::to_writer(&mut writer, &self.notice)?;
        writer.write_all(b"\n")?;
        Ok(())
    }

    fn publish(&self, report: &RecoveryReport, destination: &Path, owner: &AgentCx) -> Result<()> {
        publish_new_file(destination, owner, require_unused_sidecars, |file| {
            self.write(report, file, owner)
        })
    }
}

#[cfg(test)]
mod tests;
