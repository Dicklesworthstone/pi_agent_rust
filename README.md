<p align="center">
  <img src="https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/main/assets/pi-logo.svg" alt="Pi Logo" width="200"/>
</p>

<h1 align="center">pi_agent_rust</h1>

<p align="center">
  <strong>pi_agent_rust — High-performance AI coding agent CLI written in Rust</strong>
</p>

<p align="center">
  <a href="#quick-start">Quick Start</a> •
  <a href="#features">Features</a> •
  <a href="#installation">Installation</a> •
  <a href="#commands">Commands</a> •
  <a href="#configuration">Configuration</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-2024%20edition-orange?logo=rust" alt="Rust 2024">
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT">
  <img src="https://img.shields.io/badge/unsafe-forbidden-brightgreen" alt="No Unsafe Code">
</p>

---

## The Problem

You want an AI coding assistant in your terminal, but existing tools are:
- **Slow to start** — Node.js/Python runtimes add 500ms+ before you can type
- **Memory hungry** — Electron apps or heavy runtimes eat gigabytes
- **Unreliable** — Streaming breaks, sessions corrupt, tools fail silently
- **Hard to extend** — Closed ecosystems or complex plugin systems

## The Solution

**Pi** is a from-scratch Rust rewrite of the Pi Agent TypeScript CLI. Single binary, instant startup, rock-solid streaming, and 7 battle-tested built-in tools.

```bash
# Start a session
pi "Help me refactor this function to use async/await"

# Continue a previous session
pi --continue

# Single-shot mode (no session)
pi -p "What does this error mean?" < error.log
```

## Why Pi?

| Feature | Pi (Rust) | Typical TS/Python CLI |
|---------|-----------|----------------------|
| **Startup** | <100ms | 500ms-2s |
| **Binary size** | ~15MB | 100MB+ (with runtime) |
| **Memory (idle)** | <50MB | 200MB+ |
| **Streaming** | Native SSE parser | Library-dependent |
| **Tool execution** | Process tree management | Basic subprocess |
| **Sessions** | JSONL with branching | Varies |
| **Unsafe code** | Forbidden | N/A |

---

## Quick Start

### 1. Install

```bash
# From source (requires Rust nightly)
git clone https://github.com/Dicklesworthstone/pi_agent_rust.git
cd pi_agent_rust
cargo install --path .
```

### 2. Configure API Key

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
```

### 3. Run

```bash
# Interactive mode
pi

# With an initial message
pi "Explain this codebase structure"

# Read files as context
pi @src/main.rs "What does this do?"
```

---

## Features

### Streaming Responses

Real-time token streaming with extended thinking support:

```
pi "Write a quicksort implementation"
```

Watch the response appear token-by-token, with thinking blocks shown inline.

### 7 Built-in Tools

| Tool | Description | Example |
|------|-------------|---------|
| `read` | Read files with line numbers, supports images | Read src/main.rs lines 1-50 |
| `write` | Create or overwrite files | Write a new config file |
| `edit` | Surgical string replacement | Fix the typo on line 42 |
| `bash` | Execute shell commands with timeout | Run the test suite |
| `grep` | Search file contents with context | Find all TODO comments |
| `find` | Discover files by pattern | Find all *.rs files |
| `ls` | List directory contents | What's in src/? |

All tools include:
- Automatic truncation for large outputs (2000 lines / 50KB)
- Detailed metadata in responses
- Process tree cleanup for bash (no orphaned processes)

### Session Management

Sessions persist as JSONL files with full conversation history:

```bash
# Continue most recent session
pi --continue

# Open specific session
pi --session ~/.pi/sessions/2024-01-15T10-30-00.jsonl

# Ephemeral (no persistence)
pi --no-session
```

Sessions support:
- Tree structure for conversation branching
- Model/thinking level change tracking
- Automatic compaction for long conversations

### Extended Thinking

Enable deep reasoning for complex problems:

```bash
pi --thinking high "Design a distributed rate limiter"
```

Thinking levels: `none`, `low`, `medium`, `high`, `very_high`, `max`

---

## Installation

### From Source (Recommended)

Requires Rust nightly (2024 edition features):

```bash
# Install Rust nightly
rustup install nightly
rustup default nightly

# Clone and build
git clone https://github.com/Dicklesworthstone/pi_agent_rust.git
cd pi_agent_rust
cargo build --release

