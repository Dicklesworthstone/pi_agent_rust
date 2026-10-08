//! Credential-scoped Gemini Files API staging for large inline media.
//!
//! Called after the request-rewrite hook, immediately before generation. The
//! session's original media stays intact; only the outbound request is rewritten.
//! File resources are reused by content hash, validated before reuse, and
//! re-uploaded after expiry or deletion. No upload runs in a detached task.

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::http::client::{Client, RequestBuilder, Response};
use crate::models::CompatConfig;
use crate::provider::StreamOptions;
use asupersync::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_CACHE_ENTRIES: usize = 64;
const EXPIRY_MARGIN_SECS: i64 = 120;

type ContentKey = [u8; 32];
type FileSlot = Arc<AsyncMutex<Option<RemoteFile>>>;

/// These are staging policies, not generation request admission limits.
/// Google's image/audio guides still recommend a 20 MB total inline request,
/// while its general file-input guide documents 100 MB. Use the conservative
/// media budget, including the serialized prompt, tools and system instruction:
/// https://ai.google.dev/gemini-api/docs/generate-content/image-understanding
/// https://ai.google.dev/gemini-api/docs/generate-content/file-input-methods
/// Text-only requests retain the generation API's own limits. The separate
/// `read_media` admission cap and session attachment limits remain independent.
#[derive(Clone, Copy)]
struct StagingPolicy {
    inline_part_bytes: usize,
    inline_total_bytes: usize,
    timeout: Duration,
    poll_interval: Duration,
}

impl Default for StagingPolicy {
    fn default() -> Self {
        Self {
            inline_part_bytes: 4 * 1024 * 1024,
            inline_total_bytes: 20_000_000,
            timeout: Duration::from_secs(300),
            poll_interval: Duration::from_secs(2),
        }
    }
}

/// Secrets live only for this request. Cache keys contain a digest, never the
/// API key, bearer token, or quota-project header itself.
pub(super) struct UploadAuth {
    headers: Vec<(String, String)>,
}

impl UploadAuth {
    pub(super) fn for_request(
        options: &StreamOptions,
        compat: Option<&CompatConfig>,
        fallback_api_key: Option<&str>,
    ) -> Self {
        let mut headers = Vec::new();
        let key_override = super::google_api_key_override(options, compat);
        let bearer_override = super::authorization_override(options, compat);
        let fallback = if key_override.is_none() && bearer_override.is_none() {
            fallback_api_key.map(str::to_owned)
        } else {
            None
        };
        if let Some(key) = key_override.or(fallback) {
            headers.push(("x-goog-api-key".to_string(), key));
        }
        if let Some(bearer) = bearer_override {
            headers.push(("authorization".to_string(), bearer));
        }
        let quota_project = super::super::first_non_empty_header_value_case_insensitive(
            &options.headers,
            &["x-goog-user-project"],
        )
        .or_else(|| {
            compat
                .and_then(|value| value.custom_headers.as_ref())
                .and_then(|headers| {
                    super::super::first_non_empty_header_value_case_insensitive(
                        headers,
                        &["x-goog-user-project"],
                    )
                })
        });
        if let Some(project) = quota_project {
            headers.push(("x-goog-user-project".to_string(), project));
        }
        Self { headers }
    }

    fn apply<'a>(&self, mut request: RequestBuilder<'a>) -> RequestBuilder<'a> {
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        request
    }
}

#[derive(Clone)]
struct Endpoint {
    base: Url,
    upload: Url,
}

impl Endpoint {
    /// Files API is not a Vertex or Cloud Code Assist API. Custom Gemini
    /// gateways retain their existing inline behavior rather than receiving an
    /// invented upload route, or having their credentials sent to Google.
    fn for_provider(base: &str) -> Option<Self> {
        let base = if base.trim().is_empty() {
            API_BASE
        } else {
            base.trim()
        };
        let parsed = Url::parse(base).ok()?;
        if parsed.scheme() != "https"
            || parsed.host_str() != Some("generativelanguage.googleapis.com")
            || parsed.port_or_known_default() != Some(443)
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.path().trim_end_matches('/') != "/v1beta"
        {
            return None;
        }
        Self::from_base(parsed).ok()
    }

    fn from_base(mut base: Url) -> Result<Self> {
        base.set_path("/v1beta/");
        let upload = base
            .join("/upload/v1beta/files")
            .map_err(|_| files_error("invalid upload endpoint"))?;
        Ok(Self { base, upload })
    }

    fn metadata_url(&self, name: &str) -> Result<Url> {
        let id = name
            .strip_prefix("files/")
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            })
            .ok_or_else(|| files_error("invalid file resource name"))?;
        self.base
            .join(&format!("files/{id}"))
            .map_err(|_| files_error("invalid file metadata endpoint"))
    }

    fn validate_upload_url(&self, value: &str) -> Result<Url> {
        // The returned upload URL is a capability. Never print it, forward
        // credentials to it, or allow it to redirect media to another origin.
        if value.chars().any(char::is_control) || value.contains('\\') {
            return Err(files_error("invalid resumable upload URL"));
        }
        let url = Url::parse(value).map_err(|_| files_error("invalid resumable upload URL"))?;
        if url.origin() != self.base.origin()
            || url.path() != self.upload.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(files_error(
                "refused an off-origin or unexpected upload URL",
            ));
        }
        Ok(url)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileState {
    Processing,
    Active,
    Failed,
}

#[derive(Clone, Debug)]
struct RemoteFile {
    name: String,
    uri: String,
    expires_at: DateTime<Utc>,
    state: FileState,
}

impl RemoteFile {
    fn parse(value: &Value, endpoint: &Endpoint, mime: &str, size: usize) -> Result<Self> {
        let file = value.get("file").unwrap_or(value);
        let name = required_string(file, "name")?.to_string();
        let expected_uri = endpoint.metadata_url(&name)?;
        let uri = required_string(file, "uri")?;
        if uri != expected_uri.as_str() {
            return Err(files_error(
                "file metadata returned an unexpected resource URI",
            ));
        }
        if !required_string(file, "mimeType")?.eq_ignore_ascii_case(mime) {
            return Err(files_error(
                "uploaded file MIME type does not match the input",
            ));
        }
        let returned_size = file
            .get("sizeBytes")
            .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
            .ok_or_else(|| files_error("file metadata has no valid byte count"))?;
        if returned_size != u64::try_from(size).unwrap_or(u64::MAX) {
            return Err(files_error(
                "uploaded file byte count does not match the input",
            ));
        }
        let expires_at = DateTime::parse_from_rfc3339(required_string(file, "expirationTime")?)
            .map_err(|_| files_error("file metadata has an invalid expiry"))?
            .with_timezone(&Utc);
        let state = match required_string(file, "state")? {
            "PROCESSING" => FileState::Processing,
            "ACTIVE" => FileState::Active,
            "FAILED" => FileState::Failed,
            _ => return Err(files_error("file metadata has an unknown processing state")),
        };
        Ok(Self {
            name,
            uri: uri.to_string(),
            expires_at,
            state,
        })
    }

    fn expires_soon(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.signed_duration_since(now).num_seconds() <= EXPIRY_MARGIN_SECS
    }
}

struct CacheEntry {
    slot: FileSlot,
    last_used: u64,
}

#[derive(Default)]
struct CacheState {
    entries: BTreeMap<ContentKey, CacheEntry>,
    clock: u64,
}

/// Per-provider, bounded metadata-only cache. A per-content async lock prevents
/// concurrent foreground/background requests from uploading the same blob twice.
#[derive(Default)]
pub(super) struct FileCache {
    state: Mutex<CacheState>,
    policy: StagingPolicy,
}

