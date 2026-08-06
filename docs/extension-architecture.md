# Extension Runtime Architecture

This document describes the extension runtime architecture for `pi_agent_rust`,
covering the runtime model, hostcall dispatch, capability policy, trust
boundaries, and structured concurrency.

## Overview

Extensions support two runtime modes:

1. Legacy JS/TS entrypoints (`.js/.jsx/.ts/.mjs/.cjs/.tsx/.mts/.cts`) run inside an
   embedded QuickJS interpreter.
2. Native descriptor entrypoints (`*.native.json`) run through the
   native-rust descriptor runtime.

For legacy Pi compatibility, JS/TS entrypoints are loaded directly with no
manual conversion step. Runtime selection is automatic from the entrypoint type.
The host enforces a configurable capability policy before dispatching each
request.

```
 Extension JS (untrusted)          Rust Host (trusted)
 ┌──────────────────────┐         ┌──────────────────────────────┐
 │  pi.tool(...)        │         │  ToolRegistry.execute()      │
 │  pi.exec(...)        │ ─────►  │  subprocess spawn            │
 │  pi.http(...)        │ hostcall│  HttpConnector.dispatch()    │
 │  pi.session(...)     │ channel │  ExtensionSession trait      │
 │  pi.ui(...)          │         │  UI channel (mpsc)           │
 │  pi.events(...)      │         │  ExtensionManager state      │
 │  pi.log(...)         │         │  structured log sink         │
 └──────────────────────┘         └──────────────────────────────┘
```

## Core Types

### `ExtensionManager` (`src/extensions.rs`)

Central registry wrapping `Arc<Mutex<ExtensionManagerInner>>`. Thread-safe,
cheaply cloneable. Owns:

| Field | Type | Purpose |
|-------|------|---------|
| `extensions` | `Vec<RegisterPayload>` | Registered extension metadata |
| `runtime` | `Option<ExtensionRuntimeHandle>` | Active extension runtime handle (QuickJS or native-rust) |
| `ui_sender` | `Option<mpsc::Sender<ExtensionUiRequest>>` | Channel to TUI for user prompts |
| `session` | `Option<Arc<dyn ExtensionSession>>` | Current session state access |
| `active_tools` | `Option<Vec<String>>` | Enabled tool names for this session |
| `providers` | `Vec<Value>` | Custom `streamSimple` provider models |
| `flags` | `Vec<Value>` | Extension-registered feature flags |
| `policy_prompt_cache` | `HashMap<String, HashMap<String, bool>>` | Cached per-session permission decisions |
| `permission_store` | `Option<PermissionStore>` | Persistent Allow/Deny Always store |
| `extension_budget` | `Budget` | Structured concurrency timeout budget |

### `ExtensionRegion` (`src/extensions.rs`)

RAII guard wrapping `ExtensionManager` for structured concurrency. When
dropped, it shuts down the active extension runtime handle with a configurable
cleanup budget (default 5 seconds).

```rust
pub struct ExtensionRegion {
    manager: ExtensionManager,
    cleanup_budget: Duration,       // default 5s
    shutdown_done: AtomicBool,      // prevents double-shutdown
}
```

Usage: `AgentSession.extensions: Option<ExtensionRegion>`. Callers access
the inner manager via `region.manager()`.

### `JsExtensionLoadSpec` (`src/extensions.rs`)

Declarative specification for loading a JavaScript extension from disk:

- `extension_id` -- unique identifier (e.g. `ext.github_copilot`)
- `entry_path` -- canonical `PathBuf` to `.js`/`.ts` entry point
- `name`, `version`, `api_version` -- metadata from `extension.json`

Factory: `JsExtensionLoadSpec::from_entry_path(path)` parses the manifest
and canonicalizes the path.

### `NativeRustExtensionLoadSpec` (`src/extensions.rs`)

Declarative specification for loading a native descriptor extension:

- `extension_id` -- unique identifier (e.g. `ext.some_native_extension`)
- `entry_path` -- canonical `PathBuf` to `*.native.json`
- `name`, `version`, `api_version` -- metadata from `extension.json`

### `RegisterPayload` (`src/extensions.rs`)

Data returned by an extension's `activate()` call:

- `name`, `version`, `api_version` -- identity
- `capabilities: Vec<String>` -- requested capability tokens
- `capability_manifest: Option<CapabilityManifest>` -- structured capability declarations
- `tools`, `slash_commands`, `shortcuts`, `flags`, `event_hooks` -- registered features

## Hostcall Dispatch

Every `pi.*()` call from JavaScript enqueues a `HostcallRequest` on the
hostcall channel. The QuickJS thread blocks on the response.

