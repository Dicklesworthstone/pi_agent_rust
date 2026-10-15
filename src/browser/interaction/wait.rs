//! Readiness observations on one pinned document, under the native call budget.
//!
//! Waits never perform input, retry a trigger, enter another document or turn a
//! transport error into evidence of absence. Stable means repeated samples of
//! one resolved node (or its absence), not an atomic promise about future DOM.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Cdp, Document, References, document, resolve, selectors};
use crate::agent_cx::AgentCx;
use crate::browser::{output, required};
use crate::error::{Error, Result};
use crate::tools::ToolOutput;

const PROBE: &str = include_str!("wait.js");
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_TEXT_BYTES: usize = 16 * 1024;

fn error(code: &'static str) -> Error {
    Error::tool("browser", code)
}

fn usage() -> Error {
    error("BROWSER_WAIT_USAGE: wait_for requires selector; state is visible, hidden, attached or detached; text accepts text_match contains/exact; stable_ms is 0..10000")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Visible,
    Hidden,
    Attached,
    Detached,
}

impl State {
    const fn name(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::Attached => "attached",
            Self::Detached => "detached",
        }
    }
}

struct Condition {
    state: State,
    text: Option<String>,
    exact: bool,
    stable: Duration,
}

impl Condition {
    fn parse(args: &Value) -> Result<Self> {
        let state = match args.get("state") {
            None => State::Visible,
            Some(Value::String(value)) => match value.as_str() {
                "visible" => State::Visible,
                "hidden" => State::Hidden,
                "attached" => State::Attached,
                "detached" => State::Detached,
                _ => return Err(usage()),
            },
            _ => return Err(usage()),
        };
        let selector = args.get("selector").and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 64 * 1024 && !value.contains('\0'))
            .ok_or_else(usage)?;
        let semantic = selectors::parse(selector)?.is_some();
        if semantic && state != State::Visible {
            // Semantic lookup intentionally omits hidden/accessibility-ignored
            // nodes. No match therefore cannot establish DOM absence.
            return Err(error("BROWSER_WAIT_USAGE: hidden/attached/detached require CSS or an existing snapshot reference; semantic selectors are visibility-filtered"));
        }
        let text = args.get("text").map(|value| {
            let text = value.as_str()
                .filter(|value| value.len() <= MAX_TEXT_BYTES && !value.contains('\0'))
                .ok_or_else(usage)?;
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() || matches!(state, State::Hidden | State::Detached) {
                return Err(usage());
            }
            Ok(normalized)
        }).transpose()?;
        let exact = match args.get("text_match") {
            None => false,
            Some(Value::String(value)) if text.is_some() => match value.as_str() {
                "contains" => false,
                "exact" => true,
                _ => return Err(usage()),
            },
            _ => return Err(usage()),
        };
        let stable = match args.get("stable_ms") {
            None => Duration::ZERO,
            Some(value) => Duration::from_millis(value.as_u64()
                .filter(|value| *value <= 10_000).ok_or_else(usage)?),
        };
        Ok(Self { state, text, exact, stable })
    }

    fn matches(&self, observed: &Observation) -> bool {
        let state = match self.state {
            State::Visible => observed.attached && observed.visible,
            State::Hidden => !observed.attached || !observed.visible,
            State::Attached => observed.attached,
            State::Detached => !observed.attached,
        };
        state && (self.text.is_none() || observed.text_matches == Some(true))
    }
}

/// Reject ignored or malformed wait options before native connection/input.
/// The boolean selects native-only behavior in the explicit fixture backend.
pub(in crate::browser) fn validate_input(args: &Value) -> Result<bool> {
    let configured = ["state", "text_match", "stable_ms"].iter()
        .any(|field| args.get(*field).is_some());
    if args.get("action").and_then(Value::as_str) != Some("wait_for") {
        if configured {
            return Err(usage());
        }
        return Ok(after_condition(args)?.is_some());
    }
    let fields = args.as_object().ok_or_else(usage)?;
    if fields.keys().any(|field| !matches!(field.as_str(),
        "action" | "tab" | "frame" | "selector" | "state" | "text" |
        "text_match" | "stable_ms" | "timeout_ms" | "diagnostics"
    )) {
        return Err(usage());
    }
    Condition::parse(args)?;
    Ok(configured || args.get("text").is_some())
}

