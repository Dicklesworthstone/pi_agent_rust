//! MCP 2025-11-25 task negotiation and bounded response validation.
//!
//! These types describe one admitted call's remote task. They do not grant
//! trust, start requests, retain transports, or extend the caller's deadline.
//! Protocol: https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks

use std::time::Duration;

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};

const TASK_PROTOCOL_VERSION: &str = "2025-11-25";
const MAX_TASK_ID_BYTES: usize = 1024;
const MAX_TIMESTAMP_BYTES: usize = 64;
const MAX_STATUS_MESSAGE_BYTES: usize = 4096;
const RELATED_TASK_KEY: &str = "io.modelcontextprotocol/related-task";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn invalid(reason: &str) -> Error {
    Error::tool("mcp", format!("[MCP_PROTOCOL] {reason}"))
}

/// Tool-level task support supplements, but never overrides, negotiation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskSupport {
    /// Ordinary calls only, including tools that omit execution metadata.
    #[default]
    Forbidden,
    /// The caller may choose ordinary or task-augmented execution.
    Optional,
    /// The tool must only be invoked with task augmentation.
    Required,
}

impl TaskSupport {
    /// Read the execution metadata of one `tools/list` entry.
    ///
    /// # Errors
    /// Returns a protocol error for malformed known execution fields.
    pub fn from_tool(tool: &Value) -> Result<Self> {
        let tool = tool
            .as_object()
            .ok_or_else(|| invalid("tool definition must be an object"))?;
        let Some(execution) = optional_object(tool, "execution")? else {
            return Ok(Self::Forbidden);
        };
        match execution.get("taskSupport") {
            None => Ok(Self::Forbidden),
            Some(Value::String(value)) => match value.as_str() {
                "forbidden" => Ok(Self::Forbidden),
                "optional" => Ok(Self::Optional),
                "required" => Ok(Self::Required),
                _ => Err(invalid("tool execution.taskSupport is not a known value")),
            },
            Some(_) => Err(invalid("tool execution.taskSupport must be a string")),
        }
    }
}

/// Server operations explicitly negotiated on this transport generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskCapabilities {
    /// The server accepts task-augmented `tools/call` requests.
    pub tool_call: bool,
    /// The server supports `tasks/cancel`.
    pub cancel: bool,
}

impl TaskCapabilities {
    /// Extract task capabilities from an already admitted initialize result.
    /// Older protocol revisions cannot enable this experimental extension.
    /// Unknown extension fields remain inert; known fields must be objects.
    ///
    /// # Errors
    /// Returns a protocol error for malformed initialization or task metadata.
    pub fn from_initialize(result: &Value) -> Result<Self> {
        let result = result
            .as_object()
            .ok_or_else(|| invalid("initialize result must be an object"))?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("initialize result requires a protocolVersion"))?;
        let capabilities = result
            .get("capabilities")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("initialize capabilities must be an object"))?;
        if version != TASK_PROTOCOL_VERSION {
            return Ok(Self::default());
        }
        let Some(tasks) = optional_object(capabilities, "tasks")? else {
            return Ok(Self::default());
        };
        // Validate list even though this call-scoped client does not list tasks.
        let _ = optional_object(tasks, "list")?;
        let cancel = optional_object(tasks, "cancel")?.is_some();
        let tool_call = match optional_object(tasks, "requests")? {
            Some(requests) => match optional_object(requests, "tools")? {
                Some(tools) => optional_object(tools, "call")?.is_some(),
                None => false,
            },
            None => false,
        };
        Ok(Self { tool_call, cancel })
    }
}

fn optional_object<'a>(
    parent: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a Map<String, Value>>> {
    parent
        .get(key)
        .map(|value| {
            value
                .as_object()
                .ok_or_else(|| invalid("known task capability or execution field must be an object"))
        })
        .transpose()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TaskStatus {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::InputRequired => "input_required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(super) const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    fn parse(value: Option<&Value>) -> Result<Self> {
        match value.and_then(Value::as_str) {
            Some("working") => Ok(Self::Working),
            Some("input_required") => Ok(Self::InputRequired),
            Some("completed") => Ok(Self::Completed),
            Some("failed") => Ok(Self::Failed),
            Some("cancelled") => Ok(Self::Cancelled),
            _ => Err(invalid("task status is missing or invalid")),
        }
    }
}

