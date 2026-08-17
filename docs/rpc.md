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
  - Params: `message` (string), `images` (optional array), `streamingBehavior` ("steer" or "follow-up").
- **steer**: Interrupt current generation and steer.
  - Params: `message`.
- **follow_up**: Queue a message to follow current turn.
  - Params: `message`.
- **abort**: Stop generation.

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
- **get_state**: Get current model, settings, token usage.
- **get_messages**: Get conversation history.
- **get_available_models**: List models.
- **set_model**: Change model.
  - Params: `provider`, `modelId`.
- **set_thinking_level**: Set thinking budget.
  - Params: `level` ("off", "low", etc.).
- **set_steering_mode**: "one-at-a-time" or "all".
- **set_follow_up_mode**: "one-at-a-time" or "all".

### Extension UI
- **extension_ui_response**: Reply to a pending extension UI request.
  - Params: `requestId` (preferred) or legacy alias `id`, plus one of:
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
- `extension_ui_request`: Extension requested host UI interaction (confirm/select/input/editor/notify/etc.).
- `ask_request`: The ask tool needs the user to answer question cards.
  Carries `id`, `questions` (each `{id?, question, header?, options:
  [{label, description?}], recommended?, multi}`), and `timeoutMs`.
  Answer with the `ask_response` command.
- `agent_end`: Turn complete.
- `auto_retry_start` / `auto_retry_end`: Transient error retries.
- `auto_compaction_start` / `auto_compaction_end`: Auto-compaction status.
- `extension_error`: Extension event dispatch/runtime error.
