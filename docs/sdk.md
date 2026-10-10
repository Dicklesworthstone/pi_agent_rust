# SDK Cookbook and Migration Guide

This guide is for teams embedding Pi as a Rust library. The Rust SDK provides
idiomatic Rust APIs for Pi's core embedding workflows, using Rust-native
patterns such as `Result` types and structured concurrency.

**Note**: This SDK is an idiomatic Rust companion to the pi-mono TypeScript
SDK, not a drop-in equivalent. Parity remains governed by the active
certification contract and its provenance-matched verdict.

## Install

```toml
[dependencies]
pi = { package = "pi_agent_rust", version = "0.2.0" }
futures = "0.3"
```

When developing against a local checkout, replace `version = "0.2.0"` with
`path = "/path/to/pi_agent_rust"` while retaining `package = "pi_agent_rust"`.

### Raise your crate's `recursion_limit`

Add this at the top of the crate that drives a session:

```rust
#![recursion_limit = "256"]
```

Pi's runtime nests its future types deeply enough that proving `Send` for a
session future can exceed rustc's default limit of 128. `recursion_limit` is
per-crate and is **not** inherited from a dependency, so pi raising it
internally does nothing for yours. Without it you get an `overflow evaluating
the requirement ...: std::marker::Send` error, or a
`recursion_depth_exceeding_limit` warning that `-D warnings` makes fatal — and
neither names the real cause.

This is not hypothetical: every one of pi's own binaries, examples and
integration tests needed the attribute, `examples/basic_sdk.rs` included.

## SemVer Surface

The supported library surface is the crate root aliases `pi::Error`,
`pi::PiResult`, and the `pi::sdk` module. Other root modules are implementation
details for the CLI, examples, and in-repository tests; they are hidden from the
published API documentation and may change without SemVer guarantees.

The `semver` GitHub Actions workflow runs `cargo-semver-checks` on PRs and
`main` pushes that touch the SDK/API surface. It compares the current public
API to the PR target branch or previous push baseline. An incompatible change
to a stable item requires a SemVer-incompatible bump (`0.y` to `0.(y+1)` before
1.0, or a major-version bump after 1.0). Only semver-compatible additions
remain compatible; adding public enum variants or required struct fields can
be breaking for Rust consumers.

### Stability Annotations

| Item | Stability | Notes |
| --- | --- | --- |
| `pi::Error` | Stable | Crate-root error type alias target. |
| `pi::PiResult` | Stable | Crate-root result alias for `pi::Error`. |
| `pi::sdk::{Error, Result}` | Stable | SDK error/result exports. |
| `pi::sdk::{AbortHandle, AbortSignal}` | Stable | Prompt cancellation handles. |
| `pi::sdk::{Agent, AgentConfig, AgentEvent, AgentSession, QueueMode}` | Stable | In-process agent/session integration exports. |
| `pi::sdk::{AssistantMessage, ContentBlock, Cost, CustomMessage, ImageContent, MediaContent, Message, StopDetails, StopReason, StreamEvent, TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage, UserContent, UserMessage}` | Stable | Message, content, streaming, and accounting model types. |
| `pi::sdk::{Config, ExtensionManager, ExtensionPolicy, ExtensionRegion, Session, ThinkingLevel}` | Stable | Configuration, extension, session, and thinking-control exports. |
| `pi::sdk::{InputType, Model, ModelCost, Provider, ProviderContext, ProviderThinkingBudgets, StreamOptions, ToolDef}` | Stable | Provider integration exports. |
| `pi::sdk::{ModelEntry, ModelRegistry}` | Stable | Model registry exports. |
| `pi::sdk::{Tool, ToolDefinition, ToolOutput, ToolRegistry, ToolUpdate}` | Stable | Tool integration exports. |
| `pi::sdk::BUILTIN_TOOL_NAMES` | Stable | Canonical default non-delegating tool-name inventory; opt-in `subagent` is separate. |
| `pi::sdk::{create_read_tool, create_bash_tool, create_edit_tool, create_write_tool, create_grep_tool, create_find_tool, create_ls_tool, create_hashline_edit_tool, create_all_tools}` | Stable | Default non-delegating tool constructors. |
| `pi::sdk::{tool_to_definition, all_tool_definitions}` | Stable | Default non-delegating tool schema helpers. |
| `pi::sdk::{SubscriptionId, EventListeners, EventSubscriber, OnStreamEvent, OnToolEnd, OnToolStart}` | Stable | Event subscription and hook types. |
| `pi::sdk::{SessionOptions, ToolFactory, default_tool_registry}` | Stable | In-process session construction and custom tool registry extension points. |
| `pi::sdk::{AgentSessionHandle, AgentSessionState, create_agent_session}` | Stable | Primary in-process SDK entry point and state handle. |
| `pi::sdk::{SessionPromptResult, SessionTransport, SessionTransportEvent, SessionTransportState}` | Stable | Unified in-process/RPC transport adapter. |
| `pi::sdk::{RpcTransportClient, RpcTransportOptions}` | Stable | Subprocess RPC transport client. |
| `pi::sdk::{RpcBashResult, RpcCancelledResult, RpcCommandInfo, RpcCompactionResult, RpcCycleModelResult, RpcExportHtmlResult, RpcExtensionUiResponse, RpcForkMessage, RpcForkResult, RpcLastAssistantText, RpcModelInfo, RpcSessionState, RpcSessionStats, RpcThinkingLevelResult, RpcTokenStats}` | Stable | RPC request/response payloads. |