### `HostcallKind` (`src/extensions_js.rs`)

```rust
pub enum HostcallKind {
    Tool { name: String },     // pi.tool(name, input)
    Exec { cmd: String },      // pi.exec(cmd, args)
    Http,                      // pi.http(request)
    Session { op: String },    // pi.session(op, args)
    Ui { op: String },         // pi.ui(op, args)
    Events { op: String },     // pi.events(op, args)
    Log,                       // pi.log(entry)
}
```

### Dispatch Flow

```
HostcallRequest
  │
  ▼
dispatch_hostcall_with_runtime()     [src/extensions.rs]
  ├── 1. Test interceptor check (short-circuit for mocking)
  ├── 2. Convert to canonical HostCallPayload
  ├── 3. Build HostCallContext (policy, tools, http, manager)
  ├── 4. dispatch_host_call_shared()  [src/extensions.rs]
  │       └── capability derivation + policy check
  └── 5. Kind-specific handler:
          ├── dispatch_hostcall_tool()     → ToolRegistry.execute()
          ├── dispatch_hostcall_exec()     → subprocess spawn + capture
          ├── dispatch_hostcall_http()     → HttpConnector.dispatch()
          ├── dispatch_hostcall_session()  → ExtensionSession trait methods
          ├── dispatch_hostcall_ui()       → mpsc channel to TUI
          ├── dispatch_hostcall_events()   → event hook registration
          └── dispatch_hostcall_log()      → structured log emission
```

### Session Operations

`dispatch_hostcall_session()` (`src/extensions.rs`) routes `op` values to
`ExtensionSession` trait methods:

| JS call                          | Session method                    |
|----------------------------------|-----------------------------------|
| `pi.session("getState")`         | `get_state()`                     |
| `pi.session("getMessages")`      | `get_messages()`                  |
| `pi.session("setName", name)`    | `set_name(name)`                  |
| `pi.session("appendMessage", m)` | `append_message(m)`               |
| `pi.session("setModel", p, m)`   | `set_model(provider, model_id)`   |
| `pi.session("getModel")`         | `get_model()`                     |
| `pi.session("setThinkingLevel")` | `set_thinking_level(level)`       |
| `pi.session("getThinkingLevel")` | `get_thinking_level()`            |
| `pi.session("setLabel", id, l)`  | `set_label(target_id, label)`     |

The `ExtensionSession` trait (`src/extensions.rs`) is implemented by:

- `SessionHandle` (`session.rs`) -- production session backed by SQLite
- `InteractiveExtensionSession` (`interactive.rs`) -- TUI interactive mode
- `NullSession` / `TestSession` (`extension_dispatcher.rs`) -- test doubles

### Event Operations

`dispatch_hostcall_events()` (`src/extensions.rs`) handles registration
API calls:

| JS call                              | Action                          |
|--------------------------------------|---------------------------------|
| `pi.events("registerTool", spec)`    | Add tool to extension's tools   |
| `pi.events("registerSlashCommand")`  | Add slash command               |
| `pi.events("registerShortcut")`      | Add keyboard shortcut           |
| `pi.events("registerFlag")`          | Add feature flag                |
| `pi.events("registerProvider")`      | Register custom LLM provider    |
| `pi.events("getActiveTools")`        | List enabled tool names         |
| `pi.events("getAllTools")`           | List all registered tools       |
| `pi.events("registerMessageRenderer")`| Register message renderer      |

## Capability Policy

### Policy Model (`src/extensions.rs`)

```rust
pub enum ExtensionPolicyMode {
    Strict,      // deny-by-default
    Prompt,      // ask user for unknown capabilities
    Permissive,  // allow all with audit logging
}

pub struct ExtensionPolicy {
    pub mode: ExtensionPolicyMode,
    pub max_memory_mb: u32,                              // default 256
    pub default_caps: Vec<String>,                       // auto-allowed
    pub deny_caps: Vec<String>,                          // always denied
    pub per_extension: HashMap<String, ExtensionOverride>,// per-ext overrides
}
```

Default policy (`Prompt` mode):
- **Allowed**: `read`, `write`, `http`, `events`, `session`
- **Denied**: `exec`, `env`

### Policy Profiles (`src/extensions.rs`)

| Profile      | Mode        | Allowed caps              | Denied caps   |
|-------------|-------------|---------------------------|---------------|
| `Safe`      | `Strict`    | read, write               | exec, env     |
| `Standard`  | `Prompt`    | read, write, http, events, session | exec, env |
| `Permissive`| `Permissive`| all                       | none          |

