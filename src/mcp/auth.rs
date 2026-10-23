//! Native MCP OAuth. Secrets live in private per-server credential files.
//!
//! A login epoch identifies an account/session; refresh only advances its
//! revision. Transport bindings pin the first epoch they see, so delayed
//! cleanup for an old session can never borrow a newly signed-in account.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use url::Url;

use crate::agent_cx::AgentCx;
use crate::auth::{AuthCredential, AuthStorage};
use crate::error::{Error, Result};

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const FLOW_TIMEOUT: Duration = Duration::from_secs(300);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const BODY_LIMIT: usize = 64 * 1024;
const STORE_KEY: &str = "mcp_oauth_v1";
type Admission = Arc<dyn Fn() -> Result<()> + Send + Sync>;

fn error(code: &str, message: &'static str) -> Error {
    Error::tool("mcp", format!("[MCP_AUTH_{code}] {message}"))
}

fn required() -> Error {
    error("REQUIRED", "this MCP server requires login; use /mcp reauth")
}

fn storage_error() -> Error {
    error("STORE", "cannot safely read or persist the MCP credential store")
}

fn check_owner(owner: &AgentCx) -> Result<()> {
    if !owner.capabilities().io || !owner.capabilities().time {
        return Err(error("CAPABILITY_DENIED", "MCP authentication requires I/O and timer capabilities"));
    }
    owner.checkpoint().map_err(|_| error("CANCELLED", "MCP authentication was cancelled"))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn nonce() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Clone, Default)]
enum ObservedLogin {
    #[default]
    Unobserved,
    Anonymous,
    Epoch(String),
}

#[derive(Clone)]
pub(crate) struct AuthBinding {
    path: PathBuf,
    identity: String,
    resource: String,
    observed: Arc<Mutex<ObservedLogin>>,
    managed: Arc<AtomicBool>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    identity: String,
    resource: String,
    epoch: String,
    revision: String,
    revoked: bool,
    credentials: Option<Credentials>,
}

enum ExpectedWrite {
    /// A browser login may replace the same account after its token rotated.
    Epoch(Option<String>),
    /// Refresh must replace exactly the token set it exchanged.
    Revision(Option<(String, String)>),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    issuer: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<String>,
    auth_method: String,
    scope: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    expires_at_ms: Option<i64>,
}

struct Attempt(Arc<AtomicBool>);

impl Drop for Attempt {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl AuthBinding {
    pub(crate) fn new(global_dir: &Path, identity: &str, resource_url: &str) -> Result<Self> {
        let parsed = Url::parse(resource_url)
            .map_err(|_| error("CONFIG", "the MCP endpoint is not a valid absolute HTTP URL"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(error("CONFIG", "the MCP endpoint must be HTTP(S) without URL credentials or a fragment"));
        }
        let digest = sha2::Sha256::digest(format!("{identity}\0{resource_url}").as_bytes());
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        Ok(Self {
            path: global_dir.join("mcp-auth").join(format!("{key}.json")),
            identity: identity.to_string(),
            resource: resource_url.to_string(),
            observed: Arc::new(Mutex::new(ObservedLogin::Unobserved)),
            managed: Arc::new(AtomicBool::new(false)),
        })
    }

    fn observe(&self, record: Option<&Record>) -> Result<()> {
        let mut observed = self.observed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match (&*observed, record) {
            (ObservedLogin::Unobserved, None) => *observed = ObservedLogin::Anonymous,
            (ObservedLogin::Unobserved, Some(record)) => {
                *observed = ObservedLogin::Epoch(record.epoch.clone());
            }
            (ObservedLogin::Anonymous, None) => {}
            (ObservedLogin::Epoch(epoch), Some(record)) if epoch == &record.epoch => {}
            _ => return Err(error("CHANGED", "the MCP login changed; reconnect before using this session")),
        }
        Ok(())
    }

    fn read_sync(&self) -> Result<Option<Record>> {
        let metadata = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return if self.managed.load(Ordering::Acquire) { Err(required()) } else { Ok(None) };
            }
            Err(_) => return Err(storage_error()),
        };
        self.managed.store(true, Ordering::Release);
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 256 * 1024 {
            return Err(storage_error());
        }
        let store = AuthStorage::load_with_lock_timeout(self.path.clone(), LOCK_TIMEOUT)
            .map_err(|_| storage_error())?;
        let Some(AuthCredential::Unknown(value)) = store.get(STORE_KEY) else {
            // AuthStorage tolerates malformed legacy JSON; managed MCP records
            // must instead fail closed rather than downgrade to anonymous.
            return Err(storage_error());
        };
        let record: Record = serde_json::from_value(value.clone()).map_err(|_| storage_error())?;
        if record.version != 1
            || record.identity != self.identity
            || record.resource != self.resource
            || record.epoch.is_empty()
            || record.revision.is_empty()
            || record.revoked == record.credentials.is_some()
        {
            return Err(storage_error());
        }
        Ok(Some(record))
    }

    async fn read(&self) -> Result<Option<Record>> {
        let binding = self.clone();
        asupersync::runtime::spawn_blocking(move || binding.read_sync()).await
    }