## Migration Map (TypeScript -> Rust)

| TypeScript surface | Rust SDK surface |
| --- | --- |
| `createAgentSession(options)` | `pi::sdk::create_agent_session(SessionOptions)` |
| `session.prompt(text, onEvent)` | `AgentSessionHandle::prompt(text, on_event)` |
| `session.subscribe(listener)` | `AgentSessionHandle::subscribe(listener)` |
| `unsubscribe()` | `AgentSessionHandle::unsubscribe(subscription_id)` |
| `session.setModel(provider, model)` | `AgentSessionHandle::set_model(provider, model)` |
| `session.setThinkingLevel(level)` | `AgentSessionHandle::set_thinking_level(level)` |
| `session.compact()` | `AgentSessionHandle::compact(on_event)` |
| `session.abort()` | `AgentSessionHandle::new_abort_handle()` + `prompt_with_abort(...)` |
| `session.steer(...)`, `session.followUp(...)` | `RpcTransportClient::steer(...)`, `RpcTransportClient::follow_up(...)` |
| RPC bridge client | `RpcTransportClient` / `SessionTransport::RpcSubprocess` |

## Recipe 1: Create In-Process Session and Prompt

```rust
use futures::executor::block_on;
use pi::sdk::{AgentEvent, SessionOptions, create_agent_session};

fn main() -> pi::sdk::Result<()> {
    let mut session = block_on(create_agent_session(SessionOptions {
        provider: Some("openai".to_string()),
        model: Some("gpt-4o".to_string()),
        api_key: Some(std::env::var("OPENAI_API_KEY").unwrap_or_default()),
        no_session: true,
        ..SessionOptions::default()
    }))?;

    let message = block_on(session.prompt("Summarize src/sdk.rs", |event: AgentEvent| {
        eprintln!("{event:?}");
    }))?;

    println!("{message:#?}");
    Ok(())
}
```

### Native image, audio, and video prompts

Use `prompt_with_content` when the prompt contains ordered text and native
attachments. `ImageContent` carries images; `MediaContent` carries audio or
video with a MIME type and optional display name. The `data` field contains
base64 payload bytes, not a path or URL. Pi does not fetch input sources from
this method.

```rust
use pi::sdk::{AgentSessionHandle, ContentBlock, MediaContent, TextContent};

async fn summarize_clip(
    session: &mut AgentSessionHandle,
    clip_base64: String,
) -> pi::sdk::Result<()> {
    let response = session
        .prompt_with_content(
            vec![
                ContentBlock::Text(TextContent::new("Summarize this clip.")),
                ContentBlock::Media(MediaContent {
                    data: clip_base64,
                    mime_type: "video/mp4".to_string(),
                    name: Some("clip.mp4".to_string()),
                }),
            ],
            |_| {},
        )
        .await?;
    println!("{response:#?}");
    Ok(())
}
```

`prompt_with_content_with_abort` accepts an `AbortSignal` for the same input.
Both methods use the handle's normal persistence, retry, failover, and event
subscriptions. Recovery reuses the accepted user message, without appending
the media again or repeating input hooks. Media-only prompts are supported;
an empty block list or an assistant-only block is rejected before appending
the user message.

`SessionTransport::prompt_with_content(content, on_event)` provides this input
over either backend. `RpcTransportClient::prompt_with_content(content)` and
`prompt_with_content_streaming(content, streaming_behavior, on_event)` send
the ordered RPC `content` array. The subprocess path validates before assigning
a request ID or writing input, including a 256-block limit, at most 32 audio/video
blocks, and a 64 MiB serialized content limit that counts JSON escaping and
metadata. The server also enforces its configured per-media limit (5 MiB by
default). RPC native text is literal; use the ordinary text API when you want
server-side template, extension-command, or magic-keyword processing.

