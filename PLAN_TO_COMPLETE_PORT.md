# Plan: Complete pi_agent_rust Port

> **Goal:** 100% feature/functionality coverage with clear conformance harness and benchmarking, fully leveraging asupersync, rich_rust, and charmed_rust.

---

## Executive Summary

**Current State:** ~85-90% complete
- Core types, tools, sessions, CLI, interactive TUI: ✅ Implemented
- Multi-provider (Anthropic/OpenAI/Gemini/Azure): ✅ Implemented
- RPC mode (stdin/stdout JSON protocol): ✅ Implemented (see `src/rpc.rs`, `tests/rpc_mode.rs`)
- Conformance harness: ✅ Tools fixture suite + integration tests
- Benchmarks: ✅ Truncation + SSE parsing baseline

**Target State:** Production-ready CLI with:
- Full interactive TUI using charmed_rust (Elm Architecture)
- All streaming via asupersync (cancel-correct, structured concurrency)
- Beautiful output via rich_rust (markup, tables, panels)
- Comprehensive conformance test suite (tools + RPC + extensions) with TypeScript reference capture
- Benchmark harness proving performance targets (including extension hostcall dispatch)

**Primary Remaining Work:**
1. **Extensions runtime** (connector dispatch, event loop, hostcalls) + conformance harness (`EXTENSIONS.md`, `CONFORMANCE.md`)
2. **Themes discovery/hot reload** (resource pipeline + UI integration)
3. **asupersync migration** (HTTP/TLS + task orchestration), then reduce/remove tokio usage

---

## Part 1: Library Integration Strategy

### 1.1 asupersync Integration

**Purpose:** Replace tokio for async runtime, HTTP, TLS, SQLite

**Migration Plan:**
1. Add features: `asupersync = { path = "../asupersync", features = ["tls", "tls-native-roots", "sqlite", "http2"] }`
2. Create `AgentCx` wrapper around `Cx` for capability-secure operations
3. Migrate HTTP client from reqwest → asupersync HTTP + TLS
4. Migrate session storage to asupersync SQLite (index) + JSONL (source of truth)
5. Remove tokio once migration complete

**Key APIs to Leverage:**
```rust
use asupersync::{Cx, Outcome, Budget, Scope};
use asupersync::http::client::HttpClient;
use asupersync::tls::TlsConnectorBuilder;
use asupersync::database::sqlite::SqliteConnection;
use asupersync::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt};
```

**Benefits:**
- Structured concurrency (no orphan tasks)
- Cancel-correct operations (bounded cleanup)
- Deterministic testing via LabRuntime
- Batteries-included HTTP/TLS/SQLite

### 1.2 rich_rust Integration

**Purpose:** All non-interactive terminal output

**Usage Patterns:**
```rust
use rich_rust::prelude::*;

// Console for formatted output
let console = Console::new();
console.print("[bold green]✓[/] Tool executed successfully");

// Tables for structured data
let mut table = Table::new().title("Session Info");
table.add_row_cells(["Tokens", &format!("{}", usage.total_tokens)]);
console.print_renderable(&table);

// Panels for boxed content
let panel = Panel::from_text(&response).title("Assistant").width(80);
console.print_renderable(&panel);

// Progress bars for long operations
let bar = ProgressBar::new().width(40);
bar.set_progress(0.5);

// Markdown rendering (requires "markdown" feature)
let md = Markdown::new(&response_text);
console.print_renderable(&md);

// Syntax highlighting (requires "syntax" feature)
let syntax = Syntax::new(&code, "rust");
console.print_renderable(&syntax);
```

**Add Features:**
```toml
rich_rust = { path = "../rich_rust", features = ["syntax", "markdown", "full"] }
```

### 1.3 charmed_rust Integration

**Purpose:** Full interactive TUI using Elm Architecture

