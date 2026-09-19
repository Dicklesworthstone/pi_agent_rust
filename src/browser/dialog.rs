//! Explicit JavaScript-dialog responses; never replay the triggering action.
//!
//! CDP's Page.handleJavaScriptDialog can resume a page suspended by alert,
//! confirm, prompt or beforeunload. It must not run page JavaScript first.

use serde_json::{Value, json};

use super::{cdp::Cdp, output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;

const MAX_PROMPT_BYTES: usize = 64 * 1024;

fn invalid(message: &str) -> Error {
    Error::tool("browser", format!("[BROWSER_DIALOG_INVALID] {message}"))
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
            return Err(invalid("prompt_text cannot be supplied when dismissing a dialog"));
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
    let fields = args.as_object().ok_or_else(|| invalid("expected an object"))?;
    if fields.keys().any(|key| {
        !matches!(key.as_str(), "action" | "tab" | "accept" | "prompt_text" | "timeout_ms")
    }) {
        return Err(invalid("handle_dialog accepts only tab, accept, prompt_text and timeout_ms"));
    }
    if args.get("tab").and_then(Value::as_str).is_none_or(|tab| {
        tab.is_empty() || tab.len() > 256 || tab.chars().any(char::is_control)
    }) {
        return Err(invalid("handle_dialog requires an explicit bounded tab name or target ID"));
    }
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
        .map_err(|_| {
            Error::tool(
                "browser",
                "[BROWSER_DIALOG_RESPONSE_FAILED] response failed or delivery is uncertain; \
                 inspect the tab before retrying. The triggering action was not replayed",
            )
        })?;
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
