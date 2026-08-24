# Media Tools: `inspect_image`, `generate_image`, `tts`

> **Tool family:** Media Trio (Opt-in)  
> **Bead ID:** `bd-cv653.2.7`  
> **Module:** `src/media_tools.rs`

---

## 1. Overview

The media tool trio provides multimodal perception, generation, and speech synthesis:
- `inspect_image`: Analyzes local image files using vision models to answer questions, describe contents, and identify visual elements.
- `generate_image`: Synthesizes images from text prompts (via Gemini, DALL-E, or compatible providers) and writes PNG/JPEG artifacts to disk.
- `tts`: Synthesizes spoken audio from text (via ElevenLabs, Grok Voice, OpenAI TTS, or system engines) saving MP3/WAV audio files.

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

---

## 4. Privacy & Approval Policy

- `inspect_image` operates with read-only effects.
- `generate_image` and `tts` declare write effects and create localized media files in the designated artifacts directory.
