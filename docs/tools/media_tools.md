# Native media tools: `inspect_image`, `generate_image`, `tts`, `read_media`

The media adapters now make provider requests rather than treating an API key's
presence as a successful operation. Image generation writes the received image
bytes, speech synthesis writes received audio, and inspection returns the
provider's answer about the uploaded image. There is no automatic fallback to
fixture images, empty WAV headers, or canned analysis.

Implementation: `src/media_tools.rs` and `src/media_tools/`.
Feature tracking: `bd-cv653.2.7`.

## Activation and credentials

Configure the existing opt-in flags in the application's settings:

```json
{
  "media": {
    "enableInspectImage": true,
    "enableGenerateImage": true,
    "enableTts": true,
    "enableReadMedia": true,
    "maxBytes": 5242880
  }
}
```

Include the desired tools in the selected tool list, for example:

```sh
pi --tools read,write,edit,bash,inspect_image,generate_image,tts,read_media
```

Native media requests use the selected provider's API key. They do not borrow
chat-session OAuth credentials or switch providers merely because another
provider has a key.

| Provider | Credential environment variables | Native media operations |
|---|---|---|
| `openai` | `OPENAI_API_KEY` | Inspection, generation/editing, speech |
| `anthropic` | `ANTHROPIC_API_KEY` | Inspection |
| `gemini` (`google` alias) | `GEMINI_API_KEY`, then `GOOGLE_API_KEY` | Inspection, generation/editing |
| `xai` (`x-ai` alias) | `XAI_API_KEY` | Generation/editing, speech |

SDK callers can supply `with_api_key(Some(key))`; an explicit empty key fails
instead of falling back to the environment. `with_client` retains the caller's
HTTP/VCR configuration. The endpoints can be overridden using `with_base_url`
or `PI_MEDIA_OPENAI_BASE_URL`, `PI_MEDIA_ANTHROPIC_BASE_URL`,
`PI_MEDIA_GEMINI_BASE_URL`, and `PI_MEDIA_XAI_BASE_URL`. An override receives the
credential, so use only a trusted gateway. HTTPS is required except for loopback
HTTP. URL credentials, query strings, fragments, and response redirects are
rejected; model-facing tool arguments cannot set the API base URL.

Tool arguments take precedence over constructor defaults, which take precedence
over the environment. Operation defaults can be selected through
`PI_VISION_PROVIDER` / `PI_VISION_MODEL`,
`PI_IMAGE_GEN_PROVIDER` / `PI_IMAGE_GEN_MODEL`, and
`PI_TTS_PROVIDER` / `PI_TTS_VOICE`.

The default registry forwards all provider/model/voice defaults from `media`:
`vision_provider`, `vision_model`, `image_gen_provider`,
`image_gen_model`, `tts_provider`, and `tts_voice`. Model-facing tool
arguments still take precedence, followed by these configured defaults and then
the environment variables documented above. Invalid configured selections fail
through the same adapter validation as explicit arguments instead of being
silently ignored.

## Image inspection

```json
{
  "path": "screenshots/failure.png",
  "prompt": "Read the error message and explain which UI component failed.",
  "provider": "openai",
  "model": "gpt-4.1-mini",
  "detail": "high"
}
```

Call `inspect_image` with these arguments. `path` is required; `prompt` is the
question/instruction field, not `query`. PNG, JPEG, WebP, and GIF are accepted up
to 20 MiB. Convert SVG/BMP first. The extension must match the basic container
signature. Provider limits can be lower than the local byte limit.

The native requests are OpenAI Chat Completions image content, Anthropic
Messages base64 image blocks, and Gemini `generateContent` inline image parts.
Default models are `gpt-4.1-mini`, `claude-opus-5`, and `gemini-2.5-flash`
respectively; the default provider is Gemini. `detail` applies to OpenAI only.
The complete answer must fit within 64 KiB. A refusal, token-limited response,
missing terminal marker, or empty answer is an error, not canned analysis.

## Image generation and editing

Generate with `generate_image`:

```json
{
  "provider": "openai",
  "prompt": "A clean isometric illustration of a Rust build pipeline",
  "size": "1536x1024",
  "output_path": "images/pipeline.png"
}
```

Generation defaults are OpenAI `gpt-image-1.5`, Gemini
`gemini-3.1-flash-image`, or xAI `grok-imagine-image-2.0`. The default provider
is OpenAI. Override `model` explicitly when needed; the provider remains the
authority on model availability and model-specific limits.

OpenAI uses `size` (`WIDTHxHEIGHT` or `auto`) and model-specific `quality`.
GPT image requests use `output_format: png`; legacy DALL-E generation requests
use `response_format: b64_json`. Gemini and xAI use `aspect_ratio` instead of
`size`. Gemini `resolution` accepts `512`, `1K`, `2K`, or `4K`; xAI accepts
`1k` or `2k`. Unsupported cross-provider options are rejected rather than
silently ignored. Gemini's REST `responseFormat.image` options are encoded as
protobuf enum names, and inline image delivery is requested explicitly.
The separate Imagen `predict` protocol is not implemented by this adapter.

