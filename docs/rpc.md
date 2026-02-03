# RPC Mode (stdin/stdout JSON protocol)

Pi supports a **headless RPC mode** intended for embedding in other applications (IDEs, custom UIs, orchestrators).
RPC mode is implemented in `src/rpc.rs`.

This document describes the Rust implementation as it exists today.

## Starting RPC mode

```bash
pi --mode rpc [options]
```

Notes:
- RPC mode **does not support** `@file` arguments (it will error). Provide context via normal prompts or your host UI.
- All RPC input is read from **stdin**; all protocol output is written to **stdout**. Logs are written to **stderr**.

## Framing

RPC uses **newline-delimited JSON** (one JSON object per line).

- **Commands (stdin)**: JSON objects with a `"type"` field (the command name).
- **Responses (stdout)**: JSON objects with `"type": "response"`.
- **Events (stdout)**: JSON objects with `"type"` set to an event name (anything other than `"response"`).

Every command may include an optional `"id"` (string). If provided, the response echoes the same `"id"`.
Events do not include `"id"`.

## Responses

Success response:

```json
{"type":"response","command":"get_state","success":true,"id":"req-1","data":{...}}
```

Error response:

```json
{"type":"response","command":"set_model","success":false,"id":"req-2","error":"Model not found: anthropic/..." }
```

Parse errors:
- Invalid JSON line:
  - `{"type":"response","command":"parse","success":false,"error":"Failed to parse command: ..."}`
- Missing `"type"` field:
  - `{"type":"response","command":"parse","success":false,"error":"Missing command type"}`

## Commands

All command objects must include:

- `type` (string): command name
- `id` (string, optional): correlation ID echoed in the response

### Prompting

#### `prompt`

Start a new agent run with a user message.

```json
{"id":"1","type":"prompt","message":"Hello"}
```

Images (base64 only in Rust implementation):

```json
{
  "type": "prompt",
  "message": "What's in this image?",
  "images": [
    {
      "type": "image",
      "source": {
        "type": "base64",
        "mediaType": "image/png",
        "data": "..."
      }
    }
  ]
}
```

If the agent is currently streaming, you must pass `streamingBehavior` to queue the message:

```json
{"type":"prompt","message":"New instruction","streamingBehavior":"steer"}
```

Allowed values:
- `"steer"`: queue as a steering message
- `"follow-up"` or `"followUp"`: queue as a follow-up message

If streaming and `streamingBehavior` is omitted/invalid, `prompt` returns an error response.

Response (acknowledged immediately; events follow asynchronously):

```json
{"id":"1","type":"response","command":"prompt","success":true}
```

#### `steer`

Queue a steering message (or start immediately if idle).

```json
{"type":"steer","message":"Stop and do X instead"}
```

Notes:
- Input expansion is applied (skills / prompt templates).
- Extension commands (messages starting with `/` that are not expanded) are rejected for `steer`.

Response:

```json
{"type":"response","command":"steer","success":true}
```

#### `follow_up`

Queue a follow-up message (or start immediately if idle).

```json
{"type":"follow_up","message":"After this finishes, also do Y"}
```

Notes:
- Input expansion is applied (skills / prompt templates).
- Extension commands are rejected for `follow_up`.

Response:

```json
{"type":"response","command":"follow_up","success":true}
```

#### `abort`

Abort an in-flight agent run.

```json
{"type":"abort"}
```

Response:

```json
{"type":"response","command":"abort","success":true}
```

### State

#### `get_state`

Get the current RPC/agent state.

```json
{"type":"get_state"}
```

