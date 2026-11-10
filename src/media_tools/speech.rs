//! Native speech synthesis. A successful call contains provider audio, not an empty WAV header.

use super::{MAX_TTS_TEXT_CHARS, artifact, transport};
use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use async_trait::async_trait;
use base64::Engine as _;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const NAME: &str = "tts";
const MAX_AUDIO_BYTES: usize = 50 * 1024 * 1024;

pub struct TtsTool {
    cwd: PathBuf,
    default_provider: Option<String>,
    default_voice: Option<String>,
    mock_mode: Option<bool>,
    api_key: Option<String>,
    transport: transport::Transport,
}

impl TtsTool {
    pub fn new(cwd: &Path) -> Self {
        Self::with_defaults(cwd, None, None)
    }

    pub fn with_voice(cwd: &Path, voice: Option<String>) -> Self {
        Self::with_defaults(cwd, None, voice)
    }

    pub fn with_defaults(cwd: &Path, provider: Option<String>, voice: Option<String>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            default_provider: provider,
            default_voice: voice,
            mock_mode: None,
            api_key: None,
            transport: transport::Transport::default(),
        }
    }

    #[must_use]
    pub fn with_provider(mut self, provider: Option<String>) -> Self {
        self.default_provider = provider;
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
impl Tool for TtsTool {
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
        "Text to Speech"
    }
    fn description(&self) -> &str {
        "Synthesize speech through OpenAI or xAI and save the actual audio to a new file. Audio is AI-generated; disclose this when sharing it. Requires the selected provider's API key."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object", "required": ["text"],
            "properties": {
                "text": {"type": "string", "maxLength": MAX_TTS_TEXT_CHARS},
                "provider": {"type": "string", "enum": ["openai", "xai"], "default": "openai"},
                "model": {"type": "string", "description": "OpenAI speech model (default gpt-4o-mini-tts); not used by xAI"},
                "voice": {"type": "string", "description": "Provider voice ID; defaults to alloy for OpenAI or eve for xAI"},
                "format": {"type": "string", "enum": ["wav", "mp3", "opus", "aac", "flac"], "default": "wav",
                    "description": "OpenAI supports all listed formats; xAI supports wav/mp3"},
                "speed": {"type": "number", "minimum": 0.25, "maximum": 4,
                    "description": "OpenAI: 0.25..4.0; xAI: 0.7..1.5"},
                "language": {"type": "string", "description": "xAI only: BCP-47 language code or auto (default)"},
                "instructions": {"type": "string", "description": "OpenAI gpt-4o-mini-tts only: style/delivery instructions"},
                "output_path": {"type": "string", "description": "New audio file with the requested format's extension; never overwritten"},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 300_000, "default": 120_000}
            }
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::write().union(ToolEffects::network())
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let text = transport::optional(&args, NAME, "text")?
            .ok_or_else(|| Error::tool(NAME, "missing required text parameter"))?;
        if text.trim().is_empty() {
            return Err(Error::tool(NAME, "text cannot be empty"));
        }
        let char_count = text.chars().count();
        if char_count > MAX_TTS_TEXT_CHARS {
            return Err(Error::tool(
                NAME,
                format!("text length {char_count} exceeds max allowed {MAX_TTS_TEXT_CHARS} chars"),
            ));
        }
        let env_provider = std::env::var("PI_TTS_PROVIDER").ok();
        let provider = transport::provider(
            NAME,
            transport::optional(&args, NAME, "provider")?
                .or(self.default_provider.as_deref())
                .or(env_provider.as_deref())
                .unwrap_or("openai"),
        )?;
        if !matches!(provider, "openai" | "xai") {
            return Err(Error::tool(NAME, "speech provider must be openai or xai"));
        }
        let is_mock = self
            .mock_mode
            .unwrap_or_else(|| std::env::var("PI_MEDIA_MOCK").unwrap_or_default() == "1");
        let env_voice = std::env::var("PI_TTS_VOICE").ok();
        let voice = transport::optional(&args, NAME, "voice")?
            .or(self.default_voice.as_deref())
            .or(env_voice.as_deref())
            .unwrap_or(if is_mock || provider == "xai" {
                "eve"
            } else {
                "alloy"
            });
        if voice.trim().is_empty() || voice.len() > 128 {
            return Err(Error::tool(
                NAME,
                "voice must be nonempty and at most 128 bytes",
            ));
        }
        let format = transport::optional(&args, NAME, "format")?.unwrap_or("wav");
        let requested = transport::optional(&args, NAME, "output_path")?;
        let duration = transport::timeout(&args, NAME, 120_000)?;
        let (endpoint, body) = request(provider, voice, text, format, &args)?;
        if !is_mock {
            self.transport.check_admission(NAME)?;
        }
        let artifact = artifact::preflight(&self.cwd, requested, "audio/speech", NAME)?;
        if let Some(path) = requested {
            let extension = Path::new(path)
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("");
            if !extension.eq_ignore_ascii_case(format) {
                return Err(Error::tool(
                    NAME,
                    "output_path extension must match the requested audio format",
                ));
            }
        }
        let api = if is_mock {
            None
        } else {
            Some(
                self.transport
                    .api(NAME, provider, self.api_key.as_deref(), duration)
                    .await?,
            )
        };
        let (bytes, mime) = if let Some(api) = api.as_ref() {
            let response = if body.get("stream_format").and_then(Value::as_str) == Some("sse") {
                let model = body["model"]
                    .as_str()
                    .ok_or_else(|| Error::tool(NAME, "speech request is missing its model"))?;
                api.post_speech(model, endpoint, &body, MAX_AUDIO_BYTES)
                    .await?
            } else {
                api.post(endpoint, &body, MAX_AUDIO_BYTES).await?
            };
            let mime = validate_audio(&response.bytes, format, &response.content_type)?;
            (response.bytes, mime)
        } else {
            if format != "wav" {
                return Err(Error::tool(
                    NAME,
                    "deterministic speech fixtures support WAV only; native synthesis supports the advertised provider formats",
                ));
            }
            (super::MIN_VALID_WAV.to_vec(), "audio/wav")
        };
        let path = artifact::publish(
            &artifact,
            format,
            &bytes,
            api.as_ref().map(|api| &api.owner),
            NAME,
        )?;
        let message = if is_mock {
            format!(
                "Successfully synthesized speech audio fixture to {}\nVoice: {voice} | Characters: {char_count} (mock; no provider request)",
                path.display()
            )
        } else {
            format!(
                "Synthesized AI-generated speech to {}\nProvider: {provider} | Voice: {voice} | Format: {format} | Bytes: {}\nDisclose that this audio is AI-generated when sharing it.",
                path.display(),
                bytes.len()
            )
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(message))],
            details: Some(json!({
                "saved_path": path.display().to_string(), "provider": provider,
                "model": body.get("model"), "voice": voice, "format": format,
                "mime_type": mime, "size_bytes": bytes.len(), "char_count": char_count,
                "mock": is_mock, "ai_generated": true
            })),
            is_error: false,
        })
    }
}

