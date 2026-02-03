# Feature Parity: pi_agent_rust vs Pi Agent (TypeScript)

> **Purpose:** Authoritative single-source-of-truth for implementation status.
> **Last Updated:** 2026-02-03 (RPC mode parity + session stats; clippy/tests green)

## Status Legend

| Status | Meaning |
|--------|---------|
| ✅ Implemented | Feature exists, covered by tests |
| 🔶 Partial | Some functionality present, known gaps remain |
| ❌ Missing | In scope but not yet implemented |
| ⬜ Out of Scope | Intentionally excluded from this port |

---

## Executive Summary

| Category | Implemented | Partial | Missing | Out of Scope | Total |
|----------|-------------|---------|---------|--------------|-------|
| **Core Types** | 8 | 0 | 0 | 0 | 8 |
| **Provider Layer** | 18 | 0 | 0 | 9 | 27 |
| **Tools (7 total)** | 7 | 0 | 0 | 0 | 7 |
| **Agent Runtime** | 7 | 0 | 0 | 0 | 7 |
| **Session Management** | 10 | 0 | 0 | 0 | 10 |
| **CLI** | 10 | 0 | 0 | 0 | 10 |
| **Resources & Customization** | 6 | 0 | 2 | 0 | 8 |
| **TUI** | 18 | 0 | 0 | 2 | 20 |
| **Configuration** | 2 | 0 | 0 | 0 | 2 |
| **Authentication** | 6 | 1 | 1 | 0 | 8 |

---

## 1. Core Types (Message/Content/Usage)

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| Message union (User/Assistant/ToolResult) | ✅ | `src/model.rs:13-19` | Unit | Complete enum with serde |
| UserMessage | ✅ | `src/model.rs:22-27` | Unit | Text or Blocks content |
| AssistantMessage | ✅ | `src/model.rs:38-50` | Unit | Full metadata |
| ToolResultMessage | ✅ | `src/model.rs:53-63` | Unit | Error flag, details |
| ContentBlock enum | ✅ | `src/model.rs:86-93` | Unit | Text/Thinking/Image/ToolCall |
| StopReason enum | ✅ | `src/model.rs:70-79` | Unit | All 5 variants |
| Usage tracking | ✅ | `src/model.rs:145-166` | Unit | Input/output/cache/cost |
| StreamEvent enum | ✅ | `src/model.rs:172-232` | Unit | All 12 event types |

---

## 2. Provider Layer

### 2.1 Provider Trait

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| Provider trait definition | ✅ | `src/provider.rs:18-31` | - | async_trait based |
| Context struct | ✅ | `src/provider.rs:38-43` | - | System prompt + messages + tools |
| StreamOptions | ✅ | `src/provider.rs:62-72` | - | Temperature, max_tokens, thinking |
| ToolDef struct | ✅ | `src/provider.rs:49-55` | - | JSON Schema parameters |
| Model definition | ✅ | `src/provider.rs:108-121` | - | Cost, context window, etc. |
| ThinkingLevel enum | ✅ | `src/model.rs:239-265` | Unit | 6 levels with budgets |
| CacheRetention enum | ✅ | `src/provider.rs:75-81` | - | None/Short/Long |

### 2.2 Provider Implementations

| Provider | Status | Rust Location | Tests | Notes |
|----------|--------|---------------|-------|-------|
| **Anthropic** | ✅ | `src/providers/anthropic.rs` | Unit | Full streaming + thinking + tools |
| **OpenAI** | ✅ | `src/providers/openai.rs` | Unit | Full streaming + tool use |
| **Google Gemini** | ✅ | `src/providers/gemini.rs` | 4 | Full streaming + tool use |
| **Azure OpenAI** | ✅ | `src/providers/azure.rs` | 4 | Full streaming + tool use |
| Amazon Bedrock | ⬜ | - | - | Low priority |
| Google Vertex | ⬜ | - | - | Low priority |
| GitHub Copilot | ⬜ | - | - | OAuth complexity |
| XAI | ⬜ | - | - | Low priority |
| Groq | ⬜ | - | - | Low priority |
| Cerebras | ⬜ | - | - | Low priority |
| OpenRouter | ⬜ | - | - | Low priority |
| Mistral | ⬜ | - | - | Low priority |
| Custom providers | ⬜ | - | - | Defer |