    fn prepare_directory(&self) -> Result<()> {
        let parent = self.path.parent().ok_or_else(storage_error)?;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(parent).map_err(|_| storage_error())?;
        let metadata = std::fs::symlink_metadata(parent).map_err(|_| storage_error())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(storage_error());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(error("STORE", "the MCP credential directory must be private (mode 0700)"));
            }
        }
        Ok(())
    }

    fn write_sync(&self, expected: Option<(String, String)>, record: Record, check: Option<Admission>) -> Result<()> {
        self.write_with_expectation(ExpectedWrite::Revision(expected), record, check)
    }

    fn write_with_expectation(&self, expected: ExpectedWrite, record: Record, check: Option<Admission>) -> Result<()> {
        self.prepare_directory()?;
        let _transaction = crate::file_lock::DirLock::acquire_for(
            &self.path.with_extension("transaction"), LOCK_TIMEOUT,
        ).map_err(|_| storage_error())?;
        let current = self.read_sync()?;
        let matches_current = match expected {
            ExpectedWrite::Epoch(epoch) => current.as_ref().map(|record| record.epoch.as_str()) == epoch.as_deref(),
            ExpectedWrite::Revision(version) => current.as_ref().map(|record| (record.epoch.clone(), record.revision.clone())) == version,
        };
        if !matches_current {
            return Err(error("SUPERSEDED", "a newer MCP login or logout superseded this operation"));
        }
        if let Some(check) = check {
            check()?;
        }
        // This is the commit admission point. All MCP writers hold the outer
        // transaction lock; a cancellation after admission cannot roll back an
        // atomic AuthStorage save that has already entered persistence.
        let mut store = AuthStorage::empty_at(self.path.clone());
        store.set(STORE_KEY, AuthCredential::Unknown(serde_json::to_value(record).map_err(|_| storage_error())?));
        store.save().map_err(|_| storage_error())?;
        self.managed.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) async fn forget(&self) -> Result<()> {
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner)?;
        let active = Arc::new(AtomicBool::new(true));
        let _attempt = Attempt(Arc::clone(&active));
        let binding = self.clone();
        asupersync::runtime::spawn_blocking(move || {
            let _owner = owner.cx().clone().set_current_restricted();
            let check: Admission = Arc::new(move || {
                if !active.load(Ordering::Acquire) {
                    return Err(error("CANCELLED", "MCP logout was cancelled"));
                }
                check_owner(&owner)
            });
            binding.forget_sync(check)
        }).await
    }

    fn forget_sync(&self, check: Admission) -> Result<()> {
        check()?;
        self.prepare_directory()?;
        let _transaction = crate::file_lock::DirLock::acquire_for(
            &self.path.with_extension("transaction"), LOCK_TIMEOUT,
        ).map_err(|_| storage_error())?;
        // Logout is also a recovery path for a corrupt credential file.
        // Check withdrawal after waiting for the lock and immediately before
        // admitting the atomic tombstone write.
        check()?;
        let record = Record {
            version: 1, identity: self.identity.clone(), resource: self.resource.clone(),
            epoch: nonce(), revision: nonce(), revoked: true, credentials: None,
        };
        let mut store = AuthStorage::empty_at(self.path.clone());
        store.set(STORE_KEY, AuthCredential::Unknown(serde_json::to_value(record).map_err(|_| storage_error())?));
        store.save().map_err(|_| storage_error())?;
        self.managed.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) async fn authorization_header(&self, allow_refresh: bool) -> Result<Option<String>> {
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner)?;
        let record = self.read().await?;
        self.observe(record.as_ref())?;
        let Some(record) = record else { return Ok(None); };
        let credentials = record.credentials.as_ref().ok_or_else(required)?;
        validate_credentials(credentials)?;
        let expires = credentials.expires_at_ms.unwrap_or(i64::MAX);
        if allow_refresh && expires <= now_ms().saturating_add(60_000) && credentials.refresh_token.is_some() {
            self.refresh_owned(record, owner.clone()).await?;
            check_owner(&owner)?;
            let updated = self.read().await?;
            self.observe(updated.as_ref())?;
            return header_for(updated.as_ref().ok_or_else(required)?).map(Some);
        }
        header_for(&record).map(Some)
    }

    async fn refresh_owned(&self, record: Record, owner: AgentCx) -> Result<()> {
        check_owner(&owner)?;
        if !owner.capabilities().spawn {
            return Err(error("CAPABILITY_DENIED", "refresh requires the owner's task-spawn capability"));
        }
        let runtime = asupersync::runtime::Runtime::current_handle()
            .ok_or_else(|| error("RUNTIME", "MCP token refresh requires an active runtime"))?;
        let binding = self.clone();
        let (sender, receiver) = futures::channel::oneshot::channel();
        {
            // Current-Cx guards must not remain in the async state across an
            // await; the owned worker reinstalls this owner on every poll.
            let _admission = owner.cx().clone().set_current_restricted();
            let _ = runtime.try_spawn(async move {
                // Once token rotation has been sent, losing the waiting request
                // must not discard its replacement refresh token. Mask only this
                // bounded completion task, under the original effect capabilities.
                let operation = bounded(binding.refresh(record), Duration::from_secs(120));
                let mut operation = std::pin::pin!(operation);
                let result = poll_fn(|task| {
                    let _scope = owner.cx().clone().set_current_restricted();
                    owner.masked(|| operation.as_mut().poll(task))
                }).await;
                let _ = sender.send(result);
            }).map_err(|_| error("RUNTIME", "cannot start the bounded MCP token refresh"))?;
        }
        receiver.await.map_err(|_| error("REFRESH", "MCP refresh stopped before credentials were saved"))?
    }

    async fn refresh(&self, original: Record) -> Result<()> {
        let binding = self.clone();
        let refresh_lock = asupersync::runtime::spawn_blocking(move || {
            binding.prepare_directory()?;
            crate::file_lock::DirLock::acquire_for(&binding.path.with_extension("refresh"), LOCK_TIMEOUT)
                .map_err(|_| storage_error())
        }).await?;
        let current = self.read().await?.ok_or_else(required)?;
        if current.epoch != original.epoch || current.revoked {
            return Err(error("SUPERSEDED", "MCP login was replaced or removed before refresh"));
        }
        if current.revision != original.revision {
            // A serialized peer already refreshed. It must have left a usable
            // token, not merely changed a bookkeeping field.
            header_for(&current)?;
            return Ok(());
        }
        let credentials = current.credentials.as_ref().ok_or_else(required)?;
        validate_credentials(credentials)?;
        let refresh_token = credentials.refresh_token.as_deref().ok_or_else(required)?;
        let mut fields = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("resource".to_string(), self.resource.clone()),
        ];
        let value = token_request(credentials, &mut fields).await?;
        let updated_credentials = parse_tokens(value, credentials.clone())?;
        let expected = Some((current.epoch.clone(), current.revision.clone()));
        let updated = Record { revision: nonce(), credentials: Some(updated_credentials), ..current };
        let binding = self.clone();
        asupersync::runtime::spawn_blocking(move || {
            // Retain serialization through an admitted save even if the owned
            // completion deadline expires while persistence is queued/running.
            let _refresh_lock = refresh_lock;
            binding.write_sync(expected, updated, None)
        }).await
    }

    pub(crate) async fn reauthenticate(
        &self,
        client_id: Option<String>,
        on_url: Arc<dyn Fn(&str) + Send + Sync>,
        check: Admission,
    ) -> Result<()> {
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner)?;
        check()?;
        let active = Arc::new(AtomicBool::new(true));
        let _attempt = Attempt(Arc::clone(&active));
        let checked: Admission = Arc::new(move || {
            if !active.load(Ordering::Acquire) {
                return Err(error("CANCELLED", "MCP login was cancelled"));
            }
            check()
        });
        bounded(self.login(client_id, on_url, checked, owner), FLOW_TIMEOUT).await
    }

    async fn login(
        &self,
        supplied_client_id: Option<String>,
        on_url: Arc<dyn Fn(&str) + Send + Sync>,
        check: Admission,
        owner: AgentCx,
    ) -> Result<()> {
        check_owner(&owner)?;
        checked_url(&self.resource)?;
        check()?;
        let old = self.read().await?;
        let expected_epoch = old.as_ref().map(|record| record.epoch.clone());
        let metadata = discover(&self.resource, &check).await?;
        check()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|_| error("CALLBACK", "cannot bind the OAuth loopback callback"))?;
        listener.set_nonblocking(true).map_err(|_| error("CALLBACK", "cannot configure the OAuth loopback callback"))?;
        let address = listener.local_addr().map_err(|_| error("CALLBACK", "cannot read the OAuth callback address"))?;
        let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", address.port());
        let mut credentials = register_client(&metadata, supplied_client_id, &redirect_uri, &check).await?;
        let verifier = nonce();
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()));
        let state = nonce();
        let mut authorization = checked_url(&metadata.authorization_endpoint)?;
        if authorization.query_pairs().any(|(name, _)| matches!(
            name.as_ref(),
            "response_type" | "client_id" | "redirect_uri" | "code_challenge"
                | "code_challenge_method" | "state" | "resource" | "scope"
        )) {
            return Err(error("METADATA", "authorization endpoint contains conflicting OAuth parameters"));
        }
        authorization.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &credentials.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", &self.resource);
        if let Some(scope) = &credentials.scope {
            authorization.query_pairs_mut().append_pair("scope", scope);
        }
        check()?;
        on_url(authorization.as_str());
        let code = await_callback(listener, &redirect_uri, &state, &metadata.issuer, metadata.require_iss, &check).await?;
        check()?;
        let mut fields = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code),
            ("redirect_uri".to_string(), redirect_uri),
            ("code_verifier".to_string(), verifier),
            ("resource".to_string(), self.resource.clone()),
        ];
        let value = token_request(&credentials, &mut fields).await?;
        credentials = parse_tokens(value, credentials)?;
        check()?;
        let record = Record {
            version: 1, identity: self.identity.clone(), resource: self.resource.clone(),
            epoch: nonce(), revision: nonce(), revoked: false, credentials: Some(credentials),
        };
        let binding = self.clone();
        asupersync::runtime::spawn_blocking(move || {
            let _scope = owner.cx().clone().set_current_restricted();
            check_owner(&owner)?;
            binding.write_with_expectation(ExpectedWrite::Epoch(expected_epoch), record, Some(check))
        }).await
    }
}