/// A same-document postcondition on a single admitted input operation. Field
/// shape, scope and budget are validated before input. Selector syntax and
/// matching that require a live document are checked by the subsequent wait.
pub(super) fn after_condition(args: &Value) -> Result<Option<Value>> {
    let Some(after) = args.get("wait_after") else {
        return Ok(None);
    };
    let invalid = || error("BROWSER_WAIT_AFTER_USAGE: wait_after requires click/type/fill/press and accepts only selector, state, text, text_match and stable_ms; tab/frame/deadline are inherited; dialog_response cannot be combined");
    if !matches!(args.get("action").and_then(Value::as_str), Some("click" | "type" | "fill" | "press"))
        || args.get("dialog_response").is_some()
    {
        return Err(invalid());
    }
    let fields = after.as_object().ok_or_else(invalid)?;
    if fields.keys().any(|key| !matches!(key.as_str(), "selector" | "state" | "text" | "text_match" | "stable_ms")) {
        return Err(invalid());
    }
    let mut condition = after.clone();
    condition["action"] = json!("wait_for");
    Condition::parse(&condition)?;
    Ok(Some(condition))
}

pub(in crate::browser) fn after_schema() -> Value {
    json!({
        "type":"object","required":["selector"],"additionalProperties":false,
        "description":"click/type/fill/press only: run the input once, then await this same-document condition on the original connection and timeout_ms. Inherits tab/frame; no independent deadline. Navigation fails, never retargets. Already-satisfied conditions are allowed; causality is not proven. A failed or timed-out wait does not undo or replay the action. Unsupported with dialog_response.",
        "properties":{
            "selector":{"type":"string","minLength":1,"maxLength":65536},
            "state":{"type":"string","enum":["visible","hidden","attached","detached"],"default":"visible"},
            "text":{"type":"string","minLength":1,"maxLength":16384},
            "text_match":{"type":"string","enum":["contains","exact"],"default":"contains"},
            "stable_ms":{"type":"integer","minimum":0,"maximum":10000,"default":0}
        }
    })
}

