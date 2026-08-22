# defims/picrab ↔ earendil-works/pi Alignment

> **English (default)** · 中文版: [sdk-mapping.zh-CN.md](./sdk-mapping.zh-CN.md)
>
> `defims/picrab` is the Rust version of the pi engine ([earendil-works/pi](https://github.com/earendil-works/pi),
> TypeScript SDK `@earendil-works/pi-coding-agent`), continuously tracking upstream.
>
> This document records the `AgentSession` interface differences between the two, serving as
> the baseline for filling missing in_process methods and detecting changes when tracking
> upstream releases.
>
> - **Alignment target**: earendil-works/pi's `AgentSessionLike` interface (agegr/pi-web `lib/pi-types.ts`)
> - **Aligned object**: defims/picrab's `AgentSessionHandle` (in_process path, `src/sdk.rs`)
> - **Rust crate lib name**: `pi` (Cargo package name `pi_agent_rust`)

## Core rules

- **Docs default to English**: the canonical copy is English (`X.md`); the Chinese version lives at `X.zh-CN.md`, cross-linked from the file header; update both copies together on any change.
- **Commits in English**: git commit messages in this repository are always written in English.

## Fork core requirements

The consumer (moho-mate) embeds the engine in-process (in_process path). Three core
requirements the fork must uphold while filling gaps / tracking upstream:

1. **Parallel-capable** — multiple `AgentSessionHandle` instances run concurrently (multi-session); no global lock or shared mutable state between handles may serialize them.
2. **Async parallelism** — concurrency is expressed as async tasks (the engine runs on the asupersync runtime, built by the caller) instead of blocking OS threads; events flow out through `on_event`/`subscribe` callbacks.
3. **Async throughout** — the whole surface is `async fn`, mirroring the TS SDK's Promise-returning shape; long operations (prompt/bash/compact) are cancellable at any time via `AbortHandle`/`Signal` and never block the caller.

## Status legend

- ✅ **Aligned** — present on both sides, same semantics (parameter shapes may differ)
- 🟡 **Different shape** — semantics match, but calling convention/params/return types differ (language-paradigm differences)
- ❌ **Gap** — the TS SDK has it, Rust does not (to fill)
- ⏭️ **Architectural difference** — extensions/custom-ui system; not implemented in Rust

---

## 1. Session core (🟡 partially aligned)

| TS SDK (`AgentSessionLike`) | Rust (`AgentSessionHandle`) | Notes |
|---|---|---|
| `prompt(text, options?)` | `prompt(input, on_event)` / `prompt_with_abort(input, signal, on_event)` | 🟡 Rust splits into with/without-abort variants; options (images/streamingBehavior) go through SessionOptions |
| `abort()` | `new_abort_handle()` + `AbortHandle.abort()` | 🟡 Rust requires a pre-built AbortHandle+Signal |
| `subscribe(listener)` | `subscribe(listener)` / `unsubscribe(id)` | ✅ Rust adds an explicit unsubscribe |
| `dispose()` | `into_inner()` | 🟡 Rust consumes the handle to take the inner session |
| `reload(options?)` | `continue_turn(on_event)` / `continue_turn_with_abort(...)` | 🟡 not fully equivalent |
| `setModel(model)` | `set_model(provider, model_id)` | 🟡 TS takes an object; Rust splits provider+model_id |
| `model` (readonly) | `model() -> (String, String)` | ✅ |
| `setThinkingLevel(level)` | `set_thinking_level(level)` | ✅ |
| `setSessionName(name)` | `set_session_name(name)` | ✅ |
| `compact(customInstructions?)` | `compact(on_event)` / `compact_with_instructions(instructions, on_event)` | ✅ fork added the instructions variant |
| `steer(text, images?)` | none on the handle (RPC path only: `RpcTransportClient::steer`) | ❌ |
| `followUp(text, images?)` | none on the handle (RPC path only) | ❌ |
| `sessionId` (readonly) | `session().session.lock().header.id` | 🟡 needs an async lock |
| `sessionFile` (readonly) | `session().session.lock()` reads the file path | 🟡 |
| `isStreaming` (readonly) | none (`AgentSessionState` has no such field; must be tracked via events) | ❌ |
| `isCompacting` (readonly) | none (`AgentSessionState` has no such field) | ❌ |

## 2. Stats / bash / compaction (✅ filled by the fork)

| TS SDK | Rust | Notes |
|---|---|---|
| `getSessionStats()` | `get_session_stats()` | ✅ fork commit e178fb48 |
| `getLastAssistantText()` | `get_last_assistant_text()` | ✅ fork commit e178fb48 |
| `setAutoCompactionEnabled(b)` | `set_auto_compaction(b)` | ✅ fork commit e178fb48 |
| `compact(customInstructions?)` | `compact_with_instructions(...)` | ✅ fork commit e178fb48 |
| `executeBash(cmd, onChunk?, opts?)` | `bash(cmd, abort_rx)` | 🟡 fork commit 52d39dc3; different param shape (abort via oneshot, no onChunk) |
| `autoCompactionEnabled` (readonly) | none (`AgentSessionState` has no such field; set-only) | ❌ missing getter |

## 3. Queue management (❌ gap)

The TS SDK exposes reading + clearing of the (steering/followUp) queue; pi's internal queue is private in Rust.

| TS SDK | Rust | Notes |
|---|---|---|
| `clearQueue()` | none (pi's internal queue is private) | ❌ moho-mate clears via a LoopState mirror |
| `getSteeringMessages()` | none | ❌ |
| `getFollowUpMessages()` | none | ❌ |
| `pendingMessageCount` (readonly) | none | ❌ |

## 4. Tool management (❌ gap)

Rust has a `ToolRegistry`, but no query/set methods are exposed on the handle.

| TS SDK | Rust | Notes |
|---|---|---|
| `getAllTools()` | no handle method (the underlying ToolRegistry has it) | ❌ |
| `getActiveToolNames()` | none | ❌ |
| `setActiveToolsByName(names)` | none (moho-mate approximates via set_system_prompt) | ❌ PARTIAL |

## 5. Navigation (❌ gap)

| TS SDK | Rust | Notes |
|---|---|---|
| `navigateTree(targetId, opts?)` | no handle method (the underlying Session.navigate_to exists) | ❌ moho-mate goes through session_mut().session.lock().navigate_to |

## 6. Bash helper state (❌ gap)

| TS SDK | Rust | Notes |
|---|---|---|
| `abortBash()` | none (bash abort is managed by the caller via a oneshot channel) | 🟡 different mechanism |
| `isBashRunning` (readonly) | none | ❌ missing state query |

## 7. Compaction / retry / context (❌ gap)

| TS SDK | Rust | Notes |
|---|---|---|
| `abortCompaction()` | none | ❌ |
| `setAutoRetryEnabled(b)` | none | ❌ retry state is RPC-subprocess-only (RpcSharedState) |
| `autoRetryEnabled` (readonly) | none | ❌ same |
| `getContextUsage()` | none | ❌ missing context usage query |

## 7.5. SessionManager (✅ aligned)

pi-web-rust wires this via `pi::sdk::SessionIndex` + `pi::sdk::SessionMeta` + `pi::sdk::build_session_context`.

| TS SDK | Rust | Status |
|---|---|---|
| `SessionManager.listAll()` → `SessionInfo[]` | `SessionIndex::new().list_sessions(None)` → `Vec<SessionMeta>` | ✅ SessionMeta carries first_message/parent_session_path/modified_ms |
| `SessionManager.open(path).getEntries()` | `Session::open(path)` + public `session.entries` field | ✅ |
| `buildSessionContext(entries, leafId, byId)` | `pi::sdk::build_session_context(entries, leaf_id, by_id)` → `SessionContextSnapshot` | ✅ free function (leaf→root via parentId + compaction cut-off) |
| `resolveModelScopeWithDiagnostics(patterns, modelRuntime)` | `pi::sdk::resolve_model_scope_with_diagnostics(patterns, registry, allow_missing)` → `(Vec<ScopedModel>, Vec<String>)` | ✅ returns structured diagnostics |
| `getAgentDir()` | `Config::global_dir()` | ✅ |

## 8. Settings / model runtime (✅ partially aligned)

| TS SDK | Rust | Status |
|---|---|---|
| `settingsManager` (readonly) | none | ❌ missing settings manager |
| `modelRuntime` (readonly) | `agent.model_registry()` / `agent.auth_storage()` | ✅ pub getters added |

## 9. Extension system (✅ wired 2026-08-20/22)

The TS SDK embeds DefaultResourceLoader (skills/extensions auto-load); the Rust port
left it in the CLI layer — our fork closed the gap from both ends (web wire + SDK
auto-load), and the custom-UI poll protocol made `extension_ui_input` a thin wrapper
over `respond_ui`.

| TS SDK | Rust | Status |
|---|---|---|
| `extensionRunner` (readonly) | `extension_manager()` / `has_extensions()` | ✅ wired (UI channel + tools/commands RPC live in picrab-web) |
| `promptTemplates` (readonly) | `get_commands()` 三源之一(load_prompt_templates) | ✅ |
| `resourceLoader` (readonly) | auto-load:skills 四源(`f74dd3a8`)+ 扩展自动发现(`88abc5f1`, `SessionOptions::no_extensions`) | ✅ |
| `bindExtensions?` | `enable_extensions_with_policy`(显式路径注入面) | ✅ |

---

## Handle-only extras (not in AgentSessionLike)

The in_process handle also exposes Rust-side additions with no TS SDK counterpart:
`messages()`, `state()`, `thinking()` / `thinking_level()`, `max_tokens()` / `set_max_tokens()`,
`listeners()` / `listeners_mut()`, `session()` / `session_mut()`, `extension_manager()` /
`has_extensions()` / `extension_region()`, `from_session_with_listeners()`. They mirror RPC
names or serve Rust-native wiring; additive, not counted in alignment.

## Alignment progress

| Category | Total | ✅ aligned | ❌ gap |
|---|---|---|---|
| Session core | 16 | 14 | 2 (isStreaming / isCompacting — snap 侧已有,SDK getter 待加) |
| Stats/bash/compaction | 6 | 6 | 0 (compact 现返 CompactionResultInfo 统计) |
| Queue management | 4 | 3 | 1 (set_steering_mode/set_follow_up_mode 已有;queue 查询 getter 待加) |
| Tool management | 3 | 2 | 1 (`Agent::tools()` + extension_tool_defs;`setTools` 走 rpc 面) |
| Navigation | 1 | 1 | 0 (`fork`/`switch_session`) |
| Bash helpers | 2 | 2 | 0 (`bash`/`abort_bash`) |
| Compaction/retry/context | 4 | 4 | 0 (`set_auto_retry`/`set_auto_compaction`/`abort_retry`/`continue_turn`) |
| Settings/model | 2 | 2 | 0 |
| Extensions | 4 | 4 | 0 |
| **Total** | **42** | **38** | **4** |

**Aligned rate: 90%** (38/42;剩余 4 项为 getter 型小面,均有运行时等价物在 picrab-web snap 层)。

## Fork fill log (defims/picrab)

| commit | methods | TS SDK aligned |
| `f74dd3a8` | SessionOptions::skills + auto-load | resourceLoader (skills half) |
| `0f6d283a` | compact → CompactionResultInfo{tokensBefore/estimatedTokensAfter/...}; AgentSession::shutdown(flush+扩展停) | compact stats / dispose semantics |
| `88abc5f1` | SessionOptions::no_extensions + discover_extensions_blocking 自动装配 | resourceLoader (extensions half) |
| `213b7c80` | Agent::tools() | getTools 面 |
| `7a2a023d` | (session_index) 索引快照 first_message/modified 真值 | SessionManager.listAll 元数据 |
|---|---|---|
| `e178fb48` | get_session_stats / get_last_assistant_text / set_auto_compaction / compact_with_instructions | getSessionStats / getLastAssistantText / setAutoCompactionEnabled / compact |
| `52d39dc3` | bash | executeBash |
| `2595adae` | (macOS type fixes, not a method fill) | — |
| `baee8763` | (removed RPC-only methods absent from AgentSessionLike: get_available_models / set_steering_mode / set_follow_up_mode / get_messages / get_state; steer/follow_up remain RPC-only) | — |
| `86e8cac6` | SessionMeta extension (first_message/parent_session_path/modified_ms) + build_session_context free function + sdk exports SessionIndex/SessionMeta | SessionManager.listAll / buildSessionContext |
| `adfdc96c` | resolve_model_scope_with_diagnostics + AgentSession model_registry()/auth_storage() getters | resolveModelScopeWithDiagnostics / modelRuntime readonly |
| `36fdac58` | createAgentSessionServices / FromServices split | createAgentSessionServices / createAgentSessionFromServices |

## Upstream tracking flow

```bash
# 1. When earendil-works/pi cuts a release, update the AgentSessionLike interface
#    and check §1-9 of this document for added/changed methods

# 2. Fill the gaps in this repository (defims/picrab)
# add methods to the AgentSessionHandle impl block in src/sdk.rs
cargo check --lib  # verify

# 3. push the fork
git push origin master:main

# 4. consumer moho-mate (host repo) updates the submodule + verifies
cd <moho-mate root>
git add pi-agent-rust
cargo check
```

## Related files

- TS SDK interface: upstream agegr/pi-web `lib/pi-types.ts` (`AgentSessionLike`; a copy exists in the consumer moho-mate's pi-web-rust submodule)
- Rust handle: `src/sdk.rs` (`AgentSessionHandle` impl)
- Rust engine core: `src/agent.rs` (`AgentSession`)
- RPC path (reference): `src/rpc.rs` (`RpcTransportClient`)
- Detailed signatures: consumer moho-mate's `docs/pi-sdk-probe-notes.md`
- 中文版: [sdk-mapping.zh-CN.md](./sdk-mapping.zh-CN.md)
