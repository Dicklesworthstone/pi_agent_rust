# Plan: Port Pi Agent to Rust

## Executive Summary

Port the **pi-mono** AI coding agent platform from TypeScript/Node.js to idiomatic Rust. The goal is a single-binary CLI that provides an interactive terminal interface for AI-assisted coding, with multi-provider LLM support, session management, and an extensible tool system.

**Why "py_agent_rust"?** The project directory was named before discovering this is TypeScript, not Python. The name stands.

---

## Why Port to Rust?

1. **Single binary distribution** - No Node.js runtime, npm, or dependencies to install
2. **Startup performance** - Native binary starts in milliseconds vs. hundreds of ms for Node.js
3. **Memory efficiency** - No V8 heap overhead; precise control over allocations
4. **Reliability** - Compile-time guarantees; no runtime type errors
5. **Cross-platform** - Easy cross-compilation for Linux/macOS/Windows
6. **Terminal handling** - Rust has excellent terminal libraries (crossterm, ratatui)

---

## What We're Porting

### Phase 1: Core Foundation
- [ ] CLI argument parsing (Clap)
- [ ] Configuration system (global + project settings)
- [ ] Error handling framework
- [ ] Logging/tracing infrastructure

### Phase 2: Provider Abstraction (`pi-ai` equivalent)
- [ ] Unified LLM API trait
- [ ] Streaming response handling
- [ ] Message types and content blocks
- [ ] Tool call/result protocol
- [ ] Provider implementations:
  - [ ] Anthropic (Claude)
  - [ ] OpenAI (GPT-4, etc.)
  - [ ] Google (Gemini)
  - [ ] Additional providers as needed

### Phase 3: Agent Runtime (`pi-agent` equivalent)
- [ ] Agent loop with tool execution
- [ ] Message history management
- [ ] Thinking/reasoning block handling
- [ ] Streaming event system
- [ ] Tool validation and error handling

### Phase 4: Session Management
- [ ] JSONL session file format (version 3 compatible)
- [ ] Session header and entry types
- [ ] Tree navigation and branching
- [ ] Compaction/summarization
- [ ] Project-based session organization

### Phase 5: Built-in Tools
- [ ] `read` - File reading with truncation
- [ ] `bash` - Shell command execution with streaming
- [ ] `edit` - Diff-based file editing
- [ ] `write` - File creation/overwrite
- [ ] `grep` - Content search (ripgrep integration)
- [ ] `find` - File search
- [ ] `ls` - Directory listing

### Phase 6: Terminal UI
- [ ] Differential rendering engine
- [ ] Multi-line editor with completions
- [ ] Slash command system
- [ ] Status line (tokens/cost/context)
- [ ] Thinking block display
- [ ] Markdown rendering
- [ ] Image handling (if terminal supports)

### Phase 7: Authentication
- [ ] API key management
- [ ] OAuth flow support (Anthropic, GitHub Copilot, etc.)
- [ ] Credential storage with proper permissions

### Phase 8: Output Modes
- [ ] Interactive mode (full TUI)
- [ ] Print mode (single response)
- [ ] JSON mode (structured output)
- [ ] HTML export

---

## What We're NOT Porting

| Feature | Reason |
|---------|--------|
| **Web UI** (`packages/web-ui`) | Out of scope; CLI-first |
| **Slack bot** (`packages/mom`) | Specialized integration; not core |
| **GPU pods** (`packages/pods`) | Infrastructure tooling; not core |
| **npm package system** | Replace with native plugin/extension system |
| **TypeScript type generation** | Not applicable |
| **Bun compilation** | Native Rust binary instead |
| **Full extension API** | Simplified plugin system; can expand later |
| **All 20+ providers initially** | Start with Anthropic, OpenAI, Google; add others on demand |
| **tmux integration** | Complex; defer to later phase |
| **GitHub Gist sharing** | Nice-to-have; defer |
| **Themes system** | Simplified color config instead |
| **Skills system** | Defer; focus on core tools first |
| **Prompt templates** | Simplified approach initially |

---