**Architecture:**
```rust
use bubbletea::{Program, Model, Message, Cmd, KeyMsg};
use lipgloss::{Style, Border, Position};
use bubbles::{textinput::TextInput, spinner::Spinner, viewport::Viewport};
use glamour::{render as render_markdown, Style as MdStyle};

struct PiTui {
    // Editor state
    input: TextInput,
    history: Vec<String>,
    history_index: Option<usize>,

    // Display state
    viewport: Viewport,
    spinner: Spinner,
    status: StatusLine,

    // Agent state
    messages: Vec<Message>,
    streaming: bool,
    current_response: String,
    thinking: Option<String>,

    // Session state
    session: Session,
    config: Config,
}

impl Model for PiTui {
    fn init(&mut self) -> Cmd {
        Cmd::batch(vec![
            TextInput::blink(),
            self.spinner.tick(),
        ])
    }

    fn update(&mut self, msg: Message) -> Cmd {
        // Handle keyboard input, API responses, tool results
    }

    fn view(&self) -> String {
        // Render using lipgloss layout
        let layout = lipgloss::join_vertical(Position::Left, &[
            self.render_header(),
            self.render_messages(),
            self.render_status(),
            self.render_input(),
        ]);
        layout
    }
}
```

**Add Dependencies:**
```toml
bubbletea = { path = "../charmed_rust", package = "bubbletea" }
lipgloss = { path = "../charmed_rust", package = "lipgloss" }
bubbles = { path = "../charmed_rust", package = "bubbles" }
glamour = { path = "../charmed_rust", package = "glamour" }
```

---

## Part 2: Implementation Phases

### Phase 1: Fix Existing Issues ✅ COMPLETE

**1.1 Fix Failing Fixture Tests**
- [x] Investigated detail field serialization in bash, edit, read, write tools
- [x] Fixed `details` field expectations in fixtures
- [x] Fixed bash exit code bug (race condition in tokio::select!)
- [x] All 67 fixture cases pass

**1.2 Clean Up Existing Code**
- [x] Run `cargo clippy --all-targets` - warnings addressed
- [x] Run `cargo fmt` - consistent formatting
- [x] Run `cargo test` - all 67 tests pass

### Phase 2: Library Dependencies ✅ COMPLETE

**2.1 Update Cargo.toml** ✅
```toml
# Interactive TUI (charmed_rust - Elm Architecture)
bubbletea = { path = "../charmed_rust/crates/bubbletea" }
lipgloss = { path = "../charmed_rust/crates/lipgloss" }
bubbles = { path = "../charmed_rust/crates/bubbles" }
glamour = { path = "../charmed_rust/crates/glamour" }
```

**2.2 Create Wrapper Types**
- [ ] `AgentCx` - Capability context for agent operations
- [x] `RichConsole` - Wrapper for rich_rust Console with Pi-specific methods (`PiConsole`)
- [x] `TuiApp` - bubbletea Model implementation (`src/interactive.rs`)

### Phase 3: Interactive TUI ✅ COMPLETE

**3.1 Core TUI Structure**
- [x] `src/interactive.rs` - Main Model implementation (PiApp)
- [x] TextInput with history navigation (up/down)
- [x] Message display with markdown rendering (glamour)
- [x] Status footer with token counts and cost
- [x] Spinner while processing
- [x] Tool execution status display
- [x] Slash command system

**3.2 Editor Component**
- [x] Multi-line text input via bubbles TextArea
- [x] History navigation (up/down)
- [x] Ctrl+C to cancel/quit
- [x] Esc to quit when idle
- [ ] Completions popup (slash commands, file paths) - deferred
- [ ] Shift+Enter for newline, Enter to submit - deferred (TextArea handles this)

**3.3 Message Display**
- [x] Assistant responses with markdown rendering (glamour)
- [x] Thinking blocks (displayed inline)
- [x] Tool execution status (spinner + tool name)
- [x] Tool results (formatted output)
- [ ] Images (if terminal supports via rich_rust) - deferred

**3.4 Slash Commands** ✅ IMPLEMENTED
- [x] `/help` - Show available commands
- [x] `/model` - Show/switch model
- [x] `/thinking` - Set thinking level
- [x] `/history` - Show input history
- [x] `/clear` - Clear conversation
- [x] `/quit` (`/exit`) - Exit application
- [x] `/export` - Export to HTML

**3.5 Status Line**
- [x] Current model/provider in header
- [x] Token counts (input/output)
- [x] Cost ($X.XX)
- [x] Streaming indicator (spinner)
- [x] Status message for slash commands

