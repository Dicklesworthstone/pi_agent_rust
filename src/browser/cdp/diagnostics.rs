//! Request-owned browser diagnostics, collected by the ordinary CDP receiver.
//!
//! No second socket, persistent subscription, page monkey-patch or replay. Only
//! the selected page's admitted main-world contexts may contribute console data.

use super::{AgentCx, Cdp, Error, Result, ToolOutput, output, policy, required};
use crate::model::{ContentBlock, TextContent};
use futures::future::{Either, select};
use serde_json::{Value, json};
use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

mod network;

const MAX_LOG_BYTES: usize = 64 * 1024;
const MAX_CONTEXTS: usize = 64;
const MAX_ARGUMENTS: usize = 8;
const MAX_STACK_FRAMES: usize = 8;
const MAX_FIELD_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Options {
    settle_ms: u64,
    max_events: usize,
    network: bool,
}

impl Options {
    fn parse(args: &Value) -> Result<Option<Self>> {
        let standalone = args["action"] == "diagnostics";
        let empty = json!({});
        let options = match args.get("diagnostics") {
            Some(options) => options,
            None if standalone => &empty,
            None => return Ok(None),
        };
        let options = options.as_object().ok_or_else(|| {
            Error::tool(
                "browser",
                "diagnostics must be an object, not a boolean or null",
            )
        })?;
        if options
            .keys()
            .any(|key| !matches!(key.as_str(), "settle_ms" | "max_events" | "network"))
        {
            return Err(Error::tool("browser", "unknown diagnostics option"));
        }
        let integer = |name: &str, default: u64, minimum: u64, maximum: u64| {
            options.get(name).map_or(Ok(default), |value| {
                value
                    .as_u64()
                    .filter(|value| (minimum..=maximum).contains(value))
                    .ok_or_else(|| {
                        Error::tool(
                            "browser",
                            format!(
                                "diagnostics.{name} must be an integer in {minimum}..={maximum}"
                            ),
                        )
                    })
            })
        };
        let settle_ms = integer("settle_ms", 250, 0, 10_000)?;
        let max_events = usize::try_from(integer("max_events", 100, 1, 200)?)
            .map_err(|_| Error::tool("browser", "invalid diagnostics event limit"))?;
        let network = options.get("network").map_or(Ok(false), |value| {
            value
                .as_bool()
                .ok_or_else(|| Error::tool("browser", "diagnostics.network must be a boolean"))
        })?;
        Ok(Some(Self {
            settle_ms,
            max_events,
            network,
        }))
    }
}

pub(super) fn validate(args: &Value) -> Result<()> {
    if Options::parse(args)?.is_none() {
        return Ok(());
    }
    let action = required(args, "action")?;
    if !matches!(
        action,
        "diagnostics"
            | "open"
            | "goto"
            | "evaluate"
            | "click"
            | "type"
            | "fill"
            | "press"
            | "scroll"
            | "wait_for"
            | "snapshot"
            | "ax_tree"
            | "select_option"
            | "set_checked"
    ) || args.get("frame").is_some()
    {
        return Err(Error::tool(
            "browser",
            "diagnostics require a main-document inspection, navigation or input action",
        ));
    }
    if args
        .get("tab")
        .and_then(Value::as_str)
        .is_none_or(|tab| tab.is_empty() || tab.len() > 256 || tab.chars().any(char::is_control))
    {
        return Err(Error::tool(
            "browser",
            "diagnostics require an explicit valid tab",
        ));
    }
    if action == "diagnostics"
        && args.as_object().is_some_and(|object| {
            object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "action" | "tab" | "diagnostics" | "timeout_ms"
                )
            })
        })
    {
        return Err(Error::tool(
            "browser",
            "the diagnostics action observes only; attach diagnostics to evaluate or an input action to trigger work",
        ));
    }
    Ok(())
}

/// This bound is in UTF-8 bytes after control filtering, not character count.
/// Console text is explicitly requested application data, not a secret scrubber.
fn bounded_text(text: &str, maximum: usize) -> String {
    let mut output = String::new();
    // Bound scanning too: a control-only input must not consume the entire frame.
    for character in text.chars().take(maximum) {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len() + character.len_utf8() > maximum {
            break;
        }
        output.push(character);
    }
    output
}

