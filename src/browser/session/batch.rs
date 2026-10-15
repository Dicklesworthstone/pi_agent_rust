//! Literal, fail-fast browser workflows under one conversation lease and budget.
//!
//! Each step uses the ordinary backend, including its request-owned connection,
//! validation, domain policy, dialogs and file guards. No nested Tool::execute:
//! re-entering the session lease would deadlock or permit ownership substitution.

use std::io::Write;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{BrowserTool, Budget, Namespace, Sessions, error};
use crate::error::Result;
use crate::model::{ContentBlock, TextContent};
use crate::tools::ToolOutput;

const MAX_STEPS: usize = 32;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const DEFAULT_RESULT_BYTES: usize = 256 * 1024;
const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXPECT_BYTES: usize = 16 * 1024;
const ACTIONS: &[&str] = &[
    "open", "goto", "close", "list_tabs", "list_frames", "snapshot", "ax_tree",
    "evaluate", "click", "hover", "type", "fill", "press", "scroll", "wait_for",
    "select_option", "set_checked", "upload", "download", "screenshot", "print_pdf",
    "handle_dialog", "diagnostics",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    call: Value,
    // A supplied null is malformed, not an absent assertion. The custom
    // deserializer preserves equals:null as a genuine expected JSON value.
    #[serde(default, deserialize_with = "deserialize_expectation")]
    expect: Option<Expectation>,
}

#[derive(Debug)]
enum Condition {
    Equals(Value),
    Exists(bool),
}

#[derive(Debug)]
struct Expectation {
    pointer: String,
    condition: Condition,
}

fn deserialize_expectation<'de, D>(deserializer: D) -> std::result::Result<Option<Expectation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Expectation::parse(&value).map(Some).map_err(serde::de::Error::custom)
}