fn request(
    provider: &str,
    voice: &str,
    text: &str,
    format: &str,
    args: &Value,
) -> Result<(&'static str, Value)> {
    if !matches!(format, "wav" | "mp3" | "opus" | "aac" | "flac") {
        return Err(Error::tool(NAME, "unsupported audio format"));
    }
    let model = transport::optional(args, NAME, "model")?;
    let instructions = transport::optional(args, NAME, "instructions")?;
    let language = transport::optional(args, NAME, "language")?;
    let speed = match args.get("speed") {
        None => None,
        Some(value) => Some(
            value
                .as_f64()
                .filter(|speed| speed.is_finite())
                .ok_or_else(|| Error::tool(NAME, "speed must be a finite number"))?,
        ),
    };
    match provider {
        "openai" => {
            if language.is_some() {
                return Err(Error::tool(
                    NAME,
                    "language is an xAI option; OpenAI infers language from the input",
                ));
            }
            let model = transport::model_id(NAME, model.unwrap_or("gpt-4o-mini-tts"))?;
            let mut body =
                json!({"model": model, "input": text, "voice": voice, "response_format": format});
            if supports_speech_usage(model) {
                body["stream_format"] = json!("sse");
            }
            if let Some(instructions) = instructions {
                if !model.starts_with("gpt-4o-mini-tts") || instructions.len() > 4096 {
                    return Err(Error::tool(
                        NAME,
                        "instructions require gpt-4o-mini-tts and must be at most 4096 bytes",
                    ));
                }
                body["instructions"] = json!(instructions);
            }
            if let Some(speed) = speed {
                if !(0.25..=4.0).contains(&speed) {
                    return Err(Error::tool(NAME, "OpenAI speed must be in 0.25..=4.0"));
                }
                body["speed"] = json!(speed);
            }
            Ok(("audio/speech", body))
        }
        "xai" => {
            if !matches!(format, "wav" | "mp3") {
                return Err(Error::tool(
                    NAME,
                    "xAI synthesis supports wav or mp3; choose OpenAI for opus/aac/flac",
                ));
            }
            if model.is_some() || instructions.is_some() {
                return Err(Error::tool(
                    NAME,
                    "xAI /tts does not accept model or instructions; use inline speech tags in text",
                ));
            }
            let language = language.unwrap_or("auto");
            if language.is_empty()
                || language.len() > 64
                || !language
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
            {
                return Err(Error::tool(
                    NAME,
                    "language must be auto or a BCP-47 language code",
                ));
            }
            let mut body = json!({"text": text, "voice_id": voice, "language": language,
                "output_format": {"codec": format, "sample_rate": 24000}});
            if let Some(speed) = speed {
                if !(0.7..=1.5).contains(&speed) {
                    return Err(Error::tool(NAME, "xAI speed must be in 0.7..=1.5"));
                }
                body["speed"] = json!(speed);
            }
            Ok(("tts", body))
        }
        _ => Err(Error::tool(NAME, "unsupported speech provider")),
    }
}