fn finite_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn admitted_url(raw: &str, allowlist: Option<&[String]>) -> bool {
    raw.len() <= 64 * 1024 && policy::check_navigation(raw, allowlist).is_ok()
}

/// Never copy query strings, fragments, URL credentials or non-web script URLs.
fn display_url(raw: &str, allowlist: Option<&[String]>) -> Option<String> {
    if !admitted_url(raw, allowlist) {
        return None;
    }
    let mut url = url::Url::parse(raw).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(bounded_text(url.as_str(), MAX_FIELD_BYTES))
}

fn remote_argument(value: &Value) -> Value {
    let kind = value["type"].as_str().unwrap_or("unknown");
    let mut argument = json!({"type": bounded_text(kind, 32)});
    match kind {
        "string" => {
            if let Some(text) = value["value"].as_str() {
                argument["value"] = json!(bounded_text(text, MAX_FIELD_BYTES));
                argument["truncated"] = json!(text.len() > MAX_FIELD_BYTES);
            }
        }
        "number" | "boolean" => {
            let scalar = &value["value"];
            if scalar.is_number() || scalar.is_boolean() {
                argument["value"] = scalar.clone();
            }
        }
        "object" if value["subtype"] == "null" => argument["value"] = Value::Null,
        _ => {}
    }
    if let Some(representation) = value["unserializableValue"].as_str() {
        argument["representation"] = json!(bounded_text(representation, MAX_FIELD_BYTES));
    }
    // Do not serialize object previews or walk getters/DOM nodes. An exception's
    // own message is handled separately; ordinary object arguments stay opaque.
    argument
}

fn stack_frames(stack: &Value, allowlist: Option<&[String]>) -> Vec<Value> {
    stack["callFrames"]
        .as_array()
        .map_or_else(Vec::new, |frames| {
            frames.iter().take(MAX_STACK_FRAMES).filter_map(|frame| {
            let url = display_url(frame["url"].as_str()?, allowlist)?;
            Some(json!({
                "function": bounded_text(frame["functionName"].as_str().unwrap_or_default(), 256),
                "url": url,
                "line": frame["lineNumber"].as_u64(),
                "column": frame["columnNumber"].as_u64(),
            }))
        }).collect()
        })
}

pub(super) struct Capture {
    options: Options,
    session: String,
    main_frame: String,
    page_url: String,
    allowlist: Option<Vec<String>>,
    contexts: BTreeSet<u64>,
    logs: VecDeque<(Value, usize)>,
    log_bytes: usize,
    dropped: u64,
    filtered: u64,
    fields_truncated: bool,
    phase: &'static str,
    action_started: bool,
    scope_lost: bool,
    network: Option<network::Trace>,
}

impl Capture {
    fn new(
        options: Options,
        session: String,
        frame: &Value,
        allowlist: Option<&[String]>,
    ) -> Result<Self> {
        let main_frame = required(frame, "id")?;
        let page_url = required(frame, "url")?;
        if main_frame.is_empty()
            || main_frame.len() > 256
            || main_frame.chars().any(char::is_control)
            || !admitted_url(page_url, allowlist)
        {
            return Err(Error::tool(
                "browser",
                "diagnostics require an admitted main document",
            ));
        }
        Ok(Self {
            options,
            session,
            main_frame: main_frame.into(),
            page_url: page_url.into(),
            allowlist: allowlist.map(<[String]>::to_vec),
            contexts: BTreeSet::new(),
            logs: VecDeque::new(),
            log_bytes: 0,
            dropped: 0,
            filtered: 0,
            fields_truncated: false,
            phase: "setup",
            action_started: false,
            scope_lost: false,
            network: options
                .network
                .then(|| network::Trace::new(options.max_events)),
        })
    }

