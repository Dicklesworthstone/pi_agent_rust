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
| `pi::sdk::{AssistantMessage, ContentBlock, Cost, CustomMessage, ImageContent, Message, StopDetails, StopReason, StreamEvent, TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage, UserContent, UserMessage}` | Stable | Message, content, streaming, and accounting model types. |
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
- In-process `AgentSessionHandle` currently exposes prompt/state/model/thinking/compaction flows; queue controls like `steer`/`follow_up` are on `RpcTransportClient`.
- `SessionTransport::prompt` returns `SessionPromptResult`, which is `InProcess(Box<AssistantMessage>)` or `RpcEvents(Vec<Value>)` depending on backend.
- Extension loading is opt-in via `extension_paths`, with `extension_policy`/`repair_policy` controls.

## Verified Reference Surfaces

- `src/sdk.rs`
- `tests/sdk_api.rs`
- `tests/sdk_unit.rs`
- `tests/sdk_integration.rs`