fn supports_speech_usage(model: &str) -> bool {
    matches!(
        model,
        "gpt-4o-mini-tts" | "gpt-4o-mini-tts-2025-03-20" | "gpt-4o-mini-tts-2025-12-15"
    )
}

/// Speech API prices checked 2026-11-10. These are text-input and audio-output
/// tokens, not ordinary chat rates or estimates derived from audio duration.
/// https://developers.openai.com/api/docs/models/gpt-4o-mini-tts
pub(super) fn response_cost(model: &str, response: &Value) -> Result<Option<f64>> {
    let Some(usage) = response.get("usage") else {
        return Ok(None);
    };
    let invalid = || Error::tool(NAME, "invalid speech billing evidence");
    let usage = usage.as_object().ok_or_else(invalid)?;
    if let Some(total) = crate::provider::reported_cost_total(usage.get("cost"), None)
        .map_err(|_| invalid())?
    {
        return Ok(Some(total));
    }
    let count = |name: &str| usage.get(name).and_then(Value::as_u64).ok_or_else(invalid);
    let input = count("input_tokens")?;
    let output = count("output_tokens")?;
    if input.checked_add(output) != Some(count("total_tokens")?) {
        return Err(invalid());
    }
    // Future or gateway-specific billing dimensions need their own tariff.
    // Do not silently price cache/audio-input/service-tier evidence as text.
    if !supports_speech_usage(model)
        || usage.keys().any(|key| {
            !matches!(key.as_str(), "input_tokens" | "output_tokens" | "total_tokens")
        })
    {
        return Ok(None);
    }
    #[allow(clippy::cast_precision_loss)]
    let cost = (input as f64).mul_add(0.60, output as f64 * 12.0) / 1_000_000.0;
    Ok(Some(cost))
}