Response data fields:
- `model`: model object or `null` (see [Model](#model))
- `thinkingLevel`: string
- `isStreaming`: boolean
- `isCompacting`: boolean
- `steeringMode`: `"all"` or `"one-at-a-time"`
- `followUpMode`: `"all"` or `"one-at-a-time"`
- `sessionFile`: string path or `null` (in-memory sessions)
- `sessionId`: string
- `sessionName`: string or `null`
- `autoCompactionEnabled`: boolean
- `messageCount`: number (messages in current branch/path)
- `pendingMessageCount`: number (queued steering + follow-up messages)

#### `get_messages`

Get all message objects for the current session path (user/assistant/tool results and bash executions).

```json
{"type":"get_messages"}
```

Response:

```json
{"type":"response","command":"get_messages","success":true,"data":{"messages":[...]}}
```

#### `get_session_stats`

Get aggregated token usage and cost for the current session path.

```json
{"type":"get_session_stats"}
```

Response `data` includes:
- `sessionFile`, `sessionId`
- `userMessages`, `assistantMessages`, `toolCalls`, `toolResults`, `totalMessages`
- `tokens`: `{input, output, cacheRead, cacheWrite, total}`
- `cost`: number

### Models and Thinking

#### `get_available_models`

List available models (from configured model registry).

```json
{"type":"get_available_models"}
```

Response:

```json
{"type":"response","command":"get_available_models","success":true,"data":{"models":[...]}}
```

#### `set_model`

Select a specific model.

```json
{"type":"set_model","provider":"anthropic","modelId":"claude-sonnet-4-20250514"}
```

Response `data` is the selected [Model](#model) object.

#### `cycle_model`

Cycle to the next model in the configured model list/scoped model list.

```json
{"type":"cycle_model"}
```

Response:

```json
{
  "type": "response",
  "command": "cycle_model",
  "success": true,
  "data": {
    "model": {...},
    "thinkingLevel": "medium",
    "isScoped": false
  }
}
```

If cycling is not available, `data` is `null`.

#### `set_thinking_level`

Set the thinking level.

```json
{"type":"set_thinking_level","level":"high"}
```

Response:

```json
{"type":"response","command":"set_thinking_level","success":true}
```

Note: levels may be clamped based on the selected model.

#### `cycle_thinking_level`

Cycle through available thinking levels for the current model.

```json
{"type":"cycle_thinking_level"}
```

Response:

```json
{"type":"response","command":"cycle_thinking_level","success":true,"data":{"level":"high"}}
```

If the current model does not support thinking, `data` is `null`.

### Queue Modes

#### `set_steering_mode`

```json
{"type":"set_steering_mode","mode":"one-at-a-time"}
```

Allowed `mode` values: `"all"`, `"one-at-a-time"`.

#### `set_follow_up_mode`

```json
{"type":"set_follow_up_mode","mode":"one-at-a-time"}
```

Allowed `mode` values: `"all"`, `"one-at-a-time"`.

### Compaction

#### `compact`

Run manual compaction for the current session path.

```json
{"type":"compact"}
```

Optional custom instructions:

```json
{"type":"compact","customInstructions":"Focus on code changes"}
```

Response `data` includes:
- `summary`
- `firstKeptEntryId`
- `tokensBefore`
- `details`

#### `set_auto_compaction`

Enable/disable auto-compaction after a successful run.

```json
{"type":"set_auto_compaction","enabled":true}
```

### Retry

#### `set_auto_retry`

Enable/disable automatic retry after transient failures.

```json
{"type":"set_auto_retry","enabled":true}
```

#### `abort_retry`

Abort an in-progress retry delay (does not abort an in-flight request; use `abort` for that).

```json
{"type":"abort_retry"}
```

### Bash

#### `bash`

Execute a shell command and record the result in the session as a `BashExecution` message.

```json
{"type":"bash","command":"ls -la"}
```

Response `data` includes:
- `output` (combined stdout+stderr, truncated to default limits)
- `exitCode`
- `cancelled`
- `truncated`
- `fullOutputPath` (present when `truncated` is true)

Only one bash command can run at a time; concurrent `bash` calls return an error response.

#### `abort_bash`

Abort a running bash command.

```json
{"type":"abort_bash"}
```

### Sessions

#### `new_session`

Start a new session (clears steering/follow-up queues).

```json
{"type":"new_session"}
```

Optional parent session tracking:

```json
{"type":"new_session","parentSession":"/path/to/parent-session.jsonl"}
```

Response:

```json
{"type":"response","command":"new_session","success":true,"data":{"cancelled":false}}
```

#### `switch_session`

Load an existing session file.

```json
{"type":"switch_session","sessionPath":"/path/to/session.jsonl"}
```

Response:

```json
{"type":"response","command":"switch_session","success":true,"data":{"cancelled":false}}
```

#### `fork`

Fork from a previous user message entry id (session branching).

```json
{"type":"fork","entryId":"a1b2c3d4"}
```

Response `data` includes:
- `text`: the selected user message text (for host UI to prefill an editor)
- `cancelled`: currently always `false` in Rust RPC mode

#### `get_fork_messages`

List user messages that can be used as fork points.

```json
{"type":"get_fork_messages"}
```

Response:

```json
{"type":"response","command":"get_fork_messages","success":true,"data":{"messages":[{"entryId":"...","text":"..."}]}}
```

#### `set_session_name`

Set the display name for the current session.

```json
{"type":"set_session_name","name":"my-feature-work"}
```

#### `get_last_assistant_text`

Get the concatenated text blocks of the last assistant message (or `null` if none).

```json
{"type":"get_last_assistant_text"}
```

Response:

```json
{"type":"response","command":"get_last_assistant_text","success":true,"data":{"text":"..."}}
```

#### `export_html`

Export the current session to an HTML file.

```json
{"type":"export_html"}
```

Optional path:

```json
{"type":"export_html","outputPath":"/tmp/session.html"}
```

Response:

```json
{"type":"response","command":"export_html","success":true,"data":{"path":"/tmp/session.html"}}
```

### Commands

#### `get_commands`

Return available slash commands sourced from skills, prompt templates, and extensions.

```json
{"type":"get_commands"}
```

Response:

```json
{"type":"response","command":"get_commands","success":true,"data":{"commands":[...]}}
```

### Extension UI

#### `extension_ui_response`

Placeholder command (currently acknowledged but not acted upon in Rust).

```json
{"type":"extension_ui_response", "id":"..."}
```

## Events

Events are emitted during agent runs started by `prompt`/`steer`/`follow_up`.

Events are JSON objects with a `"type"` field that is **not** `"response"`.

### Agent events (`AgentEvent`)

These events are serialized directly from `AgentEvent` (`src/agent.rs`):

| type | Notes |
|------|-------|
| `agent_start` | Run begins |
| `turn_start` | New assistant turn begins |
| `message_start` | A message begins (user/assistant/tool result) |
| `message_update` | Streaming update (includes `assistantMessageEvent`) |
| `tool_execution_start` | Tool call started |
| `tool_execution_update` | Tool progress update |
| `tool_execution_end` | Tool call finished |
| `message_end` | A message finished |
| `turn_end` | Turn completed (includes tool results) |
| `agent_end` | Run completed (includes all new messages; may include `error`) |

### Auto-retry events

Emitted when auto-retry is enabled and a run fails transiently.

`auto_retry_start`:

```json
{"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":1000,"errorMessage":"..."}
```

`auto_retry_end` (emitted once after retries finish or are aborted):

```json
{"type":"auto_retry_end","success":false,"attempt":2,"finalError":"Retry aborted"}
```

### Auto-compaction events

Emitted after a successful run when auto-compaction is enabled and token usage crosses the configured threshold.

`auto_compaction_start`:

```json
{"type":"auto_compaction_start","reason":"threshold"}
```

`auto_compaction_end` (success):

```json
{
  "type": "auto_compaction_end",
  "result": {"summary":"...","firstKeptEntryId":"...","tokensBefore":123,"details":{}},
  "aborted": false,
  "willRetry": false
}
```

`auto_compaction_end` (error):

```json
{"type":"auto_compaction_end","result":null,"aborted":false,"willRetry":false,"errorMessage":"..."}
```

## Model

The `model` objects returned by RPC are derived from the loaded `models.json` entries.

Shape (fields may grow over time; clients should ignore unknown fields):

```json
{
  "id": "claude-sonnet-4-20250514",
  "name": "Claude Sonnet 4",
  "api": "anthropic-messages",
  "provider": "anthropic",
  "baseUrl": "https://api.anthropic.com/v1/messages",
  "reasoning": true,
  "input": ["text", "image"],
  "contextWindow": 200000,
  "maxTokens": 8192,
  "cost": {"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0}
}
```

## Implementation pointers

- `src/rpc.rs`: protocol implementation and command handlers
- `src/agent.rs`: `AgentEvent` shapes
- `tests/rpc_mode.rs`: basic protocol tests (get_state, prompt, stats)