For an explicit retry, `prepare_retry_content().await` returns the abandoned
turn as `UserContent::Text` or `UserContent::Blocks`. Submit it through `prompt`
or `prompt_with_content` respectively. The abandoned branch remains in the
session tree, and the parent leaf is persisted before the live context moves.
Failed persistence preserves the live branch and fences further provider calls
until recovery. The existing `prepare_retry()` accepts text-only stored input;
it refuses structured input before mutation and directs callers to the native API.

Input hooks retain their existing text/image interface. When a hook leaves
those fields unchanged, the complete original block order is preserved. When
it edits them, replacement text occupies the first original text position,
replacement images occupy image positions, and extra images append at the
end. Audio/video blocks remain in the prompt. Providers use their existing
native media transport where supported; unsupported media becomes a visible
omission placeholder. The local transcript retains the native content.

Media names respect the configured secret-screening policy. A MIME type that
contains secret material is refused when screening is enabled because replacing
part of that identifier could change how the provider handles the attachment.
Encoded payload bytes remain opaque to the text secret detector.

### Checkpoints and branch rewinds

`mark_checkpoint(name, note).await` records the current conversation boundary.
`rewind_to_checkpoint(Some(name)).await` summarizes the active span after that
marker and replaces the span with its report. The original entries remain in
the session tree. The boundary is resolved from the current tree projection,
so a retained checkpoint still works after compaction changes message counts.
A checkpoint that compaction removed from the active context is refused;
restore its original branch or mark a new checkpoint.

`rewind_to_user_message(entry_id).await` moves to the parent of a selected user
message and returns its editor preparation. Sending the next prompt creates a
sibling branch. Both rewind methods save a private candidate before changing
the live conversation. Checkpoint creation uses the same persistence ordering.
An interrupted or failed save preserves the live view and blocks further
provider requests until persistence is recovered. Ephemeral sessions apply
the change in memory without writing a file.

Checkpoint summarization respects provider admission and the configured secret
policy. A summary error or privacy refusal leaves the span intact. If an
extension changes the session during summarization, the stale report is
rejected without overwriting that change.

## Recipe 2: Session-Level Subscribers and Typed Hooks

```rust
use futures::executor::block_on;
use pi::sdk::{SessionOptions, create_agent_session};
use std::sync::Arc;

fn main() -> pi::sdk::Result<()> {
    let options = SessionOptions {
        on_tool_start: Some(Arc::new(|tool, args| eprintln!("tool start: {tool} {args}"))),
        on_tool_end: Some(Arc::new(|tool, output, is_error| {
            eprintln!("tool end: {tool}, error={is_error}, output={output:?}");
        })),
        on_stream_event: Some(Arc::new(|ev| eprintln!("stream: {ev:?}"))),
        ..SessionOptions::default()
    };

    let mut session = block_on(create_agent_session(options))?;
    let sub_id = session.subscribe(|event| eprintln!("session event: {event:?}"));

    let _ = block_on(session.prompt("read Cargo.toml", |_| {}))?;
    let _removed = session.unsubscribe(sub_id);
    Ok(())
}
```

## Recipe 3: Prompt Cancellation

```rust
use futures::executor::block_on;
use pi::sdk::{AgentSessionHandle, SessionOptions, create_agent_session};

fn main() -> pi::sdk::Result<()> {
    let mut session = block_on(create_agent_session(SessionOptions::default()))?;

    let (abort_handle, abort_signal) = AgentSessionHandle::new_abort_handle();
    let fut = session.prompt_with_abort("long running prompt", abort_signal, |_| {});
    abort_handle.abort();
    let _ = block_on(fut);
    Ok(())
}
```

## Recipe 4: Model and Thinking Controls

```rust
use futures::executor::block_on;
use pi::sdk::{SessionOptions, ThinkingLevel, create_agent_session};

fn main() -> pi::sdk::Result<()> {
    let mut session = block_on(create_agent_session(SessionOptions::default()))?;
    block_on(session.set_model("openai", "gpt-4o"))?;
    block_on(session.set_thinking_level(ThinkingLevel::Low))?;

    let state = block_on(session.state())?;
    println!("provider={} model={}", state.provider, state.model_id);
    Ok(())
}
```

## Recipe 5: Load Extensions in SDK Sessions