### Edit one image or combine references

All three image providers support local references. Use `image_path` for one
input or `image_paths` for one to five ordered inputs, never both. The decoded
inputs and optional mask share a 20 MiB budget. OpenAI additionally limits each
encoded data URL to 20 MiB. Inputs are sent inline, not as fetchable source URLs.

```json
{
  "provider": "xai",
  "prompt": "Combine the subjects from these images in the same scene.",
  "image_paths": ["references/first.png", "references/second.jpg"],
  "aspect_ratio": "16:9"
}
```

Gemini inserts the references before the editing instructions in
`generateContent`. OpenAI and xAI send their distinct JSON request layouts to
`images/edits`. Editing with OpenAI requires a `gpt-image-*` model. By default,
OpenAI editing requests `size: auto`, while Gemini/xAI omit an explicit aspect
ratio to let the provider preserve the source shape.

OpenAI also accepts a PNG alpha mask for the first reference and an
`input_fidelity` of `low` or `high`:

```json
{
  "provider": "openai",
  "prompt": "Replace only the masked background with a plain studio backdrop.",
  "image_path": "references/product.png",
  "mask_path": "references/background-mask.png",
  "input_fidelity": "high",
  "output_path": "images/product-studio.png"
}
```

The mask's PNG container is checked locally; the provider validates its alpha
and dimension requirements. Masks and fidelity controls are rejected for other
providers. Source files are never modified by the editing adapter.

### Image results

One final image is accepted per call. Missing image data, invalid base64,
provider-filtered output, incomplete Gemini candidates, multiple final images,
and MIME/signature mismatches fail. Reasoning-only Gemini image parts are not
published. Hosted image URLs are not followed automatically.

The received PNG/JPEG/WebP/GIF bytes are saved without transcoding.
`output_path` must have a matching extension; omit it to choose a unique
`images/generated_<id>.<extension>` path automatically. Omitting the path is
particularly useful when Gemini or xAI chooses JPEG instead of PNG. Results
contain text plus the saved path, provider/model, format, byte count, and editing
reference count in `details`. Use `read` or `inspect_image` to inspect the saved
image; generation does not embed another copy in the result.

## Speech synthesis

Call `tts` with text and an explicit provider when not using OpenAI:

```json
{
  "provider": "openai",
  "text": "The build is complete. All requested artifacts are ready.",
  "voice": "alloy",
  "format": "wav",
  "instructions": "Speak calmly and clearly.",
  "output_path": "audio/build-complete.wav"
}
```

OpenAI uses `/audio/speech`, defaults to `gpt-4o-mini-tts` and `alloy`, and
accepts WAV, MP3, Opus, AAC, or FLAC. `speed` is 0.25 through 4.0. Delivery
`instructions` require the `gpt-4o-mini-tts` family. Model and voice IDs can be
overridden; the provider validates their availability.

```json
{
  "provider": "xai",
  "text": "The build is complete.",
  "voice": "eve",
  "language": "en",
  "speed": 1.0,
  "format": "mp3"
}
```

xAI uses `/tts` with `voice_id`, `language` (default `auto`), and the nested
`output_format` object. This adapter supports xAI WAV/MP3 at 24 kHz and speeds
from 0.7 through 1.5. It does not send OpenAI-only `model` or `instructions`
fields to xAI.

Text must be nonempty and at most 4,096 Unicode characters. The audio response
is bounded to 50 MiB. Error JSON, mismatched content types, and invalid basic
audio headers fail before publication. WAV validation additionally requires
nonempty, aligned sample data and complete chunks, including support for
streaming size sentinels. Other formats receive basic signature/header checks,
not a full codec decode. Files default to `audio/speech_<id>.<format>`.
Results identify the speech as AI-generated; disclose this when sharing it.
No ElevenLabs, local/system speech engine, or voice-cloning backend is claimed.

## Reading local video frames

The ordinary `read` tool can inspect a local video through any model that accepts
image input. A bare video path returns metadata and a contact sheet; a suffix
selects one frame or timestamp:

```json
{"path":"recordings/reproduction.mp4"}
```

```json
{"path":"recordings/reproduction.mp4:42"}
```

```json
{"path":"recordings/reproduction.mp4:1m2s"}
```

```json
{"path":"recordings/reproduction.mp4:00:01:02.5"}
```

Supported containers are MP4, MOV, M4V, MKV, WebM, AVI, and WMV. Embedded
cover art is excluded when selecting the video stream. The contact sheet
contains up to six 320 × 240 previews in chronological order; unknown-duration
videos return the first frame. A selected frame is fitted into a 1200 × 1200
image while preserving its aspect ratio.

