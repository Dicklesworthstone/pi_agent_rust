//! Explicit, origin-scoped persistence for an owned browser's authentication state.
//!
//! Credential values never enter tool output or diagnostics. The normal browser
//! owner lease, cancellation budget and CDP connection remain authoritative.

use super::cdp::Cdp;
use super::{launch, output, policy, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Component, Path};

mod restore;

const SCHEMA: &str = "pi.browser.storage-state.v1";
const MAX_STATE_BYTES: usize = 2 * 1024 * 1024;
const MAX_COOKIES: usize = 256;
const MAX_STORAGE_ENTRIES: usize = 1024;
const MAX_KEY_BYTES: usize = 4096;
const MAX_VALUE_BYTES: usize = 128 * 1024;

fn error(message: &'static str) -> Error {
    Error::tool("browser", message)
}

pub(super) fn is_action(action: &str) -> bool {
    matches!(action, "save_state" | "restore_state")
}

/// Keep the schema beside its implementation so an unsupported action cannot
/// become discoverable merely because the generic browser dispatcher accepts it.
pub(super) fn parameters(mut schema: Value) -> Value {
    schema["properties"]["action"]["enum"]
        .as_array_mut()
        .expect("browser action enum")
        .extend([json!("save_state"), json!("restore_state")]);
    schema["properties"]["state_path"] = json!({
        "type":"string", "maxLength":4096,
        "description":"save_state/restore_state: required workspace-relative .json file and explicit tab, Unix native backend only. Save creates a private file with that HTTP(S) origin's localStorage and matching unpartitioned cookies, including HttpOnly cookies. Restore reads a private singly linked regular file, requires the same origin including scheme/port, merges saved cookies/keys without clearing unrelated state, and verifies retained values. Files contain plaintext credentials: never commit or share. Save never overwrites files. No inline credential values, frames, diagnostics, batch steps, attached profiles or automatic launch/navigation/login. Partitioned/same-party cookies, IndexedDB and sessionStorage are unsupported. Restore failure may leave partial effects; no automatic retry or rollback."
    });
    schema
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || Path::new(path).is_absolute()
        || Path::new(path).file_name().is_none()
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        || !Path::new(path)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        return Err(error(
            "BROWSER_STATE_PATH: state_path must be a control-free workspace-relative .json file without parent traversal or backslashes",
        ));
    }
    Ok(())
}

pub(super) fn validate(args: &Value) -> Result<()> {
    let action = required(args, "action")?;
    if !is_action(action) {
        return Err(error(
            "BROWSER_STATE_INVALID: unsupported storage-state action",
        ));
    }
    let object = args
        .as_object()
        .ok_or_else(|| error("BROWSER_STATE_INVALID: expected an object"))?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "action" | "tab" | "state_path" | "timeout_ms"))
    {
        return Err(error(
            "BROWSER_STATE_INVALID: storage-state actions accept only action, explicit tab, state_path and timeout_ms",
        ));
    }
    let tab = required(args, "tab")?;
    if tab.is_empty() || tab.len() > 256 || tab.chars().any(char::is_control) {
        return Err(error(
            "BROWSER_STATE_INVALID: an explicit control-free tab is required",
        ));
    }
    validate_path(required(args, "state_path")?)?;
    // The shared publisher creates private descriptor-confined files here.
    // Do not claim credential-file privacy on a platform without this backend.
    if !cfg!(all(
        unix,
        not(any(target_os = "espidf", target_os = "redox"))
    )) {
        return Err(error(
            "BROWSER_STATE_PLATFORM: private browser-state persistence requires the native Unix file backend",
        ));
    }
    Ok(())
}

