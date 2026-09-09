# Media Tools: `inspect_image`, `generate_image`, `tts`, `read_media`

> **Tool family:** Media Trio (Opt-in)  
> **Bead ID:** `bd-cv653.2.7`  
> **Module:** `src/media_tools.rs`

---

## 1. Overview

The media tool trio provides multimodal perception, generation, and speech synthesis:
- `inspect_image`: Analyzes local image files using vision models to answer questions, describe contents, and identify visual elements.
- `generate_image`: Synthesizes images from text prompts (via Gemini, DALL-E, or compatible providers) and writes PNG/JPEG artifacts to disk.
- `tts`: Synthesizes spoken audio from text (via ElevenLabs, Grok Voice, OpenAI TTS, or system engines) saving MP3/WAV audio files.
- `read_media`: Attaches a local video/audio file to the conversation as an inline `media` content block so Gemini-family models can watch/listen to it (gh #212).

---

## 2. Configuration & Activation

Add the `media` section in `config.toml` or activate via `--tools inspect_image,generate_image,tts`:

```toml
[media]
enable_media = true
tts_provider = "elevenlabs"     # "openai" | "elevenlabs" | "system"
tts_voice = "alloy"
image_gen_provider = "gemini"   # "gemini" | "dall-e-3"
output_dir = ".pi/media"
```

---

## 3. Tool Parameters & Schema

### `inspect_image`
- `path` (string, required): Absolute or project-relative image file path.
- `query` (string, optional): Specific question or instruction about the image content.

### `generate_image`
- `prompt` (string, required): Detailed prompt describing the image to generate.
- `output_path` (string, optional): Target file path for the generated image.
- `aspect_ratio` (string, optional): `"1:1"` | `"16:9"` | `"9:16"` | `"4:3"`.

### `tts`
- `text` (string, required): Text content to synthesize to speech.
- `output_path` (string, optional): Destination audio file.
- `voice` (string, optional): Voice identifier.

### `read_media`
- `path` (string, required): Absolute or project-relative path to a video (`.mp4`, `.webm`, `.mov`) or audio (`.mp3`, `.wav`, `.m4a`, `.ogg`, `.flac`) file. The MIME type is derived from the extension; other extensions are rejected.

Enable with `media.enableReadMedia: true` (or `--tools read_media`). The tool returns a short text note plus a `{"type":"media","data":"<base64>","mimeType":"video/mp4","name":"clip.mp4"}` content block.

**Size cap.** One file may be at most `media.maxBytes` bytes (default 5 MiB = 5,242,880). Larger files are rejected before they are read, with an error that names the cap. The block is stored base64-inline in the session JSONL and re-sent on every turn until compaction, which is why the cap is deliberately small; blob sidecar storage and the Gemini Files API for larger inputs are tracked as a follow-up.

**Provider support.**

| Transport | Behavior |
|---|---|
| `google-generative-ai`, `google-gemini-cli`, `google-vertex` (Gemini models) | Sent natively as an `inline_data` part (`{"mimeType": …, "data": …}`). A media block inside a tool result rides on a trailing user turn after the `functionResponse`. |
| Everything else (Anthropic, OpenAI Chat/Responses, Azure, Bedrock, Cohere, Copilot, Cursor, GitLab, OpenAI-compatible gateways) | Replaced by the text placeholder `[media omitted: <name>, <mime>, <size>]`; the payload never leaves the machine. |

Bundled Gemini models on those three transports declare `input: ["text", "image", "video", "audio"]`; custom `models.json` entries can declare `"video"`/`"audio"` themselves.

---

## 4. Privacy & Approval Policy

- `inspect_image` operates with read-only effects.
- `generate_image` and `tts` declare write effects and create localized media files in the designated artifacts directory.