fn header_for(record: &Record) -> Result<String> {
    if record.revoked { return Err(required()); }
    let credentials = record.credentials.as_ref().ok_or_else(required)?;
    validate_credentials(credentials)?;
    if credentials.expires_at_ms.is_some_and(|expires| expires <= now_ms()) {
        return Err(required());
    }
    Ok(format!("Bearer {}", credentials.access_token))
}

fn validate_credentials(credentials: &Credentials) -> Result<()> {
    checked_url(&credentials.issuer)?;
    checked_url(&credentials.token_endpoint)?;
    validate_string(&credentials.client_id, 4096)?;
    if !matches!(credentials.auth_method.as_str(), "none" | "client_secret_basic" | "client_secret_post") {
        return Err(error("PROTOCOL", "unsupported OAuth token endpoint authentication method"));
    }
    if credentials.auth_method != "none" && credentials.client_secret.as_deref().is_none_or(str::is_empty) {
        return Err(error("PROTOCOL", "OAuth client authentication requires its registered secret"));
    }
    validate_token(&credentials.access_token)
}

fn validate_string(value: &str, limit: usize) -> Result<()> {
    if value.is_empty() || value.len() > limit || value.chars().any(char::is_control) {
        Err(error("PROTOCOL", "OAuth metadata or token response contained an invalid value"))
    } else {
        Ok(())
    }
}

fn validate_token(value: &str) -> Result<()> {
    validate_string(value, 16 * 1024)?;
    if value.bytes().any(|byte| !byte.is_ascii_graphic()) {
        return Err(error("PROTOCOL", "OAuth access token is not a valid HTTP bearer value"));
    }
    Ok(())
}

async fn pause(duration: Duration) {
    let now = asupersync::Cx::current()
        .and_then(|cx| cx.timer_driver())
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    asupersync::time::sleep(now, duration).await;
}

async fn bounded<T>(future: impl Future<Output = Result<T>>, timeout: Duration) -> Result<T> {
    match futures::future::select(Box::pin(future), Box::pin(pause(timeout))).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(((), _)) => Err(error("TIMEOUT", "MCP authentication exceeded its deadline")),
    }
}

#[derive(Clone)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    auth_methods: Vec<String>,
    scope: Option<String>,
    require_iss: bool,
}

fn checked_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| error("METADATA", "OAuth metadata contains an invalid URL"))?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
    {
        return Err(error("METADATA", "OAuth requires HTTPS endpoints (HTTP is allowed only on loopback), without URL credentials or fragments"));
    }
    Ok(url)
}