### 2.3 Streaming Implementation

| Feature | Status | Location | Notes |
|---------|--------|----------|-------|
| SSE parsing (Anthropic) | ✅ | `anthropic.rs` | reqwest-eventsource |
| SSE parser module | ✅ | `src/sse.rs` | Custom parser for asupersync migration |
| Text delta streaming | ✅ | `anthropic.rs:339-352` | Real-time text |
| Thinking delta streaming | ✅ | `anthropic.rs:354-367` | Extended thinking |
| Tool call streaming | ✅ | `anthropic.rs:368-382` | JSON accumulation |
| Usage updates | ✅ | `anthropic.rs:430-448` | Token counts |
| Error event handling | ✅ | `anthropic.rs:258-266` | API errors |

---

## 3. Built-in Tools

| Tool | Status | Rust Location | Tests | Conformance Tests |
|------|--------|---------------|-------|-------------------|
| **read** | ✅ | `src/tools.rs` | 4 | ✅ test_read_* |
| **bash** | ✅ | `src/tools.rs` | 3 | ✅ test_bash_* |
| **edit** | ✅ | `src/tools.rs` | 3 | ✅ test_edit_* |
| **write** | ✅ | `src/tools.rs` | 2 | ✅ test_write_* |
| **grep** | ✅ | `src/tools.rs` | 3 | ✅ test_grep_* |
| **find** | ✅ | `src/tools.rs` | 2 | ✅ test_find_* |
| **ls** | ✅ | `src/tools.rs` | 3 | ✅ test_ls_* |

### 3.1 Tool Feature Details

| Feature | read | bash | edit | write | grep | find | ls |
|---------|------|------|------|-------|------|------|-----|
| Basic operation | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Truncation (head/tail) | ✅ | ✅ | - | - | ✅ | ✅ | ✅ |
| Image support | ✅ | - | - | - | - | - | - |
| Streaming updates | - | ✅ | - | - | - | - | - |
| Line numbers | ✅ | - | - | - | ✅ | - | - |
| Fuzzy matching | - | - | ✅ | - | - | - | - |
| Path resolution | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| ~ expansion | ✅ | - | ✅ | ✅ | ✅ | ✅ | ✅ |
| macOS screenshot paths | ✅ | - | - | - | - | - | - |

### 3.2 Truncation Constants

| Constant | Value | Used By |
|----------|-------|---------|
| DEFAULT_MAX_LINES | 2000 | read, bash, grep |
| DEFAULT_MAX_BYTES | 50KB | read, bash, grep, find, ls |
| GREP_MAX_LINE_LENGTH | 500 | grep |

---

## 4. Agent Runtime

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| Agent struct | ✅ | `src/agent.rs` | Unit | Provider + tools + config |
| Agent loop | ✅ | `src/agent.rs` | - | Tool iteration limit |
| Tool execution | ✅ | `src/agent.rs` | Unit | Error handling |
| Event callbacks | ✅ | `src/agent.rs` | - | 9 event types |
| Stream processing | ✅ | `src/agent.rs` | - | Delta handling |
| Context building | ✅ | `src/agent.rs` | - | System + history + tools |
| Abort handling | ✅ | `src/agent.rs`, `src/main.rs`, `src/interactive.rs` | - | Ctrl+C cancels in-flight requests |

---

## 5. Session Management

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| Session struct | ✅ | `src/session.rs` | - | Header + entries + path |
| SessionHeader | ✅ | `src/session.rs` | - | Version 3 |
| JSONL persistence | ✅ | `src/session.rs` | - | Save/load |
| Entry types (7) | ✅ | `src/session.rs` | - | Message, ModelChange, etc. |
| Tree structure | ✅ | `src/session.rs` | 7 | Full parent/child navigation |
| CWD encoding | ✅ | `src/session.rs` | 1 | Session directory naming |
| Entry ID generation | ✅ | `src/session.rs` | - | 8-char hex |
| Continue previous | ✅ | `src/session.rs` | - | Most recent by mtime |
| Session picker UI | ✅ | `src/session_picker.rs` | 3 | TUI picker with bubbletea |
| Branching/navigation | ✅ | `src/session.rs` | 7 | navigate_to, create_branch_from, list_leaves, branch_summary |

---