**3.6 Agent Integration** ✅ COMPLETE
- [x] PiMsg enum for async agent events
- [x] Wire up agent execution from submit_message()
- [x] Handle streaming events via channel
- [x] Session persistence after each turn

### Phase 4: HTTP Migration to asupersync (2-3 days)

**4.1 Create HTTP Module**
- [x] `src/http/mod.rs` - HTTP client abstraction
- [x] `src/http/client.rs` - asupersync-based client
- [x] `src/http/sse.rs` - SSE streaming parser (reuse existing)

**4.2 Migrate Anthropic Provider**
- [ ] Replace reqwest with asupersync HTTP client
- [ ] Use asupersync TLS with native roots
- [ ] Implement cancel-correct streaming
- [ ] Test with deterministic LabRuntime

**4.3 Remove tokio Dependencies**
- [ ] Remove `tokio` from Cargo.toml
- [ ] Remove `reqwest` from Cargo.toml
- [ ] Update all async code to use asupersync

### Phase 5: Additional Providers (2-3 days)

**5.1 OpenAI Provider**
- [ ] `src/providers/openai.rs`
- [ ] Chat completions API
- [ ] Streaming support
- [ ] Function calling (tools)
- [ ] Conformance tests

**5.2 Google Gemini Provider**
- [ ] `src/providers/google.rs`
- [ ] Generative AI API
- [ ] Streaming support
- [ ] Tool calling
- [ ] Conformance tests

### Phase 6: Session Enhancements (2 days)

**6.1 SQLite Index**
- [ ] `src/session/index.rs` - SQLite-based session index
- [ ] Search by content, date, model
- [ ] Fast session listing
- [ ] Sync from JSONL source of truth

**6.2 Tree Navigation**
- [ ] Branch creation
- [ ] Branch switching
- [ ] Visual tree display in TUI
- [ ] Checkpoint/restore

**6.3 Session Picker UI**
- [ ] List recent sessions
- [ ] Search/filter
- [ ] Preview content
- [ ] Select and resume

### Phase 7: Conformance Testing (2-3 days)

**7.1 Reference Capture Infrastructure**
- [ ] `tests/conformance/reference/` - Reference implementation captures
- [ ] Go reference program for each tool
- [ ] TypeScript reference program (legacy behavior)
- [ ] JSON fixture generation scripts

**7.2 Tool Conformance**
- [ ] Capture TypeScript tool outputs for edge cases
- [ ] Expand fixture coverage to 100+ cases per tool
- [ ] Add fuzzing-discovered edge cases

**7.3 Provider Conformance**
- [ ] Mock API responses
- [ ] SSE parsing conformance
- [ ] Error handling conformance
- [ ] Rate limiting behavior

**7.4 Session Format Conformance**
- [ ] JSONL parsing/writing
- [ ] Entry type serialization
- [ ] Tree structure operations
- [ ] Migration from older versions

### Phase 8: Benchmarking Harness (1-2 days)

**8.1 Benchmark Infrastructure**
- [ ] `benches/startup.rs` - Startup time (<100ms target)
- [ ] `benches/tui_render.rs` - TUI frame rate (60fps target)
- [ ] `benches/tools.rs` - Tool execution latency
- [ ] `benches/streaming.rs` - SSE parsing throughput

**8.2 Performance Targets**
| Metric | Target | Measurement |
|--------|--------|-------------|
| Startup time | <100ms | Cold start to first prompt |
| TUI framerate | 60fps | Continuous rendering benchmark |
| Binary size | <15MB | `cargo build --release && ls -la` |
| Memory (idle) | <30MB | After startup, before first request |
| SSE throughput | >10MB/s | Parse rate for streaming events |

**8.3 CI Integration**
- [ ] GitHub Actions workflow for benchmarks
- [ ] Performance regression detection
- [ ] Badge in README

### Phase 9: Polish & Documentation (1-2 days)

**9.1 Error Messages**
- [ ] User-friendly error formatting (rich_rust panels)
- [ ] Actionable suggestions
- [ ] Context-aware hints