```rust
use futures::executor::block_on;
use pi::sdk::{SessionOptions, create_agent_session};
use std::path::PathBuf;

fn main() -> pi::sdk::Result<()> {
    let session = block_on(create_agent_session(SessionOptions {
        extension_paths: vec![PathBuf::from("extensions/my_extension.js")],
        extension_policy: Some("safe".to_string()),
        repair_policy: Some("ask".to_string()),
        ..SessionOptions::default()
    }))?;

    if session.has_extensions() {
        eprintln!("extensions loaded");
    }
    Ok(())
}
```

## Recipe 5b: Handle Extension UI Prompts and Permission Scope

Without a UI handler, SDK sessions fail closed: extension UI requests error and
capability prompts resolve to deny. Attach a handler to answer them in-process.

```rust
use futures::executor::block_on;
use pi::sdk::{
    ExtensionUiHandler, ExtensionUiRequest, ExtensionUiResponse, SessionOptions,
    create_agent_session,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

struct AllowOnce;

#[async_trait::async_trait]
impl ExtensionUiHandler for AllowOnce {
    async fn request_ui(
        &self,
        request: ExtensionUiRequest,
    ) -> pi::sdk::Result<Option<ExtensionUiResponse>> {
        Ok(Some(ExtensionUiResponse {
            id: request.id,
            // Plain `Value::Bool(allow)` keeps default persistence; an object
            // controls it per decision ("persist": false = this session only).
            value: Some(json!({ "allow": true, "persist": false })),
            cancelled: false,
        }))
    }
}

fn main() -> pi::sdk::Result<()> {
    let _session = block_on(create_agent_session(SessionOptions {
        extension_paths: vec![PathBuf::from("extensions/my_extension.js")],
        extension_ui_handler: Some(Arc::new(AllowOnce)),
        // `false` scopes all prompt decisions to this session's memory instead
        // of `~/.pi/extension-permissions.json` (default `true` = CLI behavior).
        persist_extension_permissions: false,
        ..SessionOptions::default()
    }))?;
    Ok(())
}
```

## Recipe 5c: Override Compaction Settings Per Session

```rust
use futures::executor::block_on;
use pi::sdk::{ResolvedCompactionSettings, SessionOptions, create_agent_session};

fn main() -> pi::sdk::Result<()> {
    let session = block_on(create_agent_session(SessionOptions {
        // Used verbatim; `None` keeps the config/model-derived defaults.
        compaction_settings: Some(ResolvedCompactionSettings {
            enabled: true,
            context_window_tokens: 200_000,
            reserve_tokens: 32_768,
            keep_recent_tokens: 40_000,
        }),
        ..SessionOptions::default()
    }))?;
    eprintln!("resolved: {:?}", session.compaction_settings());
    Ok(())
}
```

## Recipe 6: Use RPC Transport Client

```rust
use futures::executor::block_on;
use pi::sdk::{RpcTransportClient, RpcTransportOptions};

fn main() -> pi::sdk::Result<()> {
    let mut rpc = RpcTransportClient::connect(RpcTransportOptions::default())?;

    let state = block_on(rpc.get_state())?;
    println!("rpc session id: {}", state.session_id);

    let events = block_on(rpc.prompt("Hello from RPC"))?;
    println!("received {} rpc events", events.len());

    rpc.shutdown()?;
    Ok(())
}
```

## Recipe 7: Unified Transport Adapter (In-Process or RPC)

```rust
use futures::executor::block_on;
use pi::sdk::{SessionOptions, SessionTransport};

fn main() -> pi::sdk::Result<()> {
    let mut transport = block_on(SessionTransport::in_process(SessionOptions::default()))?;

    let _result = block_on(transport.prompt("Status?", |_event| {}))?;
    let _state = block_on(transport.state())?;
    transport.shutdown()?;
    Ok(())
}
```

## Compatibility Notes for Migrating Integrators

- `SessionOptions::default().no_session` is `true` (ephemeral by default).
- `SessionOptions::model_scope` accepts the ordered patterns and optional `:thinking` suffixes used by `--models`. Explicit provider/model and thinking choices take precedence; reopening retains the saved model when no explicit provider/model is supplied. With no scope override, the session uses its workspace's model scope before global enabled models.
- `SessionOptions::max_time` sets the wall-clock run limit (`None` by default). The agent checks it at turn boundaries, allows admitted tools to finish, and saves its time-cap marker with the conversation. The default TUI forwards `--max-time` to this option.
- In-process `AgentSessionHandle` currently exposes prompt/state/model/thinking/compaction flows; queue controls like `steer`/`follow_up` are on `RpcTransportClient`.
- `SessionTransport::prompt` returns `SessionPromptResult`, which is `InProcess(Box<AssistantMessage>)` or `RpcEvents(Vec<Value>)` depending on backend.
- Extension loading is opt-in via `extension_paths`, with `extension_policy`/`repair_policy` controls.
- Extension UI/capability prompts are answered via `SessionOptions::extension_ui_handler`; without one they fail closed (deny).
- Prompt decisions persist to disk by default (CLI parity); `persist_extension_permissions: false` or a per-response `"persist": false` scopes them to the session.
- `SessionOptions::compaction_settings` overrides the config/model-derived compaction settings verbatim when `Some`.