pub(super) fn validate_connection(args: &Value, connection: &launch::Connection) -> Result<()> {
    if is_action(required(args, "action")?) && matches!(connection, launch::Connection::Attach(_)) {
        return Err(error(
            "BROWSER_STATE_OWNERSHIP: storage-state persistence requires a Pi-owned browser, not a shared attached profile",
        ));
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct Document {
    frame: String,
    loader: String,
    origin: String,
    host: String,
    scheme: String,
}

impl Document {
    fn from_frame(frame: &Value, allowlist: Option<&[String]>) -> Result<Self> {
        let frame_id = required(frame, "id")?;
        let loader = required(frame, "loaderId")?;
        if frame_id.is_empty()
            || frame_id.len() > 256
            || loader.is_empty()
            || loader.len() > 256
            || frame_id.chars().any(char::is_control)
            || loader.chars().any(char::is_control)
        {
            return Err(error(
                "BROWSER_STATE_DOCUMENT: no stable main-document identity",
            ));
        }
        let raw_url = required(frame, "url")?;
        policy::check_navigation(raw_url, allowlist).map_err(|_| {
            error("BROWSER_STATE_ORIGIN: document is outside the admitted HTTP(S) origin")
        })?;
        let url = url::Url::parse(raw_url)
            .map_err(|_| error("BROWSER_STATE_ORIGIN: invalid document origin"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(error(
                "BROWSER_STATE_ORIGIN: storage requires a nonopaque HTTP(S) document without URL credentials",
            ));
        }
        let origin = url.origin().ascii_serialization();
        if frame.get("securityOrigin").and_then(Value::as_str) != Some(origin.as_str()) {
            return Err(error(
                "BROWSER_STATE_ORIGIN: document security origin is missing, opaque or does not match its URL",
            ));
        }
        Ok(Self {
            frame: frame_id.to_owned(),
            loader: loader.to_owned(),
            origin,
            host: url
                .host_str()
                .ok_or_else(|| error("BROWSER_STATE_ORIGIN: document has no host"))?
                .to_owned(),
            scheme: url.scheme().to_owned(),
        })
    }

    async fn capture(owner: &AgentCx, cdp: &mut Cdp, allowlist: Option<&[String]>) -> Result<Self> {
        let frame = cdp
            .frame_document(owner)
            .await
            .map_err(|_| error("BROWSER_STATE_DOCUMENT: cannot inspect the selected document"))?;
        Self::from_frame(&frame, allowlist)
    }

    async fn check(
        &self,
        owner: &AgentCx,
        cdp: &mut Cdp,
        allowlist: Option<&[String]>,
    ) -> Result<()> {
        checkpoint(owner)?;
        if *self != Self::capture(owner, cdp, allowlist).await? {
            return Err(error(
                "BROWSER_STATE_DOCUMENT_CHANGED: document changed; storage operation was not retargeted or replayed",
            ));
        }
        Ok(())
    }

    fn storage_id(&self) -> Value {
        json!({"securityOrigin":self.origin, "isLocalStorage":true})
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    http_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    same_site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_port: Option<i32>,
}

impl Cookie {
    fn validate(&self, document: &Document, allowlist: Option<&[String]>) -> Result<()> {
        if self.name.len() > MAX_KEY_BYTES
            || self.name.chars().any(char::is_control)
            || self.value.len() > MAX_VALUE_BYTES
            || self
                .value
                .chars()
                .any(|value| matches!(value, '\0' | '\r' | '\n'))
            || self.domain.len() > 253
            || self.path.len() > MAX_KEY_BYTES
            || !self.path.starts_with('/')
            || self.path.chars().any(char::is_control)
            || !domain_matches(&self.domain, &document.host)
            || (self.secure && document.scheme != "https")
            || self
                .expires
                .is_some_and(|value| !value.is_finite() || value <= 0.0)
            || self
                .same_site
                .as_deref()
                .is_some_and(|value| !matches!(value, "Strict" | "Lax" | "None"))
            || self
                .priority
                .as_deref()
                .is_some_and(|value| !matches!(value, "Low" | "Medium" | "High"))
            || self
                .source_scheme
                .as_deref()
                .is_some_and(|value| !matches!(value, "Unset" | "NonSecure" | "Secure"))
            || self
                .source_port
                .is_some_and(|value| value != -1 && !(1..=65535).contains(&value))
        {
            return Err(error(
                "BROWSER_STATE_COOKIE: invalid or out-of-scope cookie metadata",
            ));
        }
        let host = self.domain.strip_prefix('.').unwrap_or(&self.domain);
        let cookie_url = format!("{}://{host}/", document.scheme);
        policy::check_navigation(&cookie_url, allowlist)
            .map_err(|_| error("BROWSER_STATE_COOKIE_SCOPE: a cookie's domain exceeds the configured domain allowlist"))?;
        Ok(())
    }
}

fn domain_matches(domain: &str, host: &str) -> bool {
    let domain_cookie = domain.starts_with('.');
    let domain = domain.strip_prefix('.').unwrap_or(domain);
    if domain.is_empty() || domain.starts_with('.') || domain.chars().any(char::is_control) {
        return false;
    }
    match (url::Host::parse(domain), url::Host::parse(host)) {
        (Ok(url::Host::Domain(domain)), Ok(url::Host::Domain(host))) => {
            host == domain || (domain_cookie && host.ends_with(&format!(".{domain}")))
        }
        (Ok(domain), Ok(host)) => domain == host,
        _ => false,
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageEntry {
    name: String,
    value: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    schema: String,
    origin: String,
    cookies: Vec<Cookie>,
    local_storage: Vec<StorageEntry>,
}

/// Limit serialization itself, not just the completed allocation. Escaping can
/// expand credential strings substantially even when each input field is small.
struct StateBuffer(Vec<u8>);

impl std::io::Write for StateBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_STATE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("state byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl State {
    fn validate(&self, document: &Document, allowlist: Option<&[String]>) -> Result<()> {
        if self.schema != SCHEMA || self.origin != document.origin {
            return Err(error(
                "BROWSER_STATE_ORIGIN: state schema or origin does not match the explicitly selected document",
            ));
        }
        if self.cookies.len() > MAX_COOKIES || self.local_storage.len() > MAX_STORAGE_ENTRIES {
            return Err(error(
                "BROWSER_STATE_LIMIT: state exceeds 256 cookies or 1024 localStorage entries",
            ));
        }
        let mut cookies = BTreeSet::new();
        for cookie in &self.cookies {
            cookie.validate(document, allowlist)?;
            if !cookies.insert((&cookie.name, &cookie.domain, &cookie.path)) {
                return Err(error("BROWSER_STATE_COOKIE: duplicate cookie identity"));
            }
        }
        let mut names = BTreeSet::new();
        for entry in &self.local_storage {
            if entry.name.len() > MAX_KEY_BYTES
                || entry.value.len() > MAX_VALUE_BYTES
                || !names.insert(&entry.name)
            {
                return Err(error(
                    "BROWSER_STATE_STORAGE: duplicate or oversized localStorage entry",
                ));
            }
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut buffer = StateBuffer(Vec::new());
        serde_json::to_writer(&mut buffer, self).map_err(|_| {
            error("BROWSER_STATE_LIMIT: state serialization failed or exceeds 2 MiB")
        })?;
        Ok(buffer.0)
    }
}

fn cookies_from_response(response: &Value, document: &Document) -> Result<Vec<Cookie>> {
    let source = response
        .get("cookies")
        .and_then(Value::as_array)
        .filter(|cookies| cookies.len() <= 8192)
        .ok_or_else(|| {
            error("BROWSER_STATE_COOKIE: invalid or oversized browser cookie response")
        })?;
    let mut cookies = Vec::new();
    for item in source {
        let domain = item
            .get("domain")
            .and_then(Value::as_str)
            .ok_or_else(|| error("BROWSER_STATE_COOKIE: cookie response has no domain"))?;
        if !domain_matches(domain, &document.host) {
            continue;
        }
        let secure = item
            .get("secure")
            .and_then(Value::as_bool)
            .ok_or_else(|| error("BROWSER_STATE_COOKIE: cookie response has no secure flag"))?;
        if secure && document.scheme != "https" {
            continue;
        }
        if item
            .get("partitionKey")
            .is_some_and(|value| !value.is_null())
            || item
                .get("partitionKeyOpaque")
                .is_some_and(|value| value != &Value::Bool(false))
            || item
                .get("sameParty")
                .is_some_and(|value| value != &Value::Bool(false))
        {
            return Err(error(
                "BROWSER_STATE_COOKIE_UNSUPPORTED: matching partitioned or same-party cookies cannot be saved without changing their scope",
            ));
        }
        let session = item
            .get("session")
            .and_then(Value::as_bool)
            .ok_or_else(|| error("BROWSER_STATE_COOKIE: cookie response has no session flag"))?;
        let mut stored = serde_json::Map::new();
        for name in [
            "name",
            "value",
            "domain",
            "path",
            "secure",
            "httpOnly",
            "sameSite",
            "priority",
            "sourceScheme",
            "sourcePort",
        ] {
            if let Some(value) = item.get(name) {
                stored.insert(name.to_owned(), value.clone());
            }
        }
        if !session {
            let expires = item
                .get("expires")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && *value > 0.0)
                .ok_or_else(|| {
                    error("BROWSER_STATE_COOKIE: persistent cookie has no valid expiry")
                })?;
            stored.insert("expires".into(), json!(expires));
        }
        cookies.push(
            serde_json::from_value(Value::Object(stored))
                .map_err(|_| error("BROWSER_STATE_COOKIE: invalid cookie metadata"))?,
        );
        if cookies.len() > MAX_COOKIES {
            return Err(error(
                "BROWSER_STATE_LIMIT: too many cookies for the selected origin",
            ));
        }
    }
    Ok(cookies)
}

fn storage_from_response(response: &Value) -> Result<Vec<StorageEntry>> {
    let entries = response
        .get("entries")
        .and_then(Value::as_array)
        .filter(|entries| entries.len() <= MAX_STORAGE_ENTRIES)
        .ok_or_else(|| {
            error("BROWSER_STATE_STORAGE: invalid or oversized localStorage response")
        })?;
    entries
        .iter()
        .map(|entry| {
            let pair = entry
                .as_array()
                .filter(|pair| pair.len() == 2)
                .ok_or_else(|| error("BROWSER_STATE_STORAGE: invalid localStorage pair"))?;
            let name = pair[0]
                .as_str()
                .filter(|value| value.len() <= MAX_KEY_BYTES)
                .ok_or_else(|| error("BROWSER_STATE_STORAGE: invalid localStorage key"))?;
            let value = pair[1]
                .as_str()
                .filter(|value| value.len() <= MAX_VALUE_BYTES)
                .ok_or_else(|| error("BROWSER_STATE_STORAGE: invalid localStorage value"))?;
            Ok(StorageEntry {
                name: name.to_owned(),
                value: value.to_owned(),
            })
        })
        .collect()
}

fn checkpoint(owner: &AgentCx) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| error("BROWSER_STATE_CANCELLED: storage-state operation was cancelled"))
}

pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    cwd: &Path,
    tab: &str,
    allowlist: Option<&[String]>,
    args: &Value,
) -> Result<ToolOutput> {
    validate(args)?;
    checkpoint(owner)?;
    if required(args, "action")? == "restore_state" {
        return restore::execute(owner, cdp, cwd, tab, allowlist, args).await;
    }
    let target =
        crate::artifact_output::resolve_new(cwd, required(args, "state_path")?, "browser")?;
    let document = Document::capture(owner, cdp, allowlist).await?;
    // Page-scoped Storage commands use the attached target's storage partition,
    // not a possibly unrelated default browser context.
    let response = cdp
        .command(owner, "Storage.getCookies", json!({}))
        .await
        .map_err(|_| {
            error(
                "BROWSER_STATE_COOKIE: browser cookie capture failed; no state file was published",
            )
        })?;
    let cookies = cookies_from_response(&response, &document)?;
    document.check(owner, cdp, allowlist).await?;
    let response = cdp
        .command(
            owner,
            "DOMStorage.getDOMStorageItems",
            json!({"storageId":document.storage_id()}),
        )
        .await
        .map_err(|_| {
            error("BROWSER_STATE_STORAGE: localStorage capture failed; no state file was published")
        })?;
    let state = State {
        schema: SCHEMA.into(),
        origin: document.origin.clone(),
        cookies,
        local_storage: storage_from_response(&response)?,
    };
    state.validate(&document, allowlist)?;
    let bytes = state.encode()?;
    document.check(owner, cdp, allowlist).await?;
    checkpoint(owner)?;
    crate::artifact_output::publish(&target, &bytes, "browser")?;
    Ok(output(
        format!(
            "Saved private browser state for tab {tab} to {}. This file contains plaintext credentials; never commit or share it. No credential values are included in this response.",
            target.path().display()
        ),
        json!({"action":"save_state", "tab":tab, "backend":"cdp", "schema":SCHEMA,
            "origin":document.origin, "saved_path":target.path().display().to_string(),
            "cookie_count":state.cookies.len(), "local_storage_count":state.local_storage.len(),
            "size_bytes":bytes.len(), "contains_credentials":true}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(url: &str) -> Document {
        let origin = url::Url::parse(url).unwrap().origin().ascii_serialization();
        Document::from_frame(
            &json!({"id":"frame", "loaderId":"loader", "url":url, "securityOrigin":origin}),
            None,
        )
        .unwrap()
    }

    fn cookie(domain: &str, secure: bool, session: bool) -> Value {
        json!({"name":"session", "value":"PRIVATE-CANARY", "domain":domain, "path":"/",
            "secure":secure, "httpOnly":true, "session":session, "expires":1_900_000_000.0,
            "sameSite":"Lax", "priority":"Medium", "sourceScheme":"Secure", "sourcePort":443})
    }

    fn state() -> State {
        State {
            schema: SCHEMA.into(),
            origin: "https://example.com".into(),
            cookies: Vec::new(),
            local_storage: Vec::new(),
        }
    }

    #[test]
    fn state_schema_is_wired_into_the_real_browser_tool() {
        use crate::tools::Tool;
        let schema = super::super::BrowserTool::new(Path::new(".")).parameters();
        assert!(
            schema["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("save_state"))
        );
        assert!(schema["properties"].get("state_path").is_some());
    }

    #[test]
    fn only_explicit_scoped_state_requests_are_admitted() {
        let valid = json!({"action":"save_state", "tab":"app", "state_path":".pi/auth/site.json"});
        if cfg!(all(
            unix,
            not(any(target_os = "espidf", target_os = "redox"))
        )) {
            assert!(validate(&valid).is_ok());
        }
        for field in [
            "frame",
            "diagnostics",
            "script",
            "url",
            "output_path",
            "origin",
            "cookies",
        ] {
            let mut invalid = valid.clone();
            invalid[field] = Value::Null;
            assert!(validate(&invalid).is_err());
        }
        for field in ["tab", "state_path"] {
            for value in [Value::Null, json!(false), json!(""), json!("bad\nname")] {
                let mut invalid = valid.clone();
                invalid[field] = value;
                assert!(validate(&invalid).is_err());
            }
        }
    }

    #[test]
    fn state_paths_cannot_escape_the_workspace() {
        for path in [
            "../state.json",
            "/state.json",
            "a/../../state.json",
            "a\\state.json",
            "state.txt",
            "state\0.json",
        ] {
            assert!(validate_path(path).is_err());
        }
        assert!(validate_path(".pi/auth/state.JSON").is_ok());
    }

    #[test]
    fn shared_attached_profiles_are_refused_before_connection() {
        let connection =
            launch::Connection::Attach(policy::endpoint("http://127.0.0.1:9", false).unwrap());
        assert!(validate_connection(&json!({"action":"save_state"}), &connection).is_err());
        assert!(validate_connection(&json!({"action":"status"}), &connection).is_ok());
    }

    #[test]
    fn opaque_credentials_and_mismatched_security_origins_are_refused() {
        for url in [
            "about:blank",
            "file:///tmp/page",
            "https://user:password@example.com/",
        ] {
            assert!(
                Document::from_frame(&json!({"id":"f", "loaderId":"l", "url":url}), None).is_err()
            );
        }
        assert!(Document::from_frame(&json!({"id":"f", "loaderId":"l", "url":"https://example.com/", "securityOrigin":"https://other.test"}), None).is_err());
        assert!(
            Document::from_frame(&json!({"id":"f", "url":"https://example.com/"}), None).is_err()
        );
    }

    #[test]
    fn cookie_domains_use_label_boundaries_and_ip_identity() {
        assert!(domain_matches(".example.com", "app.example.com"));
        assert!(domain_matches("example.com", "example.com"));
        assert!(!domain_matches("example.com", "notexample.com"));
        assert!(!domain_matches(".example.com", "example.com.attacker.test"));
        assert!(!domain_matches("..example.com", "example.com"));
        assert!(domain_matches("127.0.0.1", "127.0.0.1"));
        assert!(!domain_matches("0.0.1", "127.0.0.1"));
    }

    #[test]
    fn cookie_capture_preserves_http_only_expiry_and_session_semantics() {
        let doc = document("https://example.com/app");
        let mut persistent = cookie("example.com", true, false);
        persistent["name"] = json!("persistent");
        let cookies = cookies_from_response(
            &json!({"cookies":[cookie("example.com", true, true), persistent]}),
            &doc,
        )
        .unwrap();
        assert_eq!(cookies.len(), 2);
        assert!(cookies[0].http_only);
        assert!(cookies[0].expires.is_none());
        assert_eq!(cookies[1].expires, Some(1_900_000_000.0));
        assert!(
            serde_json::to_value(&cookies[0])
                .unwrap()
                .get("expires")
                .is_none()
        );
    }

    #[test]
    fn unrelated_and_secure_cookies_do_not_leak_to_an_http_origin() {
        let doc = document("http://example.com/app");
        let cookies = cookies_from_response(&json!({"cookies":[cookie("other.test", false, true), cookie("example.com", true, true), cookie("example.com", false, true)]}), &doc).unwrap();
        assert_eq!(cookies.len(), 1);
        assert!(!cookies[0].secure);
    }

    #[test]
    fn unsupported_cookie_scope_is_not_silently_weakened() {
        for field in ["partitionKey", "partitionKeyOpaque", "sameParty"] {
            let mut input = cookie("example.com", true, true);
            input[field] = if field == "partitionKey" {
                json!({"topLevelSite":"https://other.test"})
            } else {
                json!(true)
            };
            let error = cookies_from_response(
                &json!({"cookies":[input]}),
                &document("https://example.com"),
            )
            .err()
            .unwrap();
            assert!(
                error
                    .to_string()
                    .contains("BROWSER_STATE_COOKIE_UNSUPPORTED")
            );
            assert!(!error.to_string().contains("PRIVATE-CANARY"));
        }
    }

    #[test]
    fn malformed_cookie_and_storage_payloads_do_not_become_empty_successes() {
        let doc = document("https://example.com");
        for response in [
            json!({}),
            json!({"cookies":null}),
            json!({"cookies":[{"domain":"example.com", "value":"PRIVATE-CANARY"}]}),
        ] {
            let error = cookies_from_response(&response, &doc).err().unwrap();
            assert!(!error.to_string().contains("PRIVATE-CANARY"));
        }
        for response in [
            json!({}),
            json!({"entries":[["key"]]}),
            json!({"entries":[["key", 42]]}),
        ] {
            assert!(storage_from_response(&response).is_err());
        }
    }

    #[test]
    fn schema_origin_duplicates_and_item_limits_are_checked() {
        let doc = document("https://example.com");
        let mut stored = state();
        stored.origin = "http://example.com".into();
        assert!(stored.validate(&doc, None).is_err());
        stored.origin = doc.origin.clone();
        stored.local_storage =
            storage_from_response(&json!({"entries":[["key","one"],["key","two"]]})).unwrap();
        assert!(stored.validate(&doc, None).is_err());
        stored.local_storage.clear();
        let cookies = cookies_from_response(
            &json!({"cookies":[cookie("example.com", true, true)]}),
            &doc,
        )
        .unwrap();
        stored.cookies = vec![cookies[0].clone(), cookies[0].clone()];
        assert!(stored.validate(&doc, None).is_err());
        stored.cookies = vec![cookies[0].clone(); MAX_COOKIES + 1];
        assert!(stored.validate(&doc, None).is_err());
    }

    #[test]
    fn combined_serialized_state_is_bounded_even_when_individual_entries_fit() {
        let mut stored = state();
        for index in 0..32 {
            stored.local_storage.push(StorageEntry {
                name: index.to_string(),
                value: "x".repeat(MAX_VALUE_BYTES),
            });
        }
        assert!(
            stored
                .validate(&document("https://example.com"), None)
                .is_ok()
        );
        assert!(stored.encode().is_err());
    }

    #[test]
    fn serialized_state_rejects_duplicate_or_unknown_contract_fields() {
        let bytes = state().encode().unwrap();
        assert!(serde_json::from_slice::<State>(&bytes).is_ok());
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["extra"] = json!("not a supported authority");
        assert!(serde_json::from_value::<State>(value).is_err());
        assert!(serde_json::from_str::<State>(r#"{"schema":"pi.browser.storage-state.v1","schema":"other","origin":"https://example.com","cookies":[],"localStorage":[]}"#).is_err());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn state_publication_is_private_create_only_and_symlink_confined() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let target =
            crate::artifact_output::resolve_new(root.path(), ".pi/auth/state.json", "browser")
                .unwrap();
        let bytes = state().encode().unwrap();
        crate::artifact_output::publish(&target, &bytes, "browser").unwrap();
        let mode = std::fs::metadata(target.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
        assert_eq!(std::fs::read(target.path()).unwrap(), bytes);
        assert!(
            crate::artifact_output::resolve_new(root.path(), ".pi/auth/state.json", "browser")
                .is_err()
        );
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(
            crate::artifact_output::resolve_new(root.path(), "linked/state.json", "browser")
                .is_err()
        );
    }

    #[test]
    fn host_only_cookies_are_not_captured_for_a_child_host() {
        assert!(!domain_matches("example.com", "app.example.com"));
        let captured = cookies_from_response(&json!({"cookies":[cookie("example.com", true, true), cookie(".example.com", true, true)]}), &document("https://app.example.com")).unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].domain, ".example.com");
    }

    #[test]
    fn state_requires_explicit_browser_security_origin_evidence() {
        for origin in [None, Some(Value::Null), Some(json!("://")), Some(json!(42))] {
            let mut frame = json!({"id":"f", "loaderId":"l", "url":"https://example.com/"});
            if let Some(origin) = origin {
                frame["securityOrigin"] = origin;
            }
            assert!(Document::from_frame(&frame, None).is_err());
        }
    }
}
