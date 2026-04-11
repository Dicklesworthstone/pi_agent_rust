# f-0006: IDE Integration Expansion

> Epic: [Rust Investment Roadmap](../epics/rust-investment-roadmap.md) — Item 6
> Priority: Medium | Effort: 1-2 weeks per editor

---

## Problem

`src/acp.rs` (1,509 lines) implements Agent Client Protocol for Zed editor
integration — a feature the TypeScript version entirely lacks. However, Zed
has ~3% editor market share. Expanding to VS Code (~70%), Neovim (~10%),
and JetBrains (~15%) would make this feature reach the majority of developers.

## Current State

**ACP Protocol (Zed):**
- JSON-RPC 2.0 over stdio
- Single entry point: `pub async fn run_stdio(options: AcpOptions)`
- Supports: model listing, mode listing, prompt submission, streaming responses
- Thinking level support added in recent commits

**RPC Mode:**
- `src/rpc.rs` (7,267 lines) — full headless JSON protocol
- Already used for programmatic access
- Could serve as the backend for IDE extensions

## Features

### 6a. VS Code Extension

**Architecture:** VS Code extension (TypeScript) → spawn `pi --rpc` → communicate via JSON-RPC over stdio.

**Extension features:**
- Inline chat panel (like GitHub Copilot Chat)
- Context from open files and workspace
- Tool approval prompts as VS Code notifications
- Session tree in sidebar
- Model/thinking level selection in status bar

**Implementation:**
- Create `editors/vscode/` directory with extension scaffold
- Use `@anthropic-ai/sdk` patterns for the extension UI
- Backend: spawn `pi` binary with `--rpc` flag
- Protocol: reuse existing RPC protocol from `src/rpc.rs`

**Key RPC methods to surface:**
- `initialize` — start session, pass workspace context
- `sendMessage` — send user message
- `streamResponse` — stream assistant response
- `approveToolUse` — user approves/denies tool execution
- `switchModel` — change model mid-session

### 6b. Neovim Plugin

**Architecture:** Lua plugin → spawn `pi --rpc` → JSON-RPC over stdio.

**Features:**
- `:Pi <prompt>` command for inline queries
- Split/floating window for conversation
- Telescope integration for session picker
- Which-key integration for keybindings

**Implementation:**
- Create `editors/neovim/` directory
- Lua plugin using `vim.fn.jobstart` for process management
- JSON encode/decode via `vim.json`

### 6c. JetBrains Plugin

**Architecture:** Kotlin plugin → spawn `pi --rpc` → JSON-RPC over stdio.

**Features:**
- Tool window panel for conversation
- Editor context integration
- Action for inline code explanation

### 6d. ACP Protocol Specification

**What to document:**
- All JSON-RPC methods with request/response schemas
- Authentication flow
- Streaming protocol (SSE over stdio)
- Tool approval flow
- Extension capability negotiation

**Format:** OpenRPC specification + human-readable docs in `docs/acp-protocol.md`

## Implementation Order

1. **VS Code** — largest user base, highest impact
2. **Protocol docs** — enables community plugins
3. **Neovim** — high engagement developer community
4. **JetBrains** — last due to Kotlin toolchain complexity

## Acceptance Criteria

- VS Code extension: install from VSIX, chat with agent, approve tool use
- Neovim plugin: `:Pi` command works with streaming output
- Protocol documented with JSON Schema for all methods
- All editors share the same `--rpc` backend — no editor-specific logic in core