A bare integer suffix is a **zero-based decoded frame index**. Timestamp
selectors accept explicit units, decimal seconds, or colon-separated time.
Frame selection does not estimate the index by multiplying a timestamp by an
average frame rate, so variable-frame-rate videos keep their frame ordering.
An existing literal filename containing a colon takes precedence over a suffix.

Install `ffmpeg` and `ffprobe` on an absolute directory in `PATH`; both must
support the `fd` protocol. Pi checks that capability and gives an explicit
dependency error if it is missing. No shell is involved. The decoder receives
the already-opened file admitted by the normal workspace checks, so replacing
a pathname does not redirect the read. Decoder protocol access is restricted
to the inherited descriptor and pipes.

Each read admits at most 256 MiB of source data, a 16-megapixel source frame,
and a 24-hour duration. Frame indices are capped at 10,000,000. Metadata and
stderr are each capped at 64 KiB, and the PNG output is capped at 4.5 MiB.
At most two video reads run concurrently, with one 60-second budget covering
helper checks, metadata, and all frame extractions. Cancellation owns and
reaps the helper processes. Decoder allocation limits are per allocation,
not a total process-memory cap. Length and modification-time checks reject
detected in-place source changes; the descriptor is not an immutable snapshot.

Visual video reads honor the image-blocking setting and reject `hashline`,
`offset`, and `limit`: a rendered frame has no source-line edit anchors.
`raw: true` keeps the ordinary source-text behavior. Video results use normal
image content blocks, so the existing provider conversion and session
attachment storage handle them.

This path extracts still images. Use `read_media` below to attach the original
video or audio to a Gemini-family model.

## Local video/audio attachments

`read_media` takes `path` to a video (`mp4`, `webm`, `mov`) or audio (`mp3`,
`wav`, `m4a`, `ogg`, `flac`) file. MIME type is derived from the extension.
The result contains a short text note and an inline `media` content block:

```json
{"type":"media","data":"<base64>","mimeType":"video/mp4","name":"clip.mp4"}
```

The default cap is 5 MiB (`media.maxBytes` overrides it). It is checked both
before opening and while reading, so concurrent file growth cannot cause an
unbounded whole-file allocation. Empty inputs are rejected. In memory the block
contains the original base64 bytes. Session storage moves large image/media
payloads into integrity-checked attachment blobs and restores them for replay.
The Gemini developer transport can stage large request media through its Files
API; remote file URIs are transport state, not a replacement for the durable
local attachment.

Gemini-family transports serialize these media parts natively. Other provider
transports retain the existing text-placeholder behavior instead of sending the
inline media payload. This attachment path is distinct from `inspect_image`,
which explicitly sends its input to the separately selected vision provider.

## Operational boundaries and validation

All live media operations declare network effects; generation and synthesis
also declare local writes, and editing declares reads. Enabling a tool means
its prompts, image references, or speech text can leave the machine and incur
provider charges. These adapters do not establish filesystem sandboxing or add
a new approval system.

`timeout_ms` bounds the complete network request, including response-body
consumption, not local file preprocessing. Defaults are 120 seconds for vision
and speech, 180 seconds for images; the maximum is 300 seconds. Cancellation
stops further transport polling and drops the request. It cannot reverse work
or charges already accepted by a provider. Requests are not automatically
retried.

Artifacts are staged beside their destination, synced, and published with
no-clobber persistence. Existing files and symlinks at the destination are
rejected, including a destination created concurrently. A failed request does
not publish a success artifact. Image checks are basic container checks, not a
full decoder or decompression-bomb guard. No claim of decoded visual or acoustic
correctness is made merely because a container passes these checks.

`with_mock(true)` or `PI_MEDIA_MOCK=1` explicitly selects deterministic fixture
behavior, marked `mock: true`. `with_mock(false)` overrides the environment.
Speech fixtures support WAV only; they never stand in for a native response.

Twenty regression test functions were added under the media modules, including
real loopback TCP peers exercising the actual HTTP adapter and provider request
layouts, malformed/filtered responses, credential redaction, no-clobber writes,
ordered image references, masks, and one Unix-only path-serialization case.
These are protocol fixtures, not live-provider validation.

**Validation status of the implementation session:** no Rust compiler or DSR
runner was available. `dsr quality --tool pi_agent_rust` failed with
`dsr: command not found`. The Rust tests were authored but not executed, and
neither a passing build nor live provider success was established. The
required authoritative check remains:

```sh
dsr quality --tool pi_agent_rust
```

Protocol references consulted: [OpenAI image edits](https://developers.openai.com/api/reference/resources/images/methods/edit),
[OpenAI speech](https://developers.openai.com/api/reference/resources/audio/subresources/speech/methods/create),
[Gemini generateContent](https://ai.google.dev/api/generate-content),
[xAI multi-image editing](https://docs.x.ai/developers/model-capabilities/images/multi-image-editing),
and [xAI speech](https://docs.x.ai/developers/model-capabilities/audio/text-to-speech).
