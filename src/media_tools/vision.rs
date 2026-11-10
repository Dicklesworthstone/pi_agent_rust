//! Native image inspection: upload the actual image and return the model's answer.

use super::MAX_IMAGE_FILE_SIZE_BYTES;
use super::transport::{self, Transport};
use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const NAME: &str = "inspect_image";
const DEFAULT_PROMPT: &str =
    "Describe this image in detail, including visible text, diagrams, UI components, and errors.";
const MAX_ANALYSIS_BYTES: usize = 64 * 1024;

pub struct InspectImageTool {
    cwd: PathBuf,
    workspace: crate::workspace::WorkspaceHandle,
    default_provider: Option<String>,
    default_model: Option<String>,
    mock_mode: Option<bool>,
    api_key: Option<String>,
    transport: Transport,
}

impl InspectImageTool {
    pub fn new(cwd: &Path) -> Self {
        Self::with_defaults(cwd, None, None)
    }

    pub fn with_defaults(cwd: &Path, provider: Option<String>, model: Option<String>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: crate::workspace::WorkspaceHandle::default(),
            default_provider: provider,
            default_model: model,
            mock_mode: None,
            api_key: None,
            transport: Transport::default(),
        }
    }

    /// Share the session's live read roots, including additions and revocations.
    #[must_use]
    pub fn with_workspace(mut self, workspace: crate::workspace::WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    #[must_use]
    pub const fn with_mock(mut self, mock: bool) -> Self {
        self.mock_mode = Some(mock);
        self
    }

    #[must_use]
    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        self.api_key = key;
        self
    }

    /// Override the API base for an explicitly trusted gateway or a protocol test.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.transport = self.transport.with_base_url(base_url.into());
        self
    }

    /// Preserve the caller's HTTP configuration and optional VCR recorder.
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.transport = self.transport.with_client(client);
        self
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for InspectImageTool {
    fn bind_cost_scope(&mut self, scope: crate::cost::CostScope) {
        self.transport.bind_cost_scope(scope);
    }

    fn bind_media_auth_scope(&mut self, scope: super::MediaAuthScope) {
        self.transport.bind_auth_scope(scope);
    }

    fn name(&self) -> &str {
        NAME
    }
    fn label(&self) -> &str {
        "Inspect Image"
    }
    fn description(&self) -> &str {
        "Send a local PNG, JPEG, WebP or GIF to a vision provider for description, OCR or diagram analysis. Returns the actual provider answer; requires a media API key."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object", "required": ["path"],
            "properties": {
                "path": {"type": "string", "description": "Local image path (PNG, JPEG, WebP, GIF; at most 20 MiB)"},
                "prompt": {"type": "string", "description": "Question or analysis instructions for the image"},
                "provider": {"type": "string", "enum": ["openai", "anthropic", "gemini"]},
                "model": {"type": "string", "description": "Vision model ID; overrides media.vision_model"},
                "detail": {"type": "string", "enum": ["low", "high", "auto"], "description": "OpenAI image detail (default auto)"},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 300_000, "default": 120_000}
            }
        })
    }
    fn effects(&self) -> ToolEffects {
        ToolEffects::read().union(ToolEffects::network())
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let path_str = transport::required(&args, NAME, "path")?;
        let path = transport::resolve_input(&self.cwd, Path::new(path_str), &self.workspace, NAME)?;
        if !path.is_file() {
            return Err(Error::tool(
                NAME,
                format!("image file not found: {}", path.display()),
            ));
        }
        let metadata = std::fs::metadata(&path)
            .map_err(|error| Error::tool(NAME, format!("cannot stat image file: {error}")))?;
        if metadata.len() > MAX_IMAGE_FILE_SIZE_BYTES {
            return Err(Error::tool(
                NAME,
                format!("image file exceeds 20 MiB limit: {} bytes", metadata.len()),
            ));
        }
        let ext = Path::new(path_str)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mime = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "bmp" => "image/bmp",
            _ => {
                return Err(Error::tool(
                    NAME,
                    format!("unsupported image extension: .{ext}"),
                ));
            }
        };
        let prompt = transport::optional(&args, NAME, "prompt")?.unwrap_or(DEFAULT_PROMPT);
        if prompt.trim().is_empty() || prompt.len() > 128 * 1024 {
            return Err(Error::tool(
                NAME,
                "prompt must be nonempty and at most 128 KiB",
            ));
        }
        let detail = transport::optional(&args, NAME, "detail")?.unwrap_or("auto");
        if !matches!(detail, "auto" | "low" | "high") {
            return Err(Error::tool(NAME, "detail must be low, high or auto"));
        }
        let timeout = transport::timeout(&args, NAME, 120_000)?;
        let is_mock = self
            .mock_mode
            .unwrap_or_else(|| std::env::var("PI_MEDIA_MOCK").unwrap_or_default() == "1");
        if is_mock {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "Image Analysis for {path_str} ({mime}, {} bytes):\nPrompt: {prompt}\nCanned test fixture inspection completed successfully; no provider request was made.",
                    metadata.len()
                )))],
                details: Some(
                    json!({"path": path.display().to_string(), "mime_type": mime, "size_bytes": metadata.len(),
                    "provider": self.default_provider, "model": self.default_model, "mock": true}),
                ),
                is_error: false,
            });
        }
        let env_provider = std::env::var("PI_VISION_PROVIDER").ok();
        let provider = transport::provider(
            NAME,
            transport::optional(&args, NAME, "provider")?
                .or(self.default_provider.as_deref())
                .or(env_provider.as_deref())
                .unwrap_or("gemini"),
        )?;
        let fallback_model = match provider {
            "openai" => "gpt-4.1-mini",
            "anthropic" => "claude-opus-5",
            "gemini" => "gemini-2.5-flash",
            _ => {
                return Err(Error::tool(
                    NAME,
                    "vision provider must be openai, anthropic or gemini",
                ));
            }
        };
        let env_model = std::env::var("PI_VISION_MODEL").ok();
        let model = transport::model_id(
            NAME,
            transport::optional(&args, NAME, "model")?
                .or(self.default_model.as_deref())
                .or(env_model.as_deref())
                .unwrap_or(fallback_model),
        )?;
        let api = self
            .transport
            .api(NAME, provider, self.api_key.as_deref(), timeout)
            .await?;
        if matches!(mime, "image/svg+xml" | "image/bmp") {
            return Err(Error::tool(
                NAME,
                "convert SVG/BMP to PNG, JPEG, WebP or GIF before vision inspection",
            ));
        }
        let bytes = transport::read_capped(
            &path,
            &self.cwd,
            &self.workspace,
            NAME,
            MAX_IMAGE_FILE_SIZE_BYTES,
        )?;
        if transport::image_mime(&bytes) != Some(mime) {
            return Err(Error::tool(
                NAME,
                "image bytes are truncated, unsupported, or do not match the file extension",
            ));
        }
        let size = bytes.len();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let (endpoint, payload) = match provider {
            "openai" => (
                "chat/completions".to_string(),
                json!({
                    "model": model, "stream": false, "max_completion_tokens": 4096,
                    "messages": [{"role": "user", "content": [
                        {"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{encoded}"), "detail": detail}},
                        {"type": "text", "text": prompt}
                    ]}]
                }),
            ),
            "anthropic" => (
                "messages".to_string(),
                json!({
                    "model": model, "max_tokens": 4096,
                    "messages": [{"role": "user", "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": mime, "data": encoded}},
                        {"type": "text", "text": prompt}
                    ]}]
                }),
            ),
            _ => (
                transport::gemini_path(NAME, model)?,
                json!({
                    "contents": [{"role": "user", "parts": [
                        {"inlineData": {"mimeType": mime, "data": encoded}}, {"text": prompt}
                    ]}],
                    "generationConfig": {"maxOutputTokens": 4096}
                }),
            ),
        };
        let response = api
            .post_vision(model, &endpoint, &payload, 1024 * 1024)
            .await?;
        let analysis = parse_analysis(provider, &response)?;
        let analysis = api.scrub(&analysis, MAX_ANALYSIS_BYTES);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(analysis))],
            details: Some(
                json!({"path": path.display().to_string(), "mime_type": mime, "size_bytes": size,
                "provider": provider, "model": model, "mock": false}),
            ),
            is_error: false,
        })
    }
}