/// Decode ordered audio chunks under the transport's cancellation/deadline
/// race. A terminal receipt is settled before returning audio to the adapter.
/// Neither an incomplete stream nor a protocol error publishes partial audio.
pub(super) async fn read_stream<S, F>(stream: S, limit: usize, mut on_usage: F) -> Result<Vec<u8>>
where
    S: Stream<Item = std::io::Result<Vec<u8>>> + Unpin,
    F: FnMut(&[u8], &Value) -> Result<()>,
{
    #[derive(Deserialize)]
    struct EventType {
        #[serde(rename = "type")]
        kind: String,
    }
    #[derive(Deserialize)]
    struct Delta {
        audio: String,
    }

    let wire_limit = limit.saturating_mul(2).saturating_add(1024 * 1024);
    let mut received = 0usize;
    let bounded = stream.map(move |chunk| {
        let chunk = chunk?;
        received = received
            .checked_add(chunk.len())
            .filter(|size| *size <= wire_limit)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "speech stream exceeds byte limit",
                )
            })?;
        Ok(chunk)
    });
    let mut events = crate::sse::SseStream::new(bounded);
    let mut audio = Vec::new();
    while let Some(event) = events.next().await {
        let event =
            event.map_err(|_| Error::tool(NAME, "speech stream failed or exceeded its limit"))?;
        if event.event == "error" {
            return Err(Error::tool(
                NAME,
                "speech provider reported a streaming error",
            ));
        }
        let header: EventType = serde_json::from_str(&event.data)
            .map_err(|_| Error::tool(NAME, "speech provider returned an invalid stream event"))?;
        match header.kind.as_str() {
            "speech.audio.delta" => {
                let delta: Delta = serde_json::from_str(&event.data)
                    .map_err(|_| Error::tool(NAME, "speech stream has invalid audio data"))?;
                let remaining = limit.saturating_sub(audio.len());
                let encoded_limit = remaining.saturating_add(2) / 3 * 4;
                if delta.audio.len() > encoded_limit {
                    return Err(Error::tool(NAME, "speech audio exceeds its byte limit"));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(delta.audio)
                    .map_err(|_| Error::tool(NAME, "speech stream has invalid base64 audio"))?;
                if bytes.len() > remaining {
                    return Err(Error::tool(NAME, "speech audio exceeds its byte limit"));
                }
                audio.extend_from_slice(&bytes);
            }
            "speech.audio.done" => {
                let terminal: Value = serde_json::from_str(&event.data)
                    .map_err(|_| Error::tool(NAME, "speech stream has an invalid terminal event"))?;
                if terminal.get("usage").is_none() {
                    return Err(Error::tool(
                        NAME,
                        "speech stream ended without a usage receipt",
                    ));
                }
                on_usage(event.data.as_bytes(), &terminal)?;
                return Ok(audio);
            }
            "error" | "speech.audio.error" => {
                return Err(Error::tool(
                    NAME,
                    "speech provider reported a streaming error",
                ));
            }
            // New metadata events cannot complete the stream or add audio.
            _ => {}
        }
    }
    Err(Error::tool(
        NAME,
        "speech stream ended before speech.audio.done",
    ))
}

fn validate_audio(bytes: &[u8], format: &str, content_type: &str) -> Result<&'static str> {
    let (mime, aliases): (&str, &[&str]) = match format {
        "wav" => ("audio/wav", &["audio/wav", "audio/wave", "audio/x-wav"]),
        "mp3" => ("audio/mpeg", &["audio/mpeg", "audio/mp3"]),
        "opus" => ("audio/ogg", &["audio/ogg", "audio/opus", "application/ogg"]),
        "aac" => ("audio/aac", &["audio/aac", "audio/x-aac"]),
        "flac" => ("audio/flac", &["audio/flac", "audio/x-flac"]),
        _ => return Err(Error::tool(NAME, "unsupported audio format")),
    };
    if !content_type.is_empty()
        && content_type != "application/octet-stream"
        && !aliases.contains(&content_type)
    {
        return Err(Error::tool(
            NAME,
            "speech provider returned a content type that does not match the requested audio",
        ));
    }
    let valid = match format {
        "wav" => wav_has_samples(bytes),
        "mp3" => mp3_has_frame(bytes),
        "opus" => {
            bytes.len() > 48
                && bytes.starts_with(b"OggS")
                && bytes[..bytes.len().min(128)]
                    .windows(8)
                    .any(|part| part == b"OpusHead")
        }
        "aac" => aac_has_frame(bytes),
        "flac" => bytes.len() > 42 && bytes.starts_with(b"fLaC"),
        _ => false,
    };
    if !valid {
        return Err(Error::tool(
            NAME,
            "speech provider returned empty, truncated, or mismatched audio bytes",
        ));
    }
    Ok(mime)
}