fn string_field(value: &Value, name: &str) -> Result<String> {
    let text = value.get(name).and_then(Value::as_str)
        .ok_or_else(|| error("METADATA", "OAuth metadata is missing a required string field"))?;
    validate_string(text, 8192)?;
    Ok(text.to_string())
}

fn string_list(value: &Value, name: &str) -> Result<Option<Vec<String>>> {
    let Some(value) = value.get(name) else { return Ok(None); };
    let values = value.as_array().filter(|values| values.len() <= 64)
        .ok_or_else(|| error("METADATA", "OAuth metadata has an invalid string list"))?;
    values.iter().map(|value| {
        let text = value.as_str().ok_or_else(|| error("METADATA", "OAuth metadata has an invalid string list"))?;
        validate_string(text, 8192)?;
        Ok(text.to_string())
    }).collect::<Result<Vec<_>>>().map(Some)
}

fn scope_string(value: &str) -> Result<Option<String>> {
    if value.len() > 8192 || value.bytes().any(|byte| !(byte == b' ' || byte.is_ascii_graphic())) {
        return Err(error("METADATA", "OAuth scope advertisement is malformed"));
    }
    let scope = value.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok((!scope.is_empty()).then_some(scope))
}

async fn fetch_json(url: &str) -> Result<Option<Value>> {
    checked_url(url)?;
    bounded(async {
        let client = crate::http::client::Client::new();
        let response = client.get(url).header("Accept", "application/json")
            .timeout(HTTP_TIMEOUT).send().await
            .map_err(|_| error("DISCOVERY", "cannot fetch OAuth discovery metadata"))?;
        if matches!(response.status(), 404 | 405) { return Ok(None); }
        if response.status() != 200 {
            return Err(error("DISCOVERY", "OAuth discovery endpoint rejected the request"));
        }
        let bytes = response.bytes_limited(BODY_LIMIT).await
            .map_err(|_| error("DISCOVERY", "OAuth discovery body exceeded its limit or could not be read"))?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| error("METADATA", "OAuth discovery endpoint did not return valid JSON"))?;
        if !value.is_object() { return Err(error("METADATA", "OAuth metadata must be an object")); }
        Ok(Some(value))
    }, HTTP_TIMEOUT).await
}

/// Split authentication parameters without treating commas inside quoted
/// metadata URLs as challenge separators. Ignore other authentication schemes.
fn bearer_parameters(header: &str) -> Result<HashMap<String, String>> {
    if header.len() > 16 * 1024 { return Err(error("METADATA", "OAuth challenge is too large")); }
    let mut segments = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, ch) in header.char_indices() {
        if escaped { escaped = false; continue; }
        if quoted && ch == '\\' { escaped = true; continue; }
        if ch == '"' { quoted = !quoted; }
        if ch == ',' && !quoted {
            segments.push(&header[start..index]);
            start = index + 1;
        }
    }
    if quoted || escaped { return Err(error("METADATA", "OAuth challenge has an unterminated quoted value")); }
    segments.push(&header[start..]);
    let mut active = false;
    let mut values = HashMap::new();
    for segment in segments {
        let mut segment = segment.trim();
        if let Some((scheme, rest)) = segment.split_once(char::is_whitespace)
            && scheme.eq_ignore_ascii_case("bearer")
        {
            if active { break; }
            active = true;
            segment = rest.trim();
        }
        if !active { continue; }
        let Some((key, raw)) = segment.split_once('=') else { break; };
        let key = key.trim();
        if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
            break;
        }
        let raw = raw.trim();
        let value = if let Some(quoted) = raw.strip_prefix('"') {
            let quoted = quoted.strip_suffix('"')
                .ok_or_else(|| error("METADATA", "OAuth challenge has an invalid quoted value"))?;
            let mut value = String::new();
            let mut chars = quoted.chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    value.push(chars.next().ok_or_else(|| error("METADATA", "OAuth challenge has an invalid escape"))?);
                } else {
                    value.push(ch);
                }
            }
            value
        } else {
            if raw.chars().any(char::is_whitespace) { break; }
            raw.to_string()
        };
        if values.insert(key.to_ascii_lowercase(), value).is_some() {
            return Err(error("METADATA", "OAuth challenge repeated an authentication parameter"));
        }
    }
    Ok(values)
}

