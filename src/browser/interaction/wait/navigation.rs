//! Explicit same-origin destination waits following one native input.
//!
//! Capture the source before input. Poll only browser frame metadata until a
//! changed document/URL matches the caller's exact destination. Then pin that
//! incarnation for readiness and optional DOM conditions; never replay input.

use super::{
    AgentCx, Cdp, Document, Error, POLL_INTERVAL, Result, ToolOutput, Value, json, output,
};

const MAX_URL_BYTES: usize = 16 * 1024;
// interactive/complete alone precede the corresponding DOM events. Only
// navigation timing from this admitted document proves an event has started.
const READY_PROBE: &str = "(() => { const n = performance.getEntriesByType('navigation')[0]; return {url:location.href,state:document.readyState,domContentLoaded:!!n && n.domContentLoadedEventStart > 0,load:!!n && n.loadEventStart > 0}; })()";

fn error(code: &'static str) -> Error {
    Error::tool("browser", code)
}

fn usage() -> Error {
    error(
        "BROWSER_NAVIGATION_USAGE: navigation requires an exact absolute HTTP(S) url and optional wait_until commit/domcontentloaded/load; use one unmodified left click or keypress on an explicit main-document tab",
    )
}

fn http_url(raw: &str) -> Result<url::Url> {
    if raw.is_empty()
        || raw.len() > MAX_URL_BYTES
        || raw.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        || raw.contains('\\')
        || raw.split_once("://").is_none_or(|(scheme, _)| {
            !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https")
        })
    {
        return Err(usage());
    }
    let parsed = url::Url::parse(raw).map_err(|_| usage())?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(usage());
    }
    Ok(parsed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Until {
    Commit,
    DomContentLoaded,
    Load,
}

impl Until {
    const fn name(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::DomContentLoaded => "domcontentloaded",
            Self::Load => "load",
        }
    }

    fn ready(self, value: &Value) -> Result<bool> {
        let state = value["state"].as_str().unwrap_or_default();
        let (Some(dom), Some(load)) =
            (value["domContentLoaded"].as_bool(), value["load"].as_bool())
        else {
            return Err(error(
                "BROWSER_NAVIGATION_PROTOCOL: invalid lifecycle evidence",
            ));
        };
        if !matches!(state, "loading" | "interactive" | "complete")
            || (dom && state == "loading")
            || (load && (!dom || state != "complete"))
        {
            return Err(error(
                "BROWSER_NAVIGATION_PROTOCOL: invalid document readiness",
            ));
        }
        Ok(match self {
            Self::Commit => true,
            Self::DomContentLoaded => dom,
            Self::Load => load,
        })
    }
}

struct Options {
    target: url::Url,
    until: Until,
}

impl Options {
    fn parse(value: &Value) -> Result<Self> {
        let fields = value.as_object().ok_or_else(usage)?;
        if fields
            .keys()
            .any(|key| !matches!(key.as_str(), "url" | "wait_until"))
        {
            return Err(usage());
        }
        let target = http_url(value["url"].as_str().ok_or_else(usage)?)?;
        let until = match value.get("wait_until") {
            None => Until::Load,
            Some(Value::String(value)) => match value.as_str() {
                "commit" => Until::Commit,
                "domcontentloaded" => Until::DomContentLoaded,
                "load" => Until::Load,
                _ => return Err(usage()),
            },
            _ => return Err(usage()),
        };
        Ok(Self { target, until })
    }
}

pub(super) fn schema() -> Value {
    json!({
        "type":"object","required":["url"],"additionalProperties":false,
        "description":"Explicit post-input navigation, same HTTP(S) security origin only. Requires a changed loader or URL; an already-matching unchanged page is insufficient. Main document and explicit tab only. Refuses cross-origin redirects, error documents, old @eN references and new-tab click modifiers. Optional selector/text then observes the pinned destination. No navigation is initiated by this wait; timeout includes the trigger.",
        "properties":{
            "url":{"type":"string","minLength":1,"maxLength":16384,"description":"Exact absolute destination including query and fragment; normal URL canonicalization applies. No credentials, glob or regular expression."},
            "wait_until":{"type":"string","enum":["commit","domcontentloaded","load"],"default":"load","description":"commit: matching frame metadata; domcontentloaded/load: the admitted document's navigation timing records that event has begun. Neither implies HTTP success, network idle or application readiness."}
        }
    })
}

