//! Native image generation and editing. Live artifacts contain provider bytes.

use super::{artifact, transport};
use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

mod inputs;

const NAME: &str = "generate_image";
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const COMMON_RATIOS: &[&str] = &["1:1", "16:9", "9:16", "4:3", "3:4", "3:2", "2:3", "21:9"];

pub struct GenerateImageTool {
    cwd: PathBuf,
    default_provider: Option<String>,
    default_model: Option<String>,
    mock_mode: Option<bool>,
    api_key: Option<String>,
    transport: transport::Transport,
}

impl GenerateImageTool {
    pub fn new(cwd: &Path) -> Self {
        Self::with_defaults(cwd, None, None)
    }

    pub fn with_provider(cwd: &Path, provider: Option<String>) -> Self {
        Self::with_defaults(cwd, provider, None)
    }

    pub fn with_defaults(cwd: &Path, provider: Option<String>, model: Option<String>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            default_provider: provider,
            default_model: model,
            mock_mode: None,
            api_key: None,
            transport: transport::Transport::default(),
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

    #[must_use]
    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }

    /// Explicitly trusted gateway or loopback protocol-test endpoint.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.transport = self.transport.with_base_url(base_url.into());
        self
    }

    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.transport = self.transport.with_client(client);
        self
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for GenerateImageTool {
    fn name(&self) -> &str {
        NAME
    }
    fn label(&self) -> &str {
        "Generate Image"
    }
    fn description(&self) -> &str {
        "Generate or edit an image through OpenAI, Gemini or xAI and save the actual provider bytes. Supply image_path or ordered image_paths for editing; OpenAI GPT image models also support mask_path. Existing files are never overwritten."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object", "required": ["prompt"],
            "properties": {
                "prompt": {"type": "string", "description": "Image description or editing instructions"},
                "provider": {"type": "string", "enum": ["openai", "gemini", "xai"]},
                "model": {"type": "string", "description": "Provider image model ID"},
                "size": {"type": "string", "description": "OpenAI only: WIDTHxHEIGHT or auto (default 1024x1024 for generation, auto for editing). Other providers use aspect_ratio and resolution."},
                "aspect_ratio": {"type": "string", "enum": COMMON_RATIOS, "description": "Gemini/xAI only; default 1:1 for generation, omitted for editing to preserve input shape"},
                "resolution": {"type": "string", "description": "Gemini: 512, 1K, 2K, 4K; xAI: 1k, 2k"},
                "quality": {"type": "string", "description": "OpenAI model-specific quality or xAI auto/low/medium"},
                "image_path": {"type": "string", "description": "One local image to edit. Mutually exclusive with image_paths."},
                "image_paths": {"type": "array", "minItems": 1, "maxItems": 5, "items": {"type": "string"}, "description": "Ordered local reference images to edit/combine. Inputs and mask share a 20 MiB decoded-byte budget."},
                "mask_path": {"type": "string", "description": "OpenAI editing only: local PNG alpha mask for the first input image. Provider validates mask dimensions and alpha semantics."},
                "input_fidelity": {"type": "string", "enum": ["low", "high"], "description": "OpenAI editing only: fidelity to the source images"},
                "output_path": {"type": "string", "description": "New destination file. Omit to choose an extension matching the received image."},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 300_000, "default": 180_000}
            }
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
            .union(ToolEffects::write())
            .union(ToolEffects::network())
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let prompt = transport::required(&args, NAME, "prompt")?;
        let requested = transport::optional(&args, NAME, "output_path")?;
        let env_provider = std::env::var("PI_IMAGE_GEN_PROVIDER").ok();
        let provider = transport::provider(
            NAME,
            transport::optional(&args, NAME, "provider")?
                .or(self.default_provider.as_deref())
                .or(env_provider.as_deref())
                .unwrap_or("openai"),
        )?;
        let fallback_model = match provider {
            "openai" => "gpt-image-1.5",
            "gemini" => "gemini-3.1-flash-image",
            "xai" => "grok-imagine-image-2.0",
            _ => {
                return Err(Error::tool(
                    NAME,
                    "image generation provider must be openai, gemini or xai",
                ));
            }
        };
        let env_model = std::env::var("PI_IMAGE_GEN_MODEL").ok();
        let model = transport::model_id(
            NAME,
            transport::optional(&args, NAME, "model")?
                .or(self.default_model.as_deref())
                .or(env_model.as_deref())
                .unwrap_or(fallback_model),
        )?;
        let duration = transport::timeout(&args, NAME, 180_000)?;
        let (mut endpoint, mut payload) = request(provider, model, prompt, &args)?;
        let is_mock = self
            .mock_mode
            .unwrap_or_else(|| std::env::var("PI_MEDIA_MOCK").unwrap_or_default() == "1");
        let api = if is_mock {
            None
        } else {
            Some(
                self.transport
                    .api(NAME, provider, self.api_key.as_deref(), duration)?,
            )
        };
        artifact::preflight(&self.cwd, requested, NAME)?;
        let reference_count = inputs::attach(
            &self.cwd,
            provider,
            model,
            &args,
            &mut endpoint,
            &mut payload,
        )?;
        let (bytes, mime) = match api.as_ref() {
            None => (super::MIN_VALID_PNG.to_vec(), "image/png"),
            Some(api) => {
                let response = api
                    .post(&endpoint, &payload, MAX_RESPONSE_BYTES)
                    .await?
                    .json(NAME)?;
                parse_image(provider, &response)?
            }
        };
        let extension = match mime {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => {
                return Err(Error::tool(
                    NAME,
                    "provider returned an unsupported image format",
                ));
            }
        };
        let path = artifact::publish(
            &self.cwd,
            requested,
            "images/generated",
            extension,
            &bytes,
            api.as_ref().map(|api| &api.owner),
            NAME,
        )?;
        let message = if is_mock {
            let size_str = payload
                .get("size")
                .and_then(Value::as_str)
                .map(|s| format!("\nSize: {s}"))
                .unwrap_or_default();
            format!(
                "Successfully generated image for '{prompt}' and saved to {} (mock; no provider request){size_str}",
                path.display()
            )
        } else {
            format!(
                "Generated image and saved to {}\nProvider: {provider} | Model: {model} | Format: {mime} | Bytes: {}",
                path.display(),
                bytes.len()
            )
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(message))],
            details: Some(json!({
                "saved_path": path.display().to_string(), "provider": provider, "model": model,
                "mime_type": mime, "size_bytes": bytes.len(), "mock": is_mock,
                "size": payload.get("size"),
                "aspect_ratio": transport::optional(&args, NAME, "aspect_ratio")?,
                "edited": reference_count > 0,
                "reference_count": reference_count
            })),
            is_error: false,
        })
    }
}