## Reference Projects

| Project | Path | Patterns to Copy |
|---------|------|------------------|
| dcg | `/data/projects/destructive_command_guard` | Clap derive, error handling, release profile |
| cass | `/data/projects/coding_agent_session_search` | SQLite, JSONL parsing, session formats |

---

## Architecture Decisions

### 1. Single Binary
All functionality in one binary. No separate packages like the TypeScript version.

### 2. Async Runtime
Use `tokio` for async I/O (HTTP requests, streaming, file operations).

### 3. Session Storage
- **JSONL files** as source of truth (Git-friendly, human-readable)
- No SQLite for sessions (unlike cass); JSONL is sufficient
- May use SQLite for search index later

### 4. Terminal UI
- Use `crossterm` for terminal handling
- Custom differential renderer (not ratatui initially - too heavyweight)
- RAII guards for terminal state

### 5. Provider Architecture
- Trait-based provider abstraction
- Each provider in separate module
- Feature flags for optional providers

### 6. Configuration
- TOML format for config files
- Environment variable overrides
- Project-local `.pi/` directory support

---

## Implementation Phases

### Phase 1: Foundation (Week 1)
- Cargo.toml with dependencies
- rust-toolchain.toml (nightly, edition 2024)
- Error types with thiserror
- Config loading (global + project)
- CLI skeleton with Clap

### Phase 2: Provider Layer (Week 2)
- Provider trait definition
- Message/content types
- Streaming event types
- Anthropic provider implementation
- Basic request/response cycle

### Phase 3: Agent Core (Week 3)
- Agent loop implementation
- Tool trait and built-in tools
- Message history
- Streaming response handling

### Phase 4: Session Persistence (Week 4)
- JSONL session format
- Session read/write
- Session listing and selection
- Basic tree navigation

### Phase 5: Terminal UI (Week 5-6)
- Terminal state management
- Differential renderer
- Editor component
- Slash commands
- Status line

### Phase 6: Polish (Week 7+)
- Additional providers
- Compaction system
- HTML export
- OAuth flows
- Performance optimization

---

## Key Data Structures

### Message Types
```rust
pub enum AgentMessage {
    User { content: Content, timestamp: i64 },
    Assistant { content: Content, timestamp: i64, model: String },
    ToolResult { tool_use_id: String, content: Content, is_error: bool },
}

pub enum ContentBlock {
    Text(String),
    Image { data: String, media_type: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
    Thinking { thinking: String },
}
```

### Session Format
```rust
pub struct SessionHeader {
    pub version: u8,  // 3
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub parent_session: Option<String>,
}

pub enum SessionEntry {
    Message(SessionMessage),
    ThinkingLevelChange { level: String, timestamp: i64 },
    ModelChange { model: String, timestamp: i64 },
    Compaction { summary: String, removed_count: usize },
    BranchSummary { summary: String, checkpoint_id: String },
}
```

### Provider Trait
```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &CompletionOptions,
    ) -> Result<impl Stream<Item = Result<StreamEvent>>>;
}
```

---

## Success Criteria

1. **Functional parity** with core pi-mono features (interactive mode, tools, sessions)
2. **Performance**: <100ms startup, smooth TUI at 60fps
3. **Binary size**: <20MB stripped
4. **Cross-platform**: Linux x86_64/ARM64, macOS Intel/Apple Silicon, Windows
5. **Reliability**: No panics in normal operation; graceful error handling

---

## Open Questions

1. **Plugin system**: How to support extensions? WASM? Dynamic libraries? Scripting?
2. **Image rendering**: Which terminals support images? How to detect?
3. **OAuth**: How to handle browser-based OAuth in CLI? Local server callback?
4. **Compaction**: Use LLM to summarize? Or simpler heuristic-based approach?

---

## Next Steps

1. Create `EXISTING_PI_STRUCTURE.md` - Deep dive spec extraction from legacy code
2. Create `PROPOSED_ARCHITECTURE.md` - Detailed Rust design
3. Bootstrap Cargo project with dependencies
4. Begin Phase 1 implementation