    fn context_admitted(&self, context: &Value) -> bool {
        if self.scope_lost
            || context["auxData"]["frameId"] != self.main_frame
            || context["auxData"]["isDefault"] != true
        {
            return false;
        }
        let origin = context["origin"].as_str().unwrap_or_default();
        if self.page_url == "about:blank" && matches!(origin, "" | "://") {
            return true;
        }
        if !admitted_url(origin, self.allowlist.as_deref()) {
            return false;
        }
        match (url::Url::parse(&self.page_url), url::Url::parse(origin)) {
            (Ok(page), Ok(context)) => page.origin() == context.origin(),
            _ => false,
        }
    }

    fn record(&mut self, record: Value) {
        let bytes = record.to_string().len();
        if bytes > MAX_LOG_BYTES {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        while self.logs.len() >= self.options.max_events || self.log_bytes + bytes > MAX_LOG_BYTES {
            let Some((_, removed)) = self.logs.pop_front() else {
                break;
            };
            self.log_bytes -= removed;
            self.dropped = self.dropped.saturating_add(1);
        }
        self.log_bytes += bytes;
        self.logs.push_back((record, bytes));
    }

    pub(super) const fn begin_action(&mut self) {
        self.action_started = true;
        self.phase = "action";
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn observe(&mut self, event: &Value) {
        if event["sessionId"].as_str() != Some(self.session.as_str()) {
            return;
        }
        if !self.scope_lost
            && let Some(network) = self.network.as_mut()
        {
            network.observe(
                event,
                &self.main_frame,
                self.allowlist.as_deref(),
                self.phase,
            );
        }
        let params = &event["params"];
        match event["method"].as_str().unwrap_or_default() {
            "Page.frameNavigated" if params["frame"].get("parentId").is_none() => {
                let frame = &params["frame"];
                self.contexts.clear();
                if frame["id"] != self.main_frame
                    || !frame["url"]
                        .as_str()
                        .is_some_and(|url| admitted_url(url, self.allowlist.as_deref()))
                {
                    self.scope_lost = true;
                } else if let Some(url) = frame["url"].as_str() {
                    url.clone_into(&mut self.page_url);
                }
            }
            "Page.frameDetached" if params["frameId"] == self.main_frame => {
                self.contexts.clear();
                self.scope_lost = true;
            }
            "Runtime.executionContextsCleared" => self.contexts.clear(),
            "Runtime.executionContextDestroyed" => {
                if let Some(id) = params["executionContextId"].as_u64() {
                    self.contexts.remove(&id);
                }
            }
            "Runtime.executionContextCreated" => {
                let context = &params["context"];
                if let Some(id) = context["id"].as_u64() {
                    self.contexts.remove(&id);
                    if self.context_admitted(context) {
                        if self.contexts.len() < MAX_CONTEXTS {
                            self.contexts.insert(id);
                        } else {
                            self.filtered = self.filtered.saturating_add(1);
                        }
                    }
                }
            }
            "Runtime.consoleAPICalled" | "Runtime.exceptionThrown" => {
                let exception = event["method"] == "Runtime.exceptionThrown";
                let data = if exception {
                    &params["exceptionDetails"]
                } else {
                    params
                };
                if self.scope_lost
                    || !data["executionContextId"]
                        .as_u64()
                        .is_some_and(|id| self.contexts.contains(&id))
                {
                    self.filtered = self.filtered.saturating_add(1);
                    return;
                }
                let stack_truncated =
                    data["stackTrace"]["callFrames"]
                        .as_array()
                        .is_some_and(|frames| {
                            frames.len() > MAX_STACK_FRAMES
                                || frames.iter().take(MAX_STACK_FRAMES).any(|frame| {
                                    frame["functionName"]
                                        .as_str()
                                        .is_some_and(|s| s.len() > 256)
                                        || frame["url"]
                                            .as_str()
                                            .is_some_and(|s| s.len() > MAX_FIELD_BYTES)
                                })
                        });
                self.fields_truncated |= stack_truncated;
                let record = if exception {
                    let text = data["text"].as_str().unwrap_or_default();
                    let description = data["exception"]["description"]
                        .as_str()
                        .unwrap_or_default();
                    let fields_truncated = text.len() > MAX_FIELD_BYTES
                        || description.len() > MAX_FIELD_BYTES
                        || stack_truncated;
                    self.fields_truncated |= fields_truncated;
                    json!({
                        "kind": "exception", "phase": self.phase,
                        "timestamp": finite_number(&params["timestamp"]),
                        "text": bounded_text(text, MAX_FIELD_BYTES),
                        "description": bounded_text(description, MAX_FIELD_BYTES),
                        "fields_truncated": fields_truncated,
                        "exception_id": data["exceptionId"].as_u64(),
                        "stack": stack_frames(&data["stackTrace"], self.allowlist.as_deref()),
                    })
                } else {
                    let Some(arguments) = data["args"].as_array() else {
                        self.filtered = self.filtered.saturating_add(1);
                        return;
                    };
                    let fields_truncated = arguments.len() > MAX_ARGUMENTS
                        || stack_truncated
                        || arguments.iter().take(MAX_ARGUMENTS).any(|argument| {
                            ["value", "unserializableValue"].iter().any(|field| {
                                argument[*field]
                                    .as_str()
                                    .is_some_and(|s| s.len() > MAX_FIELD_BYTES)
                            })
                        });
                    self.fields_truncated |= fields_truncated;
                    json!({
                        "kind": "console", "phase": self.phase,
                        "level": bounded_text(data["type"].as_str().unwrap_or("unknown"), 32),
                        "timestamp": finite_number(&data["timestamp"]),
                        "arguments": arguments.iter().take(MAX_ARGUMENTS).map(remote_argument).collect::<Vec<_>>(),
                        "arguments_truncated": arguments.len() > MAX_ARGUMENTS,
                        "fields_truncated": fields_truncated,
                        "stack": stack_frames(&data["stackTrace"], self.allowlist.as_deref()),
                    })
                };
                self.record(record);
            }
            _ => {}
        }
    }

    fn report(self, completion: &str, action_succeeded: bool) -> Value {
        let network = self.network.map(network::Trace::report);
        let network_truncated = network
            .as_ref()
            .is_some_and(|report| report["truncated"] == true);
        let mut report = json!({
            "schema": "pi.browser.diagnostics.v1", "scope": "main_document",
            "action_started": self.action_started, "action_succeeded": action_succeeded,
            "completion": completion, "scope_lost": self.scope_lost,
            "truncated": self.dropped > 0 || self.fields_truncated || network_truncated,
            "fields_truncated": self.fields_truncated, "dropped_console_events": self.dropped,
            "filtered_console_events": self.filtered,
            "console_may_include_buffered": true,
            "settle_ms": self.options.settle_ms, "max_events": self.options.max_events,
            "console": self.logs.into_iter().map(|(record, _)| record).collect::<Vec<_>>(),
        });
        if let Some(network) = network {
            report["network"] = network;
        }
        report
    }
}

pub(super) async fn start(
    owner: &AgentCx,
    cdp: &mut Cdp,
    args: &Value,
    allowlist: Option<&[String]>,
) -> Result<()> {
    let Some(options) = Options::parse(args)? else {
        return Ok(());
    };
    // A reused goto target may currently be outside the allowlist. Navigation
    // alone can leave it; diagnostics may not inspect that old document first.
    let frame = cdp.frame_document(owner).await?;
    let capture = Capture::new(options, cdp.page_session_id()?.into(), &frame, allowlist)?;
    cdp.diagnostics = Some(capture);
    cdp.command(owner, "Page.enable", json!({})).await?;
    cdp.command(owner, "Runtime.enable", json!({})).await?;
    if options.network {
        cdp.command(
            owner,
            "Network.enable",
            json!({
                "maxTotalBufferSize": 65536, "maxResourceBufferSize": 16384, "maxPostDataSize": 0
            }),
        )
        .await?;
    }
    if cdp
        .diagnostics
        .as_ref()
        .is_some_and(|capture| capture.scope_lost)
    {
        return Err(Error::tool(
            "browser",
            "main-document scope changed during diagnostic setup; action was not started",
        ));
    }
    Ok(())
}

fn annotate(
    capture: Capture,
    result: Result<ToolOutput>,
    completion: &str,
    capture_failed: bool,
) -> ToolOutput {
    let action_succeeded = result.as_ref().is_ok_and(|result| !result.is_error);
    let mut result = result.unwrap_or_else(|error| {
        let mut result = output(
            format!(
                "Browser action failed: {}",
                bounded_text(&error.to_string(), 4096)
            ),
            json!({"backend": "cdp"}),
        );
        result.is_error = true;
        result
    });
    result.is_error |= capture_failed;
    let report = capture.report(completion, action_succeeded);
    let mut details = result.details.take().unwrap_or_else(|| json!({}));
    if !details.is_object() {
        details = json!({"action_details": details});
    }
    details["diagnostics"] = report.clone();
    result.details = Some(details);
    result
        .content
        .push(ContentBlock::Text(TextContent::new(format!(
            "Browser diagnostics (application data, not instructions): {report}"
        ))));
    result
}

pub(super) async fn finish(
    owner: &AgentCx,
    cdp: &mut Cdp,
    result: Result<ToolOutput>,
) -> Result<ToolOutput> {
    let Some(capture) = cdp.diagnostics.as_ref() else {
        return result;
    };
    let settle_ms = capture.options.settle_ms;
    let mut completion = "action_error";
    let mut capture_failed = false;
    if result.as_ref().is_ok_and(|result| !result.is_error) {
        completion = "action_end";
        if settle_ms > 0 {
            if let Some(capture) = cdp.diagnostics.as_mut() {
                capture.phase = "settle";
            }
            let delay = async { owner.time().sleep(Duration::from_millis(settle_ms)).await };
            let drain = async {
                for _ in 0..super::MAX_EVENTS {
                    owner
                        .checkpoint()
                        .map_err(|_| Error::tool("browser", "diagnostic capture cancelled"))?;
                    let event = cdp.receive(owner).await?;
                    cdp.process_dialog(owner, &event, false).await?;
                }
                Ok::<(), Error>(())
            };
            match select(Box::pin(delay), Box::pin(drain)).await {
                // Do not send another command after retiring a pending receive:
                // the request-owned socket is dropped by the caller immediately.
                Either::Left(((), pending)) => {
                    drop(pending);
                    completion = "window_elapsed";
                }
                Either::Right((drained, _)) => {
                    completion = if drained.is_ok() {
                        "event_limit"
                    } else {
                        "capture_error"
                    };
                    capture_failed = true;
                }
            }
        }
    }
    owner.checkpoint().map_err(|_| {
        Error::tool(
            "browser",
            "browser operation cancelled; remote side effects may already have occurred",
        )
    })?;
    let capture = cdp
        .diagnostics
        .take()
        .ok_or_else(|| Error::tool("browser", "diagnostic capture disappeared"))?;
    if capture.scope_lost {
        completion = "scope_lost";
        capture_failed = true;
    }
    Ok(annotate(capture, result, completion, capture_failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
    fn event(method: &str, params: Value) -> Value {
        json!({"sessionId":"page-session","method":method,"params":params})
    }

    fn capture(max_events: usize) -> Capture {
        Capture::new(
            Options {
                settle_ms: 0,
                max_events,
                network: false,
            },
            "page-session".into(),
            &json!({"id":"main-frame","url":"https://example.com/app"}),
            Some(&["example.com".to_string()]),
        )
        .unwrap()
    }

    fn context(id: u64, frame: &str, origin: &str, default: bool) -> Value {
        event(
            "Runtime.executionContextCreated",
            json!({"context":{
                "id":id,"origin":origin,"auxData":{"frameId":frame,"isDefault":default}
            }}),
        )
    }

    fn console(id: u64, text: &str) -> Value {
        event(
            "Runtime.consoleAPICalled",
            json!({
                "executionContextId":id,"type":"error","timestamp":123.0,
                "args":[{"type":"string","value":text}]
            }),
        )
    }

    #[test]
    fn preflight_is_strict_and_requires_an_explicit_supported_target() {
        assert!(
            Options::parse(&json!({"action":"evaluate"}))
                .unwrap()
                .is_none()
        );
        assert!(validate(&json!({"action":"diagnostics","tab":"app"})).is_ok());
        assert!(validate(&json!({"action":"click","tab":"app","diagnostics":{}})).is_ok());
        for args in [
            json!({"action":"diagnostics"}),
            json!({"action":"diagnostics","tab":"app","script":"sideEffect()"}),
            json!({"action":"diagnostics","tab":"app","frame":"child"}),
            json!({"action":"status","tab":"app","diagnostics":{}}),
            json!({"action":"diagnostics","tab":"app","diagnostics":true}),
            json!({"action":"diagnostics","tab":"app","diagnostics":null}),
            json!({"action":"diagnostics","tab":"app","diagnostics":{"max_events":0}}),
            json!({"action":"diagnostics","tab":"app","diagnostics":{"max_events":201}}),
            json!({"action":"diagnostics","tab":"app","diagnostics":{"settle_ms":10001}}),
            json!({"action":"diagnostics","tab":"app","diagnostics":{"settle_ms":1.5}}),
            json!({"action":"diagnostics","tab":"app","diagnostics":{"future":true}}),
            json!({"action":"diagnostics","tab":"app","diagnostics":{"network":"true"}}),
        ] {
            assert!(validate(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn console_admission_requires_session_frame_default_world_and_origin() {
        let mut capture = capture(100);
        capture.observe(&console(1, "not-yet-admitted"));
        for (id, frame, origin, default) in [
            (1, "main-frame", "https://example.com", true),
            (2, "child-frame", "https://example.com", true),
            (3, "main-frame", "https://elsewhere.test", true),
            (4, "main-frame", "https://example.com", false),
        ] {
            capture.observe(&context(id, frame, origin, default));
            capture.observe(&console(id, &format!("message-{id}")));
        }
        let mut foreign = console(1, "foreign-session-secret");
        foreign["sessionId"] = json!("other-page");
        capture.observe(&foreign);
        let report = capture.report("action_end", true);
        assert_eq!(report["console"].as_array().unwrap().len(), 1);
        assert_eq!(report["console"][0]["arguments"][0]["value"], "message-1");
        assert!(!report.to_string().contains("secret"));
    }

    #[test]
    fn navigation_and_context_destruction_revoke_old_console_authority() {
        let mut capture = capture(100);
        capture.observe(&context(1, "main-frame", "https://example.com", true));
        capture.observe(&event(
            "Runtime.executionContextDestroyed",
            json!({"executionContextId":1}),
        ));
        capture.observe(&console(1, "destroyed"));
        capture.observe(&context(1, "main-frame", "https://example.com", true));
        capture.observe(&event(
            "Page.frameNavigated",
            json!({"frame":{
                "id":"main-frame","url":"https://forbidden.test/private"
            }}),
        ));
        capture.observe(&console(1, "after-navigation"));
        capture.observe(&context(2, "main-frame", "https://example.com", true));
        capture.observe(&console(2, "after-scope-loss"));
        let report = capture.report("action_end", true);
        assert_eq!(report["scope_lost"], true);
        assert_eq!(report["console"], json!([]));
    }

    #[test]
    fn default_world_is_readmitted_after_allowed_navigation_only() {
        let mut capture = capture(100);
        capture.observe(&context(1, "main-frame", "https://example.com", true));
        capture.observe(&event(
            "Page.frameNavigated",
            json!({"frame":{
                "id":"main-frame","url":"https://example.com/next"
            }}),
        ));
        capture.observe(&console(1, "old"));
        capture.observe(&context(2, "main-frame", "https://example.com", true));
        capture.begin_action();
        capture.observe(&console(2, "new"));
        let report = capture.report("action_end", true);
        assert_eq!(report["console"].as_array().unwrap().len(), 1);
        assert_eq!(report["console"][0]["phase"], "action");
        assert_eq!(report["scope_lost"], false);
    }

    #[test]
    fn latest_console_survives_event_and_byte_limits() {
        let mut capture = capture(2);
        capture.observe(&context(1, "main-frame", "https://example.com", true));
        for text in ["first", "second", "last"] {
            capture.observe(&console(1, text));
        }
        assert_eq!(capture.logs.len(), 2);
        assert_eq!(
            capture.logs.back().unwrap().0["arguments"][0]["value"],
            "last"
        );
        assert_eq!(capture.dropped, 1);
        capture.options.max_events = 200;
        let huge = "界".repeat(3000);
        for _ in 0..200 {
            capture.observe(&console(1, &huge));
        }
        assert!(capture.log_bytes <= MAX_LOG_BYTES);
        assert!(capture.logs.len() < 200);
        let report = capture.report("action_end", true);
        assert_eq!(report["truncated"], true);
        assert!(report.to_string().len() < MAX_LOG_BYTES + 4096);
    }

    #[test]
    fn object_previews_and_hidden_credential_fields_are_never_serialized() {
        let argument = remote_argument(&json!({
            "type":"object","objectId":"PRIVATE-OBJECT","description":"PRIVATE-DESCRIPTION",
            "value":{"password":"PRIVATE-VALUE"},"preview":{"properties":[{"value":"PRIVATE-PREVIEW"}]}
        }));
        assert_eq!(argument, json!({"type":"object"}));
        assert_eq!(
            remote_argument(&json!({"type":"object","subtype":"null"}))["value"],
            Value::Null
        );
        assert_eq!(
            remote_argument(&json!({"type":"number","unserializableValue":"NaN"}))["representation"],
            "NaN"
        );
        assert!(bounded_text("界界", 4).len() <= 4);
        assert!(
            !bounded_text("hello\u{001b}[31m\n", 32)
                .chars()
                .any(char::is_control)
        );
    }

    #[test]
    fn exceptions_keep_bounded_source_locations_without_url_secrets() {
        let mut capture = capture(100);
        capture.observe(&context(1, "main-frame", "https://example.com", true));
        capture.observe(&event("Runtime.exceptionThrown", json!({"timestamp":99,"exceptionDetails":{
            "executionContextId":1,"exceptionId":7,"text":"Uncaught",
            "exception":{"description":"TypeError: broken"},
            "stackTrace":{"callFrames":[
                {"functionName":"load","url":"https://example.com/app.js?token=PRIVATE#PRIVATE","lineNumber":4,"columnNumber":2},
                {"functionName":"foreign","url":"https://forbidden.test/PRIVATE","lineNumber":1,"columnNumber":0}
            ]}
        }})));
        let report = capture.report("action_end", true);
        assert_eq!(report["console"][0]["kind"], "exception");
        assert_eq!(report["console"][0]["description"], "TypeError: broken");
        assert_eq!(
            report["console"][0]["stack"][0]["url"],
            "https://example.com/app.js"
        );
        assert!(!report.to_string().contains("PRIVATE"));
    }

    #[test]
    fn diagnostic_annotation_preserves_results_and_failure_truth() {
        let mut capture = capture(100);
        capture.begin_action();
        let result = annotate(
            capture,
            Ok(output("answer", json!({"result":42}))),
            "window_elapsed",
            false,
        );
        assert!(!result.is_error);
        assert_eq!(result.details.as_ref().unwrap()["result"], 42);
        assert_eq!(
            result.details.as_ref().unwrap()["diagnostics"]["action_succeeded"],
            true
        );
        let result = annotate(
            self::capture(100),
            Err(Error::tool("browser", "native failure")),
            "action_error",
            false,
        );
        assert!(result.is_error);
        assert_eq!(
            result.details.as_ref().unwrap()["diagnostics"]["action_started"],
            false
        );
        let result = annotate(
            self::capture(100),
            Ok(output("effect completed", json!({}))),
            "capture_error",
            true,
        );
        assert!(result.is_error);
        assert_eq!(
            result.details.as_ref().unwrap()["diagnostics"]["action_succeeded"],
            true
        );
    }
}
