//! Explicit JavaScript-dialog responses; never replay the triggering action.
//!
//! CDP's Page.handleJavaScriptDialog can resume a page suspended by alert,
//! confirm, prompt or beforeunload. It must not run page JavaScript first.
//!
//! `handle_dialog` requires an explicit tab and boolean `accept`; optional
//! `prompt_text` is sent literally when accepting. For an action that will open
//! a dialog, supply `dialog_response` on `evaluate`, `click`, `type`, `fill` or
//! `press`, with exact `type`, `message`, `url`, and an explicit `accept` choice.
//! It requires one matching dialog before the operation's original deadline:
//! absence, mismatch or failed acknowledgement is an error, not a retry of the
//! input. The expectation is consumed before dispatch and never persists into
//! another call. URL/message matching is not cryptographic proof of causality;
//! a concurrent same-page dialog with identical fields cannot be distinguished.
//! Browser/OS permission, authentication and file-picker dialogs are not handled.

use serde_json::{Value, json};

use super::{cdp::Cdp, output, policy, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;

const MAX_PROMPT_BYTES: usize = 64 * 1024;

fn invalid(message: &str) -> Error {
    Error::tool("browser", format!("[BROWSER_DIALOG_INVALID] {message}"))
}

pub(super) fn response_failed() -> Error {
    Error::tool(
        "browser",
        "[BROWSER_DIALOG_RESPONSE_FAILED] response failed or delivery is uncertain; \
         inspect the tab before retrying. The triggering action was not replayed",
    )
}

/// A one-operation expectation, never a persistent auto-accept policy.
/// The message and URL are exact strings; neither is executed or logged.
pub(super) struct Expected {
    kind: String,
    message: String,
    url: String,
    response: Value,
}

impl Expected {
    pub(super) fn from_args(args: &Value, allowlist: Option<&[String]>) -> Result<Option<Self>> {
        let Some(value) = args.get("dialog_response") else {
            return Ok(None);
        };
        if !matches!(
            required(args, "action")?,
            "evaluate" | "click" | "type" | "fill" | "press"
        ) {
            return Err(invalid(
                "dialog_response is only for evaluate, click, type, fill or press",
            ));
        }
        validate_tab(args)?;
        let fields = value
            .as_object()
            .ok_or_else(|| invalid("dialog_response must be an object"))?;
        if fields.keys().any(|key| {
            !matches!(
                key.as_str(),
                "type" | "message" | "url" | "accept" | "prompt_text"
            )
        }) {
            return Err(invalid("unknown dialog_response field"));
        }
        let bounded = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .filter(|text| text.len() <= MAX_PROMPT_BYTES)
                .map(str::to_owned)
                .ok_or_else(|| invalid("dialog type, message and url must be bounded strings"))
        };
        let kind = bounded("type")?;
        if !matches!(
            kind.as_str(),
            "alert" | "confirm" | "prompt" | "beforeunload"
        ) {
            return Err(invalid("unknown JavaScript dialog type"));
        }
        if kind != "prompt" && value.get("prompt_text").is_some() {
            return Err(invalid("prompt_text requires a prompt dialog"));
        }
        let message = bounded("message")?;
        let url = bounded("url")?;
        policy::check_navigation(&url, allowlist)
            .map_err(|_| invalid("expected dialog URL is not allowed"))?;
        Ok(Some(Self {
            kind,
            message,
            url,
            response: response_parameters(value)?,
        }))
    }
}

/// Transport-local state for exactly one expected response. Taking the
/// expectation before sending prevents a second dialog from reusing consent.
#[derive(Default)]
pub(super) struct State {
    requested: bool,
    expected: Option<Expected>,
    pending: Option<(u64, bool)>,
    acknowledged: Option<bool>,
}

impl State {
    pub(super) fn arm(&mut self, expected: Expected) {
        self.requested = true;
        self.expected = Some(expected);
    }

    pub(super) const fn requested(&self) -> bool {
        self.requested
    }

