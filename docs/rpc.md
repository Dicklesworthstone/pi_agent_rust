# RPC Protocol

Pi supports a headless RPC mode for integration with IDEs and other tools.

## Usage

Start Pi in RPC mode:
```bash
pi --mode rpc
```

Communication is via **JSON Lines** over stdin/stdout. Each line must be a valid JSON object.

## Message Format

### Request
```json
{
  "id": "req-1",
  "type": "command_name",
  "param": "value"
}
```

### Response
```json
{
  "id": "req-1",
  "type": "response",
  "command": "command_name",
  "success": true,
  "data": { ... },
  "error": "Error message if success is false"
}
```

### Events (Server-Sent)
```json
{
  "type": "event_name",
  "data": "..."
}
```

## Commands

### Chat

- **prompt**: Send a user message.
  - Params: `message` (string), `images` (optional array), `media` (optional array), or an exclusive `content` array; `streamingBehavior` ("steer" or "follow-up").
- **steer**: Interrupt current generation and steer.
  - Params: `message`, `images` (optional array), `media` (optional array), or an exclusive `content` array.
- **follow_up**: Queue a message to follow current turn.
  - Params: `message`, `images` (optional array), `media` (optional array), or an exclusive `content` array.
- **retry**: Rerun the last durable user turn, preserving its text, images, and media.
- **abort**: Stop generation.

Audio and video attachments use native content blocks in `media`. For example,
replace the example payload with the standard base64 encoding of your file:

```json
{
  "id": "clip-1",
  "type": "prompt",
  "message": "Describe this clip",
  "media": [
    {"type": "media", "mimeType": "video/mp4", "data": "YQ==", "name": "clip.mp4"}
  ]
}
```

When using separate attachment fields, `message` is required and can be empty
for an attachment-only turn. Images accept the public SDK shape
`{"data":"YQ==","mimeType":"image/png"}` as well as the existing shape:
`{"type":"image","source":{"type":"base64","mediaType":"image/png","data":"..."}}`.
The user message contains nonempty text first, then `images` in array order,
then `media` in array order. Empty attachment arrays behave like text-only input.

Use `content` to preserve arbitrary interleaving of text, images, audio, and
video. It cannot be combined with `message`, `images`, or `media`, including
null or empty values for those fields:

```json
{
  "id": "compare-1",
  "type": "prompt",
  "content": [
    {"type": "text", "text": "Listen to this first."},
    {"type": "media", "data": "YQ==", "mimeType": "audio/wav", "name": "voice.wav"},
    {"type": "image", "data": "Yg==", "mimeType": "image/png"},
    {"type": "text", "text": "Compare it with this frame."}
  ]
}
```

These payload bytes are illustrative; replace them with your encoded files.
Native content is literal: text whitespace and block order are retained, and
text does not invoke slash commands, prompt templates, or magic keywords.
Only user text/image/media blocks are accepted. The array must contain 1–256
blocks, including at most 32 audio/video blocks, and the normalized content
JSON must fit 64 MiB, counting text escaping, base64, metadata, and block syntax.
Native images require canonical standard base64 and an `image/*` MIME type,
with a 20 MiB decoded per-image limit. Direct SDK images in `images` also have
aggregate bounds of 256 native images and 64 MiB of encoded image payloads.

Each media item requires `type: "media"`, an `audio/*` or `video/*` `mimeType`,
and nonempty canonical standard base64 `data`; padding is optional. `name` is
an optional source label, sanitized and bounded before entering session state.
URLs, file paths, and session-local blob references are not accepted as payloads.
Malformed media is rejected before acknowledgment, queue mutation, or session
recovery. The decoded per-file cap is `media.maxBytes` (5 MiB by default); RPC
also limits a command to 32 media items and 64 MiB of decoded media in total.

Both input forms work with `steer`, `follow_up`, and a streaming `prompt`
that specifies `streamingBehavior`. Accepted attachments survive automatic
retry and terminal queue recovery. Explicit `retry` preserves the original
structured block order, including attachment-only turns. Large payloads use
the session's content-addressed sidecar when saved and hydrate on resume.
Extension commands reject image/media attachments because their command
handlers have no attachment input path.