/// Preserve acknowledged action facts even when a subsequent predicate fails.
/// Timeout/cancellation can still drop the whole future at its outer deadline;
/// those errors already disclose that remote effects may have occurred.
pub(super) fn after_result(mut action: ToolOutput, readiness: Result<ToolOutput>) -> ToolOutput {
    let (report, note) = match readiness {
        Ok(observed) if !observed.is_error => (json!({
            "satisfied":true,"observation":observed.details,
            "action_replayed":false,"causality_verified":false,
        }), "Post-action wait satisfied on the original document. The action was not replayed; this observation does not prove that the action caused the state."),
        other => {
            action.is_error = true;
            // Never copy arbitrary page/protocol exception content into a new
            // report. Known categories retain actionable diagnosis only.
            let message = other.err().map(|error| error.to_string()).unwrap_or_default();
            let kind = if message.contains("BROWSER_WAIT_STALE") {
                "document_changed"
            } else if message.contains("BROWSER_SELECTOR_AMBIGUOUS") {
                "ambiguous_selector"
            } else if message.contains("BROWSER_WAIT_LIMIT") {
                "observation_limit"
            } else if message.contains("CANCELLED") || message.contains("cancelled") {
                "cancelled"
            } else {
                "observation_failed"
            };
            (json!({"satisfied":false,"error_kind":kind,"action_replayed":false}),
                "Post-action wait failed after the input completed. Its effects are not undone. Inspect the page before deciding what to do next; do not automatically repeat the input.")
        }
    };
    let previous = action.details.take().unwrap_or(Value::Null);
    let mut details = if previous.is_object() { previous } else { json!({"action_details":previous}) };
    details["wait_after"] = report;
    action.details = Some(details);
    action.content.push(crate::model::ContentBlock::Text(crate::model::TextContent::new(note)));
    action
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Observation {
    attached: bool,
    visible: bool,
    text_matches: Option<bool>,
}

impl Observation {
    const fn absent(text: bool) -> Self {
        Self { attached: false, visible: false, text_matches: if text { Some(false) } else { None } }
    }

    fn parse(value: Value, text: bool) -> Result<Self> {
        let observed: Self = serde_json::from_value(value)
            .map_err(|_| error("BROWSER_WAIT_PROTOCOL: invalid DOM observation"))?;
        if observed.text_matches.is_some() != text
            || (!observed.attached && (observed.visible || observed.text_matches == Some(true)))
        {
            return Err(error("BROWSER_WAIT_PROTOCOL: contradictory DOM observation"));
        }
        Ok(observed)
    }
}

#[derive(Default)]
struct Stability {
    since: Option<(Option<u64>, Duration)>,
}

impl Stability {
    fn sample(&mut self, identity: Option<u64>, matched: bool, now: Duration, target: Duration) -> bool {
        if !matched {
            self.since = None;
            return false;
        }
        if self.since.is_none_or(|(id, since)| id != identity || now < since) {
            self.since = Some((identity, now));
        }
        self.since.is_some_and(|(_, since)| now.saturating_sub(since) >= target)
    }
}

async fn check_document(owner: &AgentCx, cdp: &mut Cdp, expected: &Document) -> Result<()> {
    owner.checkpoint().map_err(|_| error("BROWSER_WAIT_CANCELLED"))?;
    if document(owner, cdp).await? != *expected {
        return Err(error("BROWSER_WAIT_STALE: document changed while waiting; no other document was inspected"));
    }
    Ok(())
}

fn probe_value(response: &Value) -> Result<Value> {
    if let Some(exception) = response.get("exceptionDetails") {
        let description = exception.pointer("/exception/description").and_then(Value::as_str)
            .unwrap_or_default();
        let first = description.lines().next().unwrap_or_default();
        return Err(match first {
            "Error: BROWSER_WAIT_LIMIT" => error("BROWSER_WAIT_LIMIT: DOM observation exceeded its bounded traversal"),
            "Error: BROWSER_WAIT_STALE" => error("BROWSER_WAIT_STALE: node belongs to another document"),
            _ => error("BROWSER_WAIT_PROBE_FAILED: DOM observation failed"),
        });
    }
    response.pointer("/result/value").cloned()
        .ok_or_else(|| error("BROWSER_WAIT_PROTOCOL: missing DOM observation"))
}

async fn observe(
    owner: &AgentCx,
    cdp: &mut Cdp,
    expected: &Document,
    id: u64,
    condition: &Condition,
) -> Result<Observation> {
    let world = cdp.command(owner, "Page.createIsolatedWorld", json!({
        "frameId":expected.frame, "worldName":"pi-browser-tools",
    })).await?;
    let context = world["executionContextId"].as_u64().filter(|id| *id > 0)
        .ok_or_else(|| error("BROWSER_WAIT_PROTOCOL: missing isolated world"))?;
    check_document(owner, cdp, expected).await?;
    let node = cdp.command(owner, "DOM.resolveNode", json!({
        "backendNodeId":id,"executionContextId":context,
    })).await?;
    let object = node["object"]["objectId"].as_str()
        .ok_or_else(|| error("BROWSER_WAIT_PROTOCOL: missing DOM object"))?;
    let response = cdp.command(owner, "Runtime.callFunctionOn", json!({
        "objectId":object,"functionDeclaration":PROBE,
        "arguments":[{"value":{"text":condition.text,"exact":condition.exact}}],
        "returnByValue":true,
    })).await;
    // The call owns its socket and remote objects. On cancellation the same
    // backend connection cleanup applies; do not invent a fresh cleanup budget.
    let _ = cdp.command(owner, "Runtime.releaseObject", json!({"objectId":object})).await;
    let observed = Observation::parse(probe_value(&response?)?, condition.text.is_some())?;
    check_document(owner, cdp, expected).await?;
    Ok(observed)
}

pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    refs: Option<&References>,
    args: &Value,
) -> Result<ToolOutput> {
    let expected = document(owner, cdp).await?;
    execute_pinned(owner, cdp, tab, refs, args, &expected).await
}

pub(super) async fn execute_pinned(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    refs: Option<&References>,
    args: &Value,
    expected: &Document,
) -> Result<ToolOutput> {
    let condition = Condition::parse(args)?;
    let selector = required(args, "selector")?;
    let started = Instant::now();
    let mut stability = Stability::default();
    let mut samples = 0_u64;
    loop {
        check_document(owner, cdp, expected).await?;
        let id = resolve(owner, cdp, selector, refs).await?;
        let observed = match id {
            Some(id) => observe(owner, cdp, expected, id, &condition).await?,
            None => Observation::absent(condition.text.is_some()),
        };
        check_document(owner, cdp, expected).await?;
        samples = samples.saturating_add(1);
        let identity = if observed.attached { id } else { None };
        if stability.sample(identity, condition.matches(&observed), started.elapsed(), condition.stable) {
            return Ok(output(
                format!("Browser wait condition satisfied in tab {tab}; this is sampled readiness, not a guarantee of later state."),
                json!({
                    "action":"wait_for","selector":selector,"state":condition.state.name(),
                    "found":observed.attached,"visible":observed.visible,"matched":true,
                    "textMatched":observed.text_matches,"stable_ms":condition.stable.as_millis(),
                    "samples":samples,"tab":tab,"backend":"cdp","document_pinned":true,
                }),
            ));
        }
        // The existing outer native/session watchdog bounds every query and
        // sleep. Neither a new poll nor a matched-but-unstable sample resets it.
        owner.time().sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests;