# Binary is at target/release/pi
./target/release/pi --version
```

### Dependencies

Pi has minimal runtime dependencies:
- `fd` — Required for the `find` tool (install via `apt install fd-find` or `brew install fd`)
- `rg` — Optional, improves grep performance (install via `apt install ripgrep` or `brew install ripgrep`)

---

## Commands

### Basic Usage

```bash
pi [OPTIONS] [MESSAGE]...

# Examples
pi                              # Start interactive session
pi "Hello"                      # Start with message
pi @file.rs "Explain this"      # Include file as context
pi -p "Quick question"          # Print mode (no session)
```

### Options

| Option | Description |
|--------|-------------|
| `-c, --continue` | Continue most recent session |
| `-s, --session <PATH>` | Open specific session file |
| `--no-session` | Don't persist conversation |
| `-p, --print` | Single response, no interaction |
| `--model <MODEL>` | Model to use (default: claude-sonnet-4-20250514) |
| `--thinking <LEVEL>` | Thinking level: none/low/medium/high/very_high/max |
| `--tools <TOOLS>` | Comma-separated tool list |
| `--api-key <KEY>` | API key (or use ANTHROPIC_API_KEY) |

### Subcommands

```bash
pi config              # Show configuration
pi list models         # List available models
pi list sessions       # List saved sessions
```

---

## Configuration

Pi reads configuration from `~/.config/pi/config.json`:

```json
{
  "model": "claude-sonnet-4-20250514",
  "thinkingLevel": "medium",
  "maxTokens": 16384,

  "compaction": {
    "enabled": true,
    "reserveTokens": 8192,
    "keepFirstMessages": 2
  },

  "retry": {
    "enabled": true,
    "maxRetries": 3,
    "baseDelayMs": 1000,
    "maxDelayMs": 30000
  },

  "images": {
    "autoResize": true,
    "blockImages": false
  },

  "terminal": {
    "showImages": true,
    "clearOnStart": false
  },

  "shellPath": "/bin/bash",
  "shellCommandPrefix": "set -e"
}
```

### Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `PI_CONFIG_PATH` | Custom config file path |
| `PI_SESSIONS_DIR` | Custom sessions directory |

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                           CLI (clap)                            │
│  • Argument parsing    • @file expansion    • Subcommands       │
└─────────────────────────────────┬───────────────────────────────┘
                                  │
┌─────────────────────────────────▼───────────────────────────────┐
│                          Agent Loop                             │
│  • Message history     • Tool iteration    • Event callbacks    │
└────────────┬────────────────────────────────────────┬───────────┘
             │                                        │
┌────────────▼────────────┐              ┌───────────▼────────────┐
│    Provider Layer       │              │    Tool Registry       │
│  • Anthropic (SSE)      │              │  • read    • bash      │
│  • OpenAI (planned)     │              │  • write   • grep      │
│  • Gemini (planned)     │              │  • edit    • find      │
└────────────┬────────────┘              │  • ls                  │
             │                           └───────────┬────────────┘
┌────────────▼─────────────────────────────────────▼─────────────┐
│                     Session Persistence                         │
│  • JSONL format (v3)   • Tree structure   • Per-project dirs    │
└─────────────────────────────────────────────────────────────────┘
```

### Key Design Decisions

1. **No unsafe code** — `#![forbid(unsafe_code)]` enforced project-wide
2. **Streaming-first** — Custom SSE parser, no blocking on responses
3. **Process tree management** — `sysinfo` crate ensures no orphaned processes
4. **Structured errors** — `thiserror` with specific error types per component
5. **Size-optimized release** — LTO + strip + opt-level=z for lean binaries

---

## Tool Details

### read

Read file contents with automatic line numbering:

```
Input: { "path": "src/main.rs", "offset": 10, "limit": 50 }
```

- Supports images (jpg, png, gif, webp) with optional auto-resize
- Truncates at 2000 lines or 50KB
- Returns continuation hint if truncated

### bash

Execute shell commands with timeout and output capture:

```
Input: { "command": "cargo test", "timeout": 120 }
```

- Default 120s timeout, configurable per-call
- Process tree cleanup on timeout (kills children)
- Rolling buffer for real-time output
- Full output saved to temp file if truncated

### edit

Surgical string replacement:

```
Input: { "path": "src/lib.rs", "old": "fn foo()", "new": "fn bar()" }
```

- Exact string matching (no regex)
- Fails if old string not found or ambiguous
- Returns diff preview

### grep

Search file contents:

```
Input: { "pattern": "TODO", "path": "src/", "context": 2, "limit": 100 }
```

- Regex patterns supported
- Context lines before/after matches
- Respects .gitignore