## 6. CLI

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| Argument parsing | ✅ | `src/cli.rs` | - | Clap derive |
| Subcommands | ✅ | `src/cli.rs`, `src/main.rs` | - | Install, Remove, Update, List, Config |
| @file arguments | ✅ | `src/cli.rs` | - | File inclusion |
| Message arguments | ✅ | `src/cli.rs` | - | Positional text |
| Tool selection | ✅ | `src/cli.rs` | - | --tools flag |
| Model listing | ✅ | `src/main.rs` | - | Table output |
| Session export | ✅ | `src/main.rs` | - | HTML export |
| Print mode | ✅ | `src/main.rs` | - | Single-shot mode |
| RPC mode | ✅ | `src/main.rs`, `src/rpc.rs` | `tests/rpc_mode.rs` | Headless stdin/stdout JSON protocol (prompt/steer/follow_up/state/stats/model/thinking/compact/bash/fork) |
| Package management | ✅ | `src/package_manager.rs`, `src/main.rs` | Unit | install/remove/update/list + settings updates + startup auto-install + resource resolution |

---

## 6A. Resources & Customization

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| Skills loader + validation | ✅ | `src/resources.rs` | Unit | Agent Skills frontmatter + diagnostics |
| Skills prompt inclusion | ✅ | `src/main.rs` | Unit | Appends `<available_skills>` if `read` tool enabled |
| Skill command expansion (`/skill:name`) | ✅ | `src/resources.rs`, `src/interactive.rs` | Unit | Expands to `<skill ...>` block |
| Prompt template loader | ✅ | `src/resources.rs` | Unit | Global/project + explicit paths |
| Prompt template expansion (`/name args`) | ✅ | `src/resources.rs`, `src/interactive.rs` | Unit | `$1`, `$@`, `$ARGUMENTS`, `${@:N}` |
| Package resource discovery | ✅ | `src/resources.rs` | Unit | Reads `package.json` `pi` field or defaults |
| Extension discovery/runtime | ❌ | `src/extensions.rs` | - | Protocol scaffold only (see `EXTENSIONS.md` for connector + event loop design) |
| Themes discovery/hot reload | ❌ | - | - | Not yet implemented |

---

## 7. Configuration

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| Config loading | ✅ | `src/config.rs` | - | Global + project merge |
| Settings struct | ✅ | `src/config.rs` | - | All fields optional |
| Default accessors | ✅ | `src/config.rs` | - | Fallback values |
| Compaction settings | ✅ | `src/config.rs` | - | enabled, reserve, keep |
| Retry settings | ✅ | `src/config.rs` | - | enabled, max, delays |
| Image settings | ✅ | `src/config.rs` | - | auto_resize, block |
| Terminal settings | ✅ | `src/config.rs` | - | show_images, clear |
| Thinking budgets | ✅ | `src/config.rs` | - | Per-level overrides |
| Environment variables | ✅ | `src/config.rs` | - | PI_CONFIG_PATH/PI_CODING_AGENT_DIR/PI_PACKAGE_DIR/PI_SESSIONS_DIR + provider API keys |

---

## 8. Terminal UI

### 8.1 Non-Interactive Output (rich_rust)

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| PiConsole wrapper | ✅ | `src/tui.rs` | 3 | rich_rust integration |
| Styled output (markup) | ✅ | `src/tui.rs` | - | Colors, bold, dim |
| Agent event rendering | ✅ | `src/tui.rs` | - | Text, thinking, tools, errors |
| Table rendering | ✅ | `src/tui.rs` | - | Via rich_rust Tables |
| Panel rendering | ✅ | `src/tui.rs` | - | Via rich_rust Panels |
| Rule rendering | ✅ | `src/tui.rs` | - | Horizontal dividers |
| Spinner styles | ✅ | `src/tui.rs` | 1 | Dots, line, simple |