struct UploadContext<'a> {
    client: &'a Client,
    endpoint: &'a Endpoint,
    auth: &'a UploadAuth,
    owner: &'a AgentCx,
}

struct Candidate {
    pointer: String,
    inline_key: &'static str,
    encoded_bytes: usize,
}

impl FileCache {
    pub(super) async fn prepare(
        &self,
        client: &Client,
        base: &str,
        auth: &UploadAuth,
        body: &mut Value,
    ) -> Result<()> {
        let Some(endpoint) = Endpoint::for_provider(base) else {
            return Ok(());
        };
        self.prepare_at(client, &endpoint, auth, body).await
    }

    async fn prepare_at(
        &self,
        client: &Client,
        endpoint: &Endpoint,
        auth: &UploadAuth,
        body: &mut Value,
    ) -> Result<()> {
        let candidates = staging_plan(body, self.policy)?;
        if candidates.is_empty() {
            return Ok(());
        }
        if auth.headers.is_empty() {
            return Err(files_error(
                "media upload requires the generation request's credentials",
            ));
        }
        let owner = AgentCx::for_current_or_request();
        if !owner.capabilities().io {
            return Err(files_error(
                "the request context does not permit media upload I/O",
            ));
        }
        if !owner.capabilities().time {
            return Err(files_error(
                "the request context does not permit bounded media upload timers",
            ));
        }
        checkpoint(&owner)?;
        let now = owner
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let context = UploadContext {
            client,
            endpoint,
            auth,
            owner: &owner,
        };
        asupersync::time::timeout(
            now,
            self.policy.timeout,
            self.prepare_inner(&context, body, candidates),
        )
        .await
        .map_err(|_| files_error("media staging timed out; generation was not started"))?
    }

    async fn prepare_inner(
        &self,
        context: &UploadContext<'_>,
        body: &mut Value,
        candidates: Vec<Candidate>,
    ) -> Result<()> {
        if candidates.is_empty() {
            return Ok(());
        }
        let mut request_bytes = serialized_bytes(body)?;
        // Repeated parts in a single request share the already-validated URI,
        // without an extra files.get call for every occurrence.
        let mut prepared: BTreeMap<ContentKey, String> = BTreeMap::new();
        for candidate in candidates {
            // Candidates are largest first. Once neither the per-part nor
            // complete-request policy requires another upload, all remaining
            // parts can stay inline. Include the actual returned URI in each
            // update: removing just the base64 length undercounts fileData.
            if candidate.encoded_bytes < self.policy.inline_part_bytes
                && request_bytes <= self.policy.inline_total_bytes
            {
                break;
            }
            checkpoint(context.owner)?;
            let part = body
                .pointer_mut(&candidate.pointer)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| files_error("media part changed during staging"))?;
            let previous_part_bytes = serialized_bytes(part)?;
            let inline = part
                .get(candidate.inline_key)
                .ok_or_else(|| files_error("inline media disappeared during staging"))?;
            let mime = inline
                .get("mimeType")
                .or_else(|| inline.get("mime_type"))
                .and_then(Value::as_str)
                .filter(|mime| valid_mime(mime))
                .ok_or_else(|| files_error("inline media has no valid MIME type"))?
                .to_ascii_lowercase();
            let data = required_string(inline, "data")?;
            let bytes = decode_media(data)?;
            let key = content_key(context.endpoint, context.auth, &mime, &bytes);
            let uri = if let Some(uri) = prepared.get(&key) {
                uri.clone()
            } else {
                let uri = self.stage_one(context, key, &mime, data, bytes).await?;
                prepared.insert(key, uri.clone());
                uri
            };
            // Retain every other part field, including thought signatures.
            part.remove(candidate.inline_key);
            part.insert(
                "fileData".to_string(),
                json!({"mimeType": mime, "fileUri": uri}),
            );
            let replacement_part_bytes = serialized_bytes(part)?;
            request_bytes = request_bytes
                .checked_sub(previous_part_bytes)
                .and_then(|bytes| bytes.checked_add(replacement_part_bytes))
                .ok_or_else(|| files_error("generation request byte count overflow"))?;
        }
        // If the prompt itself exceeds the media budget, every inline part
        // has been staged. Do not impose a new limit on the remaining text.
        Ok(())
    }

    fn slot(&self, key: ContentKey) -> Result<FileSlot> {
        let mut cache = self
            .state
            .lock()
            .map_err(|_| files_error("media cache lock poisoned"))?;
        cache.clock = cache.clock.saturating_add(1);
        let used = cache.clock;
        if let Some(entry) = cache.entries.get_mut(&key) {
            entry.last_used = used;
            return Ok(Arc::clone(&entry.slot));
        }
        if cache.entries.len() >= MAX_CACHE_ENTRIES {
            let victim = cache
                .entries
                .iter()
                .filter(|(_, entry)| Arc::strong_count(&entry.slot) == 1)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
                .ok_or_else(|| {
                    files_error("media upload cache is busy; retry after active uploads finish")
                })?;
            cache.entries.remove(&victim);
        }
        let slot = Arc::new(AsyncMutex::new(None));
        cache.entries.insert(
            key,
            CacheEntry {
                slot: Arc::clone(&slot),
                last_used: used,
            },
        );
        // Released before returning rather than at the end of the expression:
        // the caller immediately awaits on the slot, and holding the cache lock
        // a moment longer than the map mutation needs it serialises every other
        // request's cache lookup behind this one.
        drop(cache);
        Ok(slot)
    }

    async fn stage_one(
        &self,
        context: &UploadContext<'_>,
        key: ContentKey,
        mime: &str,
        encoded: &str,
        bytes: Vec<u8>,
    ) -> Result<String> {
        let slot = self.slot(key)?;
        // OWNED guard, not a borrowed one: this is held across the metadata
        // fetch, the upload and the processing poll below, and
        // `asupersync::sync::MutexGuard` is not `Send` — which made this whole
        // future non-`Send`, and with it `FileCache::prepare` and the
        // `Provider::stream` that awaits it, so the crate did not compile.
        // `OwnedMutexGuard` is `Send` for `T: Send` and keeps the lock held for
        // exactly the same span, so two requests for the same content still
        // cannot upload it twice.
        let mut cached = OwnedMutexGuard::lock(slot, context.owner.cx())
            .await
            .map_err(|_| {
                files_error("media upload cancelled while waiting for its content lock")
            })?;
        checkpoint(context.owner)?;
        let size = bytes.len();
        let mut had_remote_file = cached.is_some();
        let mut replacement_used = false;
        // The request body already retains the canonical encoded bytes. Move
        // this decoded buffer into the first upload and decode again only for
        // an actual replacement, rather than holding a second 64 MiB copy
        // across every successful upload and processing poll.
        let mut upload_bytes = Some(bytes);
        // Do not assume a still-live URI exists just because its TTL has not
        // elapsed: the user may have deleted the resource out of band.
        if let Some(file) = cached.as_ref()
            && !file.expires_soon(Utc::now())
            && file.state != FileState::Failed
        {
            let name = file.name.clone();
            *cached = get_metadata(context, &name, mime, size).await?;
        }
        loop {
            checkpoint(context.owner)?;
            if let Some(file) = cached.as_ref() {
                if file.state == FileState::Failed {
                    *cached = None;
                    return Err(files_error(
                        "media processing failed; generation was not started",
                    ));
                }
                if !file.expires_soon(Utc::now()) {
                    if file.state == FileState::Active {
                        return Ok(file.uri.clone());
                    }
                    let name = file.name.clone();
                    context.owner.time().sleep(self.policy.poll_interval).await;
                    *cached = get_metadata(context, &name, mime, size).await?;
                    continue;
                }
                *cached = None;
            }
            // One replacement budget covers both initial cache validation and
            // every processing poll. A cold upload is free; replacing any
            // expired/deleted reference spends the budget. The outer staging
            // timeout remains the same absolute deadline across replacements.
            if had_remote_file {
                if replacement_used {
                    return Err(files_error(
                        "media file disappeared or expired again after one replacement; generation was not started",
                    ));
                }
                replacement_used = true;
            }
            let bytes = match upload_bytes.take() {
                Some(bytes) => bytes,
                None => decode_media(encoded)?,
            };
            let uploaded = upload(context, mime, bytes).await?;
            // Install PROCESSING metadata before awaiting the next poll. A
            // cancelled/timeout request can resume waiting instead of uploading
            // the same media again on the next turn.
            *cached = Some(uploaded);
            had_remote_file = true;
        }
    }
}

