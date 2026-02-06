<p align="center">
  <img src="pi_agent_rust_illustration.webp" alt="Pi Agent Rust" width="600"/>
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
  <a href="https://github.com/Dicklesworthstone/pi_agent_rust/actions/workflows/ci.yml">
    <img src="https://github.com/Dicklesworthstone/pi_agent_rust/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI">
  </a>
  <a href="https://github.com/Dicklesworthstone/pi_agent_rust/actions/workflows/bench.yml">
    <img src="https://github.com/Dicklesworthstone/pi_agent_rust/actions/workflows/bench.yml/badge.svg?branch=main" alt="Bench">
  </a>
</p>

---

## The Problem

You want an AI coding assistant in your terminal, but existing tools are:
- **Slow to start**: Node.js/Python runtimes add 500ms+ before you can type
- **Memory hungry**: Electron apps or heavy runtimes eat gigabytes
- **Unreliable**: Streaming breaks, sessions corrupt, tools fail silently
- **Hard to extend**: Closed ecosystems or complex plugin systems

## The Solution

**pi_agent_rust** is a from-scratch Rust port of [Pi Agent](https://github.com/badlogic/pi) by [Mario Zechner](https://github.com/badlogic) (made with his blessing!). Single binary, instant startup, rock-solid streaming, and 7 battle-tested built-in tools.

Rather than a direct line-by-line translation, this port builds on two purpose-built Rust libraries:
- **[asupersync](https://github.com/Dicklesworthstone/asupersync)**: A structured concurrency async runtime with built-in HTTP, TLS, and SQLite
- **[rich_rust](https://github.com/Dicklesworthstone/rich_rust)**: A Rust port of [Rich](https://github.com/Textualize/rich) by [Will McGugan](https://github.com/willmcgugan), providing beautiful terminal output with markup syntax

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

## Foundation Libraries

### asupersync

[asupersync](https://github.com/Dicklesworthstone/asupersync) is a structured concurrency async runtime designed for applications that need predictable resource cleanup. Key features used by pi_agent_rust:

- **Capability-based context (`Cx`)**: Async functions receive an explicit context that controls what they can do (HTTP, filesystem, time). This makes testing deterministic.
- **HTTP client with TLS**: Built-in HTTP API with rustls, avoiding OpenSSL dependency hell
- **Structured cancellation**: When a parent task cancels, all child tasks cancel cleanly. No orphaned futures.

`pi_agent_rust` runs on `asupersync` end-to-end today (runtime + HTTP/TLS + cancellation). Provider streaming uses a minimal HTTP client (`src/http/client.rs`) feeding a custom SSE parser (`src/sse.rs`).

### rich_rust

[rich_rust](https://github.com/Dicklesworthstone/rich_rust) is a Rust port of Will McGugan's [Rich](https://github.com/Textualize/rich) Python library. It provides:

- **Markup syntax**: `[bold red]error[/]` renders as bold red text
- **Tables**: ASCII/Unicode table rendering with alignment and borders
- **Panels**: Boxed content with titles
- **Progress bars**: Animated progress indicators
- **Markdown**: Terminal-rendered markdown with syntax highlighting
- **Themes**: Consistent color schemes across components

The terminal UI (currently WIP) uses rich_rust for all output formatting, providing the same visual quality as Rich-based Python tools.

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
| `read` | Read file contents, supports images | Read src/main.rs |
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
pi --session ~/.pi/agent/sessions/--home-user-project--/2024-01-15T10-30-00.jsonl

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

Thinking levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`

### Customization (Skills & Prompt Templates)

- **Skills**: Drop `SKILL.md` under `~/.pi/agent/skills/` or `.pi/skills/` and invoke with `/skill:name`.
- **Prompt templates**: Markdown files under `~/.pi/agent/prompts/` or `.pi/prompts/`; invoke via `/<template> [args]`.
- **Packages**: Share bundles with `pi install npm:@org/pi-packages` (skills, prompts, themes, extensions).

### Extensions (Planned)

Pi’s extension runtime is designed to be **Node/Bun-free**:
- **Default:** WASM components (portable, sandboxed) via WIT hostcalls
- **JS compatibility:** compiled JS → QuickJS bytecode (or JS→WASM), with a tiny Pi event loop + capability-gated connectors
- **Security:** no ambient OS access; extensions call explicit host connectors (`tool/exec/http/session/ui`) with audit logging

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
- `fd`: Required for the `find` tool (install via `apt install fd-find` or `brew install fd`)
- `rg`: Optional, improves grep performance (install via `apt install ripgrep` or `brew install ripgrep`)

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

Interactive file references:
- Type `@relative/path` in the editor to attach a file’s contents (autocomplete inserts the `@` form).

### Options

| Option | Description |
|--------|-------------|
| `-c, --continue` | Continue most recent session |
| `-s, --session <PATH>` | Open specific session file |
| `--no-session` | Don't persist conversation |
| `-p, --print` | Single response, no interaction |
| `--model <MODEL>` | Model to use (default: claude-sonnet-4-20250514) |
| `--thinking <LEVEL>` | Thinking level: off/minimal/low/medium/high/xhigh |
| `--tools <TOOLS>` | Comma-separated tool list |
| `--api-key <KEY>` | API key (or use ANTHROPIC_API_KEY) |
| `--list-models [PATTERN]` | List available models (optional fuzzy filter) |
| `--export <PATH>` | Export session file to HTML |

### Subcommands

```bash
# Package management
pi install <source> [-l|--local]    # Install a package source and add to settings
pi remove <source> [-l|--local]     # Remove a package source from settings
pi update [source]                 # Update all (or one) non-pinned packages
pi list                            # List user + project packages from settings

# Configuration
pi config                          # Show settings paths + precedence
```

---

## Configuration

Pi reads configuration from `~/.pi/agent/settings.json`:

```json
{
  "default_provider": "anthropic",
  "default_model": "claude-sonnet-4-20250514",
  "default_thinking_level": "medium",

  "compaction": {
    "enabled": true,
    "reserve_tokens": 8192,
    "keep_recent_tokens": 20000
  },

  "retry": {
    "enabled": true,
    "max_retries": 3,
    "base_delay_ms": 1000,
    "max_delay_ms": 30000
  },

  "images": {
    "auto_resize": true,
    "block_images": false
  },

  "terminal": {
    "show_images": true,
    "clear_on_shrink": false
  },

  "shell_path": "/bin/bash",
  "shell_command_prefix": "set -e"
}
```

### Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `OPENAI_API_KEY` | OpenAI API key |
| `GOOGLE_API_KEY` | Google Gemini API key |
| `AZURE_OPENAI_API_KEY` | Azure OpenAI API key |
| `COHERE_API_KEY` | Cohere API key |
| `GROQ_API_KEY` | Groq API key (OpenAI-compatible) |
| `DEEPINFRA_API_KEY` | DeepInfra API key (OpenAI-compatible) |
| `CEREBRAS_API_KEY` | Cerebras API key (OpenAI-compatible) |
| `OPENROUTER_API_KEY` | OpenRouter API key (OpenAI-compatible) |
| `MISTRAL_API_KEY` | Mistral API key (OpenAI-compatible) |
| `MOONSHOT_API_KEY` | Moonshot/Kimi API key (OpenAI-compatible) |
| `DASHSCOPE_API_KEY` | DashScope/Qwen API key (OpenAI-compatible) |
| `DEEPSEEK_API_KEY` | DeepSeek API key (OpenAI-compatible) |
| `FIREWORKS_API_KEY` | Fireworks API key (OpenAI-compatible) |
| `TOGETHER_API_KEY` | Together API key (OpenAI-compatible) |
| `PERPLEXITY_API_KEY` | Perplexity API key (OpenAI-compatible) |
| `XAI_API_KEY` | xAI API key (OpenAI-compatible) |
| `PI_CONFIG_PATH` | Custom config file path |
| `PI_CODING_AGENT_DIR` | Override the global config directory |
| `PI_PACKAGE_DIR` | Override the packages directory |
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
│  • OpenAI               │              │  • write   • grep      │
│  • Gemini               │              │  • edit    • find      │
│  • Azure OpenAI         │              │                        │
└────────────┬────────────┘              │  • ls                  │
             │                           └───────────┬────────────┘
┌────────────▼─────────────────────────────────────▼─────────────┐
│                     Session Persistence                         │
│  • JSONL format (v3)   • Tree structure   • Per-project dirs    │
└─────────────────────────────────────────────────────────────────┘
```

### Key Design Decisions

1. **No unsafe code**: `#![forbid(unsafe_code)]` enforced project-wide
2. **Streaming-first**: Custom SSE parser, no blocking on responses
3. **Process tree management**: `sysinfo` crate ensures no orphaned processes
4. **Structured errors**: `thiserror` with specific error types per component
5. **Size-optimized release**: LTO + strip + opt-level=z for lean binaries

---

## Deep Dive: Core Algorithms

### SSE Streaming Parser

The SSE (Server-Sent Events) parser is a custom implementation that handles Anthropic's streaming response format. Unlike library-based approaches, the parser operates as a state machine that processes bytes incrementally:

```
Bytes → Line Accumulator → Event Parser → Typed StreamEvent
```

**Key characteristics:**

| Property | Implementation |
|----------|----------------|
| **Buffering** | Zero-copy where possible; lines accumulated only when incomplete |
| **Event types** | 12 distinct variants: MessageStart, ContentBlockStart, ContentBlockDelta, ContentBlockStop, MessageDelta, MessageStop, Ping, Error, and thinking-specific events |
| **Error recovery** | Malformed events logged but don't crash the stream |
| **Memory** | Fixed-size rolling buffer prevents unbounded growth |

The parser handles edge cases like:
- Multi-line `data:` fields (concatenated with newlines)
- Events split across TCP packet boundaries
- The `event:` field appearing before or after `data:`
- CRLF and LF line endings interchangeably

### Truncation Algorithm

Large outputs from tools (file reads, command output, grep results) must be truncated to avoid exhausting the LLM's context window. The truncation algorithm preserves usefulness while staying within limits:

```
┌─────────────────────────────────────────┐
│           Original Content              │
│         (potentially huge)              │
└─────────────────────────────────────────┘
                    │
                    ▼
┌─────────────────────────────────────────┐
│  HEAD: First N/2 lines                  │
│  ─────────────────────────              │
│  [... X lines truncated ...]            │
│  ─────────────────────────              │
│  TAIL: Last N/2 lines                   │
└─────────────────────────────────────────┘
```

**Constants:**

| Limit | Value | Rationale |
|-------|-------|-----------|
| `MAX_LINES` | 2000 | Balances context usage vs. completeness |
| `MAX_BYTES` | 50KB | Prevents binary file accidents |
| `GREP_MAX_LINE_LENGTH` | 500 chars | Truncates minified code |

The algorithm:
1. Splits content into lines
2. If line count exceeds `MAX_LINES`, takes first 1000 and last 1000
3. Inserts a marker showing how many lines were omitted
4. If byte count still exceeds `MAX_BYTES`, applies byte-level truncation
5. Returns metadata indicating truncation occurred, enabling the LLM to request specific ranges

### Process Tree Management

The `bash` tool must handle runaway processes, infinite loops, and fork bombs without leaving orphans. The implementation uses the `sysinfo` crate to walk the process tree:

```rust
// Pseudocode for process cleanup
fn kill_process_tree(root_pid: Pid) {
    let system = System::new();
    let children = find_all_descendants(root_pid, &system);

    // Kill children first (deepest first), then parent
    for child in children.iter().rev() {
        kill(child, SIGKILL);
    }
    kill(root_pid, SIGKILL);
}
```

**Timeout behavior:**

1. Command starts with configurable timeout (default 120s)
2. Output streams to a rolling buffer in real-time
3. On timeout: SIGTERM sent, 5s grace period, then SIGKILL
4. Process tree walked and all descendants killed
5. Exit code set to indicate timeout vs. normal termination

To avoid orphaned background jobs (e.g. `cmd &`), the bash script installs an `EXIT` trap that waits for any remaining child processes and then exits with the original command's status.

This prevents the common failure mode where killing a shell leaves its children running.

### Session Tree Structure

Sessions use a tree structure rather than a flat list, enabling conversation branching (useful when exploring different approaches):

```
                    ┌─────────┐
                    │ Message │ (root)
                    │   #1    │
                    └────┬────┘
                         │
                    ┌────▼────┐
                    │ Message │
                    │   #2    │
                    └────┬────┘
                         │
              ┌──────────┼──────────┐
              │                     │
         ┌────▼────┐          ┌────▼────┐
         │ Message │          │ Message │ (branch)
         │   #3    │          │   #3b   │
         └────┬────┘          └────┬────┘
              │                    │
         ┌────▼────┐          ┌────▼────┐
         │ Message │          │ Message │
         │   #4    │          │   #4b   │
         └─────────┘          └─────────┘
```

**JSONL format (v3):**

Each line is a self-contained JSON object with a `type` discriminator:

```json
{"type":"session","version":3,"cwd":"/project","created":"2024-01-15T10:30:00Z"}
{"type":"message","id":"a1b2c3d4","parent":"root","role":"user","content":[...]}
{"type":"message","id":"e5f6g7h8","parent":"a1b2c3d4","role":"assistant","content":[...]}
{"type":"model_change","id":"i9j0k1l2","parent":"e5f6g7h8","model":"claude-sonnet-4-20250514"}
```

The `parent` field creates the tree. Replaying a session walks the tree from root to the current leaf. Branching creates a new message with a different `parent` than the previous continuation.

### Provider Abstraction

The `Provider` trait abstracts over different LLM backends:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn models(&self) -> &[Model];

    async fn stream(
        &self,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<impl Stream<Item = Result<StreamEvent>>>;
}
```

**Context structure:**

```rust
pub struct Context {
    pub system: Option<String>,      // System prompt
    pub messages: Vec<Message>,       // Conversation history
    pub tools: Vec<ToolDef>,          // Available tools with JSON schemas
}
```

**StreamOptions:**

```rust
pub struct StreamOptions {
    pub model: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub thinking: Option<ThinkingConfig>,  // Extended thinking settings
    pub stop_sequences: Vec<String>,
}
```

This design allows adding new providers (OpenAI, Gemini) without modifying the agent loop. Each provider translates the common types to its wire format and emits a unified `StreamEvent` stream.

---

## Tool Details

### read

Read file contents (optionally images):

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
- Set `timeout: 0` to disable the default timeout
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

## Performance Engineering

### Why Rust Matters for CLI Tools

CLI tools have different performance requirements than servers or GUI applications. The critical metric is **time-to-first-interaction**: how quickly can the user start typing after invoking the command?

| Phase | TypeScript/Node.js | Rust |
|-------|-------------------|------|
| Process spawn | ~10ms | ~10ms |
| Runtime initialization | 200-500ms | 0ms (no runtime) |
| Module loading | 100-300ms | 0ms (static linking) |
| JIT warmup | 50-100ms | 0ms (AOT compiled) |
| **Total** | **360-910ms** | **~10ms** |

This difference compounds with usage frequency. A developer invoking `pi` 50 times per day saves 15-45 minutes per week in startup latency alone.

### Binary Size Optimization

The release profile aggressively optimizes for size:

```toml
[profile.release]
opt-level = "z"      # Size optimization (not speed)
lto = true           # Link-time optimization across all crates
codegen-units = 1    # Single codegen unit (slower compile, better optimization)
panic = "abort"      # No unwinding machinery
strip = true         # Remove symbol tables
```

**Size breakdown (approximate):**

| Component | Contribution |
|-----------|-------------|
| Core binary logic | ~3MB |
| HTTP + TLS (rustls) | ~5MB |
| serde + JSON | ~1MB |
| clap (CLI) | ~1MB |
| Other dependencies | ~3MB |
| **Total (stripped)** | **~13-15MB** |

Compare to Node.js: the `node` binary alone is 80MB+, before any application code.

### Memory Usage

Rust's ownership model enables predictable memory usage without garbage collection pauses:

| State | Memory |
|-------|--------|
| Startup (idle) | ~15MB |
| Active session (small) | ~25MB |
| Large file in context | ~30-50MB |
| Streaming response | +0MB (streamed, not buffered) |

The absence of a GC means no surprise latency spikes during streaming output.

### Streaming Architecture

Responses stream token-by-token from the API to the terminal with minimal buffering:

```
API Server → TCP → SSE Parser → Event Handler → Terminal
     │                              │
     └──────── no buffering ────────┘
```

Each token appears on screen within milliseconds of leaving Anthropic's servers. The SSE parser processes events as they arrive rather than waiting for complete responses.

---

## Troubleshooting

For a more complete guide, see [docs/troubleshooting.md](docs/troubleshooting.md).

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

# Or in settings.json
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
rm ~/.pi/agent/sessions/--home-user-project--/corrupted-session.jsonl
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
| **Not all legacy providers** | Anthropic/OpenAI/Gemini/Azure supported; others TBD |
| **No web browsing** | Use bash with curl |
| **No GUI** | Terminal-only by design |
| **No plugins** | Fork and extend directly |
| **English-centric** | Works but not optimized for other languages |
| **Nightly Rust required** | Uses 2024 edition features |

---

## Design Philosophy

### Specification-First Porting

This port follows a "specification extraction" methodology rather than line-by-line translation:

1. **Extract behavior**: Study the TypeScript implementation to understand *what* it does, not *how*
2. **Document the spec**: Write down expected behaviors, edge cases, and invariants
3. **Implement from spec**: Write idiomatic Rust that satisfies the spec
4. **Conformance testing**: Verify behavior matches via fixture-based tests

This approach yields better code than mechanical translation. TypeScript idioms (callbacks, promises, class hierarchies) don't map cleanly to Rust (ownership, traits, enums). Fighting the language produces worse results than embracing it.

### Conformance Testing

The test suite includes fixture-based conformance tests that validate tool behavior:

```json
{
  "version": "1.0",
  "tool": "edit",
  "cases": [
    {
      "name": "edit_simple_replace",
      "setup": [
        {"type": "create_file", "path": "test.txt", "content": "Hello, World!"}
      ],
      "input": {
        "path": "test.txt",
        "oldText": "World",
        "newText": "Rust"
      },
      "expected": {
        "content_contains": ["Successfully replaced"],
        "details": {"oldLength": 5, "newLength": 4}
      }
    }
  ]
}
```

Each fixture specifies:
- **Setup**: Files/directories to create before the test
- **Input**: Tool parameters
- **Expected**: Output content patterns, exact field matches, or error conditions

This allows validating that the Rust implementation produces equivalent results to the TypeScript original without coupling to implementation details.

### No Plugin Architecture

Pi deliberately excludes a plugin system. The reasoning:

1. **Complexity cost**: Plugin systems require stable APIs, versioning, sandboxing, and documentation
2. **Security surface**: Plugins executing arbitrary code in a tool that runs shell commands is risky
3. **Maintenance burden**: Plugin compatibility across versions creates ongoing work
4. **Fork-friendly**: The codebase is small enough (~5K lines) that forking is practical

To add a tool: fork the repo, implement the `Tool` trait in `src/tools.rs`, and build your custom binary. This takes less time than learning a plugin API.

### Unsafe Forbidden

The `#![forbid(unsafe_code)]` directive is project-wide and non-negotiable. Rationale:

- **Attack surface**: Pi executes user-provided shell commands and reads arbitrary files
- **Memory bugs = security bugs**: Buffer overflows or use-after-free in this context could be exploitable
- **Performance irrelevant**: The bottleneck is network latency to the API, not CPU cycles
- **Dependencies audited**: All dependencies either use no unsafe or are well-audited (e.g., `rustls`)

The safe Rust subset provides all necessary functionality without compromising security.

---

## FAQ

**Q: What's the relationship to the original Pi Agent?**
A: This is an authorized Rust port of [Pi Agent](https://github.com/badlogic/pi) by [Mario Zechner](https://github.com/badlogic), created with his blessing. The architecture differs significantly from the TypeScript original: it uses [asupersync](https://github.com/Dicklesworthstone/asupersync) for structured concurrency and [rich_rust](https://github.com/Dicklesworthstone/rich_rust) (a port of Will McGugan's [Rich](https://github.com/Textualize/rich) library) for terminal rendering. The goal is idiomatic Rust while preserving Pi Agent's UX.

**Q: Why rewrite in Rust?**
A: Startup time matters when you're in a terminal all day. Rust gives us <100ms startup vs 500ms+ for Node.js. Plus, no runtime dependencies to manage.

**Q: Can I use OpenAI/Gemini models?**
A: Yes. Set `OPENAI_API_KEY` or `GOOGLE_API_KEY` and use `--provider`/`--model` (e.g. `--provider openai --model gpt-4o`).

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

### Release & Publishing

Releases are tag-driven and must align with `Cargo.toml` versions.

- Tag format: `vX.Y.Z` (pre-releases like `vX.Y.Z-rc.N` are allowed but skip crates.io publish).
- The tag version **must** match `package.version` in `Cargo.toml`.
- Publish order for dependencies: `asupersync` → `rich_rust` → `charmed-*` (lipgloss, bubbletea, bubbles, glamour) → `pi_agent_rust`.
- `.github/workflows/publish.yml` handles crates.io publish when `CARGO_REGISTRY_TOKEN` is set.

### Coverage

Coverage uses `cargo-llvm-cov`:

```bash
# One-time install
cargo install cargo-llvm-cov --locked
rustup component add llvm-tools-preview

# Summary (fastest)
cargo llvm-cov --all-targets --workspace --summary-only

# LCOV report (for CI/artifacts)
CI=true VCR_MODE=playback VCR_CASSETTE_DIR=tests/fixtures/vcr \
  cargo llvm-cov --all-targets --workspace --lcov --output-path lcov.info

# HTML report (defaults to target/llvm-cov/html)
cargo llvm-cov --all-targets --workspace --html
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