pub(super) fn validate(args: &Value, after: &Value) -> Result<bool> {
    let Some(value) = after.get("navigation") else {
        return Ok(false);
    };
    Options::parse(value)?;
    if !matches!(args["action"].as_str(), Some("click" | "press"))
        || args.get("frame").is_some()
        || args.get("dialog_response").is_some()
        || args["tab"].as_str().is_none_or(str::is_empty)
        || after["selector"]
            .as_str()
            .is_some_and(|selector| selector.starts_with('@'))
    {
        return Err(usage());
    }
    if args["action"] == "click"
        && (args
            .get("button")
            .is_some_and(|value| value.as_str() != Some("left"))
            || args
                .get("click_count")
                .is_some_and(|value| value.as_u64() != Some(1))
            || args
                .get("modifiers")
                .is_some_and(|value| value.as_array().is_none_or(|values| !values.is_empty())))
    {
        return Err(usage());
    }
    Ok(true)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    document: Document,
    url: url::Url,
}

impl Snapshot {
    fn parse(raw: &Value) -> Result<Self> {
        let malformed = || error("BROWSER_NAVIGATION_PROTOCOL: invalid frame metadata");
        let id = |key: &str| {
            raw[key]
                .as_str()
                .filter(|value| {
                    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
                })
                .map(str::to_owned)
                .ok_or_else(malformed)
        };
        if raw
            .get("unreachableUrl")
            .is_some_and(|value| value.as_str() != Some(""))
        {
            return Err(error(
                "BROWSER_NAVIGATION_FAILED: browser reported an unreachable document",
            ));
        }
        let mut address =
            http_url(raw["url"].as_str().ok_or_else(malformed)?).map_err(|_| malformed())?;
        // CDP separates the fragment, including '#', from Frame.url.
        if let Some(fragment) = raw.get("urlFragment") {
            let fragment = fragment.as_str().ok_or_else(malformed)?;
            if !fragment.is_empty() {
                let value = fragment.strip_prefix('#').ok_or_else(malformed)?;
                if fragment.len() > MAX_URL_BYTES
                    || fragment
                        .chars()
                        .any(|ch| ch.is_control() || ch.is_whitespace())
                    || address.fragment().is_some_and(|current| current != value)
                {
                    return Err(malformed());
                }
                address.set_fragment(Some(value));
            }
        }
        if address.as_str().len() > MAX_URL_BYTES {
            return Err(malformed());
        }
        let origin =
            http_url(raw["securityOrigin"].as_str().ok_or_else(malformed)?).map_err(|_| {
                error("BROWSER_NAVIGATION_SCOPE: opaque or unavailable browser security origin")
            })?;
        if origin.origin() != address.origin()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(error(
                "BROWSER_NAVIGATION_SCOPE: frame URL and security origin disagree",
            ));
        }
        Ok(Self {
            document: Document {
                frame: id("id")?,
                loader: id("loaderId")?,
            },
            url: address,
        })
    }
}

async fn snapshot(owner: &AgentCx, cdp: &mut Cdp) -> Result<Snapshot> {
    owner
        .checkpoint()
        .map_err(|_| error("BROWSER_WAIT_CANCELLED"))?;
    Snapshot::parse(&cdp.frame_document(owner).await?)
}

pub(in crate::browser::interaction) struct Navigation {
    before: Snapshot,
    options: Options,
}

/// Also constrain same-document history changes during the optional DOM wait.
/// A loader-only check cannot notice a SPA changing its URL between samples.
pub(in crate::browser::interaction) struct Destination<'a> {
    navigation: &'a Navigation,
    pinned: &'a Snapshot,
}

impl Destination<'_> {
    pub(super) async fn verify(&self, owner: &AgentCx, cdp: &mut Cdp) -> Result<()> {
        self.navigation.verify(owner, cdp, self.pinned).await
    }
}