fn wav_has_samples(bytes: &[u8]) -> bool {
    if bytes.len() < 44 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WAVE" {
        return false;
    }
    let declared = u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes"));
    // Streaming WAV writers may use the all-ones sentinel until the body ends.
    let end = if declared == u32::MAX {
        bytes.len()
    } else {
        let Some(end) = usize::try_from(declared)
            .ok()
            .and_then(|n| n.checked_add(8))
        else {
            return false;
        };
        if end > bytes.len() {
            return false;
        }
        end
    };
    let mut offset = 12usize;
    let mut block_align = 0usize;
    let mut samples = false;
    while offset.checked_add(8).is_some_and(|next| next <= end) {
        let id = &bytes[offset..offset + 4];
        let declared = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("four bytes"),
        );
        let start = offset + 8;
        let length = if declared == u32::MAX && id == b"data" {
            end - start
        } else {
            let Ok(length) = usize::try_from(declared) else {
                return false;
            };
            length
        };
        let Some(next) = start.checked_add(length).filter(|next| *next <= end) else {
            return false;
        };
        if id == b"fmt " {
            if length < 16 {
                return false;
            }
            let channels =
                u16::from_le_bytes(bytes[start + 2..start + 4].try_into().expect("two bytes"));
            let sample_rate =
                u32::from_le_bytes(bytes[start + 4..start + 8].try_into().expect("four bytes"));
            block_align = usize::from(u16::from_le_bytes(
                bytes[start + 12..start + 14].try_into().expect("two bytes"),
            ));
            if channels == 0 || sample_rate == 0 || block_align == 0 {
                return false;
            }
        } else if id == b"data" {
            if block_align == 0 || length == 0 || length % block_align != 0 {
                return false;
            }
            samples = true;
        }
        let Some(next) = next.checked_add(length % 2) else {
            return false;
        };
        offset = next;
    }
    samples && offset == end
}

fn mp3_has_frame(bytes: &[u8]) -> bool {
    let offset = if bytes.starts_with(b"ID3") {
        if bytes.len() < 10 || bytes[6..10].iter().any(|byte| byte & 0x80 != 0) {
            return false;
        }
        let length = bytes[6..10]
            .iter()
            .fold(0usize, |value, byte| (value << 7) | usize::from(*byte));
        let Some(end) = length.checked_add(10 + if bytes[5] & 0x10 != 0 { 10 } else { 0 }) else {
            return false;
        };
        end
    } else {
        0usize
    };
    let Some(audio) = bytes.get(offset..) else {
        return false;
    };
    if audio.len() <= 4 {
        return false;
    }
    audio[0] == 0xff
        && audio[1] & 0xe0 == 0xe0
        && audio[1] & 0x18 != 0x08
        && audio[1] & 0x06 != 0
        && !matches!(audio[2] >> 4, 0 | 15)
        && audio[2] & 0x0c != 0x0c
}

fn aac_has_frame(bytes: &[u8]) -> bool {
    if bytes.len() < 7 || bytes[0] != 0xff || bytes[1] & 0xf6 != 0xf0 {
        return false;
    }
    let length = (usize::from(bytes[3] & 3) << 11)
        | (usize::from(bytes[4]) << 3)
        | usize::from(bytes[5] >> 5);
    let header = if bytes[1] & 1 == 0 { 9 } else { 7 };
    length > header && length <= bytes.len()
}

#[cfg(test)]
mod tests {
    use super::super::transport::tests::peer;
    use super::*;

    fn wav() -> Vec<u8> {
        let mut bytes = super::super::MIN_VALID_WAV.to_vec();
        bytes[4..8].copy_from_slice(&40_u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&4_u32.to_le_bytes());
        bytes.extend_from_slice(&[0x34, 0x12, 0xcd, 0xab]);
        bytes
    }

    fn speech_sse(audio: &[u8], usage: &Value) -> Vec<u8> {
        let mut wire = String::from(": speech heartbeat\n\n");
        for chunk in audio.chunks(7) {
            let delta = json!({
                "type":"speech.audio.delta",
                "audio":base64::engine::general_purpose::STANDARD.encode(chunk)
            });
            wire.push_str(&format!("data: {delta}\n\n"));
        }
        let done = json!({"type":"speech.audio.done","usage":usage});
        wire.push_str(&format!("data: {done}\n\n"));
        wire.into_bytes()
    }