**9.2 Documentation**
- [ ] Update README with new features
- [ ] API documentation (rustdoc)
- [ ] Configuration reference
- [ ] Troubleshooting guide

**9.3 Release Preparation**
- [ ] Version bump to 1.0.0
- [ ] CHANGELOG.md
- [ ] Cross-compilation testing (Linux/macOS/Windows)
- [ ] Binary distribution (GitHub Releases)

---

## Part 3: Conformance Test Strategy

### 3.1 Fixture-Based Testing

Each tool has a JSON fixture file with this structure:

```json
{
  "version": "1.1",
  "tool": "read",
  "reference_impl": "typescript",
  "captured_at": "2026-02-02T00:00:00Z",
  "cases": [
    {
      "name": "read_simple_file",
      "description": "Read a simple text file",
      "setup": [
        {"type": "create_file", "path": "test.txt", "content": "Hello\nWorld\n"}
      ],
      "input": {"path": "test.txt"},
      "expected": {
        "is_error": false,
        "content_contains": ["Hello", "World"],
        "content_regex": "^\\s*1→Hello",
        "details": {
          "lines_read": 2,
          "truncated": false
        }
      }
    }
  ]
}
```

### 3.2 Reference Capture Process

1. **TypeScript Reference:**
   ```bash
   cd legacy_pi_mono_code/pi-mono
   pnpm test:capture -- --tool read --output ../fixtures/read_tool.json
   ```

2. **Go Reference (for additional validation):**
   ```bash
   cd tests/conformance/go_reference
   go run ./cmd/capture_read --output ../fixtures/go_read_tool.json
   ```

3. **Rust Implementation Test:**
   ```bash
   cargo test conformance::test_read -- --nocapture
   ```

### 3.3 Coverage Targets

| Component | Cases | Status |
|-----------|-------|--------|
| read tool | 25+ | 10 done |
| write tool | 15+ | 7 done |
| edit tool | 20+ | 6 done |
| bash tool | 30+ | 12 done |
| grep tool | 25+ | 12 done |
| find tool | 15+ | 6 done |
| ls tool | 15+ | 8 done |
| truncation | 20+ | 9 done |
| SSE parsing | 30+ | 11 done |
| Session format | 25+ | 0 done |
| Provider responses | 20+ | 0 done |
| **Total** | **240+** | **81 done** |

---

## Part 4: Architecture Diagram

```
┌────────────────────────────────────────────────────────────────────┐
│                          pi CLI Binary                              │
├────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────────────────┐│
│  │   CLI       │───▶│   Config    │───▶│      Agent Loop         ││
│  │  (clap)     │    │   Loader    │    │   (tool iteration)      ││
│  └─────────────┘    └─────────────┘    └───────────┬─────────────┘│
│                                                     │              │
│  ┌──────────────────────────────────────────────────┼──────────────┤
│  │                    TUI Layer (charmed_rust)      │              │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────▼───────────┐ │
│  │  │   Editor    │  │   Status    │  │     Message Display     │ │
│  │  │ (bubbles)   │  │   Line      │  │  (glamour markdown)     │ │
│  │  └─────────────┘  └─────────────┘  └─────────────────────────┘ │
│  │  ┌─────────────┐  ┌─────────────┐                              │
│  │  │  Thinking   │  │   Slash     │                              │
│  │  │   Block     │  │  Commands   │                              │
│  │  └─────────────┘  └─────────────┘                              │
│  └─────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────────┤
│  │                 Provider Layer (asupersync HTTP)                │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
│  │  │  Anthropic  │  │   OpenAI    │  │   Google    │             │
│  │  │  Provider   │  │  Provider   │  │  Provider   │             │
│  │  └─────────────┘  └─────────────┘  └─────────────┘             │
│  └─────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────────┤
│  │                    Tool Layer                                    │
│  │  ┌────┐ ┌────┐ ┌────┐ ┌─────┐ ┌────┐ ┌────┐ ┌──┐              │
│  │  │read│ │bash│ │edit│ │write│ │grep│ │find│ │ls│              │
│  │  └────┘ └────┘ └────┘ └─────┘ └────┘ └────┘ └──┘              │
│  └─────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────────┤
│  │              Session Layer (JSONL + SQLite index)               │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
│  │  │   JSONL     │  │   SQLite    │  │    Tree     │             │
│  │  │   Files     │  │   Index     │  │  Navigator  │             │
│  │  └─────────────┘  └─────────────┘  └─────────────┘             │
│  └─────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────────┤
│  │              Output Layer (rich_rust)                           │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
│  │  │  Console    │  │   Tables    │  │   Panels    │             │
│  │  │  (markup)   │  │  Progress   │  │  Spinners   │             │
│  │  └─────────────┘  └─────────────┘  └─────────────┘             │
│  └─────────────────────────────────────────────────────────────────┤
└────────────────────────────────────────────────────────────────────┘
```