async fn discover(resource: &str, check: &Admission) -> Result<Metadata> {
    let resource_url = checked_url(resource)?;
    check()?;
    let challenge = bounded(async {
        let client = crate::http::client::Client::new();
        let response = client.get(resource).header("Accept", "application/json, text/event-stream")
            .timeout(HTTP_TIMEOUT).send().await
            .map_err(|_| error("DISCOVERY", "cannot obtain the MCP authentication challenge"))?;
        let mut selected = HashMap::new();
        for (name, value) in response.headers() {
            if name.eq_ignore_ascii_case("www-authenticate") {
                let parameters = bearer_parameters(value)?;
                if parameters.contains_key("resource_metadata") || parameters.contains_key("scope") {
                    selected = parameters;
                    break;
                }
            }
        }
        Ok(selected)
    }, HTTP_TIMEOUT).await?;
    let mut origin = resource_url.clone();
    origin.set_path("/");
    origin.set_query(None);
    let mut candidates = Vec::new();
    if let Some(url) = challenge.get("resource_metadata") {
        checked_url(url)?;
        candidates.push(url.clone());
    } else {
        let path = resource_url.path().trim_end_matches('/');
        candidates.push(format!("{}.well-known/oauth-protected-resource{path}", origin.as_str()));
        let root = format!("{}.well-known/oauth-protected-resource", origin.as_str());
        if !candidates.contains(&root) { candidates.push(root); }
    }
    let mut resource_metadata = None;
    for candidate in candidates {
        check()?;
        if let Some(metadata) = fetch_json(&candidate).await? {
            let advertised = checked_url(&string_field(&metadata, "resource")?)?;
            if advertised != resource_url {
                return Err(error("METADATA", "protected-resource metadata does not match the configured MCP endpoint"));
            }
            resource_metadata = Some(metadata);
            break;
        }
    }
    if challenge.contains_key("resource_metadata") && resource_metadata.is_none() {
        return Err(error("DISCOVERY", "the advertised protected-resource metadata is unavailable"));
    }
    let issuers = match resource_metadata.as_ref() {
        Some(metadata) => string_list(metadata, "authorization_servers")?,
        None => None,
    };
    let issuers = match issuers {
        Some(issuers) if issuers.is_empty() => return Err(error("METADATA", "protected-resource metadata advertised no authorization servers")),
        Some(issuers) => issuers,
        None => vec![origin.to_string()],
    };
    let scope = if let Some(scope) = challenge.get("scope") {
        scope_string(scope)?
    } else if let Some(metadata) = resource_metadata.as_ref() {
        string_list(metadata, "scopes_supported")?.map(|values| scope_string(&values.join(" "))).transpose()?.flatten()
    } else {
        None
    };
    for issuer in issuers {
        check()?;
        let issuer_url = checked_url(&issuer)?;
        if issuer_url.query().is_some() {
            return Err(error("METADATA", "OAuth issuer URLs must not contain a query"));
        }
        let mut issuer_origin = issuer_url.clone();
        issuer_origin.set_path("/");
        let suffix = issuer_url.path().trim_end_matches('/');
        let candidates = [
            format!("{}.well-known/oauth-authorization-server{suffix}", issuer_origin.as_str()),
            format!("{}.well-known/openid-configuration{suffix}", issuer_origin.as_str()),
            format!("{}/.well-known/openid-configuration", issuer_url.as_str().trim_end_matches('/')),
        ];
        for candidate in candidates {
            check()?;
            let Some(value) = fetch_json(&candidate).await? else { continue; };
            let advertised_issuer = checked_url(&string_field(&value, "issuer")?)?;
            if advertised_issuer != issuer_url {
                return Err(error("METADATA", "authorization-server metadata issuer does not match its advertised identity"));
            }
            if string_list(&value, "code_challenge_methods_supported")?
                .is_none_or(|methods| !methods.iter().any(|method| method == "S256"))
            {
                return Err(error("PKCE", "the authorization server does not support PKCE S256"));
            }
            if string_list(&value, "response_types_supported")?
                .is_some_and(|types| !types.iter().any(|kind| kind == "code"))
            {
                return Err(error("METADATA", "the authorization server does not support authorization codes"));
            }
            let authorization_endpoint = string_field(&value, "authorization_endpoint")?;
            let token_endpoint = string_field(&value, "token_endpoint")?;
            checked_url(&authorization_endpoint)?;
            checked_url(&token_endpoint)?;
            let registration_endpoint = value.get("registration_endpoint").map(|_| string_field(&value, "registration_endpoint")).transpose()?;
            if let Some(endpoint) = &registration_endpoint { checked_url(endpoint)?; }
            let auth_methods = string_list(&value, "token_endpoint_auth_methods_supported")?
                .unwrap_or_else(|| vec!["client_secret_basic".to_string()]);
            let require_iss = match value.get("authorization_response_iss_parameter_supported") {
                None => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => return Err(error("METADATA", "authorization response issuer metadata must be a boolean")),
            };
            return Ok(Metadata {
                issuer: issuer_url.to_string(), authorization_endpoint, token_endpoint,
                registration_endpoint, auth_methods, scope, require_iss,
            });
        }
    }
    // Never fall back to an unrelated root issuer after a resource advertised
    // its authorization_servers. Only those identities were searched above.
    Err(error("DISCOVERY", "no advertised authorization server supplied usable OAuth metadata"))
}

async fn register_client(
    metadata: &Metadata,
    supplied: Option<String>,
    redirect_uri: &str,
    check: &Admission,
) -> Result<Credentials> {
    let (client_id, client_secret, auth_method) = if let Some(client_id) = supplied {
        validate_string(&client_id, 4096)?;
        (client_id, None, "none".to_string())
    } else {
        let endpoint = metadata.registration_endpoint.as_deref()
            .ok_or_else(|| error("CLIENT_ID_REQUIRED", "this server has no dynamic client registration; supply a public client ID"))?;
        let auth_method = ["none", "client_secret_basic", "client_secret_post"].into_iter()
            .find(|method| metadata.auth_methods.iter().any(|advertised| advertised == method))
            .ok_or_else(|| error("CLIENT_AUTH", "the server offers no supported native-client authentication method"))?;
        check()?;
        let value = bounded(async {
            let client = crate::http::client::Client::new();
            let response = client.post(endpoint).header("Accept", "application/json")
                .json(&json!({
                    "client_name": "Pi Rust",
                    "redirect_uris": [redirect_uri],
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                    "token_endpoint_auth_method": auth_method,
                })).map_err(|_| error("REGISTRATION", "cannot encode OAuth client registration"))?
                .timeout(HTTP_TIMEOUT).send().await
                .map_err(|_| error("REGISTRATION", "OAuth client registration failed"))?;
            if !matches!(response.status(), 200 | 201) {
                return Err(error("REGISTRATION", "the authorization server rejected native-client registration"));
            }
            let bytes = response.bytes_limited(BODY_LIMIT).await
                .map_err(|_| error("REGISTRATION", "cannot read the bounded OAuth registration response"))?;
            serde_json::from_slice::<Value>(&bytes).map_err(|_| error("REGISTRATION", "OAuth registration did not return valid JSON"))
        }, HTTP_TIMEOUT).await?;
        let client_id = string_field(&value, "client_id")?;
        let client_secret = value.get("client_secret").map(|_| string_field(&value, "client_secret")).transpose()?;
        let returned_method = value.get("token_endpoint_auth_method")
            .map(|_| string_field(&value, "token_endpoint_auth_method")).transpose()?
            .unwrap_or_else(|| auth_method.to_string());
        if !matches!(returned_method.as_str(), "none" | "client_secret_basic" | "client_secret_post")
            || (returned_method != "none" && client_secret.is_none())
        {
            return Err(error("CLIENT_AUTH", "registration returned an unsupported client-authentication requirement"));
        }
        (client_id, client_secret, returned_method)
    };
    Ok(Credentials {
        issuer: metadata.issuer.clone(), token_endpoint: metadata.token_endpoint.clone(),
        client_id, client_secret, auth_method, scope: metadata.scope.clone(),
        access_token: String::new(), refresh_token: None, expires_at_ms: None,
    })
}