## Verified Reference Surfaces

- `src/sdk.rs`
- `tests/sdk_api.rs`
- `tests/sdk_unit.rs`
- `tests/sdk_integration.rs`


### RPC subprocess streaming

`RpcTransportClient::prompt_with_options_streaming` delivers each raw RPC event
as it is read instead of buffering the complete turn first. `SessionTransport::prompt`
uses that path, so its callback has the same live-delivery contract in subprocess
mode as in-process mode.

A server event that races ahead of the matching prompt acknowledgement is retained
under explicit count and byte bounds, then delivered in order after a successful
acknowledgement. A failed acknowledgement does not expose those speculative events.
Prompt acknowledgements must match both request id and command. Individual
line-delimited JSON frames are capped at 128 MiB; the pre-acknowledgement buffer
is bounded to 256 events and 256 MiB, including room for native user-message
echoes. Oversized or truncated frames fail
the transport rather than allocating without bound. Public generic RPC requests
cannot override the SDK-generated `type` or `id` fields.

The returned `RpcEvents` vector still contains the delivered events for callers
that need the completed transcript. Live callbacks are therefore additive, not a
change to the completion payload.

### Cancellation and subprocess ownership

The ordinary `RpcTransportClient` request and prompt methods await pipe I/O
without blocking the caller's executor. A slow reader, a partial JSON response,
or a quiet provider stream leaves other futures on the same executor able to run.
Each connection owns a stdin worker and a stdout worker. The stdin queue holds
one pending frame; the stdout pump retains at most one queued frame and one
additional frame it is preparing or delivering. The existing 128 MiB per-frame
and pre-acknowledgement limits still apply. The completed event vector remains a
caller-visible transcript and is not covered by the pump's queue bound.

Dropping an in-flight request or prompt future closes the entire connection,
revokes retained control handles, and starts owned process-tree cleanup. This is
also the response to malformed acknowledgements, duplicate prompt acknowledgements,
truncated frames, or transport failure. Reconnect before making another request;
an interrupted exchange cannot safely donate its delayed response or terminal
events to the next operation. The SDK does not replay interrupted operations,
whose tool effects may already have happened. A well-formed server refusal
completes that request and leaves the connection usable.

To abort a turn while keeping its session and connection, keep polling the prompt
future and send `RpcControlHandle::abort()` instead. Continue consuming through
its terminal `agent_end`; the control method confirms dispatch, while the prompt
stream reports the completed turn.

`shutdown()` and client destruction stop the owned subprocess and join its pipe
workers and reaper, including a reap started by earlier future cancellation.
Unix subprocesses get a private process group; Windows subprocesses enter the
existing Job discipline before running. Unix pipe workers also observe connection
closure while polling, so an inherited descriptor held outside the owned process
group does not keep a worker blocked on pipe EOF. If operating-system termination
fails, explicit shutdown reports the error and retains cleanup ownership for a
retry instead of waiting on pipe workers that may still be blocked.


### Mid-turn RPC control

Call `RpcTransportClient::control_handle()` before starting a subprocess prompt
when another thread, event loop, or the prompt's live callback may need to steer
or abort it. The cloned `RpcControlHandle` shares the serialized stdin writer,
request-id allocator, and connection lifetime. The prompt remains the **only
consumer of parsed response frames**, so concurrent control never races a
second consumer over the RPC event stream.

`RpcControlHandle::steer`, `follow_up`, and `abort` synchronously write a complete
command to the pipe and return its SDK-owned request id. These control methods
can wait on pipe backpressure; the async request and prompt methods use the I/O
workers described above. A successful control return means
the command was dispatched to the subprocess pipe; it does not claim the RPC
server accepted or completed the operation. Its acknowledgement is consumed by
the prompt's single reader. Use the ordinary `RpcTransportClient` methods when
you need an acknowledgement and no prompt currently owns the read lane.

The control lane and ordinary requests use one atomic id sequence and one mutexed
writer, preventing duplicate IDs or interleaved JSON lines. Holding a control
handle does not keep the child process alive after the owning client shuts down
or an in-flight exchange is dropped; subsequent writes then fail.