    fn usage() -> Value {
        json!({"input_tokens":10,"output_tokens":100,"total_tokens":110})
    }

    #[test]
    fn native_synthesis_uses_provider_specific_requests_and_retains_audio_samples() {
        for (provider, voice, endpoint_path) in
            [("openai", "alloy", "audio/speech"), ("xai", "eve", "tts")]
        {
            let audio = wav();
            let (mime, response) = if provider == "openai" {
                ("text/event-stream", speech_sse(&audio, &usage()))
            } else {
                ("audio/wav", audio.clone())
            };
            let (endpoint, worker) = peer(200, mime, response);
            let dir = tempfile::tempdir().unwrap();
            let tool = TtsTool::with_defaults(dir.path(), Some(provider.into()), None)
                .with_mock(false)
                .with_api_key(Some("speech-test-key".into()))
                .with_base_url(endpoint);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let output = runtime
                .block_on(tool.execute(
                    "speak",
                    json!({"text":"Hello, world.","format":"wav","output_path":"hello.wav"}),
                    None,
                ))
                .unwrap();
            assert_eq!(std::fs::read(dir.path().join("hello.wav")).unwrap(), audio);
            let details = output.details.unwrap();
            assert_eq!(details["voice"], voice);
            assert_eq!(details["provider"], provider);
            assert_eq!(details["mock"], false);
            assert_eq!(details["ai_generated"], true);
            let request = worker.join().unwrap();
            assert!(
                request
                    .headers
                    .starts_with(&format!("POST /v1/{endpoint_path} "))
            );
            assert!(
                request
                    .headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer speech-test-key")
            );
            if provider == "openai" {
                assert_eq!(request.body["input"], "Hello, world.");
                assert_eq!(request.body["model"], "gpt-4o-mini-tts");
                assert_eq!(request.body["response_format"], "wav");
                assert_eq!(request.body["stream_format"], "sse");
            } else {
                assert_eq!(request.body["text"], "Hello, world.");
                assert_eq!(request.body["voice_id"], "eve");
                assert_eq!(request.body["language"], "auto");
                assert_eq!(request.body["output_format"]["codec"], "wav");
                assert!(request.body.get("model").is_none());
            }
        }
    }