### 8.2 Interactive TUI (charmed_rust/bubbletea)

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| PiApp Model | ✅ | `src/interactive.rs` | 2 | Elm Architecture |
| TextInput with history | ✅ | `src/interactive.rs` | - | bubbles TextInput |
| Markdown rendering | ✅ | `src/interactive.rs` | - | glamour Dark style |
| Token/cost footer | ✅ | `src/interactive.rs` | - | Usage tracking |
| Spinner animation | ✅ | `src/interactive.rs` | - | bubbles spinner |
| Tool status display | ✅ | `src/interactive.rs` | - | Running tool indicator |
| Keyboard navigation | ✅ | `src/interactive.rs` | - | Up/Down history, Esc quit |
| Agent integration | ✅ | `src/interactive.rs` | - | Agent events wired; CLI interactive uses PiApp |
| Multi-line editor | ✅ | `src/interactive.rs` | - | TextArea with line wrapping |
| Slash command system | ✅ | `src/interactive.rs` | - | /help, /login, /logout, /clear, /model, /thinking, /exit, /history, /export, /session, /resume, /new, /copy, /name, /hotkeys |
| Viewport scrolling | ✅ | `src/interactive.rs` | - | Viewport with scroll_to_bottom() |
| Image display | ⬜ | - | - | Terminal dependent |
| Autocomplete | ⬜ | - | - | Defer |

### 8.3 Interactive Commands (Slash)

| Command | Status | Rust Location | Notes |
|---------|--------|---------------|-------|
| `/help` | ✅ | `src/interactive.rs` | Help text |
| `/clear` | ✅ | `src/interactive.rs` | Clears in-memory conversation view |
| `/model` | ✅ | `src/interactive.rs` | Switch model/provider |
| `/thinking` | ✅ | `src/interactive.rs` | Set thinking level |
| `/history` | ✅ | `src/interactive.rs` | Show input history |
| `/export` | ✅ | `src/interactive.rs` | Export session to HTML |
| `/exit` / `/quit` | ✅ | `src/interactive.rs` | Exit Pi |
| `/login` | 🔶 | `src/interactive.rs`, `src/auth.rs` | OAuth login (Anthropic supported; others pending) |
| `/logout` | ✅ | `src/interactive.rs`, `src/auth.rs` | Remove stored credentials |
| `/session` | ✅ | `src/interactive.rs` | Show session info (path/tokens/cost) |
| `/resume` | 🔶 | `src/interactive.rs` | Shows hint to use --resume flag |
| `/new` | 🔶 | `src/interactive.rs` | Shows hint to restart Pi |
| `/name <name>` | ✅ | `src/interactive.rs` | Set session display name |
| `/copy` | 🔶 | `src/interactive.rs` | Clipboard feature not enabled (placeholder) |
| `/hotkeys` | ✅ | `src/interactive.rs` | Show keybindings |
| `/scoped-models` | 🔶 | `src/interactive.rs` | Scoped list stored; cycling keybind pending |
| `/settings` | 🔶 | `src/interactive.rs` | Shows merged settings JSON (no editor UI) |
| `/tree` | ✅ | `src/interactive.rs` | List leaves and switch branch by id/index |
| `/fork` | ✅ | `src/interactive.rs` | Forks new session file from user message |
| `/compact [prompt]` | ✅ | `src/interactive.rs`, `src/compaction.rs` | Manual compaction |
| `/share` | 🔶 | `src/interactive.rs` | Saves HTML to temp file (no remote share) |
| `/reload` | 🔶 | `src/interactive.rs`, `src/resources.rs` | Reloads skills/prompts (themes/extensions pending) |
| `/changelog` | ✅ | `src/interactive.rs` | Display changelog entries |

---

## 9. Authentication

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| API key from env | ✅ | `src/auth.rs` | - | ANTHROPIC_API_KEY, etc. |
| API key from flag | ✅ | `src/main.rs` | - | --api-key |
| auth.json storage | ✅ | `src/auth.rs` | - | File with 0600 perms |
| File locking | ✅ | `src/auth.rs` | - | Exclusive lock with timeout |
| Key resolution | ✅ | `src/auth.rs` | - | override > auth.json > env |
| Multi-provider keys | ✅ | `src/auth.rs` | - | 12 providers supported |
| OAuth flow | 🔶 | `src/auth.rs`, `src/interactive.rs` | - | `/login` supports Anthropic OAuth (others pending) |
| Token refresh | 🔶 | `src/auth.rs`, `src/main.rs` | - | Auto-refresh expired Anthropic OAuth tokens at startup |

---

## 10. Error Handling

