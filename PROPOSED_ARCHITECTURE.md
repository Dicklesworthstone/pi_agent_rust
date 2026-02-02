# Proposed Architecture (Pi Rust Port)

## Goals
- Single binary, fast startup, low memory
- Full feature parity with legacy pi-coding-agent
- Strict JSONL session compatibility (v3)
- Modular provider and tool system

## Module Layout

```
src/
├── main.rs                # CLI entry, mode dispatch
├── cli.rs                 # Clap definitions + arg helpers
├── config.rs              # Settings load/merge + defaults
├── auth.rs                # Auth file IO + API key resolution
├── models.rs              # Model registry + resolver
├── provider.rs            # Provider trait + shared types
├── providers/             # Provider implementations
├── tools.rs               # Built-in tools + registry
├── session.rs             # JSONL session persistence
├── session_index.rs       # SQLite index (derived, optional)
├── agent.rs               # Agent loop, tool execution, streaming
├── modes.rs               # print/rpc/interactive entry points
└── tui.rs                 # Interactive UI (line-based now, TUI-ready)
```

## Core Data Flow

```
CLI → Config → Models/Auth → Session → Agent → Provider
                 ↑                    ↓
           Session Index        Tools + Session Writes
```

1. CLI parses flags, merges config, resolves model/provider.
2. Agent prepares context (system prompt + messages + tools).
3. Provider streams events → Agent updates session + UI.
4. Tool calls execute via ToolRegistry and return ToolResult messages.
5. Session JSONL is the source of truth; SQLite index tracks metadata.

## Session Storage
- **Primary store:** JSONL session file (v3), append-only tree.
- **Index store:** SQLite (`session-index.sqlite`) with per-session metadata:
  - path, id, cwd, timestamps, last_modified, message counts
  - searchable label/title fields
- Sync strategy documented in `SYNC_STRATEGY.md`.

## Modes
- **Interactive:** line-based REPL with command parsing and streaming display.
- **Print:** non-interactive, single-shot; outputs text or JSON.
- **RPC:** JSONL event stream for programmatic integration.

## Provider Strategy
- Shared `Provider` trait for streaming events.
- Built-in providers: Anthropic first, then OpenAI/Google etc.
- Models loaded from `~/.pi/agent/models.json` with per-provider defaults.

## Tool System
- Tool schema from JSON Schema definitions.
- Built-in tools: read, bash, edit, write, grep, find, ls.
- Tool results persisted as session entries.

## Extensibility
- Packages, extensions, skills, prompts, themes loaded from config.
- All external assets are resolved before mode execution.

## Error Handling & Telemetry
- `thiserror` for structured errors, `anyhow` at boundaries.
- `tracing` for debug/verbose output, controlled by env filter.