### find

Discover files by pattern:

```
Input: { "pattern": "*.rs", "path": "src/", "limit": 1000 }
```

- Glob patterns via `fd`
- Sorted by modification time
- Respects .gitignore

### ls

List directory contents:

```
Input: { "path": "src/", "limit": 500 }
```

- Alphabetically sorted
- Directories marked with trailing `/`
- Truncates at limit

---

## Troubleshooting

### "fd not found"

The `find` tool requires `fd`:

```bash
# Ubuntu/Debian
apt install fd-find

# macOS
brew install fd

# The binary might be named fdfind
ln -s $(which fdfind) ~/.local/bin/fd
```

### "API key not set"

```bash
export ANTHROPIC_API_KEY="sk-ant-..."

# Or in config.json
{ "apiKey": "sk-ant-..." }

# Or per-command
pi --api-key "sk-ant-..." "Hello"
```

### "Session corrupted"

Sessions are append-only JSONL. If corruption occurs:

```bash
# Start fresh
pi --no-session

# Or delete the problematic session
rm ~/.pi/sessions/corrupted-session.jsonl
```

### "Streaming hangs"

Check your network connection. Pi uses SSE which requires stable connections:

```bash
# Test with curl
curl -N https://api.anthropic.com/v1/messages
```

### "Tool output truncated"

This is intentional to prevent context overflow. Use offset/limit:

```bash
# In the conversation
"Read lines 2000-4000 of that file"
```

---

## Limitations

Pi is honest about what it doesn't do:

| Limitation | Workaround |
|------------|------------|
| **Anthropic-only** | OpenAI/Gemini providers planned |
| **No web browsing** | Use bash with curl |
| **No GUI** | Terminal-only by design |
| **No plugins** | Fork and extend directly |
| **English-centric** | Works but not optimized for other languages |
| **Nightly Rust required** | Uses 2024 edition features |

---

## FAQ

**Q: Why rewrite in Rust?**
A: Startup time matters when you're in a terminal all day. Rust gives us <100ms startup vs 500ms+ for Node.js. Plus, no runtime dependencies to manage.

**Q: Can I use OpenAI/Gemini models?**
A: Not yet. Anthropic is the only supported provider currently. OpenAI support is planned.

**Q: How do sessions work?**
A: Each session is a JSONL file with message entries. Sessions are per-project (based on working directory) and support branching via parent references.

**Q: Why is unsafe forbidden?**
A: Memory safety is non-negotiable for a tool that executes arbitrary commands. The performance cost is negligible for this use case.

**Q: How do I extend Pi?**
A: Fork it. Adding a new tool means implementing the `Tool` trait in `src/tools.rs`. No plugin system by design.

**Q: Why isn't X feature included?**
A: Pi focuses on core coding assistance. Features like web browsing, image generation, etc. are out of scope. Use specialized tools for those.

---

## Comparison

| Feature | Pi | Claude Code | Aider | Cursor |
|---------|-----|-------------|-------|--------|
| **Language** | Rust | TypeScript | Python | Electron |
| **Startup** | <100ms | ~1s | ~2s | ~5s |
| **Memory** | <50MB | ~200MB | ~150MB | ~500MB |
| **Providers** | Anthropic | Anthropic | Many | Many |
| **Tools** | 7 built-in | Many | File-focused | IDE-integrated |
| **Sessions** | JSONL tree | Proprietary | Git-based | Proprietary |
| **Open source** | Yes | Yes | Yes | No |

---

## Development

### Building

```bash
cargo build           # Debug build
cargo build --release # Release build (optimized)
cargo test           # Run tests
cargo clippy         # Lint check
```

### Testing

```bash
# All tests
cargo test

# Specific module
cargo test tools::tests
cargo test sse::tests

# Conformance tests
cargo test conformance
```

### Project Structure

```
src/
├── main.rs          # CLI entry point
├── lib.rs           # Library exports
├── agent.rs         # Agent loop
├── cli.rs           # Argument parsing
├── config.rs        # Configuration
├── error.rs         # Error types
├── model.rs         # Message types
├── provider.rs      # Provider trait
├── providers/
│   └── anthropic.rs # Anthropic implementation
├── session.rs       # Session persistence
├── sse.rs           # SSE parser
├── tools.rs         # Built-in tools
└── tui.rs           # Terminal UI (WIP)
```

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---

## License

MIT License. See [LICENSE](LICENSE) for details.

---

<p align="center">
  <sub>Built with Rust, for developers who live in the terminal.</sub>
</p>
