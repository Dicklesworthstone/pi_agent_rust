//! Browser-context ownership for native download policy and recovery.
//!
//! Contexts come from the selected target, never from model arguments. An
//! unrecognized context is not a reason to fall back to the default profile.

use super::Cdp;
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const MAX_CONTEXTS: usize = 128;

fn error(message: &'static str) -> Error {
    Error::tool("browser", message)
}

fn identifier(value: &Value) -> Result<String> {
    value
        .as_str()
        .filter(|value| {
            !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        })
        .map(str::to_owned)
        .ok_or_else(|| error("BROWSER_CONTEXT_METADATA: invalid bounded identifier"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Context {
    Default,
    Named(String),
}

impl Context {
    pub(super) fn metadata(&self) -> Value {
        match self {
            Self::Default => json!({"kind":"default"}),
            Self::Named(id) => json!({"kind":"named", "id":id}),
        }
    }

    fn parameters(&self, method: &str, params: Value) -> Result<Value> {
        if !matches!(method, "Browser.setDownloadBehavior" | "Browser.cancelDownload") {
            return Err(error("BROWSER_CONTEXT_COMMAND: unsupported context operation"));
        }
        let mut params = params.as_object().cloned().ok_or_else(|| {
            error("BROWSER_CONTEXT_COMMAND: context command parameters must be an object")
        })?;
        if params.contains_key("browserContextId") {
            return Err(error("BROWSER_CONTEXT_COMMAND: context overrides are not permitted"));
        }
        if let Self::Named(id) = self {
            params.insert("browserContextId".to_owned(), json!(id));
        }
        Ok(Value::Object(params))
    }

    pub(super) async fn command(
        &self,
        owner: &AgentCx,
        cdp: &mut Cdp,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        owner.checkpoint().map_err(|_| error("BROWSER_CONTEXT_CANCELLED"))?;
        cdp.browser_command(owner, method, self.parameters(method, params)?)
            .await
            .map_err(|_| error("BROWSER_CONTEXT_COMMAND: browser context operation failed"))
    }

    /// Called before any subsequent browser action. A destroyed CDP-created
    /// context needs no policy reset; a failed inventory or reset retains the
    /// pending marker. Never retry a failed named reset against the default.
    pub(super) async fn restore(&self, owner: &AgentCx, cdp: &mut Cdp) -> Result<()> {
        if let Self::Named(id) = self {
            let catalog = Catalog::read(owner, cdp).await?;
            if !catalog.named.contains(id) {
                return Ok(());
            }
        }
        self.command(
            owner,
            cdp,
            "Browser.setDownloadBehavior",
            json!({"behavior":"deny", "eventsEnabled":true}),
        )
        .await?;
        Ok(())
    }
}

#[derive(Debug)]
struct Catalog {
    named: BTreeSet<String>,
    default: Option<String>,
}

impl Catalog {
    fn parse(value: &Value) -> Result<Self> {
        let ids = value["browserContextIds"]
            .as_array()
            .filter(|ids| ids.len() <= MAX_CONTEXTS)
            .ok_or_else(|| error("BROWSER_CONTEXT_METADATA: invalid context inventory"))?;
        let mut named = BTreeSet::new();
        for value in ids {
            if !named.insert(identifier(value)?) {
                return Err(error("BROWSER_CONTEXT_METADATA: duplicate context identity"));
            }
        }
        let default = value.get("defaultBrowserContextId").map(identifier).transpose()?;
        if default.as_ref().is_some_and(|id| named.contains(id)) {
            return Err(error("BROWSER_CONTEXT_METADATA: conflicting default context identity"));
        }
        Ok(Self { named, default })
    }

    async fn read(owner: &AgentCx, cdp: &mut Cdp) -> Result<Self> {
        owner.checkpoint().map_err(|_| error("BROWSER_CONTEXT_CANCELLED"))?;
        let response = cdp.browser_command(owner, "Target.getBrowserContexts", json!({}))
            .await
            .map_err(|_| error("BROWSER_CONTEXT_METADATA: cannot enumerate browser contexts"))?;
        Self::parse(&response)
    }

    fn classify(&self, id: &str) -> Option<Context> {
        if self.named.contains(id) {
            Some(Context::Named(id.to_owned()))
        } else if self.default.as_deref() == Some(id) {
            Some(Context::Default)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Binding {
    target: String,
    raw_context: String,
    pub(super) context: Context,
}

impl Binding {
    fn verify(&self, info: &Value) -> Result<()> {
        if info["type"] != "page"
            || identifier(&info["targetId"])? != self.target
            || identifier(&info["browserContextId"])? != self.raw_context
        {
            return Err(error("BROWSER_CONTEXT_CHANGED: selected target changed context identity"));
        }
        Ok(())
    }

    pub(super) async fn check(&self, owner: &AgentCx, cdp: &mut Cdp) -> Result<()> {
        owner.checkpoint().map_err(|_| error("BROWSER_CONTEXT_CANCELLED"))?;
        let response = cdp.browser_command(
            owner, "Target.getTargetInfo", json!({"targetId":self.target}),
        ).await.map_err(|_| error("BROWSER_CONTEXT_CHANGED: selected target is unavailable"))?;
        self.verify(&response["targetInfo"])
    }
}

/// Older Chromium returns a default-profile ID in TargetInfo, but neither
/// lists it in getBrowserContexts nor accepts it in Browser download commands.
/// Identify that profile through a protocol-created, hidden default target.
/// Hidden targets belong to this connection and disappear on disconnect even
/// if cancellation prevents the explicit close. No user target is navigated.
async fn default_identity(owner: &AgentCx, cdp: &mut Cdp, selected: &str) -> Result<String> {
    let created = cdp.browser_command(owner, "Target.createTarget", json!({
        "url":"about:blank", "hidden":true, "background":true,
    })).await.map_err(|_| error("BROWSER_CONTEXT_UNKNOWN: default context identity is unavailable"))?;
    let target = identifier(&created["targetId"])?;
    if target == selected {
        return Err(error("BROWSER_CONTEXT_METADATA: identity probe reused the selected target"));
    }
    let inspected = cdp.browser_command(
        owner, "Target.getTargetInfo", json!({"targetId":target}),
    ).await;
    // Close only the hidden target we just created; never dispose a context.
    let closed = cdp.browser_command(
        owner, "Target.closeTarget", json!({"targetId":target}),
    ).await;
    let inspected = inspected.map_err(|_| {
        error("BROWSER_CONTEXT_UNKNOWN: cannot inspect default context identity")
    })?;
    if closed.ok().is_none_or(|value| value["success"] != true) {
        return Err(error("BROWSER_CONTEXT_UNKNOWN: hidden identity target did not close"));
    }
    let info = &inspected["targetInfo"];
    if identifier(&info["targetId"])? != target
        || !matches!(info["type"].as_str(), Some("page" | "other"))
        || info["url"] != "about:blank"
    {
        return Err(error("BROWSER_CONTEXT_METADATA: invalid default identity target"));
    }
    identifier(&info["browserContextId"])
}

/// The legacy protocol form with no context field retains default semantics.
/// A present field is strict: null, unknown or malformed values never silently
/// select the default. For named contexts, deny precedes attachment and input.
pub(super) async fn select(
    owner: &AgentCx,
    cdp: &mut Cdp,
    info: &Value,
) -> Result<Option<Binding>> {
    let Some(raw) = info.get("browserContextId") else {
        return Ok(None);
    };
    let raw_context = identifier(raw)?;
    let target = identifier(&info["targetId"])?;
    if info["type"] != "page" {
        return Err(error("BROWSER_CONTEXT_METADATA: selected target is not a page"));
    }
    let mut catalog = Catalog::read(owner, cdp).await?;
    if catalog.classify(&raw_context).is_none() && catalog.default.is_none() {
        let default = default_identity(owner, cdp, &target).await?;
        if catalog.named.contains(&default) {
            return Err(error("BROWSER_CONTEXT_METADATA: default identity overlaps a named context"));
        }
        catalog.default = Some(default);
    }
    let context = catalog.classify(&raw_context).ok_or_else(|| {
        error("BROWSER_CONTEXT_UNKNOWN: target is outside the default or CDP-created contexts")
    })?;
    let binding = Binding { target, raw_context, context };
    binding.check(owner, cdp).await?;
    if matches!(binding.context, Context::Named(_)) {
        binding.context.command(owner, cdp, "Browser.setDownloadBehavior", json!({
            "behavior":"deny", "eventsEnabled":true,
        })).await?;
        binding.check(owner, cdp).await?;
    }
    Ok(Some(binding))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_inventory_is_closed_bounded_and_unambiguous() {
        let catalog = Catalog::parse(&json!({
            "browserContextIds":["incognito"], "defaultBrowserContextId":"ordinary",
        })).unwrap();
        assert_eq!(catalog.classify("incognito"), Some(Context::Named("incognito".into())));
        assert_eq!(catalog.classify("ordinary"), Some(Context::Default));
        assert_eq!(catalog.classify("unlisted"), None);
        for value in [json!({}), json!({"browserContextIds":null}),
            json!({"browserContextIds":["same","same"]}),
            json!({"browserContextIds":["same"],"defaultBrowserContextId":"same"}),
            json!({"browserContextIds":[],"defaultBrowserContextId":null}),
            json!({"browserContextIds":["PRIVATE-CANARY\n"]}),
            json!({"browserContextIds":["x".repeat(257)]})] {
            let failure = Catalog::parse(&value).unwrap_err().to_string();
            assert!(!failure.contains("PRIVATE-CANARY"));
        }
        let ids: Vec<_> = (0..MAX_CONTEXTS).map(|i| format!("context-{i}")).collect();
        assert!(Catalog::parse(&json!({"browserContextIds":ids})).is_ok());
        let ids: Vec<_> = (0..=MAX_CONTEXTS).map(|i| format!("context-{i}")).collect();
        assert!(Catalog::parse(&json!({"browserContextIds":ids})).is_err());
    }

    #[test]
    fn an_unlisted_profile_id_is_not_implicitly_the_default() {
        let catalog = Catalog::parse(&json!({"browserContextIds":["named"]})).unwrap();
        assert_eq!(catalog.classify("ordinary-profile-id"), None);
        assert_eq!(catalog.classify("unmanaged-incognito-id"), None);
    }

    #[test]
    fn policy_and_cancellation_keep_the_same_explicit_context() {
        let context = Context::Named("private".into());
        for (method, params) in [
            ("Browser.setDownloadBehavior", json!({"behavior":"allowAndName","downloadPath":"/private/staging","eventsEnabled":true})),
            ("Browser.setDownloadBehavior", json!({"behavior":"deny","eventsEnabled":true})),
            ("Browser.cancelDownload", json!({"guid":"transfer"})),
        ] {
            let scoped = context.parameters(method, params.clone()).unwrap();
            assert_eq!(scoped["browserContextId"], "private");
            assert_eq!(Context::Default.parameters(method, params.clone()).unwrap(), params);
            for (key, value) in params.as_object().unwrap() {
                assert_eq!(scoped[key], *value);
            }
        }
    }

    #[test]
    fn context_parameters_do_not_accept_overrides_or_arbitrary_commands() {
        for context in [Context::Default, Context::Named("owned".into())] {
            assert!(context.parameters("Browser.close", json!({})).is_err());
            assert!(context.parameters("Browser.cancelDownload", Value::Null).is_err());
            for value in [Value::Null, json!("owned"), json!("other")] {
                assert!(context.parameters("Browser.cancelDownload", json!({
                    "guid":"job", "browserContextId":value,
                })).is_err());
            }
        }
    }

    #[test]
    fn context_binding_rejects_replacement_targets_and_missing_identity() {
        let binding = Binding { target:"page".into(), raw_context:"private".into(),
            context:Context::Named("private".into()) };
        let good = json!({"targetId":"page","type":"page","browserContextId":"private"});
        binding.verify(&good).unwrap();
        for field in ["targetId", "type", "browserContextId"] {
            let mut wrong = good.clone();
            wrong[field] = json!("PRIVATE-CANARY");
            let failure = binding.verify(&wrong).unwrap_err().to_string();
            assert!(!failure.contains("PRIVATE-CANARY"));
            wrong[field] = Value::Null;
            assert!(binding.verify(&wrong).is_err());
        }
    }
}