/// Read only provider-defined billing fields, independently of whether its
/// answer was accepted. Images share the text-token price in these vision
/// endpoints; audio output and image generation have different billing units
/// and must never be inferred from this parser.
///
/// OpenAI prompt counts include cache hits. Anthropic input/cache buckets are
/// disjoint. Gemini prompt counts include cache hits and its thought tokens
/// are additional billed output, matching the streaming provider adapters.
pub(super) fn response_usage(
    provider: &str,
    response: &Value,
) -> Result<Option<crate::model::Usage>> {
    use crate::model::{BillingTokenCounts, BillingUsage, Usage};

    let key = if provider == "gemini" {
        "usageMetadata"
    } else {
        "usage"
    };
    let Some(usage) = response.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let invalid = || Error::session("native vision response has invalid billing evidence");
    if !usage.is_object() {
        return Err(invalid());
    }
    // OpenAI-compatible gateways may return the authoritative billed USD
    // amount accepted by the ordinary chat provider. Zero is a real bill too.
    if provider == "openai"
        && let Some(total) = usage.get("cost").filter(|value| !value.is_null())
    {
        let total = total
            .as_f64()
            .filter(|total| total.is_finite() && *total >= 0.0)
            .ok_or_else(invalid)?;
        return Ok(Some(Usage {
            billing: Some(BillingUsage::ReportedTotal { total }),
            ..Usage::default()
        }));
    }
    let count = |key: &str, optional: bool| -> Result<u64> {
        match usage.pointer(key) {
            Some(value) if !value.is_null() => value.as_u64().ok_or_else(invalid),
            _ if optional => Ok(0),
            _ => Err(invalid()),
        }
    };
    let tokens = match provider {
        "openai" => {
            if count("/prompt_tokens_details/audio_tokens", true)? > 0
                || count("/completion_tokens_details/audio_tokens", true)? > 0
            {
                return Err(Error::session(
                    "native vision response contains unsupported audio billing units",
                ));
            }
            let cached = count("/prompt_tokens_details/cached_tokens", true)?;
            // As in the ordinary chat adapter, cache writes remain part of
            // uncached prompt input; OpenAI's catalog has no separate write fee.
            let input = count("/prompt_tokens", false)?
                .checked_sub(cached)
                .ok_or_else(invalid)?;
            BillingTokenCounts {
                input,
                output: count("/completion_tokens", false)?,
                cache_read: cached,
                cache_write: 0,
            }
        }
        "anthropic" => BillingTokenCounts {
            input: count("/input_tokens", false)?,
            output: count("/output_tokens", false)?,
            cache_read: count("/cache_read_input_tokens", true)?,
            cache_write: count("/cache_creation_input_tokens", true)?,
        },
        "gemini" => {
            let cached = count("/cachedContentTokenCount", true)?;
            BillingTokenCounts {
                input: count("/promptTokenCount", false)?
                    .checked_sub(cached)
                    .ok_or_else(invalid)?,
                output: count("/candidatesTokenCount", true)?
                    .checked_add(count("/thoughtsTokenCount", true)?)
                    .ok_or_else(invalid)?,
                cache_read: cached,
                cache_write: 0,
            }
        }
        _ => return Err(Error::tool(NAME, "unsupported vision billing provider")),
    };
    let total_tokens = tokens
        .input
        .checked_add(tokens.output)
        .and_then(|total| total.checked_add(tokens.cache_read))
        .and_then(|total| total.checked_add(tokens.cache_write))
        .ok_or_else(invalid)?;
    Ok(Some(Usage {
        input: tokens.input,
        output: tokens.output,
        cache_read: tokens.cache_read,
        cache_write: tokens.cache_write,
        total_tokens,
        billing: Some(BillingUsage::Tokens {
            text: tokens,
            audio: BillingTokenCounts::default(),
            reported_total: None,
        }),
        ..Usage::default()
    }))
}