    #[test]
    fn speech_stream_preserves_bytes_at_transport_and_audio_chunk_boundaries() {
        let expected = wav();
        let wire = speech_sse(&expected, &usage());
        for chunk_size in [1, 2, 3, 17, wire.len()] {
            let chunks = wire
                .chunks(chunk_size)
                .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()))
                .collect::<Vec<_>>();
            let mut receipts = Vec::new();
            let actual = futures::executor::block_on(read_stream(
                futures::stream::iter(chunks),
                expected.len(),
                |_, event| {
                    receipts.push(event["usage"].clone());
                    Ok(())
                },
            ))
            .unwrap();
            assert_eq!(actual, expected);
            assert_eq!(receipts, [usage()]);
        }
    }

    #[test]
    fn incomplete_invalid_and_oversized_speech_never_acknowledges_completion() {
        let valid = speech_sse(&wav(), &usage());
        let invalid_base64 = b"data: {\"type\":\"speech.audio.delta\",\"audio\":\"!invalid!\"}\n\n";
        let incomplete = b"data: {\"type\":\"speech.audio.delta\",\"audio\":\"aGk=\"}\n\n";
        let failed = b"event: error\ndata: {\"message\":\"provider-secret\"}\n\n";
        for (wire, limit) in [
            (valid.as_slice(), wav().len() - 1),
            (invalid_base64.as_slice(), MAX_AUDIO_BYTES),
            (incomplete.as_slice(), MAX_AUDIO_BYTES),
            (failed.as_slice(), MAX_AUDIO_BYTES),
            (b": heartbeat\n\n".as_slice(), MAX_AUDIO_BYTES),
            (
                b"data: {\"type\":\"speech.audio.done\"}\n\n".as_slice(),
                MAX_AUDIO_BYTES,
            ),
        ] {
            let mut acknowledged = false;
            let result = futures::executor::block_on(read_stream(
                futures::stream::iter([Ok::<_, std::io::Error>(wire.to_vec())]),
                limit,
                |_, _| {
                    acknowledged = true;
                    Ok(())
                },
            ));
            assert!(result.is_err());
            assert!(!acknowledged);
            assert!(!result.unwrap_err().to_string().contains("provider-secret"));
        }
    }

    #[test]
    fn speech_charges_its_session_before_audio_validation_and_enforces_the_cap() {
        for valid_audio in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let audio = if valid_audio {
                wav()
            } else {
                b"invalid audio".to_vec()
            };
            let (endpoint, worker) = peer(200, "text/event-stream", speech_sse(&audio, &usage()));
            let budget = crate::cost::CostBudget::new(None, Some(0.001), 0.0).unwrap();
            let scope = crate::cost::CostScope::default();
            scope.set(Some(budget.clone()));
            let mut tool = TtsTool::new(directory.path())
                .with_mock(false)
                .with_api_key(Some("speech-test-key".into()))
                .with_base_url(endpoint);
            tool.bind_cost_scope(scope);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let result = runtime.block_on(tool.execute(
                "charged",
                json!({"text":"Hello","output_path":"out.wav"}),
                None,
            ));
            assert_eq!(result.is_ok(), valid_audio);
            assert_eq!(directory.path().join("out.wav").exists(), valid_audio);
            assert!((budget.spent() - 0.001_206).abs() < 1e-12);
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(matches!(
                runtime.block_on(tool.execute(
                    "refused",
                    json!({"text":"Do not send","output_path":"second.wav"}),
                    None,
                )),
                Err(Error::CostCap { .. })
            ));
            assert!(!directory.path().join("second.wav").exists());
            let captured = worker.join().unwrap();
            assert_eq!(captured.body["stream_format"], "sse");
        }
    }

    #[test]
    fn invalid_and_duplicate_speech_bills_quarantine_the_original_account() {
        for terminal in [
            r#"{"type":"speech.audio.done","usage":{"input_tokens":10,"output_tokens":100,"total_tokens":109}}"#,
            r#"{"type":"speech.audio.done","usage":{"input_tokens":-1,"output_tokens":100,"total_tokens":99}}"#,
            r#"{"type":"speech.audio.done","usage":null}"#,
            r#"{"type":"speech.audio.done","usage":{"cost":1.25,"cost":0}}"#,
            r#"{"type":"speech.audio.done","usage":{"cost":1.25},"usage":{"cost":0}}"#,
        ] {
            let (endpoint, worker) = peer(
                200,
                "text/event-stream",
                format!("data: {terminal}\n\n").into_bytes(),
            );
            let directory = tempfile::tempdir().unwrap();
            let budget = crate::cost::CostBudget::new(None, Some(10.0), 0.25).unwrap();
            let scope = crate::cost::CostScope::default();
            scope.set(Some(budget.clone()));
            let mut tool = TtsTool::new(directory.path())
                .with_mock(false)
                .with_api_key(Some("speech-test-key".into()))
                .with_base_url(endpoint);
            tool.bind_cost_scope(scope);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            assert!(
                runtime
                    .block_on(tool.execute(
                        "bad-bill",
                        json!({"text":"Hello","output_path":"out.wav"}),
                        None,
                    ))
                    .is_err()
            );
            assert_eq!(budget.spent(), 0.25);
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(matches!(budget.admit(), Err(Error::Session(_))));
            assert!(!directory.path().join("out.wav").exists());
            worker.join().unwrap();
        }
    }

    #[test]
    fn speech_receipt_stays_with_the_account_captured_before_a_session_switch() {
        let (endpoint, worker) = peer(200, "text/event-stream", speech_sse(&wav(), &usage()));
        let original = crate::cost::CostBudget::new(None, Some(10.0), 0.25).unwrap();
        let replacement = crate::cost::CostBudget::new(None, Some(10.0), 0.0).unwrap();
        let scope = crate::cost::CostScope::default();
        scope.set(Some(original.clone()));
        let mut transport = transport::Transport::default().with_base_url(endpoint);
        transport.bind_cost_scope(scope.clone());
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let api = transport
                .api(
                    NAME,
                    "openai",
                    Some("speech-test-key"),
                    std::time::Duration::from_secs(2),
                )
                .await
                .unwrap();
            scope.set(Some(replacement.clone()));
            let (_, body) = request("openai", "alloy", "Hello", "wav", &json!({})).unwrap();
            let response = api
                .post_speech("gpt-4o-mini-tts", "audio/speech", &body, MAX_AUDIO_BYTES)
                .await
                .unwrap();
            assert_eq!(response.bytes, wav());
        });
        assert!((original.spent() - 0.251_206).abs() < 1e-12);
        assert_eq!(replacement.spent(), 0.0);
        worker.join().unwrap();
    }

    #[test]
    fn speech_pricing_uses_known_tariffs_and_authoritative_reported_totals() {
        assert_eq!(
            response_cost("gpt-4o-mini-tts", &json!({"usage":{"cost":0}})).unwrap(),
            Some(0.0)
        );
        assert!(response_cost("gpt-4o-mini-tts", &json!({"usage":{"cost":-1}})).is_err());
        let response = json!({"usage":usage()});
        assert_eq!(response_cost("custom-speech", &response).unwrap(), None);
        let mut extended = response;
        extended["usage"]["cached_tokens"] = json!(1);
        assert_eq!(response_cost("gpt-4o-mini-tts", &extended).unwrap(), None);
        for model in [
            "tts-1",
            "tts-1-hd",
            "custom-speech",
            "gpt-4o-mini-tts-unpriced",
        ] {
            let (_, body) =
                request("openai", "alloy", "Hello", "wav", &json!({"model":model})).unwrap();
            assert!(body.get("stream_format").is_none());
        }
    }

    #[test]
    fn malformed_or_json_audio_never_creates_an_artifact() {
        for (content_type, bytes) in [
            (
                "application/json",
                br#"{"error":"synthesis unavailable"}"#.to_vec(),
            ),
            ("audio/wav", super::super::MIN_VALID_WAV.to_vec()),
            ("audio/wav", b"RIFF truncated".to_vec()),
        ] {
            let (endpoint, worker) = peer(200, content_type, bytes);
            let dir = tempfile::tempdir().unwrap();
            let tool = TtsTool::new(dir.path())
                .with_mock(false)
                .with_api_key(Some("speech-test-key".into()))
                .with_base_url(endpoint);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            assert!(
                runtime
                    .block_on(tool.execute(
                        "bad-speech",
                        json!({"text":"Hello","output_path":"out.wav"}),
                        None
                    ))
                    .is_err()
            );
            assert!(!dir.path().join("out.wav").exists());
            worker.join().unwrap();
        }
    }

    #[test]
    fn speech_options_do_not_cross_provider_boundaries() {
        assert!(request("xai", "eve", "hello", "opus", &json!({})).is_err());
        assert!(request("xai", "eve", "hello", "wav", &json!({"speed":2.0})).is_err());
        assert!(request("xai", "eve", "hello", "wav", &json!({"model":"tts-1"})).is_err());
        assert!(request("openai", "alloy", "hello", "wav", &json!({"language":"en"})).is_err());
        let (_, body) = request(
            "openai",
            "nova",
            "hello",
            "mp3",
            &json!({"instructions":"Speak calmly","speed":0.8}),
        )
        .unwrap();
        assert_eq!(body["instructions"], "Speak calmly");
        assert_eq!(body["speed"], 0.8);
    }

    #[test]
    fn wav_validation_accepts_streaming_sizes_and_rejects_truncated_chunks() {
        let mut audio = wav();
        assert!(wav_has_samples(&audio));
        audio[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        audio[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(wav_has_samples(&audio));
        assert!(!wav_has_samples(super::super::MIN_VALID_WAV));
        let mut truncated = wav();
        truncated.pop();
        assert!(!wav_has_samples(&truncated));
        assert!(validate_audio(&wav(), "mp3", "audio/wav").is_err());
    }
}