fn staging_plan(body: &Value, policy: StagingPolicy) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    if let Some(contents) = body.get("contents").and_then(Value::as_array) {
        for (content_index, content) in contents.iter().enumerate() {
            collect_candidates(
                content,
                &format!("/contents/{content_index}"),
                &mut candidates,
            )?;
        }
    }
    for key in ["systemInstruction", "system_instruction"] {
        if let Some(content) = body.get(key) {
            collect_candidates(content, &format!("/{key}"), &mut candidates)?;
        }
    }
    if candidates.is_empty() {
        return Ok(candidates);
    }
    if candidates
        .iter()
        .all(|item| item.encoded_bytes < policy.inline_part_bytes)
        && serialized_bytes(body)? <= policy.inline_total_bytes
    {
        return Ok(Vec::new());
    }
    // Largest first minimizes the number of Files API resources required to
    // bring the complete request under the inline budget. Keep smaller parts
    // available until the exact size of each returned file URI is known.
    candidates.sort_by_key(|item| std::cmp::Reverse(item.encoded_bytes));
    Ok(candidates)
}

/// Match `RequestBuilder::json`'s compact `serde_json` encoding without allocating
/// another copy of a media-bearing body. Counting complete parts also includes
/// escaped strings, MIME labels, thought signatures and file URI overhead.
fn serialized_bytes(value: &impl serde::Serialize) -> Result<usize> {
    struct Counter(usize);

    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("JSON byte count overflow"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| files_error("generation request byte count overflow"))?;
    Ok(counter.0)
}

fn collect_candidates(content: &Value, prefix: &str, out: &mut Vec<Candidate>) -> Result<()> {
    let Some(parts) = content.get("parts").and_then(Value::as_array) else {
        return Ok(());
    };
    for (index, part) in parts.iter().enumerate() {
        let camel = part.get("inlineData");
        let snake = part.get("inline_data");
        if camel.is_some() && snake.is_some() {
            return Err(files_error("media part contains both inlineData spellings"));
        }
        let (inline_key, inline) = match (camel, snake) {
            (Some(value), None) => ("inlineData", value),
            (None, Some(value)) => ("inline_data", value),
            _ => continue,
        };
        if part.get("fileData").is_some() || part.get("file_data").is_some() {
            return Err(files_error("media part contains both inline and file data"));
        }
        let data = required_string(inline, "data")?;
        if !data.is_empty() {
            out.push(Candidate {
                pointer: format!("{prefix}/parts/{index}"),
                inline_key,
                encoded_bytes: data.len(),
            });
        }
    }
    Ok(())
}

fn decode_media(data: &str) -> Result<Vec<u8>> {
    if data.len() > MAX_UPLOAD_BYTES.div_ceil(3) * 4 {
        return Err(files_error(
            "one media upload exceeds the 64 MiB staging limit",
        ));
    }
    // RPC/SDK native content and persisted attachments accept canonical
    // standard base64 with optional padding. Crossing the staging threshold
    // must not turn a previously accepted unpadded attachment into an error.
    let engine = if data.ends_with('=') {
        &base64::engine::general_purpose::STANDARD
    } else {
        &base64::engine::general_purpose::STANDARD_NO_PAD
    };
    let bytes = engine
        .decode(data)
        .map_err(|_| files_error("inline media is not valid base64"))?;
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(files_error(
            "one media upload exceeds the 64 MiB staging limit",
        ));
    }
    if bytes.is_empty() {
        return Err(files_error("cannot upload empty media"));
    }
    Ok(bytes)
}

fn valid_mime(mime: &str) -> bool {
    let Some((kind, subtype)) = mime.split_once('/') else {
        return false;
    };
    let token = |value: &str| {
        !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'+' | b'.' | b'_')
            })
    };
    mime.len() <= 128 && token(kind) && token(subtype)
}

fn content_key(endpoint: &Endpoint, auth: &UploadAuth, mime: &str, bytes: &[u8]) -> ContentKey {
    let mut hash = Sha256::new();
    let mut field = |value: &[u8]| {
        hash.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        hash.update(value);
    };
    field(endpoint.base.as_str().as_bytes());
    for (name, value) in &auth.headers {
        field(name.as_bytes());
        field(value.as_bytes());
    }
    field(mime.as_bytes());
    field(bytes);
    let mut key = [0; 32];
    key.copy_from_slice(&hash.finalize());
    key
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| files_error(&format!("file data has no valid {field} field")))
}

fn checkpoint(owner: &AgentCx) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| files_error("media upload cancelled"))
}

fn files_error(message: &str) -> Error {
    Error::provider("google", format!("Gemini Files API: {message}"))
}

async fn metadata_body(response: Response) -> Result<Value> {
    let body = response
        .text_limited(MAX_METADATA_BYTES)
        .await
        .map_err(|_| files_error("failed to read bounded file metadata"))?;
    serde_json::from_str(&body).map_err(|_| files_error("invalid file metadata JSON"))
}

async fn get_metadata(
    context: &UploadContext<'_>,
    name: &str,
    mime: &str,
    size: usize,
) -> Result<Option<RemoteFile>> {
    checkpoint(context.owner)?;
    let url = context.endpoint.metadata_url(name)?;
    let request = context.auth.apply(context.client.get(url.as_str()));
    let response = Box::pin(request.send())
        .await
        .map_err(|_| files_error("file metadata request failed"))?;
    let status = response.status();
    if status == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&status) {
        return Err(files_error(&format!(
            "file metadata request failed (HTTP {status})"
        )));
    }
    let value = metadata_body(response).await?;
    let file = RemoteFile::parse(&value, context.endpoint, mime, size)?;
    if file.name != name {
        return Err(files_error("file lookup returned a different resource"));
    }
    Ok(Some(file))
}