impl Expectation {
    fn parse(value: &Value) -> Result<Self> {
        let invalid = || error("BROWSER_BATCH_EXPECT_INVALID: expect requires a bounded JSON pointer and exactly one equals value or exists boolean");
        if encoded_size(value, MAX_EXPECT_BYTES).is_none() {
            return Err(invalid());
        }
        let fields = value.as_object().filter(|fields| fields.len() == 2).ok_or_else(invalid)?;
        let pointer = fields.get("pointer").and_then(Value::as_str).ok_or_else(invalid)?;
        if pointer.len() > 1024 || pointer.chars().any(char::is_control)
            || (!pointer.is_empty() && !pointer.starts_with('/'))
            || pointer.bytes().filter(|byte| *byte == b'/').count() > 32
        {
            return Err(invalid());
        }
        let mut bytes = pointer.bytes();
        while let Some(byte) = bytes.next() {
            if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
                return Err(invalid());
            }
        }
        let condition = if let Some(expected) = fields.get("equals") {
            Condition::Equals(expected.clone())
        } else {
            Condition::Exists(fields.get("exists").and_then(Value::as_bool).ok_or_else(invalid)?)
        };
        Ok(Self { pointer: pointer.to_owned(), condition })
    }

    /// Inspect only the native structured details already returned by this
    /// step. Never evaluate code, read getters, coerce types or dispatch work.
    fn matches(&self, details: Option<&Value>) -> bool {
        let actual = details.and_then(|details| details.pointer(&self.pointer));
        match &self.condition {
            Condition::Equals(expected) => actual == Some(expected),
            Condition::Exists(expected) => actual.is_some() == *expected,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    action: String,
    steps: Vec<Step>,
    timeout_ms: Option<u64>,
    max_result_bytes: Option<usize>,
}

pub(super) struct Plan {
    steps: Vec<Step>,
    result_bytes: usize,
}

impl Plan {
    /// Validate the complete envelope and every step's dispatch shape before
    /// identity lookup, namespace reservation or connection. Action-specific
    /// backend preflight still runs just before that action, not speculatively.
    pub(super) fn parse(args: &Value) -> Result<Option<Self>> {
        if args["action"] != "batch" {
            if args.get("steps").is_some() || args.get("max_result_bytes").is_some() || args.get("expect").is_some() {
                return Err(error("BROWSER_BATCH_INVALID: steps and max_result_bytes require action batch; expect belongs on a batch step"));
            }
            return Ok(None);
        }
        if encoded_size(args, MAX_INPUT_BYTES).is_none() {
            return Err(error("BROWSER_BATCH_INPUT_LIMIT: batch input exceeds 256 KiB"));
        }
        if args.get("max_result_bytes").is_some_and(|value| value.as_u64().is_none()) {
            return Err(error("BROWSER_BATCH_INVALID: max_result_bytes must be an integer"));
        }
        let input: Input = serde_json::from_value(args.clone())
            .map_err(|_| error("BROWSER_BATCH_INVALID: expected steps containing a call object and optional valid expect, and batch timeout/result limits"))?;
        if input.action != "batch" || input.steps.is_empty() || input.steps.len() > MAX_STEPS {
            return Err(error("BROWSER_BATCH_INVALID: batch requires 1 to 32 steps"));
        }
        if input.timeout_ms.is_some_and(|value| !(1..=120_000).contains(&value)) {
            return Err(error("BROWSER_BATCH_INVALID: timeout_ms must be an integer in 1..=120000"));
        }
        let result_bytes = input.max_result_bytes.unwrap_or(DEFAULT_RESULT_BYTES);
        if !(1024..=MAX_RESULT_BYTES).contains(&result_bytes) {
            return Err(error("BROWSER_BATCH_INVALID: max_result_bytes must be an integer in 1024..=4194304"));
        }
        for step in &input.steps {
            validate_call(&step.call)?;
        }
        Ok(Some(Self { steps: input.steps, result_bytes }))
    }

    pub(super) async fn execute(
        self,
        sessions: &Sessions,
        expected: &str,
        budget: &Budget,
        tool: &BrowserTool,
        namespace: &Namespace,
    ) -> Result<ToolOutput> {
        let total = self.steps.len();
        let mut report = Report::new(self.result_bytes);
        for (index, step) in self.steps.into_iter().enumerate() {
            // A ready preceding call can finish without yielding. Check at
            // this boundary as well as during each individual pending call.
            sessions.require_identity(expected, budget).await?;
            let call = timed_call(step.call, budget)?;
            let action = call["action"].as_str().ok_or_else(|| {
                error("BROWSER_BATCH_INVALID: missing action after preflight")
            })?;
            let result = sessions.run_owned(
                expected, budget, tool.execute_in_namespace(namespace, &call),
            ).await?;
            // Ownership/cancellation failures must not become a publishable
            // partial report for a different conversation. Keep them as Err.
            sessions.require_identity(expected, budget).await?;
            if !report.record(index, action, result, step.expect.as_ref()) {
                break;
            }
        }
        budget.checkpoint()?;
        Ok(report.finish(total))
    }
}

fn validate_call(call: &Value) -> Result<()> {
    let fields = call.as_object().ok_or_else(|| error("BROWSER_BATCH_INVALID: each call must be an object"))?;
    if !call["action"].as_str().is_some_and(|action| ACTIONS.contains(&action))
        || fields.contains_key("steps") || fields.contains_key("max_result_bytes") || fields.contains_key("expect")
    {
        return Err(error("BROWSER_BATCH_INVALID: unsupported step action or misplaced batch fields; nested batches and lifecycle/release actions are forbidden"));
    }
    if let Some(timeout) = call.get("timeout_ms")
        && timeout.as_u64().is_none_or(|value| !(1..=120_000).contains(&value))
    {
        return Err(error("BROWSER_BATCH_INVALID: step timeout_ms must be an integer in 1..=120000"));
    }
    Ok(())
}

fn timed_call(mut call: Value, budget: &Budget) -> Result<Value> {
    budget.checkpoint()?;
    let remaining = u64::try_from(budget.deadline.saturating_duration_since(std::time::Instant::now()).as_millis())
        .unwrap_or(u64::MAX);
    if remaining == 0 {
        return Err(error("BROWSER_SESSION_TIMEOUT: insufficient batch budget to start another step; admitted effects are not rolled back"));
    }
    let default = if call["action"] == "wait_for" { 5000 } else { 30_000 };
    let timeout = call["timeout_ms"].as_u64().unwrap_or(default);
    call["timeout_ms"] = json!(timeout.min(remaining));
    Ok(call)
}

/// Count the encoded bytes without allocating a second copy of large image
/// blocks or console records. This limit governs retained original outputs;
/// separately bounded per-step metadata and headings are additional overhead.
fn encoded_size(value: &impl serde::Serialize, limit: usize) -> Option<usize> {
    struct Counter { used: usize, limit: usize }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.used) {
                return Err(std::io::Error::other("browser batch byte limit"));
            }
            self.used += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut counter = Counter { used: 0, limit };
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.used)
}

fn text(value: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent { text: value.into(), text_signature: None })
}

struct Report {
    content: Vec<ContentBlock>,
    steps: Vec<Value>,
    remaining_bytes: usize,
    completed: usize,
    stop_reason: Option<&'static str>,
}

impl Report {
    fn new(result_bytes: usize) -> Self {
        Self {
            content: vec![text("")], steps: Vec::new(), remaining_bytes: result_bytes,
            completed: 0, stop_reason: None,
        }
    }

