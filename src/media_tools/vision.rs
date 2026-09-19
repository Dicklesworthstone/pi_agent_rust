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
            default_provider: provider,
            default_model: model,
            mock_mode: None,
            api_key: None,
            transport: Transport::default(),
        }
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
        let path = self.cwd.join(path_str);
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
        let ext = path
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
            .api(NAME, provider, self.api_key.as_deref(), timeout)?;
        if matches!(mime, "image/svg+xml" | "image/bmp") {
            return Err(Error::tool(
                NAME,
                "convert SVG/BMP to PNG, JPEG, WebP or GIF before vision inspection",
            ));
        }
        let bytes = transport::read_capped(&path, NAME, MAX_IMAGE_FILE_SIZE_BYTES)?;
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
            .post(&endpoint, &payload, 1024 * 1024)
            .await?
            .json(NAME)?;
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
mod tests {
    use super::super::transport::tests::peer;
    use super::*;

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