fn form_component(value: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new()).append_pair("", value).finish()
        .strip_prefix('=').unwrap_or_default().to_string()
}

async fn token_request(credentials: &Credentials, fields: &mut Vec<(String, String)>) -> Result<Value> {
    checked_url(&credentials.token_endpoint)?;
    validate_string(&credentials.client_id, 4096)?;
    fields.push(("client_id".to_string(), credentials.client_id.clone()));
    let authorization = match credentials.auth_method.as_str() {
        "none" => None,
        "client_secret_post" => {
            let secret = credentials.client_secret.as_deref().ok_or_else(|| error("CLIENT_AUTH", "registered OAuth client secret is missing"))?;
            fields.push(("client_secret".to_string(), secret.to_string()));
            None
        }
        "client_secret_basic" => {
            let secret = credentials.client_secret.as_deref().ok_or_else(|| error("CLIENT_AUTH", "registered OAuth client secret is missing"))?;
            Some(format!("Basic {}", base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", form_component(&credentials.client_id), form_component(secret)))))
        }
        _ => return Err(error("CLIENT_AUTH", "unsupported token endpoint authentication method")),
    };
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields.iter().map(|(name, value)| (name, value))).finish();
    bounded(async {
        let client = crate::http::client::Client::new();
        let mut request = client.post(&credentials.token_endpoint)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.into_bytes()).timeout(HTTP_TIMEOUT);
        if let Some(authorization) = authorization {
            request = request.try_header("Authorization", authorization)
                .map_err(|_| error("CLIENT_AUTH", "OAuth client authentication header is invalid"))?;
        }
        let response = request.send().await.map_err(|_| error("TOKEN", "OAuth token exchange failed"))?;
        if response.status() != 200 {
            return Err(error("REQUIRED", "the authorization server rejected the token grant; use /mcp reauth"));
        }
        let bytes = response.bytes_limited(BODY_LIMIT).await
            .map_err(|_| error("TOKEN", "cannot read the bounded OAuth token response"))?;
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| error("TOKEN", "OAuth token endpoint did not return valid JSON"))
    }, HTTP_TIMEOUT).await
}

fn parse_tokens(value: Value, mut credentials: Credentials) -> Result<Credentials> {
    let token_type = value.get("token_type").and_then(Value::as_str)
        .ok_or_else(|| error("TOKEN", "OAuth response did not include a bearer token type"))?;
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err(error("TOKEN", "OAuth response used an unsupported token type"));
    }
    let access_token = value.get("access_token").and_then(Value::as_str)
        .ok_or_else(|| error("TOKEN", "OAuth response did not include an access token"))?;
    validate_token(access_token)?;
    credentials.access_token = access_token.to_string();
    if let Some(value) = value.get("refresh_token") {
        let token = value.as_str().ok_or_else(|| error("TOKEN", "OAuth refresh token is invalid"))?;
        validate_string(token, 16 * 1024)?;
        credentials.refresh_token = Some(token.to_string());
    }
    credentials.expires_at_ms = match value.get("expires_in") {
        None => None,
        Some(value) => {
            let seconds = value.as_u64().filter(|value| *value > 0)
                .ok_or_else(|| error("TOKEN", "OAuth expiry must be a positive number of seconds"))?;
            let milliseconds = i64::try_from(seconds).ok().and_then(|seconds| seconds.checked_mul(1000))
                .and_then(|duration| now_ms().checked_add(duration))
                .ok_or_else(|| error("TOKEN", "OAuth expiry exceeds the supported range"))?;
            Some(milliseconds)
        }
    };
    if let Some(scope) = value.get("scope") {
        credentials.scope = scope_string(scope.as_str().ok_or_else(|| error("TOKEN", "OAuth response scope is invalid"))?)?;
    }
    validate_credentials(&credentials)?;
    Ok(credentials)
}

enum Callback {
    Ignore,
    Code(String),
    Denied,
}

fn parse_callback(request: &[u8], redirect_uri: &str, state: &str, issuer: &str, require_iss: bool) -> Callback {
    let Ok(request) = std::str::from_utf8(request) else { return Callback::Ignore; };
    let Some((head, _)) = request.split_once("\r\n\r\n") else { return Callback::Ignore; };
    let mut lines = head.split("\r\n");
    let Some(line) = lines.next() else { return Callback::Ignore; };
    let words = line.split_whitespace().collect::<Vec<_>>();
    if words.len() != 3 || words[0] != "GET" || !matches!(words[2], "HTTP/1.0" | "HTTP/1.1") {
        return Callback::Ignore;
    }
    let target = words[1];
    if !target.starts_with('/') || target.starts_with("//") || target.contains('#') {
        return Callback::Ignore;
    }
    let Ok(redirect) = Url::parse(redirect_uri) else { return Callback::Ignore; };
    let Ok(url) = redirect.join(target) else { return Callback::Ignore; };
    if url.origin() != redirect.origin() || url.path() != redirect.path() {
        return Callback::Ignore;
    }
    let expected_host = format!("127.0.0.1:{}", redirect.port().unwrap_or(80));
    let mut hosts = lines.filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("host"));
    let Some((_, host)) = hosts.next() else { return Callback::Ignore; };
    if host.trim() != expected_host || hosts.next().is_some() { return Callback::Ignore; }
    let mut parameters = HashMap::new();
    for (name, value) in url.query_pairs() {
        if parameters.insert(name.into_owned(), value.into_owned()).is_some() {
            return Callback::Ignore;
        }
    }
    if parameters.get("state").map(String::as_str) != Some(state) {
        return Callback::Ignore;
    }
    match parameters.get("iss") {
        Some(value) => {
            if checked_url(value).ok() != checked_url(issuer).ok() {
                return Callback::Ignore;
            }
        }
        None if require_iss => return Callback::Ignore,
        None => {}
    }
    match (parameters.get("code"), parameters.get("error")) {
        (Some(code), None) if !code.is_empty() && code.len() <= 4096 && !code.chars().any(char::is_control) => Callback::Code(code.clone()),
        (None, Some(_)) => Callback::Denied,
        _ => Callback::Ignore,
    }
}