/// Validated state for a single task returned by the admitted server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Task {
    pub id: String,
    pub status: TaskStatus,
    /// Escaped and bounded at admission; never raw terminal control text.
    pub status_message: Option<String>,
    pub status_message_truncated: bool,
    pub created_at: DateTime<FixedOffset>,
    pub last_updated_at: DateTime<FixedOffset>,
    pub ttl: Option<Duration>,
    pub poll_interval: Duration,
}

impl Task {
    /// Capture the cleanup identity before admitting the rest of the response.
    /// A malformed status or timestamp must not strand an otherwise named task.
    pub(super) fn created_id(result: &Value) -> Result<String> {
        let task = result
            .get("task")
            .filter(|value| value.is_object())
            .ok_or_else(|| invalid("task creation result requires a task object"))?;
        task_id(task.get("taskId")).map(str::to_owned)
    }

    pub(super) fn from_create_result(result: &Value) -> Result<Self> {
        let task = result
            .get("task")
            .ok_or_else(|| invalid("task creation result requires a task object"))?;
        let task = Self::parse(task)?;
        // Tasks begin working, but the receipt can describe a later state if
        // execution advances before the server sends its creation response.
        validate_related_task(result, &task.id, false)?;
        Ok(task)
    }

    /// Admit only state belonging to the same task and its creation instant.
    /// Polling may skip states, but cannot reverse time or leave a terminal one.
    pub(super) fn from_status_result(result: &Value, previous: &Self) -> Result<Self> {
        let next = Self::parse(result)?;
        if next.id != previous.id || next.created_at != previous.created_at {
            return Err(invalid("task response does not match the admitted task"));
        }
        if next.last_updated_at < previous.last_updated_at {
            return Err(invalid("task update timestamp moved backwards"));
        }
        if previous.status.is_terminal() && next.status != previous.status {
            return Err(invalid("task response changed a terminal status"));
        }
        validate_related_task(result, &previous.id, false)?;
        Ok(next)
    }

    /// `tasks/result` must return the original result with its task identity.
    /// Output content and schema validation remain with the ordinary call path.
    pub(super) fn validate_result(&self, result: &Value) -> Result<()> {
        validate_related_task(result, &self.id, true)
    }

    pub(super) fn progress_text(&self) -> String {
        let mut text = format!("MCP task: {}", self.status.as_str());
        if let Some(message) = &self.status_message {
            text.push_str(" — \"");
            text.push_str(message);
            text.push('"');
        }
        if self.status_message_truncated {
            text.push_str(" [message truncated]");
        }
        text
    }

    fn parse(value: &Value) -> Result<Self> {
        let task = value
            .as_object()
            .ok_or_else(|| invalid("task response must be an object"))?;
        let id = task_id(task.get("taskId"))?.to_string();
        let status = TaskStatus::parse(task.get("status"))?;
        let created_at = timestamp(task.get("createdAt"))?;
        let last_updated_at = timestamp(task.get("lastUpdatedAt"))?;
        if last_updated_at < created_at {
            return Err(invalid("task update timestamp precedes creation"));
        }
        let ttl = match task.get("ttl") {
            Some(Value::Null) => None,
            Some(value) => Some(milliseconds(value)?),
            None => return Err(invalid("task response requires a retention duration")),
        };
        let poll_interval = task
            .get("pollInterval")
            .map(milliseconds)
            .transpose()?
            .unwrap_or(DEFAULT_POLL_INTERVAL)
            .max(MIN_POLL_INTERVAL);
        let (status_message, status_message_truncated) = match task.get("statusMessage") {
            None => (None, false),
            Some(Value::String(message)) => {
                let (message, truncated) = display_message(message);
                (Some(message), truncated)
            }
            Some(_) => return Err(invalid("task statusMessage must be a string")),
        };
        Ok(Self {
            id,
            status,
            status_message,
            status_message_truncated,
            created_at,
            last_updated_at,
            ttl,
            poll_interval,
        })
    }
}

fn task_id(value: Option<&Value>) -> Result<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_TASK_ID_BYTES
                && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| invalid("task identifier is missing, oversized, or invalid"))
}

fn timestamp(value: Option<&Value>) -> Result<DateTime<FixedOffset>> {
    let text = value
        .and_then(Value::as_str)
        .filter(|value| value.len() <= MAX_TIMESTAMP_BYTES)
        .ok_or_else(|| invalid("task timestamp is missing or invalid"))?;
    DateTime::parse_from_rfc3339(text).map_err(|_| invalid("task timestamp is not ISO 8601"))
}