| Feature | Status | Rust Location | Tests | Notes |
|---------|--------|---------------|-------|-------|
| Error enum | ✅ | `src/error.rs` | - | thiserror based |
| Config errors | ✅ | `src/error.rs` | - | |
| Session errors | ✅ | `src/error.rs` | - | Including NotFound |
| Provider errors | ✅ | `src/error.rs` | - | Provider + message |
| Auth errors | ✅ | `src/error.rs` | - | |
| Tool errors | ✅ | `src/error.rs` | - | Tool name + message |
| Validation errors | ✅ | `src/error.rs` | - | |
| IO/JSON/HTTP errors | ✅ | `src/error.rs` | - | From impls |

---

## Test Coverage Summary

| Category | Unit Tests | Integration Tests | Fixture Cases | Total |
|----------|------------|-------------------|---------------|-------|
| Core types | 4 | 0 | 0 | 4 |
| Provider (Anthropic) | 2 | 0 | 0 | 2 |
| Provider (OpenAI) | 3 | 0 | 0 | 3 |
| Provider (Gemini) | 4 | 0 | 0 | 4 |
| Provider (Azure) | 4 | 0 | 0 | 4 |
| SSE parser | 11 | 0 | 0 | 11 |
| Tools | 5 | 20 | 122 | 147 |
| TUI (rich_rust) | 3 | 0 | 0 | 3 |
| TUI (interactive) | 2 | 0 | 0 | 2 |
| TUI (session picker) | 3 | 0 | 0 | 3 |
| Session (branching) | 7 | 0 | 0 | 7 |
| Agent | 2 | 0 | 0 | 2 |
| Conformance infra | 6 | 0 | 0 | 6 |
| Extensions | 2 | 0 | 0 | 2 |
| **Total** | **56** | **20** | **122** | **198** |

**All tests pass** (56 unit + 15 fixture wrappers + 20 integration)

---

## Conformance Testing Status

| Component | Has Fixture Tests | Fixture File | Cases | Status |
|-----------|-------------------|--------------|-------|--------|
| read tool | ✅ Yes | `read_tool.json` | 23 | ✅ All pass |
| write tool | ✅ Yes | `write_tool.json` | 7 | ✅ All pass |
| edit tool | ✅ Yes | `edit_tool.json` | 23 | ✅ All pass |
| bash tool | ✅ Yes | `bash_tool.json` | 34 | ✅ All pass |
| grep tool | ✅ Yes | `grep_tool.json` | 12 | ✅ All pass |
| find tool | ✅ Yes | `find_tool.json` | 6 | ✅ All pass |
| ls tool | ✅ Yes | `ls_tool.json` | 8 | ✅ All pass |
| truncation | ✅ Yes | `truncation.json` | 9 | ✅ All pass |
| Session format | ❌ No | - | - | - |
| Provider responses | ❌ No | - | - | - |
| CLI flags | ❌ No | - | - | - |
| **Total** | **8/11** | - | **122** | ✅ |

### Fixture Schema

Fixtures are JSON files in `tests/conformance/fixtures/` with this structure:

```json
{
  "version": "1.0",
  "tool": "tool_name",
  "cases": [
    {
      "name": "test_name",
      "setup": [{"type": "create_file", "path": "...", "content": "..."}],
      "input": {"param": "value"},
      "expected": {
        "content_contains": ["..."],
        "content_regex": "...",
        "details_exact": {"key": "value"}
      }
    }
  ]
}
```

---

## Performance Targets

| Metric | Target | Current | Status |
|--------|--------|---------|--------|
| Startup time | <100ms | 13ms (`pi --version`) | ✅ |
| Binary size (release) | <20MB | 8.3MB | ✅ |
| TUI framerate | 60fps | N/A | ⬜ Deferred |
| Memory (idle) | <50MB | Not measured | ⬜ Deferred |

---

## Next Steps (Priority Order)

1. ~~**Complete print mode** - Non-interactive single response~~ ✅ Done
2. ~~**Add OpenAI provider** - Second provider implementation~~ ✅ Done
3. ~~**Implement auth.json** - Credential storage~~ ✅ Done (src/auth.rs)
4. ~~**Session picker UI** - Basic TUI for --resume~~ ✅ Done (src/session_picker.rs)
5. ~~**Branching/navigation** - Tree operations~~ ✅ Done (src/session.rs)
6. ~~**Benchmark harness** - Performance validation~~ ✅ Done (benches/tools.rs, BENCHMARKS.md)
7. ~~**Conformance fixtures** - TypeScript reference capture~~ ✅ Done (tests/conformance/)