fn parse_analysis(provider: &str, response: &Value) -> Result<String> {
    let incomplete = || {
        Error::tool(
            NAME,
            "vision response was refused, incomplete, or missing its completion marker",
        )
    };
    let text = match provider {
        "openai" => {
            let choice = response.pointer("/choices/0").ok_or_else(incomplete)?;
            if choice["finish_reason"] != "stop"
                || choice
                    .pointer("/message/refusal")
                    .is_some_and(|value| !value.is_null())
            {
                return Err(incomplete());
            }
            let content = &choice["message"]["content"];
            content
                .as_str()
                .map_or_else(|| text_blocks(content, true), str::to_string)
        }
        "anthropic" => {
            if !matches!(
                response["stop_reason"].as_str(),
                Some("end_turn" | "stop_sequence")
            ) {
                return Err(incomplete());
            }
            text_blocks(&response["content"], true)
        }
        "gemini" => {
            if response
                .pointer("/promptFeedback/blockReason")
                .is_some_and(|value| !value.is_null())
            {
                return Err(incomplete());
            }
            let candidate = response.pointer("/candidates/0").ok_or_else(incomplete)?;
            if candidate["finishReason"] != "STOP" {
                return Err(incomplete());
            }
            text_blocks(&candidate["content"]["parts"], false)
        }
        _ => return Err(Error::tool(NAME, "unsupported vision response")),
    };
    if text.trim().is_empty() || text.len() > MAX_ANALYSIS_BYTES {
        return Err(Error::tool(
            NAME,
            "vision provider returned no analysis or exceeded the 64 KiB analysis limit",
        ));
    }
    Ok(text)
}

