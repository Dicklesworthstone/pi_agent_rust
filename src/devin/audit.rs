//! Bounded, secret-minimizing audit records for Devin tool calls.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;

use super::policy::{PolicyAction, RiskClass};

/// Coarse effect recorded without retaining sensitive arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    Read,
    Write,
    Process,
    Network,
    SessionState,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    Pending,
    Allowed,
    Denied,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

impl AuditStatus {
    /// Whether this status closes the lifecycle of a call.
    ///
    /// `Allowed` is deliberately non-terminal: policy admits the call, and the
    /// executor is still expected to report the final outcome on the same
    /// record.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Denied | Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }
}

/// Stable audit shape. Raw tool arguments and credentials are deliberately
/// excluded; callers store a canonical hash and redacted error text instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub call_id: String,
    pub session_id: String,
    pub parent_agent: Option<String>,
    pub tool_name: String,
    pub argument_hash: String,
    pub effects: Vec<ToolEffect>,
    pub risk: RiskClass,
    pub policy_action: PolicyAction,
    pub approval_source: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: AuditStatus,
    pub artifact_refs: Vec<String>,
    pub redacted_error: Option<String>,
}

/// In-memory bounded audit buffer. A persistent sink can consume these records
/// without changing policy or frontend code.
///
/// # Hash comparability
///
/// `salt` is generated per `AuditLog` instance and never persisted. Argument
/// hashes are therefore only comparable *inside a single log*: the same
/// arguments produce the same hash within one session's log and a different
/// hash in any other log. This is deliberate. It lets an operator correlate
/// repeated calls within one session while making the hashes useless as
/// cross-session fingerprints of commands, paths, or credentials.
#[derive(Debug)]
pub struct AuditLog {
    capacity: usize,
    salt: [u8; 32],
    records: Mutex<VecDeque<AuditRecord>>,
}

impl AuditLog {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();
        let mut salt = [0_u8; 32];
        salt[..16].copy_from_slice(first.as_bytes());
        salt[16..].copy_from_slice(second.as_bytes());
        Self {
            capacity: capacity.max(1),
            salt,
            records: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
        }
    }

    /// Insert the record for a call id, replacing any record already held for
    /// that call id.
    ///
    /// Policy evaluation and execution both describe the *same* call, so a
    /// second evaluation of an in-flight call must never append a duplicate
    /// row. Use [`AuditLog::complete`] to close an existing record instead of
    /// pushing a second one.
    pub fn push(&self, record: AuditRecord) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = records
            .iter_mut()
            .find(|existing| existing.call_id == record.call_id)
        {
            // Never regress a closed record back to pending on re-evaluation.
            if existing.status.is_terminal() {
                return;
            }
            *existing = record;
            return;
        }
        if records.len() == self.capacity {
            records.pop_front();
        }
        records.push_back(record);
    }

    /// Record that policy admitted the call and note who approved it.
    ///
    /// Returns `false` when no record exists for `call_id` (for example after
    /// ring-buffer eviction), so callers can decide whether to re-open one.
    pub fn mark_allowed(&self, call_id: &str, approval_source: Option<&str>) -> bool {
        self.update(call_id, |record| {
            if !record.status.is_terminal() {
                record.status = AuditStatus::Allowed;
            }
            if let Some(source) = approval_source {
                record.approval_source = Some(source.to_string());
            }
        })
    }

    /// Close the existing record for `call_id` with a terminal status.
    ///
    /// Large output must be referenced through `artifact_refs` rather than
    /// copied into the record, and `redacted_error` must already have been
    /// passed through [`redact_error`].
    pub fn complete(
        &self,
        call_id: &str,
        status: AuditStatus,
        artifact_refs: &[String],
        redacted_error: Option<&str>,
    ) -> bool {
        let ended_at = Utc::now();
        self.update(call_id, |record| {
            record.status = status;
            record.ended_at = Some(ended_at);
            if !artifact_refs.is_empty() {
                record.artifact_refs = artifact_refs.to_vec();
            }
            if let Some(error) = redacted_error {
                record.redacted_error = Some(error.to_string());
            }
        })
    }

    /// Close the record for `call_id` only if the executor has not already
    /// reported a more specific terminal outcome.
    ///
    /// The agent loop uses this so a generic success/failure never overwrites a
    /// `TimedOut` or `Cancelled` status that the tool itself recorded, and so a
    /// denial already closed by policy is never reopened. `redacted_error` must
    /// already have been passed through [`redact_error`].
    pub fn complete_if_open(
        &self,
        call_id: &str,
        status: AuditStatus,
        redacted_error: Option<&str>,
    ) -> bool {
        self.update(call_id, |record| {
            if record.status.is_terminal() {
                return;
            }
            record.status = status;
            record.ended_at = Some(Utc::now());
            if let Some(error) = redacted_error {
                record.redacted_error = Some(error.to_string());
            }
        })
    }

    fn update(&self, call_id: &str, apply: impl FnOnce(&mut AuditRecord)) -> bool {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let updated = records
            .iter_mut()
            .find(|record| record.call_id == call_id)
            .is_some_and(|record| {
                apply(record);
                true
            });
        drop(records);
        updated
    }

    /// Fetch the current record for a call id.
    #[must_use]
    pub fn record_for(&self, call_id: &str) -> Option<AuditRecord> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|record| record.call_id == call_id)
            .cloned()
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<AuditRecord> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Salted, canonical hash of a call's arguments.
    ///
    /// See the type-level note: the result is comparable only against other
    /// hashes from this same `AuditLog`.
    #[must_use]
    pub fn hash_arguments(&self, arguments: &Value) -> String {
        argument_hash(arguments, &self.salt)
    }
}