Gemini-family providers receive native audio/video input through their media
transport. Other providers receive the documented `[media omitted: ...]`
placeholder. Session and RPC message events retain the native content blocks.

### Compact output and attachment retrieval

RPC output defaults to `full`, preserving the existing inline payload format.
Clients that understand attachment references can opt into `compact` output
while the connection is idle:

```json
{"id":"mode-1","type":"set_output_mode","mode":"compact"}
```

The successful response contains `data: {"mode":"compact"}`; `get_state`
also reports `outputMode`. Switching to either mode is rejected during a
streaming turn or context compaction, so a turn has one output format.
Use `{"type":"set_output_mode","mode":"full"}` to restore full output.

In compact mode, native image and media blocks in message history and
message-bearing events keep their type, MIME type, optional name, and position.
An admitted block replaces only its base64 `data` string with this object:

```json
{
  "type":"image",
  "mimeType":"image/png",
  "data":{
    "attachmentId":"rpc:...",
    "$piBlob":"sha256:...",
    "sizeBytes":3,
    "encoding":"base64"
  }
}
```

The IDs and digest above are placeholders: use the exact `attachmentId`
issued by this connection. `$piBlob` identifies the SHA-256 digest of the
decoded bytes; `encoding` is `base64` or `base64NoPad` and records the
original spelling. Identical decoded bytes share an attachment ID even when
their original padding differs. Text, signed thinking, tool arguments,
custom details, and other opaque JSON remain unchanged.

Compact `message_update` events use the delta-only assistant event shape:
incremental updates omit the cumulative `message` and `partial` snapshots.
Message start/end, turn end, agent end, and tool output events retain their
lifecycle fields and terminal results, with native attachment data projected
as above. Terminal assistant events retain their final assistant payload.
`get_messages` uses borrowed compact projections without first cloning the
complete encoded attachment strings. `get_fork_messages` keeps its existing
`{entryId, text}` user-message previews; constructing those previews also
avoids cloning attachment contents.

Retrieve an issued attachment in bounded chunks:

```json
{"id":"bytes-1","type":"get_attachment","attachmentId":"rpc:...","offset":0,"maxBytes":2}
```

For an illustrative three-byte payload, the response data can be:

```json
{
  "attachmentId":"rpc:...",
  "$piBlob":"sha256:...",
  "offset":0,
  "nextOffset":2,
  "bytesReturned":2,
  "sizeBytes":3,
  "data":"YWI=",
  "eof":false
}
```

Offsets and lengths count **decoded bytes**. The default `offset` is 0;
`maxBytes` defaults to 65,536 and must be between 1 and 1,048,576.
Each returned `data` chunk is independently encoded with standard padded
base64. Decode each chunk before concatenating its bytes, then request the
returned `nextOffset` until `eof` is true. Reading exactly at `sizeBytes`
returns an empty chunk with `eof: true`; reading beyond it is an error.

The connection retains at most 128 MiB of decoded attachments, with a
64 MiB per-attachment limit and at most 4,096 distinct attachments. It does
not evict issued references. When data is malformed, noncanonical, oversized,
or cannot fit in the catalog, the original inline string is preserved.
Clients must therefore accept either form of `data` in compact mode.
These bounds apply to retained attachment bytes, not every RPC frame or
total process memory, and do not increase the existing media input limits.

Issued IDs remain readable across output mode changes. They expire after
a successful `new_session`, `switch_session`, or `fork`, and when the RPC
connection closes. Failed or cancelled session transitions keep existing IDs
valid. Retrieval accepts issued IDs, not filesystem paths or bare digests.
Attachment reference objects are an output format; they are not accepted in
`prompt`, `steer`, or `follow_up` inputs.

Compact output changes RPC serialization and retrieval. Native session loading,
provider requests, and extension payloads continue to use their complete
in-memory content; session resume still hydrates saved sidecar payloads.

### Session
- **new_session**: Start fresh.
  - Params: `parentSession` (optional path).
- **switch_session**: Load session file.
  - Params: `sessionPath`.