async fn upload(context: &UploadContext<'_>, mime: &str, bytes: Vec<u8>) -> Result<RemoteFile> {
    use std::fmt::Write as _;
    checkpoint(context.owner)?;
    let size = bytes.len();
    let mut display_name = String::from("pi-");
    // Display names identify content, not the credential-scoped cache key.
    let digest = Sha256::digest(&bytes);
    for byte in &digest[..16] {
        let _ = write!(display_name, "{byte:02x}");
    }
    let request = context
        .auth
        .apply(context.client.post(context.endpoint.upload.as_str()))
        .header("X-Goog-Upload-Protocol", "resumable")
        .header("X-Goog-Upload-Command", "start")
        .header("X-Goog-Upload-Header-Content-Length", size.to_string())
        .header("X-Goog-Upload-Header-Content-Type", mime)
        .json(&json!({"file": {"displayName": display_name}}))?;
    let response = Box::pin(request.send())
        .await
        .map_err(|_| files_error("upload initialization failed"))?;
    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(files_error(&format!(
            "upload initialization failed (HTTP {status})"
        )));
    }
    let upload_url = response
        .headers()
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-goog-upload-url"))
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| files_error("upload initialization returned no upload URL"))?;
    let upload_url = context.endpoint.validate_upload_url(upload_url)?;
    drop(response);
    checkpoint(context.owner)?;
    // Authentication is carried by the validated upload URL itself. Do not
    // copy API keys or authorization headers into this capability request.
    let request = context
        .client
        .post(upload_url.as_str())
        .header("Content-Type", mime)
        .header("X-Goog-Upload-Offset", "0")
        .header("X-Goog-Upload-Command", "upload, finalize")
        .body(bytes);
    let response = Box::pin(request.send())
        .await
        .map_err(|_| files_error("media transfer failed"))?;
    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(files_error(&format!(
            "media transfer failed (HTTP {status})"
        )));
    }
    let value = metadata_body(response).await?;
    RemoteFile::parse(&value, context.endpoint, mime, size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::Instant;

    fn auth(key: &str) -> UploadAuth {
        UploadAuth {
            headers: vec![("x-goog-api-key".to_string(), key.to_string())],
        }
    }

    fn cache() -> FileCache {
        FileCache {
            policy: StagingPolicy {
                inline_part_bytes: 1,
                inline_total_bytes: usize::MAX,
                timeout: Duration::from_secs(10),
                poll_interval: Duration::ZERO,
            },
            ..FileCache::default()
        }
    }

    fn media_body(bytes: &[u8]) -> Value {
        json!({"contents": [{"role": "user", "parts": [
            {"text": "Describe this recording"},
            {"inlineData": {
                "mimeType": "audio/wav",
                "data": base64::engine::general_purpose::STANDARD.encode(bytes)
            }, "thoughtSignature": "keep-this-field"}
        ]}]})
    }

    fn file_metadata(endpoint: &Endpoint, id: &str, state: &str, size: usize) -> Value {
        json!({
            "name": format!("files/{id}"),
            "uri": endpoint.metadata_url(&format!("files/{id}")).unwrap().as_str(),
            "mimeType": "audio/wav",
            "sizeBytes": size.to_string(),
            "state": state,
            "expirationTime": (Utc::now() + chrono::Duration::hours(24)).to_rfc3339()
        })
    }

    fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
        RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    async fn stage(
        cache: &FileCache,
        client: &Client,
        endpoint: &Endpoint,
        auth: &UploadAuth,
        body: &mut Value,
    ) -> Result<()> {
        let owner = AgentCx::for_current_or_request();
        let context = UploadContext {
            client,
            endpoint,
            auth,
            owner: &owner,
        };
        let candidates = staging_plan(body, cache.policy)?;
        // Exercise the real HTTP/locking/processing path. Like the existing
        // provider wire fixtures, do not let a virtual overall timer race the
        // OS server thread. The outer deadline has its own non-I/O test below.
        cache.prepare_inner(&context, body, candidates).await
    }

    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Reply {
        fn json(body: &Value) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(body).unwrap(),
            }
        }

        fn start(endpoint: &Endpoint) -> Self {
            Self {
                status: 200,
                headers: vec![(
                    "X-Goog-Upload-URL".to_string(),
                    format!("{}?upload_id=test-capability", endpoint.upload),
                )],
                body: b"{}".to_vec(),
            }
        }

        fn status(status: u16) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            }
        }
    }

    #[derive(Debug)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    /// Wall-clock budget for one fixture request/response exchange.
    ///
    /// The socket read timeout below is the POLLING interval, not the budget:
    /// a read that times out means "nothing yet", and the only thing that ends
    /// the wait is this deadline (bd-eg6ng).
    const FIXTURE_EXCHANGE_BUDGET: Duration = Duration::from_secs(30);

    /// Read into `chunk`, treating a socket read timeout as "keep waiting"
    /// until `deadline` rather than as a hard error.
    ///
    /// macOS surfaces a read timeout as EAGAIN/`WouldBlock` (errno 35), not
    /// `TimedOut`, so `read().expect(..)` turned every slow exchange into
    /// `request headers: Os { code: 35, kind: WouldBlock }`. Under the full lib
    /// suite dozens of these fixtures run at once — each spins a listener
    /// thread and its own current_thread runtime — and a client that has simply
    /// not been scheduled yet routinely misses a three-second window. The
    /// accept loop in `Server::start` has always been patient this way; the
    /// reads were not (bd-eg6ng).
    fn read_with_deadline(
        stream: &mut TcpStream,
        chunk: &mut [u8],
        deadline: Instant,
        what: &str,
    ) -> usize {
        loop {
            match stream.read(chunk) {
                Ok(count) => return count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    assert!(
                        Instant::now() < deadline,
                        "fixture timed out waiting for {what}"
                    );
                }
                // ubs:ignore an unexpected socket error in a fixture is an assertion failure
                Err(error) => panic!("{what}: {error}"),
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> CapturedRequest {
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let deadline = Instant::now() + FIXTURE_EXCHANGE_BUDGET;
        let mut data = Vec::new();
        let end = loop {
            if let Some(index) = data.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
            assert!(data.len() < 64 * 1024, "request headers bounded");
            let mut chunk = [0; 4096];
            let count = read_with_deadline(stream, &mut chunk, deadline, "request headers");
            assert!(count > 0, "request closed before headers");
            data.extend_from_slice(&chunk[..count]);
        };
        let header_text = String::from_utf8(data[..end].to_vec()).unwrap();
        let mut lines = header_text.lines();
        let mut first = lines.next().unwrap().split_whitespace();
        let method = first.next().unwrap().to_string();
        let path = first.next().unwrap().to_string();
        let headers: BTreeMap<String, String> = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect();
        let length: usize = headers
            .get("content-length")
            .map_or(0, |value| value.parse().unwrap());
        assert!(length <= 1024 * 1024, "fixture payload bounded");
        let mut body = data[end..].to_vec();
        while body.len() < length {
            let mut chunk = [0; 4096];
            let count = read_with_deadline(stream, &mut chunk, deadline, "request body");
            assert!(count > 0, "request closed before body");
            body.extend_from_slice(&chunk[..count]);
        }
        body.truncate(length);
        CapturedRequest {
            method,
            path,
            headers,
            body,
        }
    }

    struct Server {
        endpoint: Endpoint,
        stop: Arc<AtomicBool>,
        join: Option<JoinHandle<Vec<CapturedRequest>>>,
    }

    impl Server {
        fn start(replies: impl FnOnce(&Endpoint) -> Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            listener.set_nonblocking(true).unwrap();
            let endpoint = Endpoint::from_base(
                Url::parse(&format!("http://{}/v1beta", listener.local_addr().unwrap())).unwrap(),
            )
            .unwrap();
            let replies = replies(&endpoint);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let join = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut captured = Vec::new();
                for reply in replies {
                    let mut stream = loop {
                        if stop_thread.load(Ordering::Relaxed) || Instant::now() >= deadline {
                            return captured;
                        }
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(error) => panic!("accept: {error}"),
                        }
                    };
                    captured.push(read_request(&mut stream));
                    let mut wire = Vec::new();
                    write!(
                        wire,
                        "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                        reply.status, reply.body.len()
                    )
                    .unwrap();
                    for (name, value) in reply.headers {
                        write!(wire, "{name}: {value}\r\n").unwrap();
                    }
                    wire.extend_from_slice(b"\r\n");
                    wire.extend_from_slice(&reply.body);
                    stream.write_all(&wire).unwrap();
                }
                captured
            });
            Self {
                endpoint,
                stop,
                join: Some(join),
            }
        }

        fn finish(mut self) -> Vec<CapturedRequest> {
            self.stop.store(true, Ordering::Relaxed);
            self.join.take().unwrap().join().expect("server completed")
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    #[test]
    fn only_official_developer_api_is_staged_automatically() {
        for base in [
            "",
            API_BASE,
            "https://generativelanguage.googleapis.com:443/v1beta/",
        ] {
            assert!(Endpoint::for_provider(base).is_some(), "{base}");
        }
        for base in [
            "http://generativelanguage.googleapis.com/v1beta",
            "https://generativelanguage.googleapis.com:8443/v1beta",
            "https://generativelanguage.googleapis.com.attacker.test/v1beta",
            "https://secret@generativelanguage.googleapis.com/v1beta",
            "https://generativelanguage.googleapis.com/v1beta?key=secret",
            "https://generativelanguage.googleapis.com/v1beta#fragment",
            "https://generativelanguage.googleapis.com/v1internal",
            "https://aiplatform.googleapis.com/v1beta",
            "https://cloudcode-pa.googleapis.com",
            "http://localhost:1234/v1beta",
        ] {
            assert!(Endpoint::for_provider(base).is_none(), "{base}");
        }
    }

    #[test]
    fn upload_capabilities_cannot_redirect_media_or_credentials() {
        let endpoint = Endpoint::for_provider(API_BASE).unwrap();
        assert!(
            endpoint
                .validate_upload_url(&format!("{}?upload_id=x", endpoint.upload))
                .is_ok()
        );
        for target in [
            "https://attacker.test/upload/v1beta/files?upload_id=secret",
            "http://generativelanguage.googleapis.com/upload/v1beta/files",
            "https://generativelanguage.googleapis.com/upload/v1beta/files#secret",
            "https://secret@generativelanguage.googleapis.com/upload/v1beta/files",
            "https://generativelanguage.googleapis.com/v1beta/files",
            "https://generativelanguage.googleapis.com/upload/v1beta/files\r\nX-Key: secret",
        ] {
            let error = endpoint
                .validate_upload_url(target)
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn resource_names_cannot_escape_the_files_collection() {
        let endpoint = Endpoint::for_provider(API_BASE).unwrap();
        for name in [
            "files/",
            "files/../models",
            "files/a/b",
            "files/%2e%2e",
            "files/a?key=x",
            "models/a",
        ] {
            assert!(endpoint.metadata_url(name).is_err(), "{name}");
        }
        assert_eq!(
            endpoint.metadata_url("files/a-1_b").unwrap().path(),
            "/v1beta/files/a-1_b"
        );
    }

    #[test]
    fn cache_identity_includes_content_mime_and_credentials() {
        let endpoint = Endpoint::for_provider(API_BASE).unwrap();
        let original = content_key(&endpoint, &auth("a"), "audio/wav", b"abc");
        assert_eq!(
            original,
            content_key(&endpoint, &auth("a"), "audio/wav", b"abc")
        );
        assert_ne!(
            original,
            content_key(&endpoint, &auth("b"), "audio/wav", b"abc")
        );
        assert_ne!(
            original,
            content_key(&endpoint, &auth("a"), "audio/mpeg", b"abc")
        );
        assert_ne!(
            original,
            content_key(&endpoint, &auth("a"), "audio/wav", b"abd")
        );
        let mut quota_auth = auth("a");
        quota_auth
            .headers
            .push(("x-goog-user-project".into(), "other-project".into()));
        assert_ne!(
            original,
            content_key(&endpoint, &quota_auth, "audio/wav", b"abc")
        );
    }

    #[test]
    fn request_auth_overrides_do_not_reintroduce_a_fallback_key() {
        let mut options = StreamOptions::default();
        options
            .headers
            .insert("Authorization".into(), "Bearer caller".into());
        options
            .headers
            .insert("X-Goog-User-Project".into(), "quota-project".into());
        let auth = UploadAuth::for_request(&options, None, Some("must-not-leak"));
        assert!(!auth.headers.iter().any(|(key, _)| key == "x-goog-api-key"));
        assert!(
            auth.headers
                .contains(&("authorization".into(), "Bearer caller".into()))
        );
        assert!(
            auth.headers
                .contains(&("x-goog-user-project".into(), "quota-project".into()))
        );
        options
            .headers
            .insert("X-Goog-Api-Key".into(), "override".into());
        let auth = UploadAuth::for_request(&options, None, Some("fallback"));
        assert!(
            auth.headers
                .contains(&("x-goog-api-key".into(), "override".into()))
        );
        assert!(!auth.headers.iter().any(|(_, value)| value == "fallback"));
    }

    #[test]
    fn small_requests_do_not_create_file_resources() {
        assert!(
            staging_plan(&media_body(b"abc"), StagingPolicy::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn aggregate_policy_orders_only_media_parts_without_rewriting_tool_arguments() {
        let body = json!({"contents": [{"parts": [
            {"inlineData": {"mimeType": "audio/wav", "data": "aaaaaaaa"}},
            {"inline_data": {"mime_type": "audio/wav", "data": "bbbb"}},
            {"functionCall": {"name": "inspect", "args": {
                "inlineData": {"mimeType": "audio/wav", "data": "cccccccccccccccc"}
            }}}
        ]}]});
        let plan = staging_plan(
            &body,
            StagingPolicy {
                inline_part_bytes: 100,
                inline_total_bytes: 5,
                ..StagingPolicy::default()
            },
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].pointer, "/contents/0/parts/0");
        assert_eq!(plan[1].pointer, "/contents/0/parts/1");
    }

    #[test]
    fn aggregate_budget_counts_serialized_prompts_tools_and_escape_expansion() {
        let mut body = media_body(b"abc");
        body["systemInstruction"] = json!({"parts": [{"text": "\u{0001}".repeat(100)}]});
        body["tools"] = json!([{"functionDeclarations": [{
            "name": "inspect", "description": "\n\"\\".repeat(100)
        }]}]);
        let bytes = serde_json::to_vec(&body).unwrap().len();
        let policy = StagingPolicy {
            inline_part_bytes: usize::MAX,
            inline_total_bytes: bytes,
            ..StagingPolicy::default()
        };
        assert!(staging_plan(&body, policy).unwrap().is_empty());
        assert_eq!(
            staging_plan(
                &body,
                StagingPolicy {
                    inline_total_bytes: bytes - 1,
                    ..policy
                },
            )
            .unwrap()
            .len(),
            1,
            "one extra byte of the whole request triggers media staging"
        );
        let text_only = json!({"contents": [{"parts": [{"text": "x".repeat(bytes)}]}]});
        assert!(staging_plan(&text_only, policy).unwrap().is_empty());
    }

    #[test]
    fn ambiguous_media_shapes_fail_before_upload() {
        for part in [
            json!({"inlineData": {"data": "YQ=="}, "inline_data": {"data": "Yg=="}}),
            json!({"inlineData": {"data": "YQ=="}, "fileData": {"fileUri": "elsewhere"}}),
        ] {
            assert!(
                staging_plan(&json!({"contents": [{"parts": [part]}]}), cache().policy).is_err()
            );
        }
    }

    #[test]
    fn media_decoder_and_mime_validation_fail_closed() {
        assert_eq!(decode_media("AAEC/w==").unwrap(), [0, 1, 2, 255]);
        assert_eq!(decode_media("AAEC/w").unwrap(), [0, 1, 2, 255]);
        for data in [
            "",
            "not-base64!",
            "A",
            "AB==",
            "AB",
            "_w==",
            "AAEC/w=",
            "Y Q==",
        ] {
            assert!(decode_media(data).is_err(), "{data}");
        }
        assert!(valid_mime("audio/x-wav"));
        assert!(valid_mime("application/pdf"));
        for mime in [
            "audio",
            "/wav",
            "audio/",
            "audio/wav\r\nX-Key: x",
            "audio/a/b",
        ] {
            assert!(!valid_mime(mime));
        }
    }

    #[test]
    fn metadata_must_match_resource_mime_size_state_and_expiry() {
        let endpoint = Endpoint::for_provider(API_BASE).unwrap();
        let good = file_metadata(&endpoint, "one", "ACTIVE", 3);
        assert!(RemoteFile::parse(&good, &endpoint, "audio/wav", 3).is_ok());
        for (field, bad) in [
            ("uri", json!("https://attacker.test/file")),
            ("mimeType", json!("video/mp4")),
            ("sizeBytes", json!("4")),
            ("state", json!("UNKNOWN")),
            ("expirationTime", json!("invalid")),
        ] {
            let mut value = good.clone();
            value[field] = bad;
            assert!(
                RemoteFile::parse(&value, &endpoint, "audio/wav", 3).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn cache_is_bounded_and_never_evicts_an_in_flight_slot() {
        let cache = cache();
        let mut active = Vec::new();
        for index in 0..MAX_CACHE_ENTRIES {
            let mut key = [0; 32];
            key[0] = u8::try_from(index).unwrap();
            active.push(cache.slot(key).unwrap());
        }
        assert!(cache.slot([255; 32]).is_err());
        assert_eq!(cache.state.lock().unwrap().entries.len(), MAX_CACHE_ENTRIES);
        drop(active.remove(0));
        assert!(cache.slot([255; 32]).is_ok());
        let mut retained_key = [0; 32];
        retained_key[0] = 1;
        let state = cache.state.lock().unwrap();
        assert_eq!(state.entries.len(), MAX_CACHE_ENTRIES);
        assert!(!state.entries.contains_key(&[0; 32]));
        assert!(Arc::ptr_eq(
            &active[0],
            &state.entries.get(&retained_key).unwrap().slot,
        ));
        drop(state);
    }

    #[test]
    fn upload_reuses_padded_and_unpadded_content_across_parts_and_turns() {
        let server = Server::start(|endpoint| {
            vec![
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "one", "ACTIVE", 4)})),
                Reply::json(&file_metadata(endpoint, "one", "ACTIVE", 4)),
            ]
        });
        let cache = cache();
        let client = Client::new();
        let auth = auth("key-a");
        let original = media_body(&[0, 255, 128, 42]);
        let mut body = original.clone();
        let mut duplicate = body["contents"][0]["parts"][1].clone();
        duplicate["inlineData"]["data"] =
            json!(base64::engine::general_purpose::STANDARD_NO_PAD.encode([0, 255, 128, 42]));
        body["contents"][0]["parts"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        run_async(async {
            stage(&cache, &client, &server.endpoint, &auth, &mut body)
                .await
                .unwrap();
            let mut next_turn = original.clone();
            next_turn["contents"][0]["parts"][1]["inlineData"]["data"] =
                json!(base64::engine::general_purpose::STANDARD_NO_PAD.encode([0, 255, 128, 42]));
            stage(&cache, &client, &server.endpoint, &auth, &mut next_turn)
                .await
                .unwrap();
            assert_eq!(
                body["contents"][0]["parts"][1],
                next_turn["contents"][0]["parts"][1]
            );
        });
        assert_eq!(
            body["contents"][0]["parts"][1],
            body["contents"][0]["parts"][2]
        );
        assert_eq!(
            body["contents"][0]["parts"][1]["thoughtSignature"],
            "keep-this-field"
        );
        assert!(body["contents"][0]["parts"][1].get("inlineData").is_none());
        assert!(
            original["contents"][0]["parts"][1]
                .get("inlineData")
                .is_some()
        );
        let requests = server.finish();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/upload/v1beta/files");
        assert_eq!(requests[0].headers["x-goog-api-key"], "key-a");
        assert_eq!(
            requests[0].headers["x-goog-upload-header-content-length"],
            "4"
        );
        assert_eq!(requests[1].body, [0, 255, 128, 42]);
        assert_eq!(
            requests[1].headers["x-goog-upload-command"],
            "upload, finalize"
        );
        assert_eq!(requests[1].headers["x-goog-upload-offset"], "0");
        assert!(!requests[1].headers.contains_key("x-goog-api-key"));
        assert!(!requests[1].headers.contains_key("authorization"));
        assert_eq!(requests[2].method, "GET");
        assert_eq!(requests[2].path, "/v1beta/files/one");
    }

    #[test]
    fn aggregate_staging_accounts_for_file_uris_on_the_generation_wire() {
        let large = vec![1; 1024];
        let medium = vec![2; 769];
        let small = vec![3; 96];
        let long_name = "a".repeat(128);
        let server = Server::start(|endpoint| {
            vec![
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(
                    endpoint, &long_name, "ACTIVE", large.len()
                )})),
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(
                    endpoint, "two", "ACTIVE", medium.len()
                )})),
                Reply::json(&json!({"candidates": []})),
            ]
        });
        let mut body = media_body(&large);
        body["contents"][0]["parts"]
            .as_array_mut()
            .unwrap()
            .extend([
                json!({"inline_data": {"mime_type": "audio/wav",
                    "data": base64::engine::general_purpose::STANDARD_NO_PAD.encode(&medium)}}),
                json!({"inlineData": {"mimeType": "audio/wav",
                    "data": base64::engine::general_purpose::STANDARD.encode(&small)}}),
            ]);
        body["systemInstruction"] = json!({"parts": [{"text": "\u{0001}".repeat(300)}]});
        body["tools"] = json!([{"functionDeclarations": [{
            "name": "inspect", "description": "\n\"\\".repeat(150)
        }]}]);
        let original = body.clone();
        let mut after_first = original.clone();
        let part = after_first["contents"][0]["parts"][1]
            .as_object_mut()
            .unwrap();
        part.remove("inlineData");
        part.insert(
            "fileData".into(),
            json!({"mimeType": "audio/wav", "fileUri":
                server.endpoint.metadata_url(&format!("files/{long_name}")).unwrap().as_str()}),
        );
        // Stop one byte below the real post-upload size. Subtracting only the
        // original base64 length would incorrectly leave the medium part inline.
        let budget = serde_json::to_vec(&after_first).unwrap().len() - 1;
        assert!(budget > 4 * (large.len() + medium.len() + small.len()).div_ceil(3));
        let cache = FileCache {
            policy: StagingPolicy {
                inline_part_bytes: usize::MAX,
                inline_total_bytes: budget,
                ..cache().policy
            },
            ..FileCache::default()
        };
        let client = Client::new();
        let auth = auth("key-a");
        let url = server
            .endpoint
            .base
            .join("models/test:streamGenerateContent?alt=sse")
            .unwrap();
        run_async(async {
            stage(&cache, &client, &server.endpoint, &auth, &mut body)
                .await
                .unwrap();
            let request = client.post(url.as_str()).json(&body).unwrap();
            let response = Box::pin(request.send()).await.unwrap();
            assert_eq!(response.status(), 200);
            response.text().await.unwrap();
        });
        assert!(body["contents"][0]["parts"][1].get("fileData").is_some());
        assert!(body["contents"][0]["parts"][2].get("fileData").is_some());
        assert_eq!(
            body["contents"][0]["parts"][3],
            original["contents"][0]["parts"][3]
        );
        assert_eq!(body["systemInstruction"], original["systemInstruction"]);
        assert_eq!(body["tools"], original["tools"]);
        let requests = server.finish();
        assert_eq!(requests.len(), 5, "two uploads followed by generation");
        assert_eq!(requests[1].body, large);
        assert_eq!(requests[3].body, medium);
        let generation = &requests[4];
        assert_eq!(
            generation.path,
            "/v1beta/models/test:streamGenerateContent?alt=sse"
        );
        assert_eq!(generation.body, serde_json::to_vec(&body).unwrap());
        assert!(generation.body.len() <= budget);
        assert_eq!(
            generation.headers["content-length"],
            generation.body.len().to_string()
        );
    }

    #[test]
    fn processing_upload_is_not_exposed_until_active() {
        let server = Server::start(|endpoint| {
            vec![
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "one", "PROCESSING", 3)})),
                Reply::json(&file_metadata(endpoint, "one", "PROCESSING", 3)),
                Reply::json(&file_metadata(endpoint, "one", "ACTIVE", 3)),
            ]
        });
        let mut body = media_body(b"abc");
        run_async(stage(
            &cache(),
            &Client::new(),
            &server.endpoint,
            &auth("a"),
            &mut body,
        ))
        .unwrap();
        assert!(body["contents"][0]["parts"][1].get("fileData").is_some());
        assert_eq!(server.finish().len(), 4);
    }

    #[test]
    fn processing_reference_recovery_reuploads_identical_bytes_once() {
        for expired in [false, true] {
            let server = Server::start(|endpoint| {
                let lost = if expired {
                    let mut metadata = file_metadata(endpoint, "old", "PROCESSING", 3);
                    metadata["expirationTime"] =
                        json!((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
                    Reply::json(&metadata)
                } else {
                    Reply::status(404)
                };
                vec![
                    Reply::start(endpoint),
                    Reply::json(&json!({"file": file_metadata(endpoint, "old", "PROCESSING", 3)})),
                    lost,
                    Reply::start(endpoint),
                    Reply::json(&json!({"file": file_metadata(endpoint, "new", "PROCESSING", 3)})),
                    Reply::json(&file_metadata(endpoint, "new", "ACTIVE", 3)),
                ]
            });
            let original = media_body(b"abc");
            let mut body = original.clone();
            run_async(stage(
                &cache(),
                &Client::new(),
                &server.endpoint,
                &auth("same-credential"),
                &mut body,
            ))
            .expect("a disappeared or expired processing file is replaced");
            let part = &body["contents"][0]["parts"][1];
            assert!(
                part["fileData"]["fileUri"]
                    .as_str()
                    .unwrap()
                    .ends_with("/new")
            );
            assert_eq!(part["thoughtSignature"], "keep-this-field");
            assert!(part.get("inlineData").is_none());
            assert_eq!(
                original["contents"][0]["parts"][1]["inlineData"]["data"],
                "YWJj"
            );
            let requests = server.finish();
            assert_eq!(requests.len(), 6, "two uploads and two processing polls");
            assert_eq!(requests[1].body.as_slice(), b"abc");
            assert_eq!(
                requests[4].body.as_slice(),
                b"abc",
                "replacement bytes are identical"
            );
            assert_eq!(requests[2].path, "/v1beta/files/old");
            assert_eq!(requests[5].path, "/v1beta/files/new");
            for index in [0, 2, 3, 5] {
                assert_eq!(requests[index].headers["x-goog-api-key"], "same-credential");
            }
            for index in [1, 4] {
                assert!(!requests[index].headers.contains_key("x-goog-api-key"));
                assert!(!requests[index].headers.contains_key("authorization"));
            }
        }
    }

    #[test]
    fn processing_recovery_shares_one_replacement_budget_with_cached_reference_recovery() {
        for initial in ["cold", "expired_cache", "deleted_cache"] {
            let server = Server::start(|endpoint| {
                let mut replies = Vec::new();
                if initial == "cold" {
                    replies.push(Reply::start(endpoint));
                    replies.push(Reply::json(&json!({
                        "file": file_metadata(endpoint, "old", "PROCESSING", 3)
                    })));
                }
                if initial != "expired_cache" {
                    replies.push(Reply::status(404));
                }
                replies.extend([
                    Reply::start(endpoint),
                    Reply::json(&json!({
                        "file": file_metadata(endpoint, "replacement", "PROCESSING", 3)
                    })),
                    Reply::status(404),
                ]);
                replies
            });
            let cache = cache();
            let client = Client::new();
            let auth = auth("a");
            let key = content_key(&server.endpoint, &auth, "audio/wav", b"abc");
            let original = media_body(b"abc");
            let mut body = original.clone();
            let error = run_async(async {
                let owner = AgentCx::for_current_or_request();
                if initial != "cold" {
                    let mut old = RemoteFile::parse(
                        &file_metadata(&server.endpoint, "old", "ACTIVE", 3),
                        &server.endpoint,
                        "audio/wav",
                        3,
                    )
                    .unwrap();
                    if initial == "expired_cache" {
                        old.expires_at = Utc::now() - chrono::Duration::seconds(1);
                    }
                    let slot = cache.slot(key).unwrap();
                    *slot.lock(owner.cx()).await.unwrap() = Some(old);
                }
                let error = stage(&cache, &client, &server.endpoint, &auth, &mut body)
                    .await
                    .expect_err("a second unavailable reference must stop staging");
                let slot = cache.slot(key).unwrap();
                assert!(slot.lock(owner.cx()).await.unwrap().is_none());
                error
            });
            assert!(
                error.to_string().contains("after one replacement"),
                "{initial}: {error}"
            );
            assert_eq!(
                body, original,
                "failure cannot publish an unusable file URI"
            );
            let requests = server.finish();
            let expected = match initial {
                "cold" => 6,
                "expired_cache" => 3,
                _ => 4,
            };
            assert_eq!(requests.len(), expected, "{initial}");
            assert_eq!(requests.last().unwrap().path, "/v1beta/files/replacement");
            assert_eq!(requests[requests.len() - 2].body.as_slice(), b"abc");
        }
    }

    #[test]
    fn processing_failure_and_authorization_refusal_never_trigger_reupload() {
        for failed_state in [false, true] {
            let server = Server::start(|endpoint| {
                vec![
                    Reply::start(endpoint),
                    Reply::json(&json!({"file": file_metadata(endpoint, "one", "PROCESSING", 3)})),
                    if failed_state {
                        Reply::json(&file_metadata(endpoint, "one", "FAILED", 3))
                    } else {
                        Reply::status(403)
                    },
                ]
            });
            let original = media_body(b"abc");
            let mut body = original.clone();
            let error = run_async(stage(
                &cache(),
                &Client::new(),
                &server.endpoint,
                &auth("a"),
                &mut body,
            ))
            .expect_err("a failed or forbidden file must not be reuploaded");
            let expected = if failed_state {
                "processing failed"
            } else {
                "HTTP 403"
            };
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(body, original);
            assert_eq!(server.finish().len(), 3);
        }
    }

    #[test]
    fn failed_processing_does_not_replace_inline_media_or_start_generation() {
        let server = Server::start(|endpoint| {
            vec![
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "one", "FAILED", 3)})),
            ]
        });
        let original = media_body(b"abc");
        let mut body = original.clone();
        let error = run_async(stage(
            &cache(),
            &Client::new(),
            &server.endpoint,
            &auth("a"),
            &mut body,
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("processing failed"), "{error}");
        assert_eq!(body, original);
        assert_eq!(server.finish().len(), 2);
    }

    #[test]
    fn deleted_remote_file_is_reuploaded_on_the_next_turn() {
        let server = Server::start(|endpoint| {
            vec![
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "old", "ACTIVE", 3)})),
                Reply::status(404),
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "new", "ACTIVE", 3)})),
            ]
        });
        let cache = cache();
        let client = Client::new();
        let auth = auth("a");
        let mut body = media_body(b"abc");
        run_async(async {
            stage(&cache, &client, &server.endpoint, &auth, &mut body)
                .await
                .unwrap();
            body = media_body(b"abc");
            stage(&cache, &client, &server.endpoint, &auth, &mut body)
                .await
                .unwrap();
        });
        assert!(
            body["contents"][0]["parts"][1]["fileData"]["fileUri"]
                .as_str()
                .unwrap()
                .ends_with("/new")
        );
        assert_eq!(server.finish().len(), 5);
    }

    #[test]
    fn expired_cache_entry_is_reuploaded_without_using_the_stale_uri() {
        let server = Server::start(|endpoint| {
            vec![
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "new", "ACTIVE", 3)})),
            ]
        });
        let cache = cache();
        let client = Client::new();
        let auth = auth("a");
        let key = content_key(&server.endpoint, &auth, "audio/wav", b"abc");
        let mut body = media_body(b"abc");
        run_async(async {
            let owner = AgentCx::for_current_or_request();
            let slot = cache.slot(key).unwrap();
            let mut old = RemoteFile::parse(
                &file_metadata(&server.endpoint, "old", "ACTIVE", 3),
                &server.endpoint,
                "audio/wav",
                3,
            )
            .unwrap();
            old.expires_at = Utc::now() - chrono::Duration::seconds(1);
            *slot.lock(owner.cx()).await.unwrap() = Some(old);
            stage(&cache, &client, &server.endpoint, &auth, &mut body)
                .await
                .unwrap();
        });
        let requests = server.finish();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST", "do not reuse an expired file");
    }

    #[test]
    fn concurrent_requests_upload_identical_content_only_once() {
        let server = Server::start(|endpoint| {
            vec![
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "one", "ACTIVE", 3)})),
                Reply::json(&file_metadata(endpoint, "one", "ACTIVE", 3)),
            ]
        });
        let cache = cache();
        let client = Client::new();
        let auth = auth("a");
        let mut left = media_body(b"abc");
        let mut right = left.clone();
        run_async(async {
            let (a, b) = futures::join!(
                stage(&cache, &client, &server.endpoint, &auth, &mut left),
                stage(&cache, &client, &server.endpoint, &auth, &mut right),
            );
            a.unwrap();
            b.unwrap();
        });
        assert_eq!(left, right);
        let requests = server.finish();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "POST")
                .count(),
            2
        );
    }

    #[test]
    fn changed_credentials_never_reuse_another_projects_file() {
        let server = Server::start(|endpoint| {
            vec![
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "project-a", "ACTIVE", 3)})),
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "project-b", "ACTIVE", 3)})),
            ]
        });
        let cache = cache();
        let client = Client::new();
        run_async(async {
            let mut body = media_body(b"abc");
            stage(&cache, &client, &server.endpoint, &auth("key-a"), &mut body)
                .await
                .unwrap();
            let mut body = media_body(b"abc");
            stage(&cache, &client, &server.endpoint, &auth("key-b"), &mut body)
                .await
                .unwrap();
        });
        let requests = server.finish();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[2].method, "POST");
        assert_eq!(requests[2].headers["x-goog-api-key"], "key-b");
    }

    #[test]
    fn authorization_errors_do_not_trigger_duplicate_uploads() {
        let server = Server::start(|endpoint| {
            vec![
                Reply::start(endpoint),
                Reply::json(&json!({"file": file_metadata(endpoint, "one", "ACTIVE", 3)})),
                Reply::status(403),
            ]
        });
        let cache = cache();
        let client = Client::new();
        let auth = auth("a");
        let error = run_async(async {
            let mut body = media_body(b"abc");
            stage(&cache, &client, &server.endpoint, &auth, &mut body)
                .await
                .unwrap();
            let mut body = media_body(b"abc");
            stage(&cache, &client, &server.endpoint, &auth, &mut body)
                .await
                .unwrap_err()
        });
        assert!(error.to_string().contains("HTTP 403"));
        assert_eq!(server.finish().len(), 3);
    }

    #[test]
    fn unsafe_upload_url_is_rejected_before_sending_file_bytes() {
        let server = Server::start(|_| {
            vec![Reply {
                status: 200,
                headers: vec![(
                    "x-goog-upload-url".into(),
                    "https://attacker.test/upload/v1beta/files?upload_id=secret".into(),
                )],
                body: b"{}".to_vec(),
            }]
        });
        let mut body = media_body(b"abc");
        let error = run_async(stage(
            &cache(),
            &Client::new(),
            &server.endpoint,
            &auth("a"),
            &mut body,
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("off-origin"));
        assert!(!error.contains("secret"));
        assert!(body["contents"][0]["parts"][1].get("inlineData").is_some());
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn cancelled_owner_prevents_media_network_dispatch() {
        let endpoint = Endpoint::for_provider(API_BASE).unwrap();
        run_async(async {
            let owner = AgentCx::for_request();
            owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel test"));
            let client = Client::new();
            let auth = auth("a");
            let context = UploadContext {
                client: &client,
                endpoint: &endpoint,
                auth: &auth,
                owner: &owner,
            };
            let cache = cache();
            let mut body = media_body(b"abc");
            let plan = staging_plan(&body, cache.policy).unwrap();
            let error = cache
                .prepare_inner(&context, &mut body, plan)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("cancelled"));
            assert!(cache.state.lock().unwrap().entries.is_empty());
        });
    }

    #[test]
    fn custom_gateway_preserves_inline_body_without_contacting_google() {
        let cache = cache();
        let original = media_body(b"abc");
        let mut body = original.clone();
        run_async(cache.prepare(
            &Client::new(),
            "https://gateway.example/v1beta",
            &auth("a"),
            &mut body,
        ))
        .unwrap();
        assert_eq!(body, original);
        assert!(cache.state.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn overall_deadline_also_bounds_waiting_for_an_in_flight_upload() {
        let endpoint = Endpoint::for_provider(API_BASE).unwrap();
        let cache = FileCache {
            policy: StagingPolicy {
                timeout: Duration::ZERO,
                ..cache().policy
            },
            ..FileCache::default()
        };
        let auth = auth("a");
        let client = Client::new();
        let key = content_key(&endpoint, &auth, "audio/wav", b"abc");
        run_async(async {
            let owner = AgentCx::for_current_or_request();
            let slot = cache.slot(key).unwrap();
            let held = slot.lock(owner.cx()).await.unwrap();
            let mut body = media_body(b"abc");
            let original = body.clone();
            let error = cache
                .prepare_at(&client, &endpoint, &auth, &mut body)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("timed out"));
            assert_eq!(body, original);
            assert!(
                held.is_none(),
                "deadline must not disturb the existing owner"
            );
            drop(held);
            assert!(slot.lock(owner.cx()).await.unwrap().is_none());
        });
    }

    #[test]
    fn previously_processing_upload_resumes_without_reupload() {
        let server = Server::start(|endpoint| {
            vec![Reply::json(&file_metadata(
                endpoint, "existing", "ACTIVE", 3,
            ))]
        });
        let cache = cache();
        let client = Client::new();
        let auth = auth("a");
        let key = content_key(&server.endpoint, &auth, "audio/wav", b"abc");
        run_async(async {
            let owner = AgentCx::for_current_or_request();
            let slot = cache.slot(key).unwrap();
            let processing = RemoteFile::parse(
                &file_metadata(&server.endpoint, "existing", "PROCESSING", 3),
                &server.endpoint,
                "audio/wav",
                3,
            )
            .unwrap();
            *slot.lock(owner.cx()).await.unwrap() = Some(processing);
            let mut body = media_body(b"abc");
            stage(&cache, &client, &server.endpoint, &auth, &mut body)
                .await
                .unwrap();
        });
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/v1beta/files/existing");
    }

    #[test]
    fn staging_future_is_send_for_the_provider_stream_boundary() {
        fn assert_send<T: Send>(_: T) {}
        let cache = cache();
        let client = Client::new();
        let auth = auth("a");
        let mut body = media_body(b"abc");
        assert_send(cache.prepare(&client, API_BASE, &auth, &mut body));
    }
}