// Keep provider-specific validation beside the corresponding image request payload.
#[allow(clippy::too_many_lines)]
fn request(provider: &str, model: &str, prompt: &str, args: &Value) -> Result<(String, Value)> {
    let size = transport::optional(args, NAME, "size")?;
    let ratio = transport::optional(args, NAME, "aspect_ratio")?;
    let resolution = transport::optional(args, NAME, "resolution")?;
    let quality = transport::optional(args, NAME, "quality")?;
    if ratio.is_some_and(|ratio| !COMMON_RATIOS.contains(&ratio)) {
        return Err(Error::tool(
            NAME,
            "unsupported aspect_ratio; use a ratio from the tool schema",
        ));
    }
    match provider {
        "openai" => {
            if ratio.is_some() || resolution.is_some() {
                return Err(Error::tool(
                    NAME,
                    "OpenAI uses size, not aspect_ratio or resolution",
                ));
            }
            let size = size.unwrap_or("1024x1024");
            validate_size(size)?;
            let mut body = json!({"model": model, "prompt": prompt, "n": 1, "size": size});
            if model.starts_with("gpt-image-") {
                body["output_format"] = json!("png");
                if let Some(quality) = quality {
                    if !matches!(quality, "auto" | "low" | "medium" | "high") {
                        return Err(Error::tool(
                            NAME,
                            "GPT image quality must be auto, low, medium or high",
                        ));
                    }
                    body["quality"] = json!(quality);
                }
            } else if matches!(model, "dall-e-2" | "dall-e-3") {
                body["response_format"] = json!("b64_json");
                if let Some(quality) = quality {
                    if quality != "standard" && !(model == "dall-e-3" && quality == "hd") {
                        return Err(Error::tool(
                            NAME,
                            "DALL-E quality must be standard (or hd for dall-e-3)",
                        ));
                    }
                    body["quality"] = json!(quality);
                }
            } else {
                return Err(Error::tool(
                    NAME,
                    "OpenAI image model must be gpt-image-* or dall-e-2/dall-e-3",
                ));
            }
            Ok(("images/generations".into(), body))
        }
        "gemini" => {
            if size.is_some() || quality.is_some() {
                return Err(Error::tool(
                    NAME,
                    "Gemini uses aspect_ratio and resolution, not size or quality",
                ));
            }
            if model.starts_with("imagen-") {
                return Err(Error::tool(
                    NAME,
                    "use a Gemini image model with generateContent; this adapter does not support Imagen predict",
                ));
            }
            // REST responseFormat.image uses protobuf enum names, unlike the
            // older imageConfig string fields and the SDK's convenience values.
            let aspect_ratio = match ratio.unwrap_or("1:1") {
                "1:1" => "ASPECT_RATIO_ONE_BY_ONE",
                "16:9" => "ASPECT_RATIO_SIXTEEN_BY_NINE",
                "9:16" => "ASPECT_RATIO_NINE_BY_SIXTEEN",
                "4:3" => "ASPECT_RATIO_FOUR_BY_THREE",
                "3:4" => "ASPECT_RATIO_THREE_BY_FOUR",
                "3:2" => "ASPECT_RATIO_THREE_BY_TWO",
                "2:3" => "ASPECT_RATIO_TWO_BY_THREE",
                "21:9" => "ASPECT_RATIO_TWENTY_ONE_BY_NINE",
                _ => return Err(Error::tool(NAME, "unsupported Gemini aspect ratio")),
            };
            let mut image_config = json!({"aspectRatio": aspect_ratio, "delivery": "INLINE"});
            if let Some(resolution) = resolution {
                let image_size = match resolution {
                    "512" => "IMAGE_SIZE_FIVE_TWELVE",
                    "1K" => "IMAGE_SIZE_ONE_K",
                    "2K" => "IMAGE_SIZE_TWO_K",
                    "4K" => "IMAGE_SIZE_FOUR_K",
                    _ => {
                        return Err(Error::tool(
                            NAME,
                            "Gemini resolution must be 512, 1K, 2K or 4K",
                        ));
                    }
                };
                image_config["imageSize"] = json!(image_size);
            }
            Ok((
                transport::gemini_path(NAME, model)?,
                json!({
                    "contents": [{"role": "user", "parts": [{"text": prompt}]}],
                    "generationConfig": {
                        "responseModalities": ["TEXT", "IMAGE"],
                        "responseFormat": {"image": image_config}
                    }
                }),
            ))
        }
        "xai" => {
            if size.is_some() {
                return Err(Error::tool(
                    NAME,
                    "xAI uses aspect_ratio and resolution, not pixel size",
                ));
            }
            let mut body = json!({"model": model, "prompt": prompt, "n": 1,
                "response_format": "b64_json", "aspect_ratio": ratio.unwrap_or("1:1")});
            if let Some(resolution) = resolution {
                if !matches!(resolution, "1k" | "2k") {
                    return Err(Error::tool(NAME, "xAI resolution must be 1k or 2k"));
                }
                body["resolution"] = json!(resolution);
            }
            if let Some(quality) = quality {
                if !matches!(quality, "auto" | "low" | "medium") {
                    return Err(Error::tool(NAME, "xAI quality must be auto, low or medium"));
                }
                body["quality"] = json!(quality);
            }
            Ok(("images/generations".into(), body))
        }
        _ => Err(Error::tool(NAME, "unsupported image provider")),
    }
}