- **set_session_name**: Rename session.
  - Params: `name`.
- **export_html**: Export conversation.
  - Params: `outputPath`.
- **compact**: Trigger context compaction.
  - Params: `customInstructions` (optional), `reserveTokens` (optional), `keepRecentTokens` (optional).
- **fork**: Fork from a message.
  - Params: `entryId`.

### State & Config
- **get_state**: Get current model, settings, token usage, and `outputMode`.
- **get_messages**: Get conversation history in the negotiated output mode.
- **get_fork_messages**: Get `{entryId, text}` previews of branchable user messages.
- **set_output_mode**: Negotiate `full` (default) or `compact` output while idle.
  - Params: `mode`.
- **get_attachment**: Read a bounded byte range from an issued attachment ID.
  - Params: `attachmentId`, `offset` (optional), `maxBytes` (optional).
- **get_available_models**: List models.
- **set_model**: Change model.
  - Params: `provider`, `modelId`.
- **set_thinking_level**: Set thinking budget.
  - Params: `level` ("off", "low", etc.).
- **set_steering_mode**: "one-at-a-time" or "all".
- **set_follow_up_mode**: "one-at-a-time" or "all".

### Extension UI
- **extension_ui_response**: Reply to a pending extension UI request.
  - Params: echo the `requestGeneration` integer from the matching
    `extension_ui_request`, provide `requestId` (preferred) or legacy alias
    `id`, and include one of:
    - `confirmed` (boolean) for `confirm`
    - `value` (string/boolean depending on method) for `select`/`input`/`editor`
    - `cancelled` (`true`) to cancel

### Ask tool (requires `--tools ...,ask`)
- **ask_response**: Answer a pending `ask_request` event.
  - Params: `requestId` (or alias `id`), plus one of:
    - `answers`: array of `{questionId, selected: [labels], other?}` —
      `selected` holds chosen option labels (several when the question was
      `multi`); `other` carries a free-text answer instead.
    - `dismissed` (`true`) to cancel the whole request (the model sees a
      "user dismissed the question" tool error).
  - Reply: `{ "resolved": bool }` — `false` when the request already timed
    out (the tool waits `timeoutMs` from the event, then errors).

## Events

- `agent_start`: Agent started working.
- `text_delta`: Assistant text output chunk.
- `thinking_delta`: Assistant thinking output chunk.
- `tool_execution_start`: Tool execution started.
- `tool_execution_update`: Streaming tool output.
- `tool_execution_end`: Tool execution finished.
- `extension_ui_request`: Extension requested host UI interaction
  (confirm/select/input/editor/notify/etc.). Response-bearing events carry a
  `requestGeneration` correlation token; clients must echo it in
  `extension_ui_response` so a late response cannot resolve a newer request
  that reused the same public ID.
- `ask_request`: The ask tool needs the user to answer question cards.
  Carries `id`, `questions` (each `{id?, question, header?, options:
  [{label, description?}], recommended?, multi}`), and `timeoutMs`.
  Answer with the `ask_response` command.
- `agent_end`: Turn complete.
- `auto_retry_start` / `auto_retry_end`: Transient error retries.
- `auto_compaction_start` / `auto_compaction_end`: Auto-compaction status.
- `extension_error`: Extension event dispatch/runtime error.
- `error`: Fatal process error (gh #217). Printed exactly once on stdout
  before a non-zero exit, in both `--mode rpc` and `--mode json`:
  `{"type":"error","phase":"startup","code":"auth.missing_api_key","message":"…","exit_code":1}`.
  `phase` is `startup` when the loop never opened (config/auth/state-dir
  failures) and `run` after it did; `code` is a stable family name
  (`config`, `session`, `provider`, `auth`, `tool`, `usage`, `extension`,
  `io`, `json`, `state_store`, `aborted`, `api`, `internal`) or one of the
  finer `auth.*` / `config.*` diagnostic codes (`auth.missing_api_key`,
  `auth.no_models_available`, `auth.invalid_api_key`, …). It is not a `response`
  envelope — there is no request to answer — so clients should treat it as
  a terminal event.