pub(in crate::browser::interaction) async fn prepare(
    owner: &AgentCx,
    cdp: &mut Cdp,
    condition: &Value,
    expected: &Document,
) -> Result<Option<Navigation>> {
    let Some(value) = condition.get("navigation") else {
        return Ok(None);
    };
    if cdp.frame_selected() {
        return Err(usage());
    }
    let options = Options::parse(value)?;
    let before = snapshot(owner, cdp).await?;
    if &before.document != expected {
        return Err(error(
            "BROWSER_NAVIGATION_STALE: source changed before input",
        ));
    }
    if before.url.origin() != options.target.origin() {
        return Err(error(
            "BROWSER_NAVIGATION_SCOPE: destination must share the source document's security origin",
        ));
    }
    Ok(Some(Navigation { before, options }))
}

impl Navigation {
    fn matches(&self, current: &Snapshot) -> Result<bool> {
        if current.document.frame != self.before.document.frame {
            return Err(error(
                "BROWSER_NAVIGATION_STALE: original main frame was replaced",
            ));
        }
        if current.url.origin() != self.before.url.origin() {
            return Err(error(
                "BROWSER_NAVIGATION_SCOPE: navigation left the admitted security origin",
            ));
        }
        Ok(current != &self.before && current.url == self.options.target)
    }

    async fn verify(&self, owner: &AgentCx, cdp: &mut Cdp, pinned: &Snapshot) -> Result<()> {
        let current = snapshot(owner, cdp).await?;
        if !self.matches(&current)? || &current != pinned {
            return Err(error(
                "BROWSER_NAVIGATION_STALE: destination changed during readiness observation",
            ));
        }
        Ok(())
    }

    async fn ready(&self, owner: &AgentCx, cdp: &mut Cdp, pinned: &Snapshot) -> Result<bool> {
        self.verify(owner, cdp, pinned).await?;
        if self.options.until == Until::Commit {
            return Ok(true);
        }
        let world = cdp
            .command(
                owner,
                "Page.createIsolatedWorld",
                json!({
                    "frameId":pinned.document.frame,"worldName":"pi-browser-tools",
                }),
            )
            .await?;
        let context = world["executionContextId"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(|| {
                error("BROWSER_NAVIGATION_PROTOCOL: missing isolated destination world")
            })?;
        self.verify(owner, cdp, pinned).await?;
        let response = cdp
            .command(
                owner,
                "Runtime.evaluate",
                json!({
                    "contextId":context,"expression":READY_PROBE,"returnByValue":true,
                }),
            )
            .await?;
        if response.get("exceptionDetails").is_some() {
            return Err(error(
                "BROWSER_NAVIGATION_STALE: destination readiness could not be observed",
            ));
        }
        let value = &response["result"]["value"];
        let observed_url = http_url(value["url"].as_str().unwrap_or_default())
            .map_err(|_| error("BROWSER_NAVIGATION_PROTOCOL: invalid readiness URL"))?;
        if observed_url != pinned.url {
            return Err(error(
                "BROWSER_NAVIGATION_STALE: isolated world does not match the destination",
            ));
        }
        let ready = self.options.until.ready(value)?;
        self.verify(owner, cdp, pinned).await?;
        Ok(ready)
    }

    pub(in crate::browser::interaction) async fn finish(
        self,
        owner: &AgentCx,
        cdp: &mut Cdp,
        tab: &str,
        condition: &Value,
    ) -> Result<ToolOutput> {
        let pinned = loop {
            let current = snapshot(owner, cdp).await?;
            if self.matches(&current)? {
                break current;
            }
            owner.time().sleep(POLL_INTERVAL).await;
        };
        while !self.ready(owner, cdp, &pinned).await? {
            owner.time().sleep(POLL_INTERVAL).await;
        }
        let mut result = if condition.get("selector").is_some() {
            // Old snapshot references cannot migrate into the destination.
            let destination = Destination {
                navigation: &self,
                pinned: &pinned,
            };
            super::execute_pinned(
                owner,
                cdp,
                tab,
                None,
                condition,
                &pinned.document,
                Some(&destination),
            )
            .await?
        } else {
            output(
                "Requested navigation destination reached.",
                json!({"matched":true,"tab":tab,"backend":"cdp"}),
            )
        };
        self.verify(owner, cdp, &pinned).await?;
        let details = result.details.as_mut().expect("native readiness details");
        details["navigation"] = json!({
            "matched":true,"url":self.options.target.as_str(),"wait_until":self.options.until.name(),
            "same_document":pinned.document == self.before.document,"scope":"same-origin",
            "document_pinned":true,"http_success_verified":false,"causality_verified":false,
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