    fn record(&mut self, index: usize, action: &str, result: Result<ToolOutput>, expect: Option<&Expectation>) -> bool {
        let result = result.unwrap_or_else(|failure| ToolOutput {
            content: vec![text(failure.to_string())], details: None, is_error: true,
        });
        let Some(bytes) = encoded_size(&result, self.remaining_bytes) else {
            self.stop_reason = Some("result_limit");
            self.steps.push(json!({"index":index, "action":action, "outcome":"result_limit",
                "tool_is_error":result.is_error, "result_omitted":true,
                "expectation":expect.map(|_| "not_checked")}));
            self.content.push(text(format!(
                "Step {index} ({action}) returned, but its output exceeded the remaining batch result budget and was omitted. Its effects may already have occurred; no later step ran."
            )));
            return false;
        };
        self.remaining_bytes -= bytes;
        let expectation = expect.map(|expect| {
            if result.is_error { "not_checked" }
            else if expect.matches(result.details.as_ref()) { "passed" }
            else { "failed" }
        });
        let expectation_failed = expectation == Some("failed");
        self.content.push(text(format!("Step {index} ({action}):")));
        let start = self.content.len();
        self.content.extend(result.content);
        self.steps.push(json!({"index":index, "action":action,
            "outcome":if result.is_error { "tool_error" } else if expectation_failed { "expectation_failed" } else { "succeeded" },
            "tool_is_error":result.is_error, "expectation":expectation,
            "details":result.details, "content_range":[start, self.content.len()]}));
        if result.is_error {
            self.stop_reason = Some("tool_error");
            false
        } else if expectation_failed {
            self.stop_reason = Some("expectation_failed");
            self.content.push(text(format!(
                "BROWSER_BATCH_EXPECT_FAILED: step {index} ({action}) returned successfully but its required result condition did not match. No later step ran."
            )));
            false
        } else {
            self.completed += 1;
            true
        }
    }

    fn finish(mut self, total: usize) -> ToolOutput {
        let failed = self.stop_reason.is_some();
        self.content[0] = text(format!(
            "Browser batch: {}/{} steps returned success with retained results and satisfied any explicit expectations. {} Native acknowledgements alone do not verify application outcomes. Completed effects are not rolled back; do not blindly replay this batch.",
            self.completed, total,
            if failed { "Stopped at the first failure; later steps were not attempted." } else { "All steps finished." },
        ));
        ToolOutput {
            content: self.content,
            details: Some(json!({"schema":"pi.browser.batch.v1", "action":"batch",
                "total_steps":total, "completed_steps":self.completed,
                "attempted_steps":self.steps.len(), "remaining_steps":total - self.steps.len(),
                "failed_step":if failed { self.steps.len().checked_sub(1) } else { None },
                "stop_reason":self.stop_reason, "steps":self.steps})),
            is_error: failed,
        }
    }
}

pub(in crate::browser) fn extend_schema(schema: &mut Value) {
    if let Some(actions) = schema["properties"]["action"]["enum"].as_array_mut() {
        actions.push(json!("batch"));
    }
    schema["properties"]["steps"] = json!({
        "type":"array", "minItems":1, "maxItems":MAX_STEPS,
        "description":"batch only: literal ordered native browser calls under one conversation lease and total timeout_ms. Each step is {call:{action:..., ...ordinary arguments}, expect?:{pointer:..., equals:... or exists:...}}. Stops on the first tool error, unmet expectation or result limit; retains earlier results without rolling back or replaying effects. Native backend only. Each action keeps its normal connection, preflight, permissions, domain/frame/dialog policy and optional shorter timeout. Batch envelope/dispatch shape and expectations are validated upfront; action-specific validation runs at its step. No lifecycle/release, nesting, parallelism, retries or implicit argument substitution. Combined input is limited to 256 KiB.",
        "items":{"type":"object", "additionalProperties":false, "required":["call"],
            "properties":{"call":{"type":"object", "required":["action"],
                "properties":{"action":{"type":"string", "enum":ACTIONS}}}}}
    });
    schema["properties"]["steps"]["items"]["properties"]["expect"] = json!({
        "type":"object", "additionalProperties":false, "required":["pointer"],
        "description":"Optional host-side postcondition on this successful step's native details, before the next step. JSON pointer only, not code or a URL: empty selects all details, /result selects evaluate's value, ~1 escapes slash and ~0 escapes tilde. Exactly one equals (type-sensitive JSON equality including null) or exists (boolean). Missing differs from null. Unmet conditions stop the batch as an error without retries, waiting, or extra browser calls. Not evaluated after tool errors or omitted outputs. At most 16 KiB encoded, 1024 pointer bytes and 32 segments; expectation diagnostics do not echo expected values.",
        "properties":{"pointer":{"type":"string","maxLength":1024}, "equals":{}, "exists":{"type":"boolean"}},
        "oneOf":[{"required":["equals"]},{"required":["exists"]}]
    });
    schema["properties"]["max_result_bytes"] = json!({
        "type":"integer", "minimum":1024, "maximum":MAX_RESULT_BYTES, "default":DEFAULT_RESULT_BYTES,
        "description":"batch only: cumulative JSON-encoded bytes of retained original step outputs. Metadata/headings have a separate bounded overhead. An oversized returned result is omitted and stops the batch as an error; its effects may already have occurred. No automatic artifact spilling or result truncation."
    });
}

#[cfg(test)]
mod tests;