fn validate_size(size: &str) -> Result<()> {
    if size == "auto" {
        return Ok(());
    }
    if let Some((width, height)) = size.split_once('x')
        && let (Ok(width), Ok(height)) = (width.parse::<u32>(), height.parse::<u32>())
        && (64..=8192).contains(&width)
        && (64..=8192).contains(&height)
        && u64::from(width) * u64::from(height) <= 32 * 1024 * 1024
    {
        return Ok(());
    }
    Err(Error::tool(
        NAME,
        "size must be auto or WIDTHxHEIGHT (64..8192 per side, at most 32 megapixels); model-specific limits also apply",
    ))
}

fn parse_image(provider: &str, response: &Value) -> Result<(Vec<u8>, &'static str)> {
    let (encoded, declared_mime) = if provider == "gemini" {
        if response
            .pointer("/promptFeedback/blockReason")
            .is_some_and(|value| !value.is_null())
        {
            return Err(Error::tool(
                NAME,
                "image request was blocked by the provider",
            ));
        }
        let candidate = response
            .pointer("/candidates/0")
            .ok_or_else(|| Error::tool(NAME, "image provider returned no candidates"))?;
        if candidate["finishReason"] != "STOP" {
            return Err(Error::tool(
                NAME,
                "image response was refused, incomplete, or lacked a completion marker",
            ));
        }
        let images: Vec<_> = candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|part| part["thought"] != true)
            .filter_map(|part| part.get("inlineData"))
            .collect();
        if images.len() != 1 {
            return Err(Error::tool(
                NAME,
                "expected exactly one final image, not text-only or multiple image output",
            ));
        }
        (images[0]["data"].as_str(), images[0]["mimeType"].as_str())
    } else {
        let data = response["data"]
            .as_array()
            .filter(|data| data.len() == 1)
            .ok_or_else(|| Error::tool(NAME, "image provider did not return exactly one image"))?;
        if data[0]["respect_moderation"] == false || response["respect_moderation"] == false {
            return Err(Error::tool(
                NAME,
                "image response was filtered by provider moderation",
            ));
        }
        (data[0]["b64_json"].as_str(), None)
    };
    let encoded = encoded.ok_or_else(|| {
        Error::tool(
            NAME,
            "provider returned no inline image bytes; hosted URL downloads are not followed",
        )
    })?;
    if encoded.is_empty() || encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(Error::tool(
            NAME,
            "provider image is empty or exceeds 20 MiB",
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Error::tool(NAME, "provider image contains invalid base64"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(Error::tool(NAME, "provider image exceeds 20 MiB"));
    }
    let mime = transport::image_mime(&bytes)
        .ok_or_else(|| Error::tool(NAME, "provider bytes are not a supported image container"))?;
    if declared_mime.is_some_and(|declared| declared != mime) {
        return Err(Error::tool(
            NAME,
            "provider image MIME type does not match the received bytes",
        ));
    }
    Ok((bytes, mime))
}

#[cfg(test)]
mod tests {
    use super::super::transport::tests::peer;
    use super::*;

    const RED_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGP8z8DAwMDAxMDAwMDAAAANHQEDasKb6QAAAABJRU5ErkJggg==";

    #[test]
    fn generation_calls_each_native_api_and_publishes_the_received_pixels() {
        let cases = [
            (
                "openai",
                "gpt-image-1.5",
                json!({"data":[{"b64_json":RED_PNG}]}),
            ),
            (
                "xai",
                "grok-imagine-image-2.0",
                json!({"data":[{"b64_json":RED_PNG,"respect_moderation":true}]}),
            ),
            (
                "gemini",
                "gemini-3.1-flash-image",
                json!({"candidates":[{"finishReason":"STOP","content":{"parts":[
                    {"thought":true,"inlineData":{"mimeType":"image/png","data":"not-the-final-image"}},
                    {"inlineData":{"mimeType":"image/png","data":RED_PNG}}
                ]}}]}),
            ),
        ];
        for (provider, model, response) in cases {
            let (endpoint, worker) = peer(
                200,
                "application/json",
                serde_json::to_vec(&response).unwrap(),
            );
            let dir = tempfile::tempdir().unwrap();
            let tool = GenerateImageTool::with_defaults(
                dir.path(),
                Some(provider.into()),
                Some(model.into()),
            )
            .with_mock(false)
            .with_api_key(Some("generation-test-key".into()))
            .with_base_url(endpoint);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let output = runtime
                .block_on(tool.execute(
                    "generate",
                    json!({"prompt":"Draw a red square","output_path":"out.png"}),
                    None,
                ))
                .unwrap();
            assert_eq!(
                std::fs::read(dir.path().join("out.png")).unwrap(),
                base64::engine::general_purpose::STANDARD
                    .decode(RED_PNG)
                    .unwrap()
            );
            assert_eq!(output.details.as_ref().unwrap()["mock"], false);
            assert_eq!(output.details.as_ref().unwrap()["model"], model);
            let request = worker.join().unwrap();
            if provider == "gemini" {
                assert!(
                    request
                        .headers
                        .starts_with(&format!("POST /v1/models/{model}:generateContent "))
                );
                assert_eq!(
                    request.body.pointer("/contents/0/parts/0/text").unwrap(),
                    "Draw a red square"
                );
                assert_eq!(
                    request
                        .body
                        .pointer("/generationConfig/responseFormat/image/aspectRatio")
                        .unwrap(),
                    "ASPECT_RATIO_ONE_BY_ONE"
                );
                assert_eq!(
                    request
                        .body
                        .pointer("/generationConfig/responseFormat/image/delivery")
                        .unwrap(),
                    "INLINE"
                );
            } else {
                assert!(request.headers.starts_with("POST /v1/images/generations "));
                assert_eq!(request.body["prompt"], "Draw a red square");
                assert_eq!(request.body["model"], model);
                if provider == "openai" {
                    assert!(request.body.get("response_format").is_none());
                    assert_eq!(request.body["output_format"], "png");
                } else {
                    assert_eq!(request.body["response_format"], "b64_json");
                }
            }
        }
    }

    #[test]
    fn image_editing_sends_gemini_the_real_input_image() {
        let response = json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"inlineData":{"mimeType":"image/png","data":RED_PNG}}]}}]});
        let (endpoint, worker) = peer(
            200,
            "application/json",
            serde_json::to_vec(&response).unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source.png"), super::super::MIN_VALID_PNG).unwrap();
        let tool = GenerateImageTool::with_provider(dir.path(), Some("gemini".into()))
            .with_mock(false)
            .with_api_key(Some("editing-test-key".into()))
            .with_base_url(endpoint);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let output = runtime
            .block_on(tool.execute(
                "edit",
                json!({"prompt":"Make it red","image_path":"source.png"}),
                None,
            ))
            .unwrap();
        assert_eq!(output.details.as_ref().unwrap()["edited"], true);
        assert_eq!(output.details.as_ref().unwrap()["reference_count"], 1);
        let request = worker.join().unwrap();
        let sent = request
            .body
            .pointer("/contents/0/parts/0/inlineData/data")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(sent)
                .unwrap(),
            super::super::MIN_VALID_PNG
        );
        assert!(
            request
                .body
                .pointer("/generationConfig/responseFormat/image/aspectRatio")
                .is_none()
        );
        assert_eq!(
            std::fs::read(dir.path().join("source.png")).unwrap(),
            super::super::MIN_VALID_PNG
        );
    }

    #[test]
    fn image_failures_never_create_a_success_artifact() {
        for response in [
            json!({"data":[{"url":"http://127.0.0.1/private"}]}),
            json!({"data":[{"b64_json":"invalid"}]}),
            json!({"data":[{"b64_json":RED_PNG,"respect_moderation":false}]}),
            json!({"data":[]}),
        ] {
            let (endpoint, worker) = peer(
                200,
                "application/json",
                serde_json::to_vec(&response).unwrap(),
            );
            let dir = tempfile::tempdir().unwrap();
            let tool = GenerateImageTool::new(dir.path())
                .with_mock(false)
                .with_api_key(Some("generation-test-key".into()))
                .with_base_url(endpoint);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            assert!(
                runtime
                    .block_on(tool.execute(
                        "bad-image",
                        json!({"prompt":"test","output_path":"out.png"}),
                        None
                    ))
                    .is_err()
            );
            assert!(!dir.path().join("out.png").exists());
            worker.join().unwrap();
        }
    }

    #[test]
    fn provider_options_are_not_silently_ignored() {
        assert!(
            request(
                "xai",
                "grok-imagine-image-2.0",
                "test",
                &json!({"size":"512x512"})
            )
            .is_err()
        );
        assert!(
            request(
                "gemini",
                "gemini-3.1-flash-image",
                "test",
                &json!({"quality":"high"})
            )
            .is_err()
        );
        assert!(
            request(
                "openai",
                "gpt-image-1.5",
                "test",
                &json!({"size":"../../wrong"})
            )
            .is_err()
        );
        let (_, dalle) = request(
            "openai",
            "dall-e-3",
            "test",
            &json!({"size":"1792x1024","quality":"hd"}),
        )
        .unwrap();
        assert_eq!(dalle["response_format"], "b64_json");
        assert!(dalle.get("output_format").is_none());
        let (_, gemini) = request(
            "gemini",
            "gemini-3.1-flash-image",
            "test",
            &json!({"aspect_ratio":"16:9","resolution":"2K"}),
        )
        .unwrap();
        assert_eq!(
            gemini
                .pointer("/generationConfig/responseFormat/image/aspectRatio")
                .unwrap(),
            "ASPECT_RATIO_SIXTEEN_BY_NINE"
        );
        assert_eq!(
            gemini
                .pointer("/generationConfig/responseFormat/image/imageSize")
                .unwrap(),
            "IMAGE_SIZE_TWO_K"
        );
    }
}
