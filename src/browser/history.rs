//! Native main-document history traversal with destination policy checks.
//!
//! Back/forward select an existing Chromium navigation entry before dispatch.
//! Reload retains the current entry but requires a new document loader. Every
//! action waits for the admitted document to become interactive/complete and
//! never synthesizes keyboard shortcuts or replays a failed traversal.

use super::cdp::Cdp;
use super::{output, policy};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};

const MAX_HISTORY_ENTRIES: usize = 4096;
const MAX_SETTLE_EVENTS: usize = 8192;
const MAX_URL_BYTES: usize = 64 * 1024;

fn error(message: &'static str) -> Error {
    Error::tool("browser", message)
}

pub(super) fn validate(args: &Value) -> Result<()> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or_default();
    if !matches!(action, "back" | "forward" | "reload")
        || args.as_object().is_none_or(|fields| {
            fields.keys().any(|field| !matches!(field.as_str(), "action" | "tab" | "timeout_ms"))
        })
    {
        return Err(error(
            "BROWSER_HISTORY_USAGE: back, forward and reload accept only action, optional tab and timeout_ms; frame, URL overrides and implicit dialog responses are not supported",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    id: i64,
    url: String,
}

#[derive(Clone, Debug)]
struct History {
    current: usize,
    entries: Vec<Entry>,
}

impl History {
    fn parse(value: &Value) -> Result<Self> {
        let entries = value.get("entries").and_then(Value::as_array)
            .filter(|entries| !entries.is_empty() && entries.len() <= MAX_HISTORY_ENTRIES)
            .ok_or_else(|| error("BROWSER_HISTORY_PROTOCOL: invalid or oversized navigation history"))?;
        let current = value.get("currentIndex").and_then(Value::as_i64)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < entries.len())
            .ok_or_else(|| error("BROWSER_HISTORY_PROTOCOL: invalid current history index"))?;
        let mut parsed = Vec::with_capacity(entries.len());
        for entry in entries {
            let id = entry.get("id").and_then(Value::as_i64)
                .ok_or_else(|| error("BROWSER_HISTORY_PROTOCOL: history entry has no integer id"))?;
            let url = entry.get("url").and_then(Value::as_str)
                .filter(|url| !url.is_empty() && url.len() <= MAX_URL_BYTES && !url.chars().any(char::is_control))
                .ok_or_else(|| error("BROWSER_HISTORY_PROTOCOL: history entry has an invalid URL"))?;
            parsed.push(Entry { id, url: url.to_owned() });
        }
        Ok(Self { current, entries: parsed })
    }

    fn destination(&self, action: &str) -> Result<(usize, &Entry)> {
        let index = match action {
            "back" => self.current.checked_sub(1)
                .ok_or_else(|| error("BROWSER_HISTORY_BOUNDARY: no previous history entry"))?,
            "forward" => self.current.checked_add(1)
                .filter(|index| *index < self.entries.len())
                .ok_or_else(|| error("BROWSER_HISTORY_BOUNDARY: no forward history entry"))?,
            _ => return Err(error("BROWSER_HISTORY_USAGE: invalid traversal action")),
        };
        Ok((index, &self.entries[index]))
    }
}

fn document_identity(value: &Value) -> Result<(String, String, String)> {
    let field = |name: &str| {
        value.get(name).and_then(Value::as_str)
            .filter(|text| !text.is_empty() && text.len() <= MAX_URL_BYTES && !text.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or_else(|| error("BROWSER_HISTORY_PROTOCOL: selected document identity is invalid"))
    };
    Ok((field("id")?, field("loaderId")?, field("url")?))
}

async fn metadata(owner: &AgentCx, cdp: &mut Cdp, allowlist: Option<&[String]>) -> Result<(String, String, String)> {
    let value = cdp.evaluate(owner, "({url: location.href, title: document.title, readyState: document.readyState})").await?;
    let url = value.get("url").and_then(Value::as_str)
        .filter(|url| !url.is_empty() && url.len() <= MAX_URL_BYTES)
        .ok_or_else(|| error("BROWSER_HISTORY_PROTOCOL: destination URL is unavailable"))?;
    policy::check_navigation(url, allowlist)
        .map_err(|_| error("BROWSER_HISTORY_POLICY: destination is blocked by browser URL policy"))?;
    let title = value.get("title").and_then(Value::as_str)
        .filter(|title| title.len() <= MAX_URL_BYTES)
        .ok_or_else(|| error("BROWSER_HISTORY_PROTOCOL: destination title is unavailable"))?;
    let ready = value.get("readyState").and_then(Value::as_str)
        .filter(|ready| matches!(*ready, "loading" | "interactive" | "complete"))
        .ok_or_else(|| error("BROWSER_HISTORY_PROTOCOL: destination readyState is invalid"))?;
    Ok((url.to_owned(), title.to_owned(), ready.to_owned()))
}

async fn history(owner: &AgentCx, cdp: &mut Cdp) -> Result<History> {
    History::parse(&cdp.command(owner, "Page.getNavigationHistory", json!({})).await?)
}

async fn settle_traversal(
    owner: &AgentCx,
    cdp: &mut Cdp,
    target_id: i64,
    target_index: usize,
    allowlist: Option<&[String]>,
) -> Result<(String, String)> {
    for _ in 0..MAX_SETTLE_EVENTS {
        let current = history(owner, cdp).await?;
        if current.current == target_index && current.entries[current.current].id == target_id {
            let (url, title, ready) = metadata(owner, cdp, allowlist).await?;
            if ready != "loading" {
                return Ok((url, title));
            }
        }
        cdp.pump_event(owner).await?;
    }
    Err(error("BROWSER_HISTORY_SETTLE: history traversal did not reach a ready destination"))
}

async fn settle_reload(
    owner: &AgentCx,
    cdp: &mut Cdp,
    frame_id: &str,
    old_loader: &str,
    allowlist: Option<&[String]>,
) -> Result<(String, String)> {
    for _ in 0..MAX_SETTLE_EVENTS {
        let document = cdp.frame_document(owner).await?;
        let (frame, loader, _) = document_identity(&document)?;
        if frame != frame_id {
            return Err(error("BROWSER_HISTORY_STALE: main frame identity changed during reload"));
        }
        if loader != old_loader {
            let (url, title, ready) = metadata(owner, cdp, allowlist).await?;
            if ready != "loading" {
                return Ok((url, title));
            }
        }
        cdp.pump_event(owner).await?;
    }
    Err(error("BROWSER_HISTORY_SETTLE: reload did not produce a ready replacement document"))
}

pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    args: &Value,
    allowlist: Option<&[String]>,
) -> Result<ToolOutput> {
    validate(args)?;
    let action = args["action"].as_str().ok_or_else(|| error("BROWSER_HISTORY_USAGE"))?;
    cdp.command(owner, "Page.enable", json!({})).await?;
    cdp.command(owner, "Page.setLifecycleEventsEnabled", json!({"enabled":true})).await?;
    let source = cdp.frame_document(owner).await?;
    let (frame_id, loader_id, source_url) = document_identity(&source)?;
    policy::check_navigation(&source_url, allowlist)
        .map_err(|_| error("BROWSER_HISTORY_POLICY: current document is blocked by browser URL policy"))?;

    let (url, title, history_index, history_entry_id, document_changed) = match action {
        "back" | "forward" => {
            let current = history(owner, cdp).await?;
            let (index, entry) = current.destination(action)?;
            policy::check_navigation(&entry.url, allowlist)
                .map_err(|_| error("BROWSER_HISTORY_POLICY: destination is blocked by browser URL policy"))?;
            let target_id = entry.id;
            cdp.command(owner, "Page.navigateToHistoryEntry", json!({"entryId":target_id})).await?;
            let (url, title) = settle_traversal(owner, cdp, target_id, index, allowlist).await?;
            let final_document = cdp.frame_document(owner).await?;
            let (_, final_loader, _) = document_identity(&final_document)?;
            (url, title, Some(index), Some(target_id), final_loader != loader_id)
        }
        "reload" => {
            cdp.command(owner, "Page.reload", json!({})).await?;
            let (url, title) = settle_reload(owner, cdp, &frame_id, &loader_id, allowlist).await?;
            let current = history(owner, cdp).await?;
            (url, title, Some(current.current), Some(current.entries[current.current].id), true)
        }
        _ => return Err(error("BROWSER_HISTORY_USAGE: invalid history action")),
    };

    Ok(output(
        format!("{action} completed in tab {tab}: {url} (Title: \"{title}\")"),
        json!({
            "action":action,"tab":tab,"url":url,"title":title,"loaded":true,
            "history_index":history_index,"history_entry_id":history_entry_id,
            "document_changed":document_changed,"backend":"cdp","action_replayed":false
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({"currentIndex":1,"entries":[
            {"id":10,"url":"https://example.com/a"},
            {"id":11,"url":"https://example.com/b"},
            {"id":12,"url":"https://example.com/c"}
        ]})
    }

    #[test]
    fn history_selection_is_exact_and_bounded() {
        let history = History::parse(&sample()).unwrap();
        assert_eq!(history.destination("back").unwrap(), (0, &history.entries[0]));
        assert_eq!(history.destination("forward").unwrap(), (2, &history.entries[2]));
        let first = History::parse(&json!({"currentIndex":0,"entries":[{"id":1,"url":"https://example.com"}]})).unwrap();
        assert!(first.destination("back").is_err());
        assert!(first.destination("forward").is_err());
    }

    #[test]
    fn malformed_history_never_turns_into_navigation_authority() {
        for value in [
            json!({}), json!({"currentIndex":-1,"entries":[{"id":1,"url":"https://example.com"}]}),
            json!({"currentIndex":0,"entries":[]}),
            json!({"currentIndex":0,"entries":[{"id":"1","url":"https://example.com"}]}),
            json!({"currentIndex":0,"entries":[{"id":1,"url":"PRIVATE-CANARY\n"}]}),
        ] {
            let failure = History::parse(&value).unwrap_err().to_string();
            assert!(!failure.contains("PRIVATE-CANARY"));
        }
    }

    #[test]
    fn history_actions_reject_ignored_control_fields() {
        for action in ["back","forward","reload"] {
            assert!(validate(&json!({"action":action,"tab":"work","timeout_ms":1000})).is_ok());
            for field in ["frame","url","selector","dialog_response","diagnostics"] {
                let mut args = json!({"action":action,"tab":"work"});
                args[field] = json!("PRIVATE-CANARY");
                let failure = validate(&args).unwrap_err().to_string();
                assert!(!failure.contains("PRIVATE-CANARY"));
            }
        }
    }

    #[test]
    fn document_identity_rejects_partial_or_control_bearing_values() {
        let valid = json!({"id":"main","loaderId":"loader","url":"https://example.com"});
        assert!(document_identity(&valid).is_ok());
        for field in ["id","loaderId","url"] {
            let mut invalid = valid.clone();
            invalid[field] = json!("PRIVATE-CANARY\n");
            let failure = document_identity(&invalid).unwrap_err().to_string();
            assert!(!failure.contains("PRIVATE-CANARY"));
        }
    }
}