---

## Part 5: Timeline Estimate

| Phase | Duration | Dependencies |
|-------|----------|--------------|
| Phase 1: Fix Issues | 1-2 days | None |
| Phase 2: Dependencies | 1 day | Phase 1 |
| Phase 3: Interactive TUI | 3-5 days | Phase 2 |
| Phase 4: HTTP Migration | 2-3 days | Phase 2 |
| Phase 5: Providers | 2-3 days | Phase 4 |
| Phase 6: Sessions | 2 days | Phase 3 |
| Phase 7: Conformance | 2-3 days | Phases 1-6 |
| Phase 8: Benchmarks | 1-2 days | Phase 7 |
| Phase 9: Polish | 1-2 days | Phase 8 |
| **Total** | **15-23 days** | |

---

## Part 6: Success Criteria

### Functional Requirements

- [ ] All 7 tools pass conformance tests (100% fixture coverage)
- [ ] Interactive TUI works on Linux/macOS (Windows best-effort)
- [ ] Anthropic provider with full streaming and thinking support
- [ ] OpenAI provider with function calling
- [ ] Session persistence with tree navigation
- [ ] Print mode for non-interactive use

### Performance Requirements

- [ ] Startup: <100ms cold start
- [ ] TUI: 60fps rendering
- [ ] Binary: <15MB stripped
- [ ] Memory: <30MB idle

### Quality Requirements

- [ ] Zero unsafe code (`#![forbid(unsafe_code)]`)
- [ ] Zero clippy warnings (pedantic + nursery)
- [ ] 100% test pass rate
- [ ] rustdoc for all public APIs

### Conformance Requirements

- [ ] 240+ fixture test cases
- [ ] TypeScript reference capture for all tools
- [ ] Provider response conformance tests
- [ ] Session format migration tests

---

## Part 7: Files to Create/Modify

### New Files

```
src/tui/mod.rs           # TUI module organization
src/tui/app.rs           # Main Model implementation
src/tui/input.rs         # Multi-line editor
src/tui/messages.rs      # Message display
src/tui/status.rs        # Status line
src/tui/thinking.rs      # Thinking block
src/tui/commands.rs      # Slash commands

src/http/mod.rs          # HTTP abstraction
src/http/client.rs       # asupersync HTTP client

src/providers/openai.rs  # OpenAI provider
src/providers/google.rs  # Google Gemini provider

src/session/index.rs     # SQLite session index
src/session/tree.rs      # Tree navigation

benches/startup.rs       # Startup benchmark
benches/tui_render.rs    # TUI benchmark
benches/tools.rs         # Tool benchmark
benches/streaming.rs     # SSE benchmark

tests/conformance/reference/    # Reference implementations
tests/conformance/fixtures/     # Expanded fixtures
```

### Files to Modify

```
Cargo.toml               # Add charmed_rust, update asupersync features
src/main.rs              # Wire up TUI, remove tokio
src/agent.rs             # Use AgentCx
src/providers/anthropic.rs # Migrate to asupersync HTTP
src/session.rs           # Add SQLite index, tree navigation
src/tools.rs             # Fix detail field serialization
FEATURE_PARITY.md        # Update as features complete
```

---

## Immediate Next Steps

1. **Fix the 4 failing fixture tests** - detail field serialization
2. **Add charmed_rust dependencies** to Cargo.toml
3. **Create TUI module structure** with basic Model implementation
4. **Implement multi-line editor** using bubbles TextInput
5. **Run `cargo check`** to verify everything compiles