fn text_blocks(content: &Value, typed: bool) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| (!typed || block["type"] == "text") && block["thought"] != true)
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Exact integer/quarter-dollar fixtures verify settled billing.
mod tests {
    use super::super::transport::tests::peer;
    use super::*;
    use crate::cost::CostBudget;
    use crate::models::{ModelEntry, ModelRegistry};
    use crate::provider::{InputType, Model, ModelCost};
    use crate::tools::ToolRegistry;
    use std::sync::Arc;

    fn fixture_catalog(provider: &str, input_rate: f64) -> ModelRegistry {
        ModelRegistry::from_entries_for_tests(vec![ModelEntry {
            model: Model {
                id: "fixture-vision".to_string(),
                name: "Fixture vision".to_string(),
                api: match provider {
                    "anthropic" => "anthropic-messages",
                    "gemini" => "google-generative-ai",
                    _ => "openai-completions",
                }
                .to_string(),
                provider: provider.to_string(),
                base_url: "https://fixture.invalid/v1".to_string(),
                reasoning: false,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: input_rate,
                    output: 4.0,
                    cache_read: 1.0,
                    cache_write: 3.0,
                    audio: None,
                },
                context_window: 128_000,
                max_tokens: 4096,
                headers: Default::default(),
            },
            api_key: None,
            headers: Default::default(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }])
    }

    fn budgeted_session(
        tool: InspectImageTool,
        budget: &CostBudget,
        catalog: ModelRegistry,
    ) -> (crate::agent::AgentSession, Arc<ToolRegistry>) {
        let agent = crate::agent::Agent::new(
            Arc::new(crate::providers::openai::OpenAIProvider::new(
                "primary-fixture",
            )),
            ToolRegistry::from_tools(vec![Box::new(tool)]),
            crate::agent::AgentConfig {
                cost_budget: Some(budget.clone()),
                ..Default::default()
            },
        );
        // Retain the registry before the session publishes its catalog, as
        // background jobs and tool handles do in the real application.
        let retained = agent.shared_tools().snapshot();
        let session = crate::agent::AgentSession::new(
            agent,
            Arc::new(asupersync::sync::Mutex::new(
                crate::session::Session::in_memory(),
            )),
            false,
            crate::compaction::ResolvedCompactionSettings::default(),
        )
        .with_model_registry(catalog);
        (session, retained)
    }