#[allow(clippy::float_cmp)] // A wire integer must have exactly zero fractional part.
fn milliseconds(value: &Value) -> Result<Duration> {
    value
        .as_u64()
        .or_else(|| {
            // JSON Schema integer also permits 60000.0 and 6e4. Preserve the
            // exact u64 path above; check integral float values without a
            // saturating cast that could admit values beyond u64::MAX.
            let number = value.as_f64()?;
            if !number.is_finite() || number < 0.0 || number.fract() != 0.0 {
                return None;
            }
            format!("{number:.0}").parse::<u64>().ok()
        })
        .map(Duration::from_millis)
        .ok_or_else(|| invalid("task duration must be a nonnegative integer in milliseconds"))
}

fn display_message(message: &str) -> (String, bool) {
    let mut escaped = String::new();
    for character in message.chars() {
        let representation = character.escape_default();
        if representation.clone().count() > MAX_STATUS_MESSAGE_BYTES - escaped.len() {
            return (escaped, true);
        }
        escaped.extend(representation);
    }
    (escaped, false)
}

fn validate_related_task(result: &Value, expected: &str, required: bool) -> Result<()> {
    let result = result
        .as_object()
        .ok_or_else(|| invalid("task result must be an object"))?;
    let related =
        optional_object(result, "_meta")?.and_then(|metadata| metadata.get(RELATED_TASK_KEY));
    match related {
        None if !required => Ok(()),
        None => Err(invalid("task result is missing related-task metadata")),
        Some(value) => {
            let related = value
                .as_object()
                .ok_or_else(|| invalid("related-task metadata must be an object"))?;
            if task_id(related.get("taskId"))? != expected {
                return Err(invalid("task result belongs to a different task"));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integral_duration_representations_are_accepted_without_fraction_or_overflow() {
        for encoded in ["60000", "60000.0", "6e4"] {
            let value: Value = serde_json::from_str(encoded).unwrap();
            assert_eq!(milliseconds(&value).unwrap(), Duration::from_secs(60));
        }
        assert_eq!(
            milliseconds(&json!(u64::MAX)).unwrap(),
            Duration::from_millis(u64::MAX)
        );
        for encoded in ["250.5", "-1.0", "18446744073709551616.0", "1e100", "null"] {
            let value: Value = serde_json::from_str(encoded).unwrap();
            assert!(milliseconds(&value).is_err(), "invalid duration {encoded}");
        }
    }

    fn initialize(tasks: Value) -> Value {
        json!({"protocolVersion":"2025-11-25", "capabilities":{"tasks":tasks}})
    }

    fn task(status: &str) -> Value {
        json!({
            "taskId":"task-1", "status":status,
            "createdAt":"2025-11-25T10:30:00Z",
            "lastUpdatedAt":"2025-11-25T10:31:00Z", "ttl":60_000,
        })
    }

    fn created() -> Task {
        Task::from_create_result(&json!({"task":task("working")})).expect("working task")
    }

    #[test]
    fn tool_support_is_explicit_and_malformed_metadata_is_not_optional() {
        for (metadata, expected) in [
            (json!({}), TaskSupport::Forbidden),
            (json!({"execution":{}}), TaskSupport::Forbidden),
            (
                json!({"execution":{"taskSupport":"forbidden"}}),
                TaskSupport::Forbidden,
            ),
            (
                json!({"execution":{"taskSupport":"optional"}}),
                TaskSupport::Optional,
            ),
            (
                json!({"execution":{"taskSupport":"required"}}),
                TaskSupport::Required,
            ),
            (
                json!({"annotations":{"taskSupport":"required"}}),
                TaskSupport::Forbidden,
            ),
        ] {
            assert_eq!(TaskSupport::from_tool(&metadata).unwrap(), expected);
        }
        for metadata in [
            Value::Null,
            json!({"execution":null}),
            json!({"execution":true}),
            json!({"execution":{"taskSupport":null}}),
            json!({"execution":{"taskSupport":true}}),
            json!({"execution":{"taskSupport":"REQUIRED"}}),
            json!({"execution":{"taskSupport":"future-mode"}}),
        ] {
            assert!(TaskSupport::from_tool(&metadata).is_err());
        }
    }

    #[test]
    fn task_capabilities_require_the_pinned_revision_and_exact_nested_grants() {
        let all = initialize(json!({"list":{}, "cancel":{}, "requests":{"tools":{"call":{}}}}));
        assert_eq!(
            TaskCapabilities::from_initialize(&all).unwrap(),
            TaskCapabilities {
                tool_call: true,
                cancel: true,
            }
        );
        for version in ["2024-11-05", "2025-03-26", "2025-06-18"] {
            let mut legacy = all.clone();
            legacy["protocolVersion"] = json!(version);
            assert_eq!(
                TaskCapabilities::from_initialize(&legacy).unwrap(),
                TaskCapabilities::default()
            );
        }
        for tasks in [
            json!({}),
            json!({"requests":{}}),
            json!({"requests":{"tools":{}}}),
            json!({"requests":{"sampling":{"createMessage":{}}}}),
        ] {
            assert!(
                !TaskCapabilities::from_initialize(&initialize(tasks))
                    .unwrap()
                    .tool_call
            );
        }
        assert!(
            !TaskCapabilities::from_initialize(&initialize(json!({"list":{}})))
                .unwrap()
                .cancel
        );
        assert_eq!(
            TaskCapabilities::from_initialize(
                &json!({"protocolVersion":"2025-11-25","capabilities":{}})
            )
            .unwrap(),
            TaskCapabilities::default()
        );
    }

    #[test]
    fn malformed_known_task_capability_shapes_fail_closed() {
        for tasks in [
            Value::Null,
            json!(true),
            json!({"list":true}),
            json!({"cancel":false}),
            json!({"requests":null}),
            json!({"requests":{"tools":[]}}),
            json!({"requests":{"tools":{"call":true}}}),
        ] {
            assert!(TaskCapabilities::from_initialize(&initialize(tasks)).is_err());
        }
        assert!(TaskCapabilities::from_initialize(&json!({})).is_err());
        assert!(
            TaskCapabilities::from_initialize(
                &json!({"protocolVersion":"2025-11-25","capabilities":null})
            )
            .is_err()
        );
    }

    #[test]
    fn creation_retains_cleanup_identity_even_when_other_fields_are_invalid() {
        let invalid_task = json!({"task":{"taskId":"accepted-task", "status":"completed"}});
        assert_eq!(Task::created_id(&invalid_task).unwrap(), "accepted-task");
        assert!(Task::from_create_result(&invalid_task).is_err());
        for id in [
            String::new(),
            "x".repeat(1025),
            "é".repeat(513),
            "task\nnext".into(),
            "\u{1b}task".into(),
        ] {
            assert!(Task::created_id(&json!({"task":{"taskId":id}})).is_err());
        }
        assert_eq!(
            Task::created_id(&json!({"task":{"taskId":"é".repeat(512)}}))
                .unwrap()
                .len(),
            1024
        );
    }

    #[test]
    fn creation_receipts_can_observe_fast_terminal_or_input_required_states() {
        for status in ["working", "input_required", "completed", "failed", "cancelled"] {
            let receipt = json!({"task":task(status)});
            let parsed = Task::from_create_result(&receipt).unwrap();
            assert_eq!(parsed.status.as_str(), status);
            assert_eq!(parsed.id, Task::created_id(&receipt).unwrap());
        }
    }

    #[test]
    fn task_timestamps_and_retention_are_validated_before_state_is_accepted() {
        for (field, invalid_value) in [
            ("createdAt", json!("not-a-time")),
            ("createdAt", json!("x".repeat(65))),
            ("lastUpdatedAt", json!("2025-11-25T10:29:59Z")),
            ("ttl", json!(-1)),
            ("ttl", json!(1.5)),
            ("ttl", json!("60000")),
            ("pollInterval", Value::Null),
            ("pollInterval", json!(-1)),
            ("statusMessage", Value::Null),
            ("status", json!("unknown")),
        ] {
            let mut value = task("working");
            value[field] = invalid_value;
            assert!(
                Task::from_create_result(&json!({"task":value})).is_err(),
                "{field}"
            );
        }
        let mut missing = task("working");
        missing.as_object_mut().unwrap().remove("ttl");
        assert!(Task::from_create_result(&json!({"task":missing})).is_err());
        let mut unlimited = task("working");
        unlimited["ttl"] = Value::Null;
        assert!(
            Task::from_create_result(&json!({"task":unlimited}))
                .unwrap()
                .ttl
                .is_none()
        );
    }

    #[test]
    fn polling_defaults_floor_and_large_server_intervals_preserve_the_contract() {
        assert_eq!(created().poll_interval, Duration::from_secs(1));
        for (millis, expected) in [
            (0, 100),
            (99, 100),
            (101, 101),
            (60_000, 60_000),
            (u64::MAX, u64::MAX),
        ] {
            let mut value = task("working");
            value["pollInterval"] = json!(millis);
            let parsed = Task::from_create_result(&json!({"task":value})).unwrap();
            assert_eq!(parsed.poll_interval, Duration::from_millis(expected));
        }
    }

    #[test]
    fn status_updates_bind_identity_creation_and_monotonic_time() {
        let previous = created();
        for (field, replacement) in [
            ("taskId", "another-task"),
            ("createdAt", "2025-11-25T10:29:00Z"),
            ("lastUpdatedAt", "2025-11-25T10:30:59Z"),
        ] {
            let mut next = task("completed");
            next[field] = json!(replacement);
            assert!(Task::from_status_result(&next, &previous).is_err());
        }
        let mut timezone = task("input_required");
        timezone["createdAt"] = json!("2025-11-25T11:30:00+01:00");
        assert!(
            Task::from_status_result(&timezone, &previous).is_ok(),
            "same creation instant"
        );
    }

    #[test]
    fn polling_may_skip_working_states_but_terminal_status_never_changes() {
        let initial = created();
        let input = Task::from_status_result(&task("input_required"), &initial).unwrap();
        assert_eq!(
            Task::from_status_result(&task("working"), &input)
                .unwrap()
                .status,
            TaskStatus::Working
        );
        for status in ["completed", "failed", "cancelled"] {
            for previous in [&initial, &input] {
                let terminal = Task::from_status_result(&task(status), previous).unwrap();
                assert!(terminal.status.is_terminal());
                assert!(Task::from_status_result(&task(status), &terminal).is_ok());
                for other in ["working", "input_required", "completed", "failed", "cancelled"] {
                    if other != status {
                        assert!(Task::from_status_result(&task(other), &terminal).is_err());
                    }
                }
            }
        }
    }

    #[test]
    fn task_results_require_the_exact_related_task_without_inspecting_content() {
        let initial = created();
        let valid = json!({
            "content":[],
            "_meta":{RELATED_TASK_KEY:{"taskId":"task-1"}, "other":true},
        });
        assert!(initial.validate_result(&valid).is_ok());
        for result in [
            json!({"content":[]}),
            json!({"task":{"taskId":"task-1"}}),
            json!({"_meta":null}),
            json!({"_meta":{RELATED_TASK_KEY:null}}),
            json!({"_meta":{RELATED_TASK_KEY:{"taskId":"task-2"}}}),
            json!({"_meta":{RELATED_TASK_KEY:{"taskId":1}}}),
        ] {
            assert!(initial.validate_result(&result).is_err());
        }
        let mut status = task("working");
        status["_meta"] = json!({RELATED_TASK_KEY:{"taskId":"wrong"}});
        assert!(Task::from_status_result(&status, &initial).is_err());
    }

    #[test]
    fn status_messages_cannot_inject_terminal_sequences_or_unbounded_updates() {
        let mut value = task("working");
        value["statusMessage"] = json!("\u{1b}[2J\r\n\u{202e}\"done\"");
        let parsed = Task::from_create_result(&json!({"task":value})).unwrap();
        assert_eq!(
            parsed.progress_text(),
            "MCP task: working — \"\\u{1b}[2J\\r\\n\\u{202e}\\\"done\\\"\""
        );
        assert!(!parsed.status_message_truncated);
        let mut value = task("working");
        value["statusMessage"] = json!("\u{202e}".repeat(1000));
        let parsed = Task::from_create_result(&json!({"task":value})).unwrap();
        let message = parsed.status_message.as_ref().unwrap();
        assert!(message.len() <= MAX_STATUS_MESSAGE_BYTES);
        assert!(message.ends_with('}'), "no partial escape");
        assert!(parsed.status_message_truncated);
        assert!(parsed.progress_text().ends_with("[message truncated]"));
        assert!(!parsed.progress_text().chars().any(char::is_control));
    }
}