/// Hash canonical JSON so equivalent object key order produces the same audit
/// identity while secret-bearing values never enter the record.
fn argument_hash(arguments: &Value, salt: &[u8; 32]) -> String {
    let canonical = canonical_json(arguments);
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(left, _)| *left);
            let body = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        Value::Array(values) => {
            let body = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{body}]")
        }
        _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
    }
}

/// Redact common credential shapes before an error reaches an audit sink.
#[must_use]
pub fn redact_error(message: &str) -> String {
    let mut redact_next = false;
    message
        .split_whitespace()
        .map(|token| {
            if redact_next {
                redact_next = false;
                return "[REDACTED]";
            }
            let lower = token.to_ascii_lowercase();
            if lower.contains("token=")
                || lower.contains("password=")
                || lower.contains("secret=")
                || lower.contains("api_key=")
                || lower.contains("apikey=")
                || lower.starts_with("authorization:")
                || lower.starts_with("sk-")
            {
                "[REDACTED]"
            } else if lower == "bearer" {
                redact_next = true;
                "[REDACTED]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn argument_hash_is_independent_of_object_key_order() {
        let audit = AuditLog::new(8);
        assert_eq!(
            audit.hash_arguments(&json!({"a": 1, "b": {"x": true, "y": false}})),
            audit.hash_arguments(&json!({"b": {"y": false, "x": true}, "a": 1}))
        );
    }

    #[test]
    fn separate_audit_logs_use_distinct_hash_salts() {
        let arguments = json!({"token": "low-entropy"});
        assert_ne!(
            AuditLog::new(8).hash_arguments(&arguments),
            AuditLog::new(8).hash_arguments(&arguments)
        );
    }

    fn pending_record(call_id: &str) -> AuditRecord {
        AuditRecord {
            call_id: call_id.to_string(),
            session_id: "session".to_string(),
            parent_agent: None,
            tool_name: "exec".to_string(),
            argument_hash: "hash".to_string(),
            effects: vec![ToolEffect::Process],
            risk: RiskClass::Critical,
            policy_action: PolicyAction::Ask,
            approval_source: None,
            started_at: Utc::now(),
            ended_at: None,
            status: AuditStatus::Pending,
            artifact_refs: Vec::new(),
            redacted_error: None,
        }
    }

    #[test]
    fn execution_updates_the_pending_record_instead_of_appending() {
        let audit = AuditLog::new(8);
        audit.push(pending_record("call-1"));
        assert!(audit.mark_allowed("call-1", Some("operator")));
        assert!(audit.complete(
            "call-1",
            AuditStatus::TimedOut,
            &["artifact://proc-1".to_string()],
            Some("timed out"),
        ));

        let records = audit.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, AuditStatus::TimedOut);
        assert_eq!(records[0].approval_source.as_deref(), Some("operator"));
        assert_eq!(records[0].artifact_refs, vec!["artifact://proc-1"]);
        assert!(records[0].ended_at.is_some());
    }

    #[test]
    fn re_evaluating_a_closed_call_does_not_reopen_or_duplicate_it() {
        let audit = AuditLog::new(8);
        audit.push(pending_record("call-1"));
        assert!(audit.complete("call-1", AuditStatus::Succeeded, &[], None));
        audit.push(pending_record("call-1"));

        let records = audit.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, AuditStatus::Succeeded);
    }

    #[test]
    fn completing_an_unknown_call_is_reported_instead_of_appending() {
        let audit = AuditLog::new(8);
        assert!(!audit.complete("missing", AuditStatus::Failed, &[], None));
        assert!(audit.snapshot().is_empty());
    }

    #[test]
    fn redacts_common_secret_tokens() {
        let redacted = redact_error("failed token=abc password=hunter2 Bearer abc sk-example");
        assert_eq!(
            redacted,
            "failed [REDACTED] [REDACTED] [REDACTED] [REDACTED] [REDACTED]"
        );
    }
}