    #[test]
    fn native_vision_charges_session_catalog_rates_and_blocks_the_next_request() {
        for (provider, response, expected) in [
            (
                "openai",
                json!({"choices":[{"finish_reason":"stop","message":{"content":"A diagram."}}],
                    "usage":{"prompt_tokens":500_000,"completion_tokens":250_000,
                        "prompt_tokens_details":{"cached_tokens":250_000,"cache_write_tokens":125_000}}}),
                1.75,
            ),
            (
                "anthropic",
                json!({"stop_reason":"end_turn","content":[{"type":"text","text":"A diagram."}],
                    "usage":{"input_tokens":250_000,"output_tokens":250_000,
                        "cache_read_input_tokens":250_000,"cache_creation_input_tokens":250_000}}),
                2.5,
            ),
            (
                "gemini",
                json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"A diagram."}]}}],
                    "usageMetadata":{"promptTokenCount":500_000,"candidatesTokenCount":125_000,
                        "thoughtsTokenCount":125_000,"cachedContentTokenCount":250_000}}),
                1.75,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("input.png"),
                super::super::MIN_VALID_PNG,
            )
            .unwrap();
            let (endpoint, worker) = peer(
                200,
                "application/json",
                serde_json::to_vec(&response).unwrap(),
            );
            let tool = InspectImageTool::with_defaults(
                directory.path(),
                Some(provider.into()),
                Some("fixture-vision".into()),
            )
            .with_mock(false)
            .with_api_key(Some("vision-cost-key".into()))
            .with_base_url(endpoint);
            let budget = CostBudget::new(None, Some(1.0), 0.0).unwrap();
            let (_session, registry) =
                budgeted_session(tool, &budget, fixture_catalog(provider, 2.0));
            let tool = registry.get(NAME).unwrap();
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let output = runtime
                .block_on(tool.execute("charged", json!({"path":"input.png"}), None))
                .unwrap();
            assert!(!output.is_error);
            assert_eq!(budget.spent(), expected, "{provider}");
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(matches!(
                runtime.block_on(tool.execute("capped", json!({"path":"input.png"}), None)),
                Err(Error::CostCap { .. })
            ));
            // Vision has no primary assistant usage record; its auxiliary
            // charge must survive repeated checkpoints without duplication.
            let mut saved = crate::session::Session::in_memory();
            budget.checkpoint(&mut saved).unwrap();
            budget.checkpoint(&mut saved).unwrap();
            assert_eq!(crate::cost::recorded_spend(&saved).unwrap(), expected);
            worker.join().unwrap();
        }
    }

    #[test]
    fn refused_invalid_and_http_error_vision_answers_keep_their_reported_bill() {
        for (status, mime, response) in [
            (
                200,
                "application/json",
                json!({"choices":[{"finish_reason":"stop",
                "message":{"refusal":"declined","content":null}}],"usage":{"cost":1.25}}),
            ),
            (
                200,
                "application/json",
                json!({"choices":[],"usage":{"cost":1.25}}),
            ),
            (
                200,
                "text/plain",
                json!({"choices":[{"finish_reason":"stop",
                "message":{"content":"Wrong content type."}}],"usage":{"cost":1.25}}),
            ),
            (
                429,
                "application/json",
                json!({"error":{"message":"quota exceeded"},"usage":{"cost":1.25}}),
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("input.png"),
                super::super::MIN_VALID_PNG,
            )
            .unwrap();
            let (endpoint, worker) = peer(status, mime, serde_json::to_vec(&response).unwrap());
            let tool = InspectImageTool::with_defaults(
                directory.path(),
                Some("openai".into()),
                Some("fixture-vision".into()),
            )
            .with_mock(false)
            .with_api_key(Some("vision-cost-key".into()))
            .with_base_url(endpoint);
            let budget = CostBudget::new(None, Some(1.0), 0.0).unwrap();
            let (_session, registry) =
                budgeted_session(tool, &budget, fixture_catalog("openai", 2.0));
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let tool = registry.get(NAME).unwrap();
            assert!(
                runtime
                    .block_on(tool.execute("rejected-answer", json!({"path":"input.png"}), None))
                    .is_err()
            );
            assert_eq!(budget.spent(), 1.25);
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(matches!(
                runtime.block_on(tool.execute("no-replay", json!({"path":"input.png"}), None)),
                Err(Error::CostCap { .. })
            ));
            worker.join().unwrap();
        }
    }

    #[test]
    fn explicit_vision_bills_include_zero_and_unknown_catalog_models_stay_unpriced() {
        for known_model in [false, true] {
            for reported in [Some(0.0), Some(0.75), None] {
                let directory = tempfile::tempdir().unwrap();
                std::fs::write(
                    directory.path().join("input.png"),
                    super::super::MIN_VALID_PNG,
                )
                .unwrap();
                let mut response = json!({"choices":[{"finish_reason":"stop","message":{"content":"A diagram."}}],
                    "usage":{"prompt_tokens":1_000_000,"completion_tokens":0}});
                if let Some(total) = reported {
                    response["usage"]["cost"] = json!(total);
                }
                let (endpoint, worker) = peer(
                    200,
                    "application/json",
                    serde_json::to_vec(&response).unwrap(),
                );
                let tool = InspectImageTool::with_defaults(
                    directory.path(),
                    Some("openai".into()),
                    Some("fixture-vision".into()),
                )
                .with_mock(false)
                .with_api_key(Some("vision-cost-key".into()))
                .with_base_url(endpoint);
                let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
                let catalog = if known_model {
                    fixture_catalog("openai", 2.0)
                } else {
                    ModelRegistry::from_entries_for_tests(Vec::new())
                };
                let (_session, registry) = budgeted_session(tool, &budget, catalog);
                let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                    .build()
                    .unwrap();
                runtime
                    .block_on(registry.get(NAME).unwrap().execute(
                        "reported",
                        json!({"path":"input.png"}),
                        None,
                    ))
                    .unwrap();
                let expected = reported.unwrap_or(if known_model { 2.0 } else { 0.0 });
                assert_eq!(budget.spent(), expected);
                assert!(budget.admit().is_ok());
                worker.join().unwrap();
            }
        }
    }

    #[test]
    fn malformed_vision_billing_quarantines_the_account_without_a_second_request() {
        for usage in [
            json!({"cost":-1.0}),
            json!({"total_tokens":10}),
            json!({"prompt_tokens":1,"completion_tokens":0,"prompt_tokens_details":{"cached_tokens":2}}),
            json!({"prompt_tokens":1,"completion_tokens":-1}),
        ] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("input.png"),
                super::super::MIN_VALID_PNG,
            )
            .unwrap();
            let response = json!({"choices":[{"finish_reason":"stop","message":{"content":"A diagram."}}],"usage":usage});
            let (endpoint, worker) = peer(
                200,
                "application/json",
                serde_json::to_vec(&response).unwrap(),
            );
            let tool = InspectImageTool::with_defaults(
                directory.path(),
                Some("openai".into()),
                Some("fixture-vision".into()),
            )
            .with_mock(false)
            .with_api_key(Some("vision-cost-key".into()))
            .with_base_url(endpoint);
            let budget = CostBudget::new(None, Some(10.0), 0.25).unwrap();
            let (_session, registry) =
                budgeted_session(tool, &budget, fixture_catalog("openai", 2.0));
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let tool = registry.get(NAME).unwrap();
            let error = runtime
                .block_on(tool.execute("malformed", json!({"path":"input.png"}), None))
                .unwrap_err();
            assert!(
                matches!(error, Error::Session(ref message) if message.contains("invalid billing evidence"))
            );
            assert_eq!(budget.spent(), 0.25);
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(matches!(
                runtime.block_on(tool.execute("quarantined", json!({"path":"input.png"}), None)),
                Err(Error::Session(message)) if message.contains("model cost accounting unavailable")
            ));
            worker.join().unwrap();
        }
    }

    #[test]
    fn prepared_vision_keeps_original_prices_and_durably_charges_the_retired_session() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let budget = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        let replacement = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        let scope = crate::cost::CostScope::default();
        scope.set(Some(budget.clone()));
        let auth = super::super::MediaAuthScope::default();
        let owner = Arc::new(());
        auth.set_models(&owner, Some(&fixture_catalog("openai", 2.0)));
        let response = json!({"choices":[{"finish_reason":"stop","message":{"content":"A diagram."}}],
            "usage":{"prompt_tokens":1_000_000,"completion_tokens":0}});
        let (endpoint, worker) = peer(
            200,
            "application/json",
            serde_json::to_vec(&response).unwrap(),
        );
        let mut transport = Transport::default().with_base_url(endpoint);
        transport.bind_auth_scope(auth.clone());
        transport.bind_cost_scope(scope.clone());
        let mut saved =
            crate::session::Session::create_with_dir(Some(directory.path().to_path_buf()));
        budget.checkpoint(&mut saved).unwrap();
        runtime.block_on(saved.save()).unwrap();
        let path = saved.path.clone().unwrap();
        let transcript = std::fs::read(&path).unwrap();
        runtime.block_on(async {
            let api = transport
                .api(
                    NAME,
                    "openai",
                    Some("vision-cost-key"),
                    std::time::Duration::from_secs(2),
                )
                .await
                .unwrap();
            scope.set(Some(replacement.clone()));
            auth.set_models(&owner, Some(&fixture_catalog("openai", 20.0)));
            budget.retire_session(&saved, api.owner.cx()).unwrap();
            api.post_vision(
                "fixture-vision",
                "chat/completions",
                &json!({"model":"fixture-vision"}),
                4096,
            )
            .await
            .unwrap();
        });
        worker.join().unwrap();
        assert_eq!(budget.spent(), 2.0);
        assert_eq!(replacement.spent(), 0.0);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            transcript,
            "late billing never rewrites the source transcript"
        );
        let reopened = runtime
            .block_on(crate::session::Session::open(path.to_str().unwrap()))
            .unwrap();
        assert_eq!(crate::cost::recorded_spend(&reopened).unwrap(), 2.0);
        assert_eq!(crate::cost::recorded_spend(&reopened).unwrap(), 2.0);
    }

    #[test]
    fn all_three_native_vision_envelopes_upload_the_image_and_return_the_peer_answer() {
        let cases = [
            (
                "openai",
                "chat/completions",
                json!({"choices":[{"finish_reason":"stop","message":{"content":"A red diagram."}}]}),
            ),
            (
                "anthropic",
                "messages",
                json!({"stop_reason":"end_turn","content":[{"type":"text","text":"A red diagram."}]}),
            ),
            (
                "gemini",
                "models/fixture-vision:generateContent",
                json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"hidden reasoning","thought":true},{"text":"A red diagram."}]}}]}),
            ),
        ];
        for (provider, path, response) in cases {
            let (endpoint, worker) = peer(
                200,
                "application/json",
                serde_json::to_vec(&response).unwrap(),
            );
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("input.png"), super::super::MIN_VALID_PNG).unwrap();
            let tool = InspectImageTool::with_defaults(
                dir.path(),
                Some(provider.into()),
                Some("fixture-vision".into()),
            )
            .with_mock(false)
            .with_api_key(Some("test-vision-secret".into()))
            .with_base_url(endpoint);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let output = runtime
                .block_on(tool.execute(
                    "vision",
                    json!({"path":"input.png","prompt":"Read the diagram"}),
                    None,
                ))
                .unwrap();
            assert!(
                matches!(&output.content[0], ContentBlock::Text(text) if text.text == "A red diagram.")
            );
            assert_eq!(output.details.as_ref().unwrap()["provider"], provider);
            let request = worker.join().unwrap();
            assert!(request.headers.starts_with(&format!("POST /v1/{path} ")));
            let image = match provider {
                "openai" => request
                    .body
                    .pointer("/messages/0/content/0/image_url/url")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .strip_prefix("data:image/png;base64,")
                    .unwrap(),
                "anthropic" => request
                    .body
                    .pointer("/messages/0/content/0/source/data")
                    .unwrap()
                    .as_str()
                    .unwrap(),
                _ => request
                    .body
                    .pointer("/contents/0/parts/0/inlineData/data")
                    .unwrap()
                    .as_str()
                    .unwrap(),
            };
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(image)
                    .unwrap(),
                super::super::MIN_VALID_PNG
            );
            let headers = request.headers.to_ascii_lowercase();
            let auth_header = match provider {
                "anthropic" => "x-api-key: test-vision-secret",
                "gemini" => "x-goog-api-key: test-vision-secret",
                _ => "authorization: bearer test-vision-secret",
            };
            assert!(headers.contains(auth_header));
            assert!(
                !request
                    .headers
                    .lines()
                    .next()
                    .unwrap()
                    .contains("test-vision-secret")
            );
        }
    }

    #[test]
    fn refusal_partial_empty_and_missing_terminal_responses_are_errors() {
        for response in [
            json!({}),
            json!({"choices":[]}),
            json!({"choices":[{"finish_reason":"length","message":{"content":"partial"}}]}),
            json!({"choices":[{"finish_reason":"stop","message":{"content":"","refusal":"declined"}}]}),
        ] {
            assert!(parse_analysis("openai", &response).is_err());
        }
        assert!(
            parse_analysis(
                "anthropic",
                &json!({"stop_reason":"max_tokens","content":[{"type":"text","text":"partial"}]})
            )
            .is_err()
        );
        assert!(
            parse_analysis(
                "gemini",
                &json!({"promptFeedback":{"blockReason":"SAFETY"}})
            )
            .is_err()
        );
    }

    #[test]
    fn authentication_failure_is_not_analysis_and_does_not_echo_the_key() {
        let key = "test-secret-never-show-this";
        let (endpoint, worker) = peer(
            401,
            "application/json",
            serde_json::to_vec(&json!({"error":{"message":format!("invalid token {key}")}}))
                .unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("input.png"), super::super::MIN_VALID_PNG).unwrap();
        let tool = InspectImageTool::with_defaults(dir.path(), Some("openai".into()), None)
            .with_mock(false)
            .with_api_key(Some(key.into()))
            .with_base_url(endpoint);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let error = runtime
            .block_on(tool.execute("vision-error", json!({"path":"input.png"}), None))
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 401"));
        assert!(!error.contains(key));
        worker.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_working_directory_does_not_panic_while_serializing_details() {
        use std::os::unix::ffi::OsStringExt as _;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir
            .path()
            .join(std::ffi::OsString::from_vec(vec![b'i', 0xff]));
        // APFS rejects a name that is not valid UTF-8 outright (EILSEQ), so the
        // panic this test guards against is unreachable there: there is no such
        // working directory to serialize. Skipping is the honest result on those
        // filesystems; panicking would report a filesystem policy as a pi bug.
        match std::fs::create_dir(&cwd) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::ILSEQ.raw_os_error()) => {
                eprintln!("SKIP non-UTF-8 working directory {cwd:?}: filesystem returned {error}");
                return;
            }
            // Any other errno is a real failure, not a filesystem policy.
            // ubs:ignore-next-line test fixture — the arm EILSEQ does not take
            Err(error) => panic!("create non-UTF-8 working directory {cwd:?}: {error}"),
        }
        std::fs::write(cwd.join("input.png"), super::super::MIN_VALID_PNG).unwrap();
        let tool = InspectImageTool::new(&cwd).with_mock(true);
        let output = futures::executor::block_on(tool.execute(
            "non-utf8",
            json!({"path":"input.png"}),
            None,
        ))
        .unwrap();
        assert!(
            output.details.as_ref().unwrap()["path"]
                .as_str()
                .unwrap()
                .ends_with("input.png")
        );
    }
}