fn reply_callback(stream: &mut TcpStream, accepted: bool) {
    let (status, body) = if accepted {
        ("200 OK", "Authorization received. You may return to Pi.")
    } else {
        ("400 Bad Request", "This request is not a valid authorization callback.")
    };
    // Never reflect request fields, authorization codes, state, or provider
    // diagnostics into the browser page.
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    let _ = stream.write_all(response.as_bytes());
}

async fn await_callback(
    listener: TcpListener,
    redirect_uri: &str,
    state: &str,
    issuer: &str,
    require_iss: bool,
    check: &Admission,
) -> Result<String> {
    let deadline = Instant::now() + FLOW_TIMEOUT;
    loop {
        check()?;
        if Instant::now() >= deadline {
            return Err(error("TIMEOUT", "the OAuth browser callback did not arrive before its deadline"));
        }
        let (mut stream, peer) = match listener.accept() {
            Ok(pair) => pair,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                pause(Duration::from_millis(25)).await;
                continue;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(error("CALLBACK", "cannot accept the OAuth loopback callback")),
        };
        if !peer.ip().is_loopback() { continue; }
        stream.set_nonblocking(true).map_err(|_| error("CALLBACK", "cannot configure the OAuth callback connection"))?;
        let read_deadline = Instant::now() + Duration::from_secs(2);
        let mut request = Vec::with_capacity(2048);
        let mut chunk = [0_u8; 2048];
        while request.len() < 8192 && !request.windows(4).any(|window| window == b"\r\n\r\n") {
            check()?;
            if Instant::now() >= read_deadline { break; }
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => request.extend_from_slice(&chunk[..count]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => pause(Duration::from_millis(10)).await,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        match parse_callback(&request, redirect_uri, state, issuer, require_iss) {
            Callback::Code(code) => {
                reply_callback(&mut stream, true);
                return Ok(code);
            }
            Callback::Denied => {
                reply_callback(&mut stream, true);
                return Err(error("DENIED", "the authorization server or user declined this MCP login"));
            }
            Callback::Ignore => reply_callback(&mut stream, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            issuer: "https://issuer.example/".into(),
            token_endpoint: "https://issuer.example/token".into(),
            client_id: "native-client".into(), client_secret: None, auth_method: "none".into(),
            scope: None, access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()), expires_at_ms: Some(i64::MAX),
        }
    }

    fn record(binding: &AuthBinding) -> Record {
        Record {
            version: 1, identity: binding.identity.clone(), resource: binding.resource.clone(),
            epoch: "login-one".into(), revision: "revision-one".into(),
            revoked: false, credentials: Some(credentials()),
        }
    }

    #[test]
    fn missing_anonymous_store_does_not_create_a_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global = temp.path().join("not-created");
        let binding = AuthBinding::new(&global, "server", "http://remote.example/mcp").unwrap();
        assert!(binding.read_sync().unwrap().is_none());
        assert!(!global.exists(), "anonymous HTTP must not require writable auth storage");
    }

    #[test]
    fn transport_pins_login_epoch_but_accepts_rotated_revision() {
        let temp = tempfile::tempdir().unwrap();
        let binding = AuthBinding::new(temp.path(), "server", "https://resource.example/mcp").unwrap();
        let first = record(&binding);
        binding.observe(Some(&first)).unwrap();
        let mut refreshed = first.clone();
        refreshed.revision = "new-revision".into();
        binding.observe(Some(&refreshed)).unwrap();
        refreshed.epoch = "different-account".into();
        assert!(binding.observe(Some(&refreshed)).is_err());
        assert!(binding.observe(None).is_err());
        let anonymous = AuthBinding::new(temp.path(), "server", "https://resource.example/mcp").unwrap();
        anonymous.observe(None).unwrap();
        assert!(anonymous.observe(Some(&first)).is_err(), "old anonymous cleanup must not borrow a new login");
    }

    #[test]
    fn durable_revision_cas_prevents_late_refresh_resurrecting_logout() {
        let temp = tempfile::tempdir().unwrap();
        let binding = AuthBinding::new(temp.path(), "server", "https://resource.example/mcp").unwrap();
        let first = record(&binding);
        binding.write_sync(None, first.clone(), None).unwrap();
        let version = Some((first.epoch.clone(), first.revision.clone()));
        let tombstone = Record {
            epoch: "logout".into(), revision: "logout-revision".into(),
            revoked: true, credentials: None, ..first.clone()
        };
        binding.write_sync(version.clone(), tombstone, None).unwrap();
        let late_refresh = Record { revision: "late-refresh".into(), ..first };
        assert!(binding.write_sync(version, late_refresh, None).is_err());
        let persisted = binding.read_sync().unwrap().unwrap();
        assert!(persisted.revoked);
        assert!(persisted.credentials.is_none());
        assert!(header_for(&persisted).is_err());
    }

    #[test]
    fn logout_rechecks_withdrawal_at_commit_admission() {
        let temp = tempfile::tempdir().unwrap();
        let binding = AuthBinding::new(temp.path(), "server", "https://resource.example/mcp").unwrap();
        let original = record(&binding);
        binding.write_sync(None, original.clone(), None).unwrap();

        // Withdraw the attempt after its first check, as happens when the
        // outer future is dropped while the blocking worker waits for a lock.
        let active = Arc::new(AtomicBool::new(true));
        let attempt = Arc::new(Mutex::new(Some(Attempt(Arc::clone(&active)))));
        let check: Admission = Arc::new(move || {
            if !active.load(Ordering::Acquire) {
                return Err(error("CANCELLED", "MCP logout was cancelled"));
            }
            drop(attempt.lock().unwrap().take());
            Ok(())
        });
        assert!(binding.forget_sync(check).is_err());
        let persisted = binding.read_sync().unwrap().unwrap();
        assert_eq!(persisted.epoch, original.epoch);
        assert!(!persisted.revoked);
        assert_eq!(persisted.credentials.unwrap().access_token, "old-access");
    }

    #[test]
    fn browser_login_accepts_token_rotation_but_rejects_a_new_login_or_logout() {
        let temp = tempfile::tempdir().unwrap();
        let binding = AuthBinding::new(temp.path(), "server", "https://resource.example/mcp").unwrap();
        let first = record(&binding);
        binding.write_sync(None, first.clone(), None).unwrap();
        let expected_epoch = Some(first.epoch.clone());
        let rotated = Record { revision: "rotated".into(), ..first.clone() };
        binding.write_sync(Some((first.epoch.clone(), first.revision.clone())), rotated, None).unwrap();
        let login = Record { epoch: "new-login".into(), revision: "new-login-revision".into(), ..first.clone() };
        binding.write_with_expectation(ExpectedWrite::Epoch(expected_epoch.clone()), login.clone(), None).unwrap();
        assert_eq!(binding.read_sync().unwrap().unwrap().epoch, "new-login");
        assert!(binding.write_with_expectation(ExpectedWrite::Epoch(expected_epoch), first.clone(), None).is_err());

        let tombstone = Record {
            epoch: "logout".into(), revision: "logout-revision".into(),
            revoked: true, credentials: None, ..login.clone()
        };
        binding.write_sync(Some((login.epoch.clone(), login.revision.clone())), tombstone, None).unwrap();
        assert!(binding.write_with_expectation(ExpectedWrite::Epoch(Some(login.epoch)), first, None).is_err());
        assert!(binding.read_sync().unwrap().unwrap().revoked);
    }

    #[test]
    fn credential_identity_includes_server_definition_and_resource() {
        let temp = tempfile::tempdir().unwrap();
        let a = AuthBinding::new(temp.path(), "config-a", "https://resource.example/mcp").unwrap();
        let b = AuthBinding::new(temp.path(), "config-b", "https://resource.example/mcp").unwrap();
        let c = AuthBinding::new(temp.path(), "config-a", "https://resource.example/other").unwrap();
        assert_ne!(a.path, b.path);
        assert_ne!(a.path, c.path);
        a.write_sync(None, record(&a), None).unwrap();
        let mut wrong = record(&a);
        wrong.identity = "config-b".into();
        let mut storage = AuthStorage::empty_at(a.path.clone());
        storage.set(STORE_KEY, AuthCredential::Unknown(serde_json::to_value(wrong).unwrap()));
        storage.save().unwrap();
        assert!(a.read_sync().is_err());
    }

    #[test]
    fn token_response_requires_bearer_and_preserves_optional_refresh_token() {
        assert!(parse_tokens(json!({"token_type":"Bearer"}), credentials()).is_err());
        assert!(parse_tokens(json!({"access_token":"", "token_type":"Bearer"}), credentials()).is_err());
        assert!(parse_tokens(json!({"access_token":"good", "token_type":"mac"}), credentials()).is_err());
        assert!(parse_tokens(json!({"access_token":"x\r\ny", "token_type":"Bearer"}), credentials()).is_err());
        assert!(parse_tokens(json!({"access_token":"good", "token_type":"Bearer", "expires_in":0}), credentials()).is_err());
        let rotated = parse_tokens(json!({
            "access_token":"new-access", "token_type":"Bearer", "expires_in":3600,
            "refresh_token":"rotated-refresh",
        }), credentials()).unwrap();
        assert_eq!(rotated.refresh_token.as_deref(), Some("rotated-refresh"));
        let unchanged = parse_tokens(json!({"access_token":"new-access", "token_type":"bearer"}), credentials()).unwrap();
        assert_eq!(unchanged.refresh_token.as_deref(), Some("old-refresh"));
    }

    #[test]
    fn callback_requires_exact_path_state_host_and_issuer() {
        let redirect = "http://127.0.0.1:43210/oauth/callback";
        let request = |target: &str| format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:43210\r\n\r\n");
        let good = request("/oauth/callback?code=approved&state=random&iss=https%3A%2F%2Fissuer.example%2F");
        assert!(matches!(parse_callback(good.as_bytes(), redirect, "random", "https://issuer.example/", true), Callback::Code(code) if code == "approved"));
        for target in [
            "/favicon.ico?code=approved&state=random",
            "/oauth/callback?code=approved&state=wrong",
            "/oauth/callback?code=approved&state=random&state=random",
            "/oauth/callback?code=approved&state=random&iss=https%3A%2F%2Fevil.example%2F",
            "/oauth/callback?code=approved&state=random",
            "/oauth/callback?code=approved&error=denied&state=random",
        ] {
            assert!(matches!(parse_callback(request(target).as_bytes(), redirect, "random", "https://issuer.example/", true), Callback::Ignore));
        }
        let wrong_host = good.replace("Host: 127.0.0.1:43210", "Host: attacker.example");
        assert!(matches!(parse_callback(wrong_host.as_bytes(), redirect, "random", "https://issuer.example/", true), Callback::Ignore));
    }

    #[test]
    fn discovery_rejects_insecure_remote_endpoints_and_preserves_quoted_commas() {
        assert!(checked_url("http://remote.example/token").is_err());
        assert!(checked_url("https://user:secret@issuer.example/token").is_err());
        assert!(checked_url("https://issuer.example/token#fragment").is_err());
        assert!(checked_url("http://127.0.0.1:43210/token").is_ok());
        assert!(checked_url("http://[::1]:43210/token").is_ok());
        let params = bearer_parameters("Bearer resource_metadata=\"https://resource.example/a,b\", scope=\"read write\"").unwrap();
        assert_eq!(params["resource_metadata"], "https://resource.example/a,b");
        assert_eq!(params["scope"], "read write");
        assert!(bearer_parameters("Bearer scope=\"read\", scope=\"write\"").is_err());
    }
}