Configuration via `pi.toml`:
```toml
[extensions.policy]
profile = "safe"        # or "standard", "permissive"
allow_dangerous = false # override to allow exec/env
```

CLI override: `--extension-policy safe`

### Precedence Chain (`src/extensions.rs`)

Policy evaluation follows strict precedence:

1. **Per-extension deny** -- capability in extension override's `deny` list
2. **Global deny_caps** -- capability in global `deny_caps`
3. **Per-extension allow** -- capability in extension override's `allow` list
4. **Global default_caps** -- capability in `default_caps`
5. **Mode fallback** -- Strict: Deny, Prompt: Prompt, Permissive: Allow

Each layer either produces a terminal decision or defers to the next.

### Capability Mapping

Each `HostcallKind` maps to a required capability via
`required_capability_for_host_call()`:

| HostcallKind   | Required Capability |
|----------------|---------------------|
| `Tool`         | `tool`              |
| `Exec`         | `exec`              |
| `Http`         | `http`              |
| `Session`      | `session`           |
| `Ui`           | `ui`                |
| `Events`       | `events`            |
| `Log`          | `log`               |

## Trust Boundaries

```
┌─────────────────────────────────────────────────────────────┐
│                    Untrusted Zone                            │
│                                                             │
│   Extension JavaScript (QuickJS sandbox)                    │
│   - No direct filesystem access                            │
│   - No direct network access                               │
│   - No direct process spawning                             │
│   - Heap limited to max_memory_mb                          │
│                                                             │
├─────────────────── Hostcall Boundary ───────────────────────┤
│                                                             │
│   Policy Enforcement Layer                                  │
│   - Capability derivation from HostcallKind                │
│   - Policy evaluation (5-layer precedence)                 │
│   - Permission prompting (Prompt mode)                     │
│   - Audit logging (all modes)                              │
│                                                             │
├─────────────────── Host Dispatch ───────────────────────────┤
│                                                             │
│                    Trusted Zone                              │
│                                                             │
│   Tool execution, subprocess spawn, HTTP client,            │
│   session state, UI prompts, event hooks, logging           │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

Key security properties:

- **No ambient authority**: Extensions cannot bypass the hostcall channel.
  All dangerous operations require an explicit capability grant.
- **Fail-closed**: Unknown profiles resolve to `Safe`. Empty capability
  strings are denied. Missing session/manager yields error outcomes.
- **Per-extension isolation**: `ExtensionOverride` allows fine-grained
  allow/deny per extension ID without affecting others.
- **Prompt fatigue mitigation**: Batched prompts (250ms window),
  Allow/Deny Always persistence, and decision audit logging.
- **Weak reference cycle break**: `JsRuntimeHost` holds
  `Weak<Mutex<ExtensionManagerInner>>` to prevent Arc cycles between the
  manager and the JS thread.

## Runtime Lifecycle

### Loading

1. Discovery: scan `~/.pi/agent/extensions/` for `extension.json` manifests
2. Parse: `JsExtensionLoadSpec::from_entry_path(path)` validates manifest
3. QuickJS init: `PiJsRuntime` created with virtual modules + policy
4. Execute: extension's entry point runs, calls `pi.register(payload)`
5. Registration: `RegisterPayload` stored in `ExtensionManagerInner`

### Virtual Module System

Extensions `require()` Node/npm modules that are shimmed in QuickJS:

**Node built-ins**: `node:fs`, `node:path`, `node:os`, `node:crypto`,
`node:child_process`, `node:events`, `node:buffer`, `node:url`,
`node:http`, `node:net`, `node:readline`, `node:util`, `node:stream`

**npm/package shims and stubs**: `uuid`, bounded-HMAC `jsonwebtoken`,
`shell-quote`, `glob`, `chalk`, `chokidar`, `jsdom`, `turndown`,
`node-pty`, `@opentelemetry/*`,
`@xterm/*`, `vscode-languageserver-protocol`, `@sinclair/typebox`,
`@mariozechner/pi-ai`

**Pi SDK**: `@mariozechner/pi-coding-agent` (provides `keyHint`,
`compact`, `completeSimple`, `fuzzyMatch`, `fuzzyFilter`)

### Shutdown

1. `ExtensionRegion` dropped (session ends)
2. `JsRuntimeCommand::Shutdown` sent to QuickJS thread
3. QuickJS thread exits within cleanup budget (default 5s)
4. If budget exceeded, thread is abandoned (no force-kill, relies on
   process exit)

### Structured Concurrency

- `ExtensionRegion` guarantees cleanup on all exit paths (normal, panic,
  early return)
- `Budget` tracks remaining time for extension operations
- `effective_timeout()` intersects manager budget with per-operation
  timeout
- Cancellation propagates through the hostcall channel

## Provider Extension (`streamSimple`)

Extensions can register custom LLM providers via
`pi.events("registerProvider", spec)`. The provider implements
`streamSimple(model, context, options)` returning `AsyncIterable<string>`.

Extensions that follow the `@mariozechner/pi-ai/compat` API can instead call
`registerApiProvider({ api, stream, streamSimple }, sourceId)` before they call
`pi.registerProvider`. The runtime keeps that API provider scoped to the
registering extension, exposes it through `getApiProvider` and
`getApiProviders`, and binds its `streamSimple` handler to matching provider
models. `unregisterApiProviders(sourceId)` removes only entries owned by the
calling extension. This lets compatibility providers supply a protocol handler
without granting another extension access to it.

Rust side: `ExtensionStreamSimpleProvider` in `src/providers/mod.rs`
implements the `Provider` trait. Each chunk from JS becomes a
`StreamEvent::TextDelta`. Cancellation is via `Drop` on the stream state.

OAuth support is available via `OAuthConfig` on `ModelEntry` for providers
requiring token-based auth.

## Test Architecture

| Layer          | Infrastructure                     | Location                       |
|----------------|------------------------------------|---------------------------------|
| Unit tests     | Direct struct/function tests       | `tests/extensions_*.rs`         |
| VCR tests      | HTTP interaction playback          | `tests/provider_*.rs`           |
| Conformance    | Differential oracle (TS vs Rust)   | `tests/ext_conformance_*.rs`    |
| E2E            | Full CLI + tmux scripting          | `tests/e2e_*.rs`                |
| Property       | proptest random inputs             | `tests/ext_proptest.rs`         |
| Stress         | Concurrent load + memory profiling | `tests/extensions_stress.rs`    |
| Security       | FS escape, policy negative tests   | `tests/security_*.rs`           |

The crate-local characterization suite is routed by
`src/extensions/tests.rs` and split by behavior under
`src/extensions/tests/`: `core`, `registration`, `baseline`, `risk_math`,
`enforcement`, `policy_transition`, `exec_security`, `event_timeouts`,
`concurrency`, `reactor`, `shared_dispatch`, `runtime_parity`, `ui_protocol`,
and `security_alerts`. Keeping those names as test-path domains makes a focused
command such as `cargo test extensions::tests::reactor` stable without putting
the full suite back into the production façade.

Test interceptor: `HostcallInterceptor` trait allows test code to
short-circuit hostcall dispatch, returning predetermined outcomes without
touching real tools, network, or filesystem.

## File Map

| File | Responsibility |
|------|----------------|
| `src/extensions.rs` | Stable public façade; public type definitions, manager state, and shared dispatch entry points |
| `src/extensions/extension_manager_impl.rs` | `ExtensionManager` lifecycle, policy, registry, and event-orchestration implementation |
| `src/extensions/protocol.rs` | Versioned message validation plus hostcall reactor, marshalling, and opcode internals |
| `src/extensions/fs_connector.rs` | Capability-scoped filesystem connector implementation |
| `src/extensions/exec_mediation.rs` | Dangerous-command classification and secret-broker policy implementation |
| `src/extensions/permission_drift.rs` | Permission snapshot drift classification and evidence |
| `src/extensions/event_coalescer_impl.rs` | Coalesced event-dispatch implementation |
| `src/extensions/native_runtime_duplicate_scaffold.rs` | Active deterministic native descriptor runtime |
| `src/extensions/native_runtime_experimental.rs` | Retained, compile-disabled native runtime prototype |
| `src/extensions/wasm_host.rs` | Feature-gated Wasmtime component host and its conformance tests |
| `src/extensions/compatibility.rs` | Extension compatibility scanner |
| `src/extensions/policy_snapshot_tests.rs` | Compiled-policy snapshot characterization tests |
| `src/extensions/tests.rs`, `src/extensions/tests/` | Test router and behavior-domain unit/characterization suites |
| `src/extensions_js.rs` | QuickJS runtime, virtual modules, and `HostcallKind` |
| `src/extension_dispatcher.rs` | `ExtensionSession` implementations and dispatcher integration |
| `src/config.rs` | `ExtensionPolicyConfig` and resolved policy |
| `src/providers/mod.rs` | `ExtensionStreamSimpleProvider` |
| `src/connectors/mod.rs`, `src/connectors/http.rs` | Shared connector façade and `HttpConnector` implementation |
| `src/auth.rs` | OAuth token management for extension providers |
