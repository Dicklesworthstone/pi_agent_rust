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

`enableMedia` (alias `enable_media`) enables inspection, generation, and speech
by default; individual flags override it, including an explicit `false`.
`read_media` remains independent. See [settings precedence](../settings.md#media-multimodal-perception--speech).

Explicit CLI selection can also enable the desired tools, for example:

```sh
pi --tools read,write,edit,bash,inspect_image,generate_image,tts,read_media
```

Native media requests use the selected provider's API key. Tools inside an
AgentSession use that session's auth store, including API-key entries saved in
its `auth.json`. Provider aliases and mixed-case stored provider names follow
the normal auth resolver. Media adapters resolve credentials only when the
selected provider is invoked; binding media tools does not execute stored key
sources. Login/logout changes apply to subsequent invocations through the
existing tool instances.

The precedence is an explicit tool-builder key, a configured provider API-key
pool, a singular environment key, then a stored `type: "api_key"` credential.
Stored `$ENV:` and bounded `$CMD:` / `$COMMAND:` sources retain the session's
captured working directory. Command sources run under the invoking operation's
process capability. Their output is collected asynchronously, with separate
64 KiB stdout and 16 KiB stderr limits and a ten-second ceiling that also fits
inside the operation's remaining `timeout_ms`. Cancellation, timeout, output
overflow, or dropping the invocation stops and reaps the owned helper rather
than leaving it to finish in a detached task. Helper failures do not include
the command text or captured output. Stored OAuth, bearer, AWS, and service credentials
are not admitted as media API keys. A chat-session `--api-key` or SDK
`SessionOptions.api_key` is not forwarded to media endpoints; configure the
chosen media provider's credentials separately.

| Provider | Credential environment variables | Native media operations |
|---|---|---|
| `openai` | `OPENAI_API_KEY` | Inspection, generation/editing, speech |
| `anthropic` | `ANTHROPIC_API_KEY` | Inspection |
| `gemini` (`google` alias) | `GEMINI_API_KEY`, then `GOOGLE_API_KEY` | Inspection, generation/editing |
| `xai` (`x-ai` alias) | `XAI_API_KEY` | Generation/editing, speech |

SDK callers can supply `with_api_key(Some(key))`; an explicit empty key fails
instead of falling back to saved or environment keys. Standalone tools that
are not attached to an AgentSession retain explicit-key/environment lookup.
A session's configured plural pool (for example `OPENAI_API_KEYS`) takes
precedence over the singular variables above. When all keys are cooling, the
call reports `[CREDENTIAL_POOL_EXHAUSTED]` without falling through to a saved
key. A media HTTP 429 records cooldown for the next invocation; the adapter
never automatically retries a potentially billable request.

Custom SDK factories must create a fresh tool registry for each AgentSession.
Shallow registry copies share native tool instances; assigning that scope to
two sessions refuses implicit media authentication instead of exchanging their
saved credentials. A custom wrapper around a native adapter should forward
`Tool::bind_media_auth_scope` and `Tool::bind_cost_scope` to the wrapped tool.

`with_client` retains the caller's
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

### Spend admission

Native inspection, generation, and speech requests consult the session's shared
cost budget before credential helpers run and again immediately before HTTP
dispatch. Requests are refused when known spend reaches `--max-cost` /
`session.maxCost`, or when the account is quarantined because its spend cannot
be established. An explicit media API key does not bypass this check. Each
invocation retains the account it started with; a retained tool uses the
current account on its next invocation.

Inspection, supported image generation, and supported OpenAI speech also
increase the shared balance. Inspection uses the selected model's catalog
token prices or an explicit provider bill. Generation and editing use their
own billing units:

| Provider/model | Image charge used by the budget |
|---|---|
| OpenAI `gpt-image-1.5` and `gpt-image-1.5-2025-12-16` | Provider token counts partitioned into text/image input and output, using the published image-model prices |
| Gemini `gemini-3.1-flash-image` | Prompt tokens plus separate text, thinking and image output counts, using standard image-model prices |
| xAI image models | The actual billed total in `usage.cost_in_usd_ticks` |
| OpenAI-compatible image gateways | An explicit `usage.cost` USD total, including zero, overrides token estimates |

Image-model prices are a snapshot of the [OpenAI model pricing](https://developers.openai.com/api/docs/models/gpt-image-1.5)
and [Gemini standard pricing](https://ai.google.dev/gemini-api/docs/pricing/)
checked on November 10, 2026. xAI's [cost-tracking field](https://docs.x.ai/developers/cost-tracking)
reports the actual charge after discounts. Image tokens never use the ordinary
text-model price, and output size or pixel count is not used to invent a bill.

OpenAI speech requests for `gpt-4o-mini-tts`, `gpt-4o-mini-tts-2025-03-20`,
and `gpt-4o-mini-tts-2025-12-15` use SSE to receive both audio and a terminal
usage receipt. The receipt's input text tokens cost $0.60 per million and its
output audio tokens cost $12 per million, using the [speech-model prices](https://developers.openai.com/api/docs/models/gpt-4o-mini-tts)
checked on November 10, 2026. An explicit `usage.cost` USD total, including zero,
takes precedence. Audio duration and character counts are not used to estimate
a charge.

Received charges are retained even when the provider refuses an image, returns
invalid image or audio bytes, or the output file cannot be published. A request
that finishes after a session switch still charges its original session.
Malformed explicit bills, duplicate billing fields, or inconsistent token
partitions block further spending on that account.

An unknown image model, missing billing evidence, or an unsupported Gemini
cache, tool or service-tier pricing dimension remains unpriced. Legacy and
custom OpenAI speech models and xAI speech keep their binary audio responses
and do not add a charge without usable billing evidence. An unfamiliar speech
usage dimension also remains unpriced. Those requests still check the shared
cap before dispatch, but their unknown costs are excluded from the total.
Cancellation before billing arrives can also leave the final cost unknown.
Already-admitted work can finish above the cap; no admitted media request is
automatically replayed.

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

The supported `gpt-4o-mini-tts` models above request `stream_format: "sse"`.
Audio chunks are decoded in order, and the terminal `speech.audio.done` usage
receipt is charged before audio validation or file publication. Truncated streams,
invalid chunks, and missing terminal receipts fail without publishing partial
audio. A gateway that does not support the requested SSE protocol must use an
appropriate custom or legacy model; the tool does not replay a paid request
to fall back to binary audio.

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
At most two video reads run concurrently; further reads wait for a slot. Each
read has one 60-second budget covering that wait, helper checks, metadata,
and all frame extractions. Cancellation owns and
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

## ACP audio and video prompts

ACP editors can attach audio directly in `session/prompt`. Pi advertises the
ACP v1 `promptCapabilities.audio` capability and accepts audio-only prompts:

```json
{
  "sessionId": "<active session>",
  "prompt": [
    {"type": "text", "text": "Summarize this recording and compare the clip."},
    {"type": "audio", "mimeType": "audio/wav", "data": "<base64 audio>"},
    {"type": "resource", "resource": {
      "uri": "memory:editor-clip.mp4",
      "mimeType": "video/mp4",
      "blob": "<base64 video>"
    }}
  ]
}
```

ACP v1 carries video as an embedded binary resource. Audio resource blobs work
too. The supplied bytes retain their order beside text and images; a resource
also retains a labeled URI reference. The URI is never opened or fetched during
attachment admission. Metadata in `_meta` and annotations is not forwarded to
the model. MIME types must be bare audio/video labels such as `audio/ogg` or
`video/webm`; casing is normalized, while parameters and malformed labels are
rejected. Standard base64 may include or omit padding. Empty or malformed
payloads reject the whole prompt before a turn starts. These are MIME/base64
checks, not audio/video codec validation; the selected provider must support
the submitted format.

The decoded limit is `media.maxBytes`, defaulting to 5 MiB, with a hard 10 MiB
ceiling for each attachment. A prompt accepts at most 32 audio/video blocks and
1024 total ACP blocks. Text, URI/MIME metadata, and encoded binary payloads
share a 32 MiB prompt budget. `_session/steering` uses the same media admission
rules plus its existing smaller queue and serialized-request limits.

Gemini-family providers receive the native audio/video. Other transports keep
the explicit media-omission text; they do not receive the binary payload. The
shared session store retains the full attachment, including large attachment
sidecars, so ACP load/replay and a later model switch preserve the media. Live
audio tool outputs and embedded video outputs use the same ACP wire forms and
can be attached to a later prompt.

The wire forms follow the [official ACP v1 content schema](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v1/schema.json).

## Operational boundaries and validation

All live media operations declare network effects; generation and synthesis
also declare local writes, and editing declares reads. Enabling a tool means
its prompts, image references, or speech text can leave the machine and incur
provider charges. These adapters do not establish filesystem sandboxing or add
a new approval system.

`timeout_ms` starts before credential lookup and covers command-based key
preparation plus the complete network request, including response-body
consumption. Preparing a key does not grant a fresh network timeout. Local
input preprocessing after credential lookup consumes the same elapsed budget,
but blocking filesystem calls cannot be interrupted mid-call. Output
publication is checked for cancellation separately. Defaults are 120 seconds
for vision and speech, 180 seconds for images; the maximum is 300 seconds.
Cancellation stops the owned credential helper or further transport polling
and drops the request. Process cleanup uses synchronous OS kill/reap operations,
so this is not a hard real-time guarantee. Cancellation cannot reverse work or
charges already accepted by a provider. Requests are not automatically retried.

Artifact destinations are prepared before credential helpers or provider calls.
The same prepared target is retained until publication, including when an
automatic filename receives its extension from the provider's actual format.
On Unix, this preserves the pinned workspace descriptor across the request;
replacing the workspace before publication is rejected. Artifacts are staged
beside their destination, synced, and published with no-clobber persistence.
Existing files and symlinks at the destination are
rejected, including a destination created concurrently. A failed request does
not publish a success artifact. Image checks are basic container checks, not a
full decoder or decompression-bomb guard. No claim of decoded visual or acoustic
correctness is made merely because a container passes these checks.

`with_mock(true)` or `PI_MEDIA_MOCK=1` explicitly selects deterministic fixture
behavior, marked `mock: true`. `with_mock(false)` overrides the environment.
Speech fixtures support WAV only; they never stand in for a native response.

Regression cases under the media modules and SDK use real loopback TCP peers
to exercise the HTTP adapters, provider request layouts, saved credential
sources, cancellation, spend admission, malformed/filtered responses,
credential redaction, no-clobber writes, ordered image references, and masks.
These are protocol fixtures, not live-provider validation. Adding a regression
does not establish that it passed; the authoritative quality entry point is:

```sh
dsr quality --tool pi_agent_rust
```

Protocol references consulted: [OpenAI image edits](https://developers.openai.com/api/reference/resources/images/methods/edit),
[OpenAI speech](https://developers.openai.com/api/reference/resources/audio/subresources/speech/methods/create),
[Gemini generateContent](https://ai.google.dev/api/generate-content),
[xAI multi-image editing](https://docs.x.ai/developers/model-capabilities/images/multi-image-editing),
and [xAI speech](https://docs.x.ai/developers/model-capabilities/audio/text-to-speech).