    pub(super) const fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) const fn completed(&self) -> bool {
        self.acknowledged.is_some()
    }

    pub(super) fn record_sent(&mut self, id: u64, accepted: bool) {
        self.pending = Some((id, accepted));
    }

    pub(super) fn acknowledge(&mut self, value: &Value, session: Option<&str>) -> Result<bool> {
        let Some((id, accepted)) = self.pending else {
            return Ok(false);
        };
        if value["id"].as_u64() != Some(id) {
            return Ok(false);
        }
        if session.is_none()
            || value["sessionId"].as_str() != session
            || value.get("error").is_some()
            || !value.get("result").is_some_and(Value::is_object)
        {
            return Err(response_failed());
        }
        self.pending = None;
        self.acknowledged = Some(accepted);
        Ok(true)
    }

    pub(super) fn response_for(
        &mut self,
        value: &Value,
        session: Option<&str>,
        explicit_response: bool,
    ) -> Result<Option<Value>> {
        if value["method"] != "Page.javascriptDialogOpening" {
            return Ok(None);
        }
        // A root-channel or other target's event cannot spend this target's
        // expectation. CDP flattened page events carry the attached session ID.
        if session.is_none() || value["sessionId"].as_str() != session || explicit_response {
            return Ok(None);
        }
        let Some(expected) = self.expected.take() else {
            return Err(Error::tool(
                "browser",
                "[BROWSER_DIALOG_UNEXPECTED] a JavaScript dialog needs an explicit response; \
                 inspect the selected tab and use handle_dialog. Do not replay the triggering action",
            ));
        };
        let params = &value["params"];
        if params["type"].as_str() != Some(expected.kind.as_str())
            || params["message"].as_str() != Some(expected.message.as_str())
            || params["url"].as_str() != Some(expected.url.as_str())
        {
            return Err(Error::tool(
                "browser",
                "[BROWSER_DIALOG_MISMATCH] dialog type, message or URL did not match; \
                 no automatic response was sent. Inspect the tab; do not replay the triggering action",
            ));
        }
        Ok(Some(expected.response))
    }

    pub(super) fn annotate(&self, output: &mut ToolOutput) {
        if let Some(accepted) = self.acknowledged {
            let details = output.details.get_or_insert_with(|| json!({}));
            if !details.is_object() {
                *details = json!({"action_details": std::mem::take(details)});
            }
            details["dialog_response"] = json!({
                "acknowledged": true, "accepted": accepted, "trigger_replayed": false
            });
        }
    }
}

fn validate_tab(args: &Value) -> Result<()> {
    if args
        .get("tab")
        .and_then(Value::as_str)
        .is_none_or(|tab| tab.is_empty() || tab.len() > 256 || tab.chars().any(char::is_control))
    {
        Err(invalid(
            "dialog handling requires an explicit bounded tab name or target ID",
        ))
    } else {
        Ok(())
    }
}

fn response_parameters(args: &Value) -> Result<Value> {
    let accept = args
        .get("accept")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid("accept must be an explicit boolean"))?;
    let mut params = json!({"accept": accept});
    if let Some(value) = args.get("prompt_text") {
        let text = value
            .as_str()
            .filter(|text| text.len() <= MAX_PROMPT_BYTES)
            .ok_or_else(|| invalid("prompt_text must be a string of at most 64 KiB"))?;
        if !accept {
            return Err(invalid(
                "prompt_text cannot be supplied when dismissing a dialog",
            ));
        }
        // Do not trim, interpolate into JavaScript, or echo this potentially
        // sensitive answer in the result/diagnostic. Empty is a real answer.
        params["promptText"] = json!(text);
    }
    Ok(params)
}

pub(super) fn validate_action(args: &Value) -> Result<()> {
    if required(args, "action")? != "handle_dialog" {
        if args.get("accept").is_some() || args.get("prompt_text").is_some() {
            return Err(invalid("accept and prompt_text are only for handle_dialog"));
        }
        return Ok(());
    }
    let fields = args
        .as_object()
        .ok_or_else(|| invalid("expected an object"))?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "action" | "tab" | "accept" | "prompt_text" | "timeout_ms"
        )
    }) {
        return Err(invalid(
            "handle_dialog accepts only tab, accept, prompt_text and timeout_ms",
        ));
    }
    validate_tab(args)?;
    response_parameters(args)?;
    Ok(())
}

pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    args: &Value,
) -> Result<ToolOutput> {
    let params = response_parameters(args)?;
    let accepted = params["accept"] == true;
    cdp.command(owner, "Page.handleJavaScriptDialog", params)
        .await
        .map_err(|_| response_failed())?;
    Ok(output(
        if accepted {
            "Chromium acknowledged accepting the dialog. The triggering action was not replayed."
        } else {
            "Chromium acknowledged dismissing the dialog. The triggering action was not replayed."
        },
        json!({
            "backend": "cdp", "tab": tab, "dialog_response_acknowledged": true,
            "accepted": accepted, "prompt_text_supplied": args.get("prompt_text").is_some(),
            "trigger_replayed": false
        }),
    ))
}

#[cfg(test)]
mod tests;
