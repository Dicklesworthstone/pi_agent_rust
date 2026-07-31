//! Bounded, secret-minimizing audit records for Devin tool calls.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
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

    pub fn push(&self, record: AuditRecord) {
        let mut records = self.records.lock().unwrap_or_else(|err| err.into_inner());
        if records.len() == self.capacity {
            records.pop_front();
        }
        records.push_back(record);
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<AuditRecord> {
        self.records
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    #[must_use]
    pub(crate) fn hash_arguments(&self, arguments: &Value) -> String {
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
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
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

    #[test]
    fn redacts_common_secret_tokens() {
        let redacted = redact_error("failed token=abc password=hunter2 Bearer abc sk-example");
        assert_eq!(
            redacted,
            "failed [REDACTED] [REDACTED] [REDACTED] [REDACTED] [REDACTED]"
        );
    }
}
