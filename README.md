<p align="center">
  <img src="pi_agent_rust_illustration.webp" alt="Pi Agent Rust" width="600"/>
</p>

<h1 align="center">pi_agent_rust</h1>

<p align="center">
  <strong>pi_agent_rust - Native AI coding agent CLI written in Rust</strong>
</p>

<p align="center">
  <a href="#why-should-you-care">Why Should You Care?</a> •
  <a href="#tldr-piopenclaw-users">TL;DR</a> •
  <a href="#benchmark-methodology-and-claim-integrity">Methodology</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#features">Features</a> •
  <a href="#installation">Installation</a> •
  <a href="#commands">Commands</a> •
  <a href="#configuration">Configuration</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-2024%20edition-orange?logo=rust" alt="Rust 2024">
  <img src="https://img.shields.io/badge/license-MIT%20%2B%20Rider-blue" alt="License: MIT + Rider">
  <img src="https://img.shields.io/badge/unsafe-forbidden-brightgreen" alt="No Unsafe Code">
</p>

```bash
# Install latest release
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/main/install.sh?$(date +%s)" | bash
```

---

## The Problem

You want an AI coding assistant in your terminal, but existing tools are:
- **Slow to start**: Managed runtimes can add noticeable startup overhead
- **Resource intensive**: Electron apps or heavy runtimes can add substantial overhead
- **Unreliable**: Streaming breaks, sessions corrupt, tools fail silently
- **Hard to extend**: Closed ecosystems or complex plugin systems

## The Solution

**pi_agent_rust** is a from-scratch Rust port of [Pi Agent](https://github.com/badlogic/pi) by [Mario Zechner](https://github.com/badlogic) (made with his blessing!). Official release archives install the single end-user binary `pi`, with streaming responses and 9 built-in tools.

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

## Why Should You Care?

If you already use Pi Agent, especially through OpenClaw, this project keeps the core workflow while upgrading the engine under the hood:

- **A native single-binary design** intended to minimize startup and runtime overhead
- **A bounded-resource architecture** for long-running sessions
- **A capability-gated security model** for extension/tool execution, including command-level blocking of dangerous extension shell patterns

Security is a first-class design goal here, not a bolt-on:

- Capability-gated hostcalls (`tool`/`exec`/`http`/`session`/`ui`/`events`)
- Two-stage extension `exec` enforcement: capability gate first, then command mediation that blocks critical shell classes by default (for example recursive delete, disk/device writes, reverse shell) and can tighten to block high-tier classes in strict/safe policy
- Policy + runtime risk + quota enforcement on the execution path
- Per-extension trust lifecycle (`pending` -> `acknowledged` -> `trusted` -> `killed`) with kill-switch audit logs and explicit operator provenance
- Hostcall-lane emergency controls that can force compatibility-lane execution globally or for one extension when fast-lane behavior needs immediate containment
- Structured concurrency via `asupersync` for more predictable cancellation/lifecycle behavior
- Auditable runtime signals/ledgers and redacted security alerts for extension behavior

## TL;DR (Pi/OpenClaw Users)

The Rust port is designed around large-session, multi-agent, and extension-heavy
workloads. Release-facing performance numbers are published only when the
checked-in evidence artifacts are current, have matching run provenance, and
show data and a passing result for every declared budget, with no data-contract
failures. Historical benchmark snapshots
are retained in planning/evidence artifacts, but they are not treated as current
README claims until the performance evidence gate is regenerated cleanly.

Extension runtime guarantees are also concrete:

| Extension assurance signal | Why you should care |
|---|---|
| Two-stage `exec` guard (`exec` capability policy + command-level mediation + DCG/heredoc AST signals) | Dangerous shell intent is caught before spawn, including destructive payloads hidden in multiline wrappers |
| Trust lifecycle + kill switch (`pending/acknowledged/trusted/killed`) | You can quarantine an extension instantly, log who pulled the switch and why, and require explicit re-acknowledgement before restoring access |
| Hostcall lane kill-switch controls (`forced_compat_global_kill_switch`, `forced_compat_extension_kill_switch`) | Fast-path regressions can be contained immediately by forcing compatibility-lane execution without disabling the extension system |
| Deterministic hostcall reactor mesh (shard affinity, bounded SPSC lanes, backpressure telemetry, optional NUMA slab tracking) | Runtime behavior stays predictable under contention; queue pressure and routing decisions are observable instead of opaque |
| Cold owner-isolated JS realms + persistent transpile cache | Every reload gets a fresh realm while versioned disk-cached transpilation avoids treating mutable JavaScript state as safely reusable |
| Tamper-evident runtime risk ledger (`verify` / `replay` / `calibrate`) | Security decisions are hash-linked and can be replayed or threshold-tuned from real runtime traces |

Bottom line: Pi's architecture targets lower latency, lower memory use, and
stronger extension runtime safety under real workload pressure; current numeric
claims must come from fresh, provenance-matched evidence artifacts.

<sub>Data source: `docs/planning/BENCHMARK_COMPARISON_BETWEEN_RUST_VERSION_AND_ORIGINAL__GPT.md` (latest secure-path + full orchestrator checkpoints, 2026-04-23).</sub>

### README Citation Convention

All numeric performance claims in this README include inline citations with format:
`*(from [artifact-path], run [correlation-id])*`

Example: `*(from [artifact-path], run [correlation-id])*`

CI checks both file freshness and artifact content so stale, no-data, or
correlation-mismatched evidence cannot back user-facing performance claims.
The README evidence checker reports line-numbered proof obligations for cited
claims and extracts claim-gated performance phrases for reviewer audit. Explicit
historical snapshot citations are mapped separately and do not satisfy current
release-facing claims.

## Performance-Oriented Architecture

In this README, `we` means the project owner and collaborating coding agents.  
The design concentrates performance work in several runtime layers rather than
assuming one optimization proves an end-to-end result.

| Technique | What we do | Runtime intent |
|---|---|---|
| Cold-start minimization | Single native binary, no Node/Bun runtime bootstrap, no JIT warmup, startup prewarm for extension runtime paths | Reduce time-to-first-interaction |
| Less copying on hot paths | `Arc`/`Cow` message flow, zero-copy hostcall/tool payload handling, reduced clone-heavy provider/session paths | Reduce CPU and allocation pressure |
| Deterministic dispatch core | Typed hostcall opcodes, fast-lane/compat-lane routing, bounded shard queues with reactor-mesh telemetry | Reduce tail latency under concurrent extension load |
| Efficient long-session storage | SQLite session index + v2 sidecar (segmented log + offset index) with O(index+tail) reopen path | Avoid full-history work on eligible resumes |
| Streaming parser tuned for real networks | SSE parser tracks scanned bytes, handles UTF-8 tails, normalizes chunk boundaries, interns event-type strings | Reduce repeated scanning and parser stalls |
| Safe fast-path controls | Shadow dual execution sampling, automatic backoff on divergence/overhead, compatibility-lane kill switches for containment | Bound optimization risk and preserve fallback behavior |
| CI-level performance governance | Scenario matrices, strict artifact contracts, fail-closed perf gates | Detect regressions before release |

If you want the full implementation inventory, see [Performance Engineering](#performance-engineering).

## Benchmark Methodology and Claim Integrity

The benchmark evidence policy is designed to keep results realistic, reproducible, and hard to game.

What we measured:

- **Matched-state workloads**: resume a large session and append the same 10 messages.
- **Realistic E2E workloads**: resume + append + extension activity + slash-style state changes + forks + exports + compactions.
- **Scale levels**: from `100k` up to `5M` token-class session states.
- **Startup/readiness**: command-level readiness (`--help`, `--version`) separately from long-session workflows.

How we kept comparisons fair:

- **Two scopes** in the benchmark report:
  - apples-to-apples (`pi_agent_rust` vs legacy `coding-agent`)
  - apples-to-oranges (legacy stack components included where legacy behavior is outsourced)
- **Release-mode binaries** and repeated runs per matrix cell.
- **No paid-provider noise** in core latency/footprint tables (provider-call costs are excluded from these core comparisons).

How we kept claims honest:

- **Security controls stayed on** during secure-path measurements (no policy/risk/quota bypasses for speed claims).
- **Raw artifacts are preserved** (JSON/trace/time outputs) and called out in the benchmark report.
- **Blockers are explicitly disclosed**: when direct legacy reruns were blocked by missing workspace deps, we state that and compare against prior validated legacy artifacts instead of pretending reruns succeeded.
- **Interpretation notes are explicit**: the report distinguishes baseline sections vs fresh reruns so readers can see exactly which values came from which run set.
- **Reproducibility over marketing**: methodology, caveats, and known limits are included alongside wins.

If you want full details, see:

- `docs/planning/BENCHMARK_COMPARISON_BETWEEN_RUST_VERSION_AND_ORIGINAL__GPT.md` (methodology + results + caveats + raw artifact paths)

## Why Pi?

| Feature | Pi (Rust) | Typical TS/Python CLI |
|---------|-----------|----------------------|
| **Startup** | Native single-binary path; fresh release measurement pending | Runtime-dependent |
| **Binary size** | Size-budgeted release profile; fresh release measurement pending | Runtime-dependent |
| **Memory (idle)** | Bounded-resource design; fresh release measurement pending | Runtime-dependent |
| **Streaming** | Native SSE parser | Library-dependent |
| **Tool execution** | Process tree management | Basic subprocess |
| **Sessions** | JSONL with branching | Varies |
| **Unsafe code** | Forbidden | N/A |

## Quick Example

```bash
# 1) Start an interactive session
pi

# 2) Ask a codebase question
pi "Summarize the architecture in src/"

# 3) Attach a file inline
pi @src/main.rs "Explain startup flow"

# 4) Run single-shot mode for scripting
pi -p "List likely regression risks for this diff"

# 5) Continue your last project session
pi --continue

# 6) Inspect available models/providers
pi --list-models
pi --list-providers
```

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

The terminal UI uses rich_rust for all output formatting, providing the same visual quality as Rich-based Python tools.

---

## Quick Start

### 1. Install

```bash
# Install latest release binary
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/main/install.sh?$(date +%s)" | bash
```

If you already have the original TypeScript `pi` installed, the installer asks
whether to make Rust Pi canonical as `pi` and automatically create `legacy-pi`
for the old command.

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

### Using a local model

`ollama`, `llamacpp` (llama.cpp's `llama-server`), `mistralrs` (mistral.rs), and
`lmstudio` are built-in **local** providers. The first three need **no API key** and
work out of the box against their default ports:

```bash
pi --provider ollama    --model llama3        -p "hi"
pi --provider llamacpp  --model <gguf-repo-id> -p "hi"
pi --provider mistralrs --model default        -p "hi"
```

To point at any other OpenAI-compatible server (a custom host/port, vLLM, etc.),
add it to `~/.pi/agent/models.json`:

```json
{
  "providers": {
    "ollama": {
      "api": "openai-completions",
      "baseUrl": "http://127.0.0.1:11000/v1",
      "apiKey": "ollama",
      "models": [{ "id": "Qwen3.6-35B-A3B-4bit", "contextWindow": 32768 }]
    }
  }
}
```

Then `pi --provider ollama --model Qwen3.6-35B-A3B-4bit`. See
[docs/models.md](docs/models.md) for the full `models.json` schema, provider
aliases, and secret resolution (env vars and `!command` shell lookups).

### Refreshing provider model catalogs

For OpenAI-compatible providers, model discovery is a standalone command: it
prints one model ID per stdout line and exits without starting the TUI.

```bash
# Use a successful live response or (with a warning on stderr) the static
# registry when live discovery is unavailable
pi --fetch-models openrouter

# Bypass the cache and require a genuinely live response; never fall back
pi --fetch-models openrouter --refresh-models

# Make a successful live/cache catalog available to future --list-models and
# interactive /model pickers
pi --fetch-models openrouter --refresh-models --persist-models
```

Each standalone CLI invocation starts a new process, so its in-memory cache is
fresh. The five-minute cache only avoids repeat discovery calls made within one
long-lived process by SDK/library users; `--refresh-models` bypasses that cache.

Persistence is opt-in. The v2 `~/.pi/agent/models.fetched.json` schema stores
provider/model IDs, the fetch timestamp, and a non-secret SHA-256 identity
binding membership to the provider, API, query-free endpoint, auth-header
mode, recognized credential-query ordered name/presence shape, and
request-header name/presence shape that produced it. Pi never persists a static fallback, API
credentials, URL query values, or request-header values—not even as
offline-verifiable hashes. Credential-channel values are rotation-tolerant.
Any non-empty query/header value outside a narrowly recognized credential
channel may identify a tenant or deployment, so Pi refuses to persist that
catalog (and ignores legacy persisted rows for such a current route) rather
than reuse unverifiable membership. If a safely bound route shape no longer
matches, Pi ignores those generated rows and reports how to refresh them.
Credential rotation alone does not invalidate the catalog. The generated
membership is likewise not account-bound: switching accounts on the same
endpoint/transport shape can retain the prior account's saved model list until
you rerun `--fetch-models <provider> --refresh-models --persist-models`.
Inference still resolves and sends the current account's credential; only the
opt-in model-membership list can be stale across that switch. The generated
catalog is loaded first; your hand-written `~/.pi/agent/models.json` is loaded
afterward and remains authoritative. Pi does not rewrite or merge that
user-authored file.
Legacy `pi.models.fetched.v1` files cannot be rebound safely because they lack
this provenance, so Pi preserves them instead of overwriting them. Move the
legacy file aside to `models.fetched.v1.backup.json`, then rerun the verified
live refresh and persist command above to create a v2 catalog.

---

## Features

### Streaming Responses

Real-time response streaming with extended thinking support:

```
pi "Write a quicksort implementation"
```

Watch the response appear incrementally, with thinking blocks shown inline.

### 9 Built-in Tools

| Tool | Description | Example |
|------|-------------|---------|
| `read` | Read file contents, supports images | Read src/main.rs |
| `write` | Create or overwrite files | Write a new config file |
| `edit` | Surgical string replacement | Fix the typo on line 42 |
| `hashline_edit` | Precise edits using LINE#HASH tags | Apply edits to specific lines using hashline anchors |
| `bash` | Execute shell commands with timeout | Run the test suite |
| `grep` | Search file contents with context | Find all TODO comments |
| `find` | Discover files by pattern | Find all *.rs files |
| `ls` | List directory contents | What's in src/? |
| `subagent` | Delegate isolated work to a named Rust Pi child agent | Ask a scout to inspect a provider |

All tools include:
- Automatic truncation for large outputs (2000 lines / 1MB)
- Detailed metadata in responses
- Process tree cleanup for bash (no orphaned processes)

`subagent` is intentionally opt-in because it can start additional coding-agent
processes. Enable it explicitly with `--tools` (or in the tools setting):

```bash
pi --tools read,bash,edit,write,grep,find,ls,hashline_edit,subagent \
  "Use the scout agent to inspect the provider implementation."
```

### Native Subagents and Orchestration

Rust Pi includes a native `subagent` tool; it does not depend on a QuickJS
extension and never resolves a child executable by assuming a `pi` binary on
`PATH`. By default it starts the current Rust Pi executable. Set
`PI_SUBAGENT_PI_BINARY=/absolute/path/to/rpi` only when an explicit binary
override is needed.

Agent definitions are Markdown files in `$PI_CODING_AGENT_DIR/agents/*.md`
(normally `~/.pi/agent/agents/*.md`) or the nearest
`.pi/agents/*.md`. Project definitions take precedence over same-named user
definitions. The process inherits the parent's provider, router, authentication,
and model-registry environment, including `PI_CODING_AGENT_DIR`.

```markdown
---
name: scout
description: Inspect code and return concise, evidence-backed findings.
model: ai-router/gpt-5.6-sol
reasoning: low
tools: read,grep,find,ls
skills: ../skills/codebase-archaeology/SKILL.md
---

Read relevant code before answering. Do not edit files.
```

The tool accepts exactly one workflow shape: a single `{ agent, task }`, a
bounded parallel `tasks` array (up to 8, default concurrency 4), or a sequential
`chain` array. In chains, `{previous}` in a task is replaced by the preceding
child's final output. Children run headlessly in isolated ephemeral sessions,
stream progress into the parent TUI/JSON output, return structured status and
stderr details, and are killed/reaped if the parent is cancelled. Child agents
receive the declared tool allowlist; the default child allowlist deliberately
excludes `subagent` to prevent accidental recursive delegation.

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

Thinking levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`

### Customization (Skills & Prompt Templates)

- **Skills**: Drop `SKILL.md` under `~/.pi/agent/skills/` or `.pi/skills/` and invoke with `/skill:name`.
- **Prompt templates**: Markdown files under `~/.pi/agent/prompts/` or `.pi/prompts/`; invoke via `/<template> [args]`.
- **Packages**: Share bundles with `pi install npm:@org/pi-packages` (skills, prompts, themes, extensions).

### Autocomplete

Pi provides context-aware autocomplete in the interactive editor:

- **`@` file references**: Type `@` followed by a path fragment to attach file contents. The completion engine indexes project files (respecting `.gitignore`) via the `ignore` crate's `WalkBuilder`, capping at 5,000 entries.
- **`/` slash commands**: Built-in commands (`/help`, `/model`, `/tree`, `/clear`, `/compact`, `/exit`) and user-defined prompt templates and skills all appear as completions.
- **Fuzzy scoring**: Prefix matches rank above substring matches. Results are sorted by match quality, then by kind (commands > templates > skills > files > paths).
- **Background refresh**: A background thread re-indexes the project file tree every 30 seconds, so completions stay current without blocking the input loop.

### Three Execution Modes

Pi runs in three modes, each suited to different workflows:

| Mode | Invocation | Use Case |
|------|-----------|----------|
| **Interactive** | `pi` (default) | Full TUI with streaming, tools, session branching, autocomplete |
| **Print** | `pi -p "..."` | Single response to stdout, no TUI, scriptable |
| **RPC** | `pi --mode rpc` | Headless JSON protocol over stdin/stdout for IDE integrations |

**Interactive mode** provides the full experience: a multi-line text editor with history, scrollable conversation viewport, model selector (`Ctrl+L`), scoped model cycling (`Ctrl+P`/`Ctrl+Shift+P`), session branch navigator (`/tree`), and real-time token/cost tracking.

**Print mode** sends one message, streams the response to stdout, and exits. Useful for shell scripts and one-off queries.

**RPC mode** exposes a line-delimited JSON protocol for programmatic control. Clients send commands (`prompt`, `steer`, `follow-up`, `abort`, `get-state`, `compact`) and receive streaming events. This is how IDE extensions and custom frontends integrate with Pi. See [RPC Protocol](#rpc-protocol) for the wire format.

### Extensions

Pi supports two extension runtime families with capability-gated host connectors:

- JS/TS entrypoints run **without Node or Bun** in an embedded QuickJS runtime.
- `*.native.json` descriptors run in the native-rust descriptor runtime.

- Extension entrypoints are auto-detected:
  - `.js/.ts/.mjs/.cjs/.tsx/.mts/.cts` run directly in embedded QuickJS (no descriptor conversion).
  - `*.native.json` loads the native-rust descriptor runtime.
  - One session currently uses one runtime family at a time (JS/TS or native descriptor).
- Cold/warm extension load paths are instrumented; fresh `v0.2.0` measurements are pending
- Node API shims for `fs`, `path`, `os`, `crypto`, `child_process`, `url`, and more
- Capability-based security: extensions call explicit connectors (`tool/exec/http/session/ui`) with audit logging
- Command-level exec mediation: dangerous shell signatures are classified and blocked before spawn, with redacted denial alerts and mediation ledger entries
- Trust-state lifecycle and kill-switch controls with audited state transitions (`pending`/`acknowledged`/`trusted`/`killed`)
- Hostcall reactor mesh with deterministic shard routing, bounded queue backpressure, and optional NUMA-aware telemetry
- Fresh owner-isolated realms on every reload, with versioned persistent transpile caching instead of mutable realm reuse

### Credential-Aware Model Selection

- `/model` (or `Ctrl+L`) opens a selector focused on models that are ready to run with current credentials.
- `Ctrl+P` and `Ctrl+Shift+P` cycle through the scoped model set without opening the overlay.
- Provider IDs and aliases are matched case-insensitively in model selection and `/login`.
- Models that do not require configured credentials can run keyless.

Extensions can register tools, slash commands, event hooks, flags, providers,
and shortcuts. See [EXTENSIONS.md](docs/planning/EXTENSIONS.md) for the full architecture
and [docs/extension-catalog.json](docs/extension-catalog.json) for the
223-entry catalog with per-extension conformance status and perf budgets.

## Extension Validation Pipeline

This project validates extension compatibility with a three-track pipeline:

- **Vendored corpus (224)**: deterministic conformance, compatibility matrix, and scenario suites.
- **Unvendored corpus (777)**: source acquisition and onboarding prioritization.
- **Release-binary live-provider E2E**: real `target/release/pi` execution against a non-mocked provider/model path.

### Why this exists

- Catch runtime/API regressions in QuickJS host shims and capability policy.
- Catch dangerous extension shell call patterns with real command mediation on the release binary path.
- Verify extension behavior against real provider responses, not just fixture/mocked flows.
- Keep extension support measurable instead of anecdotal.
- Produce a prioritized queue for onboarding unvendored candidates into vendored conformance.

### Pipeline components

1. **Fetch unvendored source corpus**
   - Binary: `ext_unvendored_fetch_run`
   - Typical command:
     - `cargo run --example ext_unvendored_fetch_run -- run-all --workers 8 --no-probe`
   - Purpose:
     - Clones GitHub repos and unpacks npm tarballs into `.tmp-codex-unvendored-cache/`
     - Produces machine-readable acquisition status for all unvendored candidates
   - Artifacts:
     - `tests/ext_conformance/reports/pipeline/unvendored_fetch_probe_report.json`
     - `tests/ext_conformance/reports/pipeline/unvendored_fetch_probe_events.jsonl`

2. **Run end-to-end validation orchestration**
   - Binary: `ext_full_validation`
   - Typical command:
     - `cargo run --example ext_full_validation --`
   - Stages (in order):
     1. `refresh_onboarding_queue` (runs `ext_onboarding_queue`)
     2. `conformance_shard_0..N` (runs `ext_conformance_generated` sharded matrix)
     3. `conformance_failure_dossiers`
     4. `provider_compat_matrix`
     5. `scenario_conformance_suite`
     6. `auto_repair_full_corpus`
     7. `differential_suite` (optional, enabled via `--run-diff`; npm diff via `--run-npm-diff`)
   - Artifacts:
     - `tests/ext_conformance/reports/pipeline/full_validation_report.json`
     - `tests/ext_conformance/reports/pipeline/full_validation_report.md`
     - Plus stage-specific reports under `tests/ext_conformance/reports/**`

3. **Run dev-firstset live-provider gate (must pass before release build)**
   - Binary: `ext_release_binary_e2e`
   - Typical command:
     - `cargo build --bin pi --bin ext_release_binary_e2e`
     - `PI_HTTP_REQUEST_TIMEOUT_SECS=0 target/debug/ext_release_binary_e2e --pi-bin target/debug/pi --provider ollama --model qwen2.5:0.5b --jobs 10 --timeout-secs 600 --max-cases 20 --extension-policy balanced --out-json tests/ext_conformance/reports/release_binary_e2e/ollama_firstset_dev_20260219_jobs10_timeout600.json --out-md tests/ext_conformance/reports/release_binary_e2e/ollama_firstset_dev_20260219_jobs10_timeout600.md`
   - Purpose:
     - Proves the current codepath works end-to-end on a representative first-set before paying release-build cost.
     - Serves as the promotion gate to full release-binary validation.
   - Gate:
     - Require `pass=20 / total=20` with `fail=0`.
   - Artifacts:
     - `tests/ext_conformance/reports/release_binary_e2e/ollama_firstset_dev_20260219_jobs10_timeout600.json`
     - `tests/ext_conformance/reports/release_binary_e2e/ollama_firstset_dev_20260219_jobs10_timeout600.md`

4. **Run full release-binary live-provider E2E (after step 3 passes)**
   - Binary: `ext_release_binary_e2e`
   - Typical command:
     - `cargo build --release --bin pi --bin ext_release_binary_e2e`
     - `PI_HTTP_REQUEST_TIMEOUT_SECS=0 target/release/ext_release_binary_e2e --pi-bin target/release/pi --provider ollama --model qwen2.5:0.5b --jobs 10 --timeout-secs 600 --extension-policy balanced --out-json tests/ext_conformance/reports/release_binary_e2e/ollama_full_release_20260219_jobs10_timeout600.json --out-md tests/ext_conformance/reports/release_binary_e2e/ollama_full_release_20260219_jobs10_timeout600.md`
   - Purpose:
     - Executes `target/release/pi` directly for each selected extension case.
     - Uses a live provider/model path (default `ollama` + `qwen2.5:0.5b`) to exercise non-mocked end-to-end behavior.
     - Emits per-case stdout/stderr captures plus summary artifacts (`pi.ext.release_binary_e2e.v1`).
   - Artifacts:
     - `tests/ext_conformance/reports/release_binary_e2e/ollama_full_release_20260219_jobs10_timeout600.json`
     - `tests/ext_conformance/reports/release_binary_e2e/ollama_full_release_20260219_jobs10_timeout600.md`
     - `tests/ext_conformance/reports/release_binary_e2e/cases/*`

5. **Aggregate and triage**
   - `full_validation_report.json` combines:
     - Stage-level pass/fail (`stageSummary`, `stageResults`)
     - Corpus counts (`corpus`)
     - Vendored conformance totals (`conformance`)
     - Provider matrix totals (`providerCompat`)
     - Scenario totals (`scenario`)
     - Review queue + verdict classification (`reviewQueue`, `verdictCounts`)
   - Important interpretation rule:
     - `not_tested_unvendored` indicates unvendored candidates not yet in vendored conformance; this is inventory status, not a vendored regression.

### Recommended run environment

These runs compile many crates and can be disk-heavy. Point Cargo artifacts and temp files to a large volume:

```bash
export CARGO_TARGET_DIR="/data/tmp/pi_agent_rust_cargo/${USER:-agent}/target"
export TMPDIR="/data/tmp/pi_agent_rust_cargo/${USER:-agent}/tmp"
mkdir -p "$CARGO_TARGET_DIR" "$TMPDIR"
```

Then run:

```bash
cargo run --example ext_unvendored_fetch_run -- run-all --workers 8 --no-probe
cargo run --example ext_full_validation --
```

### Historical run snapshot (extension gate refresh 2026-05-15)

These artifacts predate `v0.2.0` and do not certify the current source revision.
Current strict drop-in status is **NOT CERTIFIED** until the active contract is
rerun successfully from a clean release source commit and the final release ref
contains no later non-evidence changes. The figures below are retained only as
a dated historical snapshot.

The current extension must-pass corpus is the exact set selected by
`docs/extension-inclusion-list.json`; the historical counts below do not prove
coverage of that authoritative list.

From:
- `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json` (generated `2026-05-15T17:03:02.000Z`, run `local-20260515T170218075Z`)
- `tests/ext_conformance/reports/health_delta/health_delta_report.json` (generated `2026-05-13T03:37:59.568Z`)
- `tests/ext_conformance/reports/journeys/journey_report.json` (generated `2026-05-13T02:59:58.302Z`)
- `tests/evidence_bundle/index.json` (generated `2026-05-12T19:26:21.441Z`, run `local-20260512T192621Z`)
- `tests/full_suite_gate/certification_verdict.json` (generated `2026-05-14T19:59:37.227Z`)
- Historical verdict blob at `2fc4b8c0b77ded267cf5e0f517f4b6fa87f45e91:docs/evidence/dropin-certification-verdict.json` (generated `2026-05-18T19:37:26Z` for source `52e9fbfb24352045985b59df9d7ea63f1f8f2ef8`; the live file may contain a later verdict)

- Historical strict drop-in result: **22/22 certification gates PASS, 16/16 blocking gates PASS** - `CERTIFIED` for source `52e9fbfb24352045985b59df9d7ea63f1f8f2ef8` only *(from the Git-pinned historical verdict blob above; it neither describes the live verdict file nor certifies `v0.2.0`)*
- Unified evidence bundle: `29/29` sections present, `0` missing, `0` invalid *(from tests/evidence_bundle/index.json)*
- Historical extension gate: `123/123` then-observed must-pass extensions passed; informational stretch set `100/101` passed with one non-blocking stretch failure *(from tests/ext_conformance/reports/gate/must_pass_gate_verdict.json)*
- Extension health delta: `223/223` tested extensions passed (`100.0%`), `0` regressions, `13` fixes vs the 2026-02-07 baseline, with `1` intentionally excluded test fixture disclosed in the report *(from tests/ext_conformance/reports/health_delta/health_delta_report.json)*
- Health-delta full-manifest non-pass extensions: `0`; `base_fixtures` is a test-only negative fixture excluded from release-facing pass-rate claims with disposition recorded in `docs/evidence/extension-health-delta-failure-disposition.json`.
- Extension journey coverage: `123/123` journey scenarios passed (`100.0%`); command, event-subscriber, multi-capability, passive, and tool-provider categories are green *(from tests/ext_conformance/reports/journeys/journey_report.json)*
- Historical stress-triage evidence is retained under `tests/perf/reports/`; it is not current enough to support a `v0.2.0` performance claim.

---

## Installation

### Curl Installer (Recommended)

```bash
# Latest release
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/main/install.sh?$(date +%s)" | bash

# Non-interactive + auto PATH update
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/main/install.sh?$(date +%s)" | bash -s -- --yes --easy-mode

# Pin a release tag
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/v0.2.0/install.sh" | bash -s -- --version v0.2.0

# Install from explicit artifact URL + checksum URL
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/v0.2.0/install.sh" | \
  bash -s -- \
    --artifact-url "https://github.com/Dicklesworthstone/pi_agent_rust/releases/download/v0.2.0/pi-linux-amd64.tar.xz" \
    --checksum-url "https://github.com/Dicklesworthstone/pi_agent_rust/releases/download/v0.2.0/SHA256SUMS"

# Skip completion setup (CI/non-interactive minimal install)
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/main/install.sh?$(date +%s)" | \
  bash -s -- --yes --no-completions
```

The installer is idempotent and supports a migration path from TypeScript Pi:
- Detect existing TS `pi` command
- Prompt to install Rust Pi as canonical `pi`
- Preserve old CLI behind `legacy-pi`
- Record state for clean uninstall/restore

Notable installer flags:
- `--offline [TARBALL]`: enforce offline mode; optional local artifact path (`.tar.gz`, `.tar.xz`, `.zip`, or raw binary)
- `--artifact-url`: force a specific release artifact URL
- `--checksum` / `--checksum-url`: override checksum source for explicit artifacts
- `--sigstore-bundle-url`: override Sigstore bundle URL used by `cosign verify-blob`
- `--completions auto|off|bash|zsh|fish`: force shell completion install target (`off` is equivalent to `--no-completions`)
- `--no-completions`: disable completion installation
- `--no-agent-skills`: skip automatic installation of the `pi-agent-rust` skill into `~/.claude/skills/` and `~/.codex/skills/`
- `--no-verify`: skip checksum + signature verification (testing only)
- `--artifact-url` without `--version` uses a synthetic tag for release mode only; if artifact download fails, install exits instead of attempting source fallback
- Installer honors `HTTPS_PROXY` / `HTTP_PROXY` for all network fetches

By default, the installer also installs a `pi-agent-rust` skill for both Claude Code and Codex CLI:
- Claude Code: `~/.claude/skills/pi-agent-rust/SKILL.md`
- Codex CLI: `~/.codex/skills/pi-agent-rust/SKILL.md` (or `$CODEX_HOME/skills/pi-agent-rust/SKILL.md` if `CODEX_HOME` is set)
- During upgrades, installer-managed legacy pre-tool entries from older versions are removed automatically (idempotent, path-scoped, and non-destructive) when prior installer state is present.

Installer regression harness (options + checksum + signature + completions):

```bash
bash tests/installer_regression.sh
```

### Distribution Compatibility Contract (Packaging/Invocation Scope)

For migration adoption, packaging and invocation compatibility follows this contract:

- This section covers packaging/invocation behavior only. Strict functional-parity claims are governed by `docs/contracts/dropin-certification-contract.json` and a provenance-matched `docs/evidence/dropin-certification-verdict.json`; `docs/parity-certification.json` is informational progress evidence only.

- Canonical executable name is `pi` across release assets and installer-managed installs.
- Installer-managed installs also create an `rpi` compatibility launcher when no conflicting `rpi` command already exists on your PATH.
- Existing TypeScript `pi` installs can be migrated in place; the prior command is preserved as `legacy-pi`.
- If you keep TypeScript `pi` as canonical (`--keep-existing-pi`), Rust Pi is installed as `pi-rust`.
- On Apple Silicon, the installer prefers the native arm64 artifact even when launched from a Rosetta-translated shell.
- Version-pinned installs are supported via `install.sh --version vX.Y.Z` for deterministic rollouts.
- Every GitHub release ships platform binaries plus `SHA256SUMS` for integrity validation.

Representative smoke checks:

```bash
# Canonical command should exist and execute
command -v pi
pi --version
pi --help >/dev/null

# If a TS migration was performed, legacy command remains available
command -v legacy-pi && legacy-pi --version
```

### From Source

Repository builds are tested with the exact toolchain pinned in
`rust-toolchain.toml` (`nightly-2026-07-05`). The locked dependency graph
requires Rust 1.95 or newer; use the pin for release-reproducible builds:

```bash
# Install the repository's pinned toolchain and components
rustup toolchain install nightly-2026-07-05 --component rustfmt --component clippy

# Clone and build
git clone https://github.com/Dicklesworthstone/pi_agent_rust.git
cd pi_agent_rust
cargo build --release

# Binary is at target/release/pi
./target/release/pi --version

# To install system-wide (--locked ensures reproducible dependency resolution)
cargo install --path . --bin pi --locked
```

### Dependencies

Pi has minimal runtime dependencies:
- `fd`: Required for the `find` tool (install via `apt install fd-find` or `brew install fd`)
- `rg`: Required for the `grep` tool (install via `apt install ripgrep` or `brew install ripgrep`)

### Uninstall

```bash
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/main/uninstall.sh" | bash
```

By default, uninstall removes installer-managed Rust binaries/aliases and skill directories,
then restores a migrated TypeScript `pi` if one was preserved.

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
| `-r, --resume` | Open session picker UI |
| `--session <PATH>` | Open specific session file |
| `--session-dir <DIR>` | Override session storage directory for this run |
| `--session-durability strict|balanced|throughput` | Tune persistence durability mode |
| `--no-session` | Don't persist conversation |
| `-p, --print` | Single response, no interaction |
| `--mode text|json|rpc` | Output/protocol mode |
| `--provider <NAME>` | Force provider for this run (aliases supported) |
| `--model <MODEL>` | Model to use (auto-select fallback: `anthropic/claude-sonnet-4-6`, then `anthropic/claude-opus-4-7`, then `openai/gpt-5.1-codex`) |
| `--thinking <LEVEL>` | Thinking level: off/minimal/low/medium/high/xhigh/max |
| `--tools <TOOLS>` | Comma-separated tool list |
| `--api-key <KEY>` | API key (or use provider-specific env vars such as `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.) |
| `--extension-policy safe|balanced|permissive` | Extension capability profile |
| `--repair-policy off|suggest|auto-safe|auto-strict` | Extension auto-repair policy |
| `--list-models [PATTERN]` | List available models (optional fuzzy filter) |
| `--list-providers` | List canonical provider IDs, aliases, and auth env keys |
| `--fetch-models <PROVIDER>` | Print provider model IDs to stdout and exit; warns when using the static fallback |
| `--refresh-models` | With `--fetch-models`, bypass the same-process cache and require a successful live response |
| `--persist-models` | With `--fetch-models`, atomically save a verified live/same-process-cache catalog for future model pickers |
| `--export <PATH>` | Export session file to HTML |

Additional high-leverage flags:

- `--no-migrations` to skip startup migration checks
- `--explain-extension-policy` to print effective capability decisions and exit
- `--explain-repair-policy` to print effective repair-policy resolution and exit

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

More utility subcommands:

```bash
# Extension catalog index + discovery
pi update-index
pi search "git"
pi info pi-search-agent

# Environment and extension diagnostics
pi doctor
pi doctor --only sessions --format json
pi doctor --only swarm --format json
pi doctor ./path/to/extension --policy safe --fix

# Read-only swarm progress SLO evaluation from normalized evidence
pi swarm-progress --input progress-slo-input.json --format json
pi swarm-progress --input progress-slo-input.json --since HEAD~1 --out-json progress-slo.json

# Session storage migration (JSONL -> v2 sidecar store)
pi migrate ~/.pi/agent/sessions --dry-run
pi migrate ~/.pi/agent/sessions
```

- `update-index` refreshes extension index metadata used by `search` and `info`.
- `search` and `info` let you discover and inspect extension metadata without leaving the CLI.
- `doctor` checks config, directories, auth, shell setup, sessions, swarm coordination readiness, and extension compatibility. `pi doctor --only swarm --format json` also reports cgroup CPU quota, cpuset size, NUMA topology, cgroup memory limits, target/tmp headroom, and recommended concurrency budgets before large multi-agent runs.
- `swarm-progress` evaluates a normalized progress SLO snapshot and emits advisory JSON/text only; it does not mutate Beads, git, Agent Mail, RCH, validation broker slots, runpacks, or source files. Operator workflow, privacy boundaries, degraded Agent Mail/RCH interpretation, stale-Beads handling, and no-open-work convergence guidance lives in [docs/swarm-operations-runbook.md#progress-slo-operator-workflow](docs/swarm-operations-runbook.md#progress-slo-operator-workflow).
- `migrate` validates or creates the v2 session sidecar format for faster resume on larger histories.

---

## Configuration

Pi reads configuration from `~/.pi/agent/settings.json`:

```json
{
  "default_provider": "anthropic",
  "default_model": "claude-opus-4-5",
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

### Configuration Precedence

Settings are resolved in priority order (first match wins):

1. **CLI flags** (`--model`, `--thinking`, `--provider`, etc.)
2. **Environment variables** (`ANTHROPIC_API_KEY`, `PI_CONFIG_PATH`, etc.)
3. **Project settings** (`.pi/settings.json` in the working directory)
4. **Global settings** (`~/.pi/agent/settings.json`)
5. **Built-in defaults**

This means a CLI flag always overrides a `settings.json` value, and a project-level setting overrides the global one.

### Resource Resolution

Skills, prompt templates, themes, and extensions follow the same resolution order:

1. CLI-specified paths (`--skill`, `--prompt-template`, `--theme`, `-e`)
2. Project directory (`.pi/skills/`, `.pi/prompts/`, `.pi/themes/`, `.pi/extensions/`)
3. Global directory (`~/.pi/agent/skills/`, `~/.pi/agent/prompts/`, etc.)
4. Installed packages (`~/.pi/agent/packages/`)

When multiple resources share the same name, the first occurrence wins. Collisions are logged as diagnostics.

**Prompt template expansion** supports positional arguments: `$1`, `$2`, `$@` (all args), and slice syntax `${@:start}`, `${@:start:length}`. For example, a template invoked as `/review src/main.rs --strict` receives `src/main.rs` as `$1` and `--strict` as `$2`.

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
| `PI_SUBAGENT_PI_BINARY` | Explicit Rust Pi executable for native child agents; defaults to the current executable |
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
└────────┬──────────────────────┬──────────────────────┬──────────┘
         │                      │                      │
┌────────▼────────┐  ┌─────────▼──────────┐  ┌───────▼──────────┐
│ Provider Layer  │  │  Tool Registry     │  │  Extension Mgr     │
│ • Anthropic     │  │  • read  • bash    │  │  • QuickJS JS/TS   │
│ • OpenAI (Chat/ │  │  • write • grep    │  │  • Native descriptor│
│   Responses)    │  │  • edit  • find    │  │    runtime          │
│ • Gemini/Cohere │  │  • ls              │  │  • Capability policy│
│ • Azure/Bedrock │  │  • ext-registered  │  │  • Node shims       │
│ • Vertex/Copilot│  │                    │  │  • Event hooks      │
│ • GitLab/Cursor │  │                    │  │  • Runtime risk ctl │
│   /Ext          │  │                    │  │                    │
└────────┬────────┘  └─────────┬──────────┘  └───────┬──────────┘
         │                     │                      │
┌────────▼─────────────────────▼──────────────────────▼──────────┐
│                     Session Persistence                         │
│  • JSONL format (v3)   • Tree structure   • Session index/cache  │
│  • Per-project dirs    • Default-enabled SQLite backend support │
└─────────────────────────────────────────────────────────────────┘
```

Provider-count rule: Pi has 11 native provider implementation modules. Those modules are `anthropic`, `openai`, `openai_responses`, `gemini`, `cohere`, `azure`, `bedrock`, `vertex`, `copilot`, `gitlab`, and `cursor`. User-visible provider IDs, aliases, OpenAI-compatible presets, and extension-provided `streamSimple` providers are counted separately because several native modules expose multiple routes. The factory module `mod.rs` and shared live-catalog helper `model_fetch.rs` are support modules, not provider implementations, and are excluded.

### Key Design Decisions

1. **No unsafe code**: `#![forbid(unsafe_code)]` enforced project-wide
2. **Streaming-first**: Custom SSE parser, no blocking on responses
3. **Process tree management**: `sysinfo` crate ensures no orphaned processes
4. **Structured errors**: `thiserror` with specific error types per component
5. **Size-budgeted release profile**: LTO + strip + `opt-level = "z"` for budget-compliant shipping artifacts

### asupersync Context vs TypeScript Pi (pi-mono)

This Rust port preserves Pi's user experience, but intentionally changes the runtime substrate. The original TypeScript Pi (`pi-mono`, `packages/coding-agent`) is built on Node.js + package-level abstractions. `pi_agent_rust` moves those same behaviors onto `asupersync` primitives so lifecycle guarantees are explicit in the runtime model.

| Concern | TypeScript Pi (pi-mono baseline) | pi_agent_rust + asupersync |
|---------|----------------------------------|----------------------------|
| **Runtime model** | Node event loop + Promise/AbortSignal conventions | `RuntimeBuilder` + explicit reactor and runtime handle |
| **Async ownership** | Task lifetimes coordinated by framework/library code | Structured task ownership and explicit cross-thread channels (TUI/RPC bridging) |
| **Cancellation semantics** | Primarily API- and tool-layer conventions | Runtime-aware cancellation checks + bounded timeout handling in tools |
| **I/O capability shape** | Ambient Node APIs + extension layer policies | Capability-scoped context (`AgentCx` over `asupersync::Cx`) and explicit hostcall policy |
| **HTTP streaming** | Provider/client dependent | Purpose-built asupersync HTTP/TLS client feeding custom SSE parser |
| **Deterministic test hooks** | Conventional async test setup | asupersync test/runtime hooks used widely in unit/integration tests |

Why this is useful in practice:
- **More predictable failure behavior** during aborts/timeouts because cancellation is checked in explicit loop boundaries and tool runners.
- **Cleaner resource lifetimes** because the runtime, timers, and I/O paths all share one concurrency substrate.
- **Less hidden coupling** because the main invariants live in Rust types/algorithms rather than spread across framework conventions.

### Runtime Invariants (and Why They Matter)

These are the concrete invariants we rely on in this implementation:

1. **Turn-scoped agent lifecycle**
   - The main loop emits `AgentStart`, `TurnStart`, `TurnEnd`, and `AgentEnd` in a stable order.
   - Tool recursion is bounded by `max_tool_iterations` (default `50`) to avoid unbounded self-tool loops.
   - Benefit: stable event ordering for TUI/RPC consumers and predictable termination behavior.

2. **Abort and timeout behavior is explicit**
   - Agent abort checks happen at turn boundaries and around tool execution.
   - `bash` timeout follows a clear escalation path: terminate process tree, grace period, then hard kill.
   - Benefit: fewer "hung" sessions and reduced orphan-process risk during aggressive tool use.

3. **Session writes are crash-resilient**
   - JSONL saves write to a temp file and persist atomically.
   - Session indexing uses SQLite WAL + lock file coordination for concurrent instances.
   - Benefit: better durability and resume reliability under multi-process usage.

4. **Compaction is threshold-driven and boundary-aware**
   - Trigger: estimated context tokens exceed `context_window - reserve_tokens`.
   - Cut-point logic prefers user-turn boundaries and preserves recent context budget.
   - Benefit: compaction recovers context without collapsing near-term task continuity.

5. **Capability policy is fail-closed and precedence-defined**
   - Resolution order: per-extension deny -> global deny -> per-extension allow -> default caps -> mode fallback.
   - Benefit: policy outcomes are explainable, deterministic, and auditable.

6. **Streaming parser tolerates real network chunking**
   - SSE parser handles CR/LF variants, multi-line `data:` fields, partial UTF-8 tails, and end-of-stream flush.
   - Benefit: incremental rendering remains robust across providers and network fragmentation.

### Design Principles Carried From asupersync Into Pi

The following `asupersync` principles are reflected directly in `pi_agent_rust` architecture:

- **Single async substrate**: runtime, timers, fs, and HTTP/TLS all run on one coherent foundation.
- **Explicit context threading**: `AgentCx` wraps `asupersync::Cx` at subsystem boundaries (agent/tools/session/rpc).
- **Bounded operations over best-effort cleanup**: timeout paths and compaction thresholds are parameterized and enforceable.
- **Determinism hooks for tests**: timer-driver aware sleeps and asupersync test helpers reduce nondeterministic flakiness.

Compared to the original TypeScript implementation, this shifts more correctness responsibility into the runtime and core algorithms themselves, instead of relying primarily on ecosystem conventions.

### Additional Major Deltas (Original pi-mono vs Rust Port)

This is a second comparison pass focused on high-impact architectural deltas and rationale.

| Area | Original pi-mono (`packages/coding-agent`) | `pi_agent_rust` | Why this divergence exists |
|------|---------------------------------------------|------------------|----------------------------|
| **Distribution model** | npm package (`npm install -g @mariozechner/pi-coding-agent`) | Single Rust binary (`pi`) | Remove Node runtime dependency and improve startup/deployment portability |
| **Execution surfaces** | Interactive + print + JSON mode + RPC + SDK | Interactive + print + JSON mode + RPC + Rust SDK | Rust SDK provides idiomatic companion API for embedding Pi programmatically (documented in `docs/sdk.md`) |
| **Default built-in tool posture** | Defaults to `read/write/edit/bash` (others available) | Eight built-ins treated as first-class (`read/write/edit/bash/grep/find/ls/hashline_edit`) | Keep common code-navigation, shell, and hashline-anchored edit workflows available without extra configuration |
| **Extension trust model** | Extension/package model documented as full system access | Embedded runtime with capability-gated hostcalls and policy profiles | Reduce ambient authority and make extension behavior auditable/deny-by-default |
| **Session architecture emphasis** | JSONL tree session model and branch navigation | JSONL v3 tree + derived SQLite metadata index + default-enabled SQLite session backend support | Bound eligible resume/lookups and coordinate multi-instance access |
| **Streaming transport stack** | Node runtime networking stack | Purpose-built HTTP/TLS client + custom SSE parser on asupersync | Tighter control over chunking, parsing, and failure handling in long streams |
| **Cancellation/timeout mechanics** | Platform/event-loop cancellation conventions | Explicit abort signaling, bounded tool iterations, process-tree termination | Minimize hangs/orphans and make stop behavior deterministic under load |
| **Runtime context model** | Framework-level conventions and extension APIs | Explicit `AgentCx`/`asupersync::Cx` capability-scoped context threading | Make effect boundaries and testability first-class architectural constraints |

Practical consequence of these deltas:
- Extension/package workflow compatibility is an implementation goal; current coverage and gaps remain governed by the active drop-in contract and provenance-matched verdict.
- The goal is parity with pi-mono through Rust-idiomatic patterns; current gaps
  remain governed by the active certification contract.
- The Rust SDK provides a companion API for core embedding workflows without
  requiring TypeScript-specific adaptation patterns.
- `docs/parity-certification.json` tracks informational functional-parity progress; it does not authorize strict replacement claims.

### Algorithmic Mechanics: pi-mono Baseline vs Rust Implementation

This section compares concrete implementation mechanics for equivalent high-level behavior.

| Algorithm | pi-mono baseline mechanism | Rust implementation mechanism | Why the Rust variant exists |
|-----------|-----------------------------|-------------------------------|-----------------------------|
| **Session context rebuild after compaction** | `buildSessionContext()` emits compaction summary, then messages from `firstKeptEntryId` (pre-compaction path), then post-compaction entries | `to_messages_for_current_path()` uses the same ordering and adds a fallback if `first_kept_entry_id` is missing | Avoid silent context loss when compaction anchors are orphaned/corrupted |
| **JSONL persistence** | Incremental append (`appendFileSync`) plus full rewrite (`writeFileSync`) for migrations/rewrites | Save via temp file + atomic persist/replace | Keep on-disk session state crash-resilient during save operations |
| **Session discovery/resume** | Directory/file scan and mtime sorting of JSONL files | Derived SQLite session metadata index + WAL + lock file + staleness-triggered full reindex | Bound resume lookup cost and coordinate concurrent processes |
| **Compaction token accounting** | Uses assistant usage (`totalTokens` else `input+output+cacheRead+cacheWrite`) plus heuristic trailing estimates | Uses assistant usage (`total_tokens` else `input+output`) plus heuristic trailing estimates; fixed image token estimate | Keep accounting stable across providers with uneven cache-token reporting while staying conservative |
| **Cut-point + split-turn handling** | Valid cut points exclude tool results; split turns are summarized as history + turn-prefix context | Same cut-point class and split-turn strategy, implemented in Rust entry/message model | Preserve tool-call/result adjacency and turn coherence under budget pressure |
| **Bash timeout/process cleanup** | Timeout/abort kills process tree (`killProcessTree`) and returns tail-truncated output | Timeout escalation (`TERM` then grace then `KILL`) + process-tree walk + shell exit trap + tail truncation | Enforce bounded cleanup and reduce descendant-process leaks from background jobs |
| **Streaming event decoding** | Transport semantics are exposed (`sse`/`websocket`/`auto`); parser details are runtime-internal | Explicit SSE parser with BOM stripping, CR/LF normalization, UTF-8 tail buffering, and flush-on-end | Make byte-to-event behavior deterministic and provider-SDK-independent |

### Feature Superset Highlights (Beyond pi-mono Baseline)

The sections above compare mechanics. This section calls out concrete features present in this Rust port that are not part of the pi-mono baseline implementation model.

| Rust-port feature | Why it is useful/compelling |
|-------------------|-----------------------------|
| **`pi doctor` diagnostics command** (`text`/`json`/`markdown`, `--only`, `--fix`, swarm preflight, extension compatibility checks) | Gives actionable environment + compatibility diagnostics, supports CI gating (non-zero on failures), can auto-fix safe issues like missing dirs/permissions, and reports read-only multi-agent readiness before swarm work |
| **Capability-gated extension policy profiles** (`safe` / `balanced` / `permissive`) with per-extension overrides | Lets operators run shared extensions with explicit capability boundaries instead of ambient full-system access |
| **Secret-aware extension env filtering** (`pi.env()` blocklist for keys/tokens/secrets) | Reduces accidental credential exposure from extension code paths |
| **Per-extension trust lifecycle + kill-switch audit trail** (`pending`/`acknowledged`/`trusted`/`killed`, `kill_switch`, `lift_kill_switch`) | Supports immediate containment, explicit operator provenance, and controlled re-entry after review |
| **Hostcall compatibility-lane emergency controls** (global/per-extension forced-compat switches + reason codes) | Gives operators a deterministic rollback path for fast-lane incidents without losing extension availability |
| **Runtime risk controller for extension hostcalls** (configurable, fail-closed by default) | Adds another enforcement layer beyond static policy for suspicious runtime behavior in extension call flows |
| **Argument-aware runtime risk scoring for shell paths** (`dcg_rule_hit`, `dcg_heredoc_hit`, heredoc AST inspection across Bash/Python/JS/TS/Ruby) | Detects destructive intent hidden in multiline scripts and wrapper commands before hostcall execution |
| **Tamper-evident runtime risk ledger tooling** (`ext_runtime_risk_ledger verify|replay|calibrate`) | Security decisions are hash-chained and can be verified, replayed, and threshold-calibrated from real traces |
| **Unified incident evidence bundle export** (risk ledger, security alerts, hostcall telemetry, exec mediation, secret-broker events) | Incident response can triage from one structured artifact set instead of stitching ad-hoc logs |
| **Deterministic hostcall reactor mesh with optional NUMA slab pool** (shard affinity, global-order drain, bounded SPSC lanes, telemetry) | Keeps extension dispatch predictable under load and surfaces queue/backpressure behavior for tuning |
| **Cold realm factory + persistent transpile cache** | Rebuilds mutable JS state for every load while reusing only versioned compiled artifacts that are safe to share across realms |
| **Extension preflight static analysis** (imports/forbidden-pattern scan with policy-aware hints) | Catches risky extension patterns before runtime execution |
| **Node/Bun-compatible extension runtime without Node/Bun dependency** (embedded QuickJS + shims) | Runs legacy extension workflows in a single native binary deployment model |
| **Extension compatibility scanner + conformance harness** | Makes extension support measurable and auditable instead of anecdotal |
| **Derived SQLite session metadata index** (WAL + lock + stale reindex path) | Gives fast session resume/list operations at scale without scanning every JSONL file on each query |
| **Session Store V2 rollback and migration ledger** (segmented log + checkpoints + rollback events) | Long-session recovery can unwind to a known checkpoint with explicit migration/rollback provenance |
| **Default-enabled SQLite session storage support** (`sqlite-sessions` feature) | Supports deployments that want database-backed session persistence in addition to JSONL; disable with `--no-default-features` when building a minimal binary |
| **Crash-resilient session save path** (temp file + atomic persist) | Improves session-file durability during writes and reduces partial-write failure modes |
| **Unified hostcall dispatcher with typed taxonomy mapping** (`timeout` / `denied` / `io` / `invalid_request` / `internal`) | Produces consistent extension/runtime error semantics and easier client handling |
| **Fail-closed evidence-lineage gates** (`run_id`/`correlation_id` + cross-artifact lineage checks) | Rejects stale or cherry-picked conformance/perf artifacts at release-gate time |
| **Structured auth diagnostics with stable machine codes** | Improves troubleshooting and operational visibility without leaking sensitive credential material |

---

## Deep Dive: Core Algorithms

### Math-Driven Decision Systems

Pi deliberately uses advanced math where it improves runtime behavior or benchmark confidence. The goal is not “fancy formulas in docs”; it is safer policy decisions, faster recovery from workload shifts, and more trustworthy performance attribution.

### Regime-Shift Detection (CUSUM + BOCPD)

In the extension dispatcher, Pi combines CUSUM and Bayesian online change-point detection to detect load-regime changes early (for example when hostcall traffic suddenly spikes or stalls).

$$
S_t^+ = \max\left(0,\;S_{t-1}^+ + (-z_t - k)\right), \quad
S_t^- = \max\left(0,\;S_{t-1}^- + (z_t - k)\right)
$$

$$
H(r)=\frac{1}{\lambda}, \quad
P(r_t=0 \mid x_{1:t}) \propto \sum_r P(r_{t-1}=r)\,H(r)\,P(x_t \mid r)
$$

Intuition: CUSUM catches persistent drift; BOCPD catches sudden regime changes without brittle fixed thresholds.

### Conformal Prediction Envelope

Pi tracks nonconformity scores (absolute residuals from the running mean) and treats out-of-interval events as anomalies.

$$
q = \text{score}_{\lceil (n+1)\cdot \text{confidence} \rceil - 1}, \quad
\text{anomaly if } |x_t - \mu_t| > q
$$

Intuition: thresholds adapt from recent behavior instead of hard-coding one static latency cutoff.

### PAC-Bayes Safety Bound

Pi’s safety envelope includes a PAC-Bayes-kl bound over extension outcomes, and can veto aggressive optimization when the bound is too high.

$$
\mathrm{kl}(\hat q \,\|\, q_{\text{bound}})\;\le\;\frac{\mathrm{KL}(Q\|P)+\ln\!\left(2\sqrt{n}/\delta\right)}{n}
$$

Intuition: this gives an explicit uncertainty-aware ceiling on true error risk before allowing more aggressive runtime behavior.

### Off-Policy Evaluation (IPS/WIS/DR + ESS + Regret Gate)

Before approving policy moves, Pi evaluates candidate behavior from trace data:

$$
w_i=\frac{\pi(a_i\mid x_i)}{\mu(a_i\mid x_i)}, \quad
\hat V_{\text{IPS}}=\frac{1}{n}\sum_i w_i r_i
$$

$$
\hat V_{\text{WIS}}=\frac{\sum_i w_i r_i}{\sum_i w_i}, \quad
\hat V_{\text{DR}}=\frac{1}{n}\sum_i\left(\hat r_i + w_i(r_i-\hat r_i)\right)
$$

$$
N_{\text{eff}}=\frac{(\sum_i w_i)^2}{\sum_i w_i^2}, \quad
\Delta_{\text{regret}}=\bar r_{\text{baseline}}-\hat V_{\text{DR}}
$$

Intuition: Pi fails closed if sample support is weak, uncertainty is high, or estimated regret is above threshold.

### VOI-Driven Experiment Selection

The VOI planner prioritizes probes that provide the most expected learning under a strict overhead budget.

$$
\text{priority}_i \propto \frac{\text{utility}_i}{\text{overhead}_i}
$$

Intuition: run only the experiments that are likely to change decisions; skip stale or low-value probes.

### Weighted Bottleneck Attribution (Benchmarking)

For phase-1 matrix benchmarking, Pi computes stage attribution weighted by realistic workload size (`session_messages`) and reports confidence intervals.

$$
\text{weighted\_contribution}_s
=
\frac{\sum_i w_i\,m_{i,s}}{\sum_i w_i\,t_i}\cdot 100,
\quad w_i=\text{session\_messages}_i
$$

$$
n_{\text{eff}}=\frac{(\sum_i w_i)^2}{\sum_i w_i^2}, \quad
\mathrm{CI}_{95}=\mu \pm 1.96\sqrt{\frac{\sigma^2}{n_{\text{eff}}}}
$$

Intuition: prioritize what dominates real end-to-end latency, not just isolated microbench hotspots.

### Online Convex Control + Regret Tracking

Pi also includes an online tuner path for batch/time-slice controls with explicit rollback behavior:

$$
\tau_{t+1}
=
\mathrm{clip}\!\left(\tau_t - \eta\nabla_{\tau}\mathcal{L}_t,\;\tau_{\min},\tau_{\max}\right)
$$

Intuition: the system adapts continuously, but if instantaneous loss exceeds a rollback threshold it immediately returns to a safer profile.

### Math At a Glance

| Technique | Where in Pi | Why it helps |
|----------|-------------|--------------|
| CUSUM + BOCPD | Extension dispatcher regime detector | Detects traffic regime shifts early and robustly |
| Conformal intervals | Safety envelope | Adaptive anomaly gating without static magic numbers |
| PAC-Bayes bound | Safety envelope veto path | Fails closed when uncertainty/risk is too high |
| IPS/WIS/DR + ESS | Off-policy evaluator | Approves policy changes only with adequate support |
| VOI planning | Experiment scheduler | Uses overhead budget on highest-value probes |
| Weighted attribution + CI | Phase-1 perf matrix reports | Ranks optimization work by realistic user impact |
| OCO + regret rollback | Runtime controller | Adapts under load while bounding unsafe drift |

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
| `MAX_BYTES` | 1MB | Prevents binary file accidents |
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
    fn api(&self) -> &str;
    fn model_id(&self) -> &str;

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;
}
```

**Context structure:**

```rust
pub struct Context<'a> {
    pub system_prompt: Option<Cow<'a, str>>, // System prompt
    pub messages: Cow<'a, [Message]>,        // Conversation history
    pub tools: Cow<'a, [ToolDef]>,           // Available tools with JSON schemas
}
```

**StreamOptions:**

```rust
pub struct StreamOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub api_key: Option<String>,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub headers: HashMap<String, String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}
```

This design allows adding new providers (OpenAI, Gemini) without modifying the agent loop. Each provider translates the common types to its wire format and emits a unified `StreamEvent` stream.

### Compaction Algorithm

Long conversations eventually exceed the model's context window. Pi's compaction algorithm reclaims space by summarizing older messages while preserving recent context.

The algorithm runs automatically after each agent turn when estimated token usage exceeds `context_window - reserve_tokens`:

```
┌──────────────────────────────────────────────────────────────┐
│                     Full Conversation                         │
│  msg1 → msg2 → msg3 → ... → msgN-5 → msgN-4 → ... → msgN   │
│  ├──── older messages ─────┤ ├─── recent messages ──────────┤ │
│                                                              │
│  Step 1: Find cut point at a valid turn boundary             │
│  Step 2: LLM summarizes msgs 1..N-5 into compact paragraph  │
│  Step 3: Store Compaction entry in session JSONL             │
│  Step 4: Next agent call uses [summary] + msgs N-4..N       │
└──────────────────────────────────────────────────────────────┘
```

**Token estimation** uses a conservative `chars ÷ 4` heuristic for text and a flat 1,200 tokens per image. When an assistant message includes a `usage` field from the API, that measured value takes precedence over the heuristic.

**Cut point selection** prefers boundaries between complete user-assistant turns. If the budget forces a mid-turn cut, the algorithm includes prefix messages from the split turn so the model retains context about what was being discussed at the boundary.

**File operation tracking** extracts `read`, `write`, and `edit` tool calls from the messages being summarized. The compaction prompt includes these paths so the summary preserves awareness of which files were examined or modified:

```
<read-files>
src/main.rs
src/config.rs
</read-files>

<modified-files>
src/auth.rs
</modified-files>
```

**Configurable parameters:**

| Parameter | Default | Purpose |
|-----------|---------|---------|
| `reserve_tokens` | 8% of context window | Safety margin for response generation |
| `keep_recent_tokens` | 10% of context window | Minimum recent context preserved |

Compaction can also be triggered manually with `/compact` in interactive mode or the `compact` RPC command.

### Multi-Provider Routing & Model Registry

Pi routes model requests through a provider factory that resolves the correct backend implementation from a `(provider, model, api)` tuple.

**Resolution flow:**

```
User specifies --provider openai --model gpt-4o
               │
               ▼
  ┌──────────────────────────┐
  │  Provider Metadata Table  │  Maps "openai" → canonical ID,
  │                           │  determines API type (Completions
  │                           │  vs Responses vs custom)
  └────────────┬──────────────┘
               │
  ┌────────────▼──────────────┐
  │  URL Normalization         │  Appends /chat/completions,
  │                            │  /responses, or /chat depending
  │                            │  on detected API type
  └────────────┬──────────────┘
               │
  ┌────────────▼──────────────┐
  │  Compat Config             │  Applies per-model overrides:
  │                            │  system_role_name, max_tokens
  │                            │  field name, feature flags
  └────────────┬──────────────┘
               │
  ┌────────────▼──────────────┐
  │  Provider Instance         │  Anthropic | OpenAI | Gemini
  │                            │  Cohere | Azure | Bedrock | ...
  └───────────────────────────┘
```

**`models.json` overrides**: Users can define custom providers in `~/.pi/agent/models.json` or `.pi/models.json`. Each entry specifies a model ID, base URL, API type, and optional compat flags, letting you route to self-hosted models, proxies, or providers that Pi does not natively support.

**Compat config** handles the differences between OpenAI-compatible APIs:

| Override | Example | Purpose |
|----------|---------|---------|
| `system_role_name` | `"developer"` | o1 models use "developer" instead of "system" |
| `max_tokens_field` | `"max_completion_tokens"` | Some models require a different field name |
| `supports_tools` | `false` | Suppress tool definitions for models that reject them |
| `supports_streaming` | `false` | Fall back to non-streaming for incompatible endpoints |
| `custom_headers` | `{"X-Custom": "val"}` | Per-provider header injection |

**Fuzzy matching**: When a provider name doesn't match any known provider, Pi computes edit distance against all registered names and suggests the closest match in the error message.

### Extension Hostcall Protocol

Extensions run in an embedded QuickJS runtime (`rquickjs` crate) and communicate with Pi through a structured hostcall protocol. This is the mechanism that lets JavaScript code invoke Pi's built-in tools, make HTTP requests, and interact with the session, all without direct OS access.

**Execution model:**

```
┌─────────────────── QuickJS VM ───────────────────┐
│                                                   │
│  extension.js calls:                              │
│    pi.tool("read", {path: "src/main.rs"})         │
│      │                                            │
│      ▼                                            │
│    enqueue HostcallRequest {                      │
│      call_id: "hc-0042",                          │
│      kind: Tool { name: "read" },                 │
│      payload: { path: "src/main.rs" },            │
│    }                                              │
│      │                                            │
│      ▼                                            │
│    return Promise (resolve/reject stored in map)  │
│                                                   │
└────────────────────────┬──────────────────────────┘
                         │
    drain_hostcall_requests()
                         │
                         ▼
┌─────────────── ExtensionDispatcher ──────────────┐
│                                                   │
│  1. Check capability policy:                      │
│     read tool → requires "read" capability        │
│     → Policy says: Allow / Deny / Prompt          │
│                                                   │
│  2. If allowed → dispatch to ToolRegistry         │
│     → Execute read tool                           │
│     → Get ToolOutput                              │
│                                                   │
│  3. complete_hostcall("hc-0042", Ok(result))      │
│     → Resolves the Promise in QuickJS             │
│                                                   │
│  4. runtime.tick()                                │
│     → Drains Promise .then() chains               │
│     → Extension JS continues execution            │
│                                                   │
└───────────────────────────────────────────────────┘
```

**Capability mapping**: Each hostcall kind maps to a required capability:

| Hostcall | Required Capability | Dangerous? |
|----------|-------------------|------------|
| `pi.tool("read", ...)` | `read` | No |
| `pi.tool("write", ...)` | `write` | No |
| `pi.tool("bash", ...)` | `exec` | Yes |
| `pi.http(request)` | `http` | No |
| `pi.exec(cmd, args)` | `exec` | Yes |
| `pi.env(key)` | `env` | Yes |
| `pi.session(op, ...)` | `session` | No |
| `pi.ui(op, ...)` | `ui` | No |
| `pi.log(entry)` | `log` | No (always allowed) |

**Deduplication**: Each hostcall's parameters are canonicalized (object keys sorted, structure normalized) and SHA-256 hashed. Identical requests within a short window can be deduplicated to avoid redundant tool executions.

**Fast lane vs compatibility lane**: Pi has two execution lanes for hostcalls:

- **Fast lane** is used when the call shape matches known safe patterns (for example common `tool` and `session` operations). This avoids extra allocation and parsing work.
- **Compatibility lane** is the fallback for uncommon or partially-specified calls.
- Both lanes still enforce the same capability policy and permission checks.
- Operators can force compatibility-lane routing globally or per extension as an emergency control path.

For observability, each call is tagged with a stable lane key (for example `tool|tool.read|filesystem` or `tool|fallback|filesystem`) so latency and failure trends can be grouped consistently.

**Built-in consistency guard (shadow dual execution)**: Pi can sample a small subset of read-only hostcalls, execute them through both lanes, and compare canonical output fingerprints. If divergence crosses a configured budget, Pi automatically backs off the fast lane for a period. This gives performance wins without silently changing behavior.

**Adaptive dispatch mode under load**: Pi can switch between:

- `sequential_fast_path` for simpler/low-contention workloads
- `interleaved_batching` when contention and queue pressure rise

Mode changes are gated by sample coverage and risk checks, so Pi does not switch based on thin or cherry-picked evidence.

**Runtime telemetry for debugging and tuning**: Pi records structured hostcall telemetry (`pi.ext.hostcall_telemetry.v1`) with lane choice, fallback reason, dispatch latency share, marshalling path, and optimization hit/miss fields. This is used by perf reports and reliability diagnostics.

**Auto-repair pipeline**: When an extension fails to load or produces runtime errors, Pi's repair system can automatically fix common issues:

| Repair Mode | Behavior |
|-------------|----------|
| `Off` | No repairs |
| `Suggest` | Log suggestions, don't apply |
| `AutoSafe` (default) | Apply provably safe fixes (missing file paths, asset references) |
| `AutoStrict` | Apply aggressive heuristic fixes (pattern-based transforms) |

**Compatibility scanner**: Before loading, Pi statically analyzes extension source code for imports, `require()` calls, and forbidden patterns (`eval`, `Function()`, `process.binding`, `dlopen`). The scan produces a capability evidence ledger that informs policy decisions.

**Environment variable filtering**: Extensions calling `pi.env()` hit a blocklist that denies access to API keys, credentials, tokens, and private keys. The filter blocks exact matches (`ANTHROPIC_API_KEY`, `AWS_SECRET_ACCESS_KEY`), suffix patterns (`*_API_KEY`, `*_SECRET`, `*_TOKEN`), and prefix patterns (`AWS_SECRET_*`, `AWS_SESSION_*`). Only `PI_*` variables are unconditionally allowed.

**Trust lifecycle and kill switch**: Extension trust state is tracked explicitly (`pending`, `acknowledged`, `trusted`, `killed`). A kill switch demotes an extension to `killed`, quarantines it in the runtime risk controller, emits a critical alert, and writes an audit record. Lifting the switch requires an explicit operator action and moves the extension back to `acknowledged`.

### Extension Runtime Decision Logic (Plain English)

The extension runtime includes a few small decision engines so behavior stays stable as workload patterns change:

- **Value-of-information planner (VOI)**: Ranks candidate probes by "expected learning per millisecond" and picks the best set under a strict overhead budget. Stale or low-value candidates are skipped with explicit reasons.
- **Shard load controller**: Adjusts routing weights, batch budgets, and backoff/help factors based on queue pressure, latency, and starvation risk. Damping and oscillation guards prevent overreaction.
- **Policy safety evaluator**: Replays historical samples with multiple estimators and only approves a policy when sample support is strong, uncertainty is low, and predicted regret stays within limit.

These pieces are intentionally conservative: if confidence is weak, Pi holds steady instead of making an aggressive switch.

### Interactive TUI Architecture

The interactive mode uses the **Elm Architecture** (Model-Update-View) via the `charmed_rust` library family, which is a Rust port of Go's [Bubble Tea](https://github.com/charmbracelet/bubbletea) framework.

**Component stack:**

```
┌────────────────────────────────────────────────────┐
│                 Terminal (crossterm)                │
│  Raw mode │ Alt screen │ Keyboard/Mouse events      │
└──────────────────────┬─────────────────────────────┘
                       │
┌──────────────────────▼─────────────────────────────┐
│             bubbletea Program Loop                  │
│  Init() → Update(Msg) → View() → render cycle      │
└──────────────────────┬─────────────────────────────┘
                       │
┌──────────────────────▼─────────────────────────────┐
│                  PiApp (Model)                      │
│                                                     │
│  ┌─────────────┐ ┌──────────────┐ ┌─────────────┐  │
│  │  TextArea    │ │  Viewport    │ │  Spinner     │  │
│  │  (editor)    │ │  (convo)     │ │  (status)    │  │
│  └─────────────┘ └──────────────┘ └─────────────┘  │
│                                                     │
│  ┌─────────────────────────────────────────────┐    │
│  │           Overlay Stack                      │    │
│  │  Model Selector │ Session Picker │ /tree     │    │
│  │  Settings UI    │ Theme Picker   │ Branches  │    │
│  │  Capability Prompt (extension UI)            │    │
│  └─────────────────────────────────────────────┘    │
└──────────────────────┬─────────────────────────────┘
                       │
              async channels (mpsc)
                       │
┌──────────────────────▼─────────────────────────────┐
│             Agent Async Task                        │
│  Runs on asupersync runtime                         │
│  Streams provider responses                         │
│  Executes tools                                     │
│  Sends PiMsg events back to TUI thread              │
└────────────────────────────────────────────────────┘
```

**The async/sync bridge**: The agent runs on the `asupersync` async runtime in a separate thread. It communicates with the bubbletea UI thread through `mpsc` channels. Each streaming event (text delta, tool start, tool update, agent done) becomes a `PiMsg` variant delivered to `PiApp::update()`, keeping the UI responsive during API streaming and tool execution.

**Viewport scrolling**: The conversation viewport tracks whether the user is at the bottom. When new content arrives and the user hasn't scrolled up, the viewport auto-follows the stream tail. Scrolling up disables auto-follow; pressing `End` or typing a new message re-enables it.

**Overlay system**: Modal UIs (model selector, session picker, branch navigator, extension capability prompts) stack on top of the main conversation view. Each overlay captures keyboard input until dismissed. Only the topmost active overlay receives events.

**Slash commands** available in the interactive editor:

| Command | Action |
|---------|--------|
| `/help` | Show available commands and keybindings |
| `/model` or `Ctrl+L` | Open model selector with fuzzy search |
| `Ctrl+P` / `Ctrl+Shift+P` | Cycle scoped models forward/backward |
| `/tree` | Browse and fork the conversation tree |
| `/clear` | Clear conversation and start fresh |
| `/compact` | Trigger manual compaction |
| `/thinking <level>` | Change thinking level mid-conversation |
| `/share` | Export session to GitHub Gist |
| `/exit` or `Ctrl+C` | Exit Pi |

### RPC Protocol

The RPC mode (`pi --mode rpc`) exposes a line-delimited JSON protocol over stdin/stdout for programmatic integration. Each line is a self-contained JSON object.

**Client → Pi (stdin):**

```json
{"type": "prompt", "message": "Explain this function", "id": "req-001"}
{"type": "steer", "message": "Focus on error handling"}
{"type": "follow_up", "message": "Now add tests"}
{"type": "abort"}
{"type": "get_state"}
{"type": "compact", "reserveTokens": 8192, "keepRecentTokens": 20000}
```

**Pi → Client (stdout):**

```json
{"type": "agent_start", "sessionId": "..."}
{"type": "message_update", "message": {...}, "assistantMessageEvent": {"type": "text_delta", "delta": "The function", "contentIndex": 0}}
{"type": "tool_execution_start", "toolCallId": "...", "toolName": "read", "args": {}}
{"type": "tool_execution_end", "toolCallId": "...", "toolName": "read", "result": {}, "isError": false}
{"type": "agent_end", "sessionId": "...", "messages": [...]}
{"type": "response", "id": "req-001", "command": "prompt", "success": true, "data": {"status": "ok"}}
```

**I/O architecture**: Two dedicated threads handle stdin reading and stdout writing, bridged to the async agent runtime via channels. The stdin thread retries on transient errors to prevent dropped input. The stdout thread flushes after every line to prevent buffering delays.

**Message queuing**: While the agent is streaming a response, incoming messages are routed to one of two queues:

| Queue | Behavior | Use Case |
|-------|----------|----------|
| **Steering** | Interrupts current response; processed on next turn | Course corrections |
| **Follow-up** | Queued until current response completes | Sequential instructions |

Queue modes (`All` or `OneAtATime`) control whether multiple queued messages are batched into a single turn or processed individually.

**Extension UI over RPC**: When an extension requests user input (capability prompt, selection dialog), Pi emits an `extension_ui_request` event. The client renders the prompt in its own UI and responds with an `extension_ui_response` message. IDE extensions can then present native UI for capability decisions instead of falling back to terminal prompts.

### Session Indexing

Session resume (`pi -c` or `pi -r`) needs to find the most recent session for the current project without scanning every JSONL file on disk. Pi maintains a SQLite index (`session-index.sqlite`) that provides constant-time lookups.

**Schema:**

```sql
CREATE TABLE sessions (
    path            TEXT PRIMARY KEY,
    id              TEXT NOT NULL,
    cwd             TEXT NOT NULL,
    timestamp       TEXT NOT NULL,
    message_count   INTEGER NOT NULL,
    last_modified   INTEGER NOT NULL,
    size_bytes      INTEGER NOT NULL,
    name            TEXT
);
```

**Update lifecycle:**

1. After saving a session JSONL file, Pi upserts its metadata into the index
2. `pi -c` queries `WHERE cwd = ? ORDER BY last_modified DESC LIMIT 1`
3. `pi -r` queries the same table and presents a picker sorted by recency

**Concurrency**: A file-based lock (`session-index.lock`) serializes writes from concurrent Pi instances. Reads use WAL mode for non-blocking access.

**Staleness-based reindexing**: If the index is older than a configurable threshold, Pi runs a full re-scan of the sessions directory to catch files created by other instances or manual edits. The re-scan keeps the index accurate without a centralized daemon.

### Session Store V2 Sidecar (Large Session Fast-Path)

Pi also supports a v2 sidecar store next to JSONL sessions for faster resume and stronger corruption checks on long histories.

**What it adds:**

- Segmented append log files (instead of one ever-growing JSONL file)
- Offset index rows for direct seeks and fast tail reads
- Periodic checkpoints and a manifest snapshot
- Migration ledger entries for auditability
- Checkpoint-based rollback path with explicit rollback event logging

**How resume works:**

1. A verified migration installs a durable `clean` source-state record, allowing the normal
   v2 open to use the sidecar index + segments without rescanning the full JSONL file.
2. Before any JSONL mutation, Pi durably marks the sidecar `dirty`; dirty, invalid, or
   otherwise stale sidecars fall back to authoritative JSONL parsing. Filesystem mtimes remain
   a secondary external-change check, and legacy sidecars are fully verified once before use.
3. If index data needs repair, Pi marks the sidecar dirty first and adopts the repaired store
   only after its entry IDs and hash chain match the source JSONL exactly.

**Integrity strategy:**

- Segment frames carry payload and chain hashes.
- Index rows store byte offsets plus CRC32C checksums.
- Validation checks offset bounds, checksum matches, and frame/index alignment before trusting the sidecar.
- Truncated trailing frames are recoverable during rebuild; non-EOF frame corruption fails closed instead of silently dropping data.

**CLI support:**

- `pi migrate <path> --dry-run` validates migration without writing.
- `pi migrate <path>` performs JSONL-to-v2 migration and verifies parity.

### Authentication & Credential Management

Beyond simple API keys, Pi supports OAuth, AWS credential chains, service key exchange, and bearer-token auth. Credentials are stored in `~/.pi/agent/auth.json` with file-locked access to prevent corruption from concurrent instances. Stored API keys can be literal strings, `$ENV:VAR_NAME` references, or `$CMD:shell command` / `$COMMAND:shell command` sources that resolve trimmed stdout at request time.

| Mechanism | Providers | Details |
|-----------|-----------|---------|
| **API Key** | Anthropic, OpenAI, Gemini, Cohere, and many OpenAI-compatible providers | Static key via env var or settings |
| **OAuth** | Anthropic, OpenAI Codex, Google Gemini CLI, Google Antigravity, Kimi for Coding, GitHub Copilot, GitLab, and extension-defined OAuth providers | PKCE/state-validated flow with automatic refresh; Kimi uses device flow |
| **AWS Credentials** | Bedrock | Access key + secret + optional session token; region-aware |
| **Service Key** | SAP AI Core | Client ID/secret exchange for bearer token |
| **Bearer Token** | Custom providers | Static token in auth storage |

**OAuth token lifecycle:**

1. User runs `pi` with an OAuth-configured provider
2. Pi checks `auth.json` for an existing token
3. If missing: opens browser to authorization URL, user authenticates, Pi receives authorization code, exchanges it for access + refresh tokens, stores both with expiry timestamp
4. If expired but refresh token valid: exchanges refresh token for new access token, updates `auth.json`
5. Bearer token attached to API requests

Google CLI-style OAuth providers carry project metadata with the token payload. Pi preserves and refreshes that payload and can resolve project IDs from `GOOGLE_CLOUD_PROJECT` or local `gcloud` config when needed.

**Credential status reporting**: `pi config` shows the status of each configured provider's credentials: `Missing`, `ApiKey`, `OAuthValid` (with time until expiry), `OAuthExpired` (with time since expiry), `AwsCredentials`, or `BearerToken`.

**Diagnostic codes**: Auth failures produce specific diagnostic codes (`MissingApiKey`, `InvalidApiKey`, `QuotaExceeded`, `OAuthTokenRefreshFailed`, `MissingAzureDeployment`, `MissingRegion`, etc.) with context-specific error hints rather than generic messages.

---

## Tool Details

### read

Read file contents (optionally images):

```
Input: { "path": "src/main.rs", "offset": 10, "limit": 50 }
```

- Supports images (jpg, png, gif, webp) with optional auto-resize
- Streams file bytes in chunks with hard size limits to reduce peak memory usage
- Applies defensive image decode limits to block decompression-bomb/OOM inputs
- Truncates at 2000 lines or 1MB
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

| Phase | Managed-runtime CLI | Pi's native binary |
|-------|---------------------|--------------------|
| Application runtime bootstrap | Required | Not required |
| Application module loading | Commonly dynamic | Ahead-of-time linked Rust code |
| JIT warmup | Runtime-dependent | Not used |
| Current release timing | Not measured here | Fresh `v0.2.0` measurement pending |

Current comparative timing numbers are intentionally omitted until a fresh,
provenance-matched benchmark run is available.

### Optimization Playbook

Pi applies performance-oriented design at multiple layers, not only in the Cargo
release profile.

Where performance work is concentrated:

- **Startup path kept minimal**: no JS runtime bootstrap, no module graph loading, no JIT warmup.
- **Hot hostcall specialization**: common extension hostcalls use typed fast paths; uncommon shapes fall back to compatibility paths.
- **Adaptive dispatch under load**: hostcall scheduling can switch modes when contention rises, then switch back when pressure drops.
- **Fast-path safety guardrails**: sampled shadow dual-execution checks ensure optimizations do not silently change behavior.
- **Low-allocation rendering**: TUI render buffers and markdown render results are cached/reused instead of rebuilt every frame.
- **Fast resume internals**: session indexing plus the v2 sidecar layout avoid expensive full-history scans on resume.
- **Bounded growth controls**: compaction and truncation keep token/context growth and tool-output payload growth from degrading responsiveness over long sessions.
- **Measurement-first culture**: perf artifacts are schema-validated and claim-gated in CI, so optimization work is driven by evidence and regressions are caught early.

These mechanisms are intended to preserve responsiveness under streaming, tool,
large-session, and extension workloads. Achieved release-level latency and
memory results require fresh, provenance-matched measurements.

### Optimization Catalog (Code + Commit History)

This catalog reflects a long sequence of changes across runtime, storage, streaming, and UI.

Concrete engineering work in this codebase includes:

| Area | What we built | Why it matters |
|------|---------------|----------------|
| Extension dispatch core | Typed hostcall opcode fast paths, compatibility fallback lane, zero-copy payload arena, canonical-hash shortcuts, interned operation paths | Cuts per-call overhead on the hottest extension operations while preserving correctness fallback paths |
| Registry/policy lookup | Immutable policy snapshots with O(1) capability checks, plus RCU-style metadata snapshots for extension registry/tool metadata | Removes repeated dynamic lookup overhead in hot authorization/dispatch paths |
| Queueing + concurrency | Core-pinned SPSC reactor mesh, S3-FIFO-inspired admission with fairness guards, BRAVO-style fallback behavior | Improves tail latency under contention instead of only optimizing median latency |
| Batched execution | AMAC-style interleaved batch executor with stall-aware toggling | Avoids head-of-line stalls when many independent hostcalls are in flight |
| IO specialization | io_uring lane policy + telemetry and explicit executor-unavailable fallback | Evaluates IO-heavy calls without presenting the placeholder bridge as real io_uring execution |
| Runtime startup for extensions | Cold owner-isolated QuickJS realms, bounded in-realm source caches, and versioned persistent transpile caching | Avoids mutable-realm reuse while reducing repeated transpilation work |
| JS bridge/scheduler path | Pending-job fast-path tuning and bridge-level dispatch cleanup in hot extension loops | Reduces overhead between JS requests and Rust execution |
| Adaptive control loops | Regime-shift detection (CUSUM/BOCPD), budget controllers, VOI-based experiment planner, mean-field load controller, off-policy evaluation gates | Lets runtime behavior adapt to workload shifts without blind tuning |
| Fast-path safety | Shadow dual-execution sampling with automatic rollback/backoff on divergence | Prevents speed work from silently changing semantics |
| Trace-level optimization | Hostcall superinstruction compiler + tier-2 trace-JIT path | Fuses repeated opcode traces and lowers repeated dispatch overhead |
| Tool execution strategy | Effect-compatible tool classification + barrier-aware parallel execution paths | Increases throughput when multiple tool calls can safely run together without reordering mutating tools |
| Session write path | Write-behind autosave queue, durability modes, incremental append, checkpointed rewrite strategy, single-buffer append serialization | Keeps interaction fast while still supporting stronger durability when needed |
| Session index path | Async coalesced index updates, pending-drain hot-path tuning, and reduced allocation/boxing overhead | Keeps resume/discovery metadata updates off the interactive hot path |
| Session resume path | Session Store V2 sidecar (segmented log + offset index), O(index+tail) open path, stale-sidecar detection, migration/rollback tooling | Avoids full-file rescans on large histories and improves recovery behavior |
| Long-session maintenance | Background compaction/snapshot workers with quotas + lazy hydration paths | Controls session growth and keeps long-running workspaces responsive |
| Compaction internals | Binary-search cut-point logic, serialization helper extraction, and zero-allocation-oriented cleanup on compaction paths | Lowers compaction overhead and keeps compaction from becoming a pause source |
| Streaming internals | SSE parser event-type interning, scanned-byte tracking (`scanned_len`), UTF-8 recovery hardening | Reduces repeated scanning/allocation during incremental response streaming and improves resilience |
| Provider/message memory path | Zero-copy request context migration (`Cow`), `Arc`-based message/result sharing, clone elimination in stream end paths | Removes hot-path cloning and allocation churn in core agent/provider loops |
| TUI rendering path | Message render cache, conversation-prefix cache, reusable render buffers, viewport/render-path refactors, Criterion perf gates | Reduces redraw cost and jitter while streaming long outputs |
| Startup/resource loading | Parallelized skill/prompt/theme loading plus precomputed tool definitions/command names | Moves heavy initialization off the critical path to improve time-to-interaction |
| Allocator/build profile | Size-budgeted default release profile, opt-in jemalloc benchmark variants, strict artifact validation for perf claims | Keeps shipping artifacts within budget and prevents benchmark/reporting drift |
| Perf governance | Scenario matrix, claim-integrity gates, strict no-data fail behavior, variance/confidence artifacts, reproducible orchestration bundles | Makes performance claims auditable and regression detection automatic |
| Hostcall marshalling planner | `HostcallRewriteEngine` uses a small cost model to pick fast opcode fusion only when it is clearly cheaper and unambiguous; otherwise it stays on the canonical path and records fallback reason | Gains speed on hot marshalling shapes without risking silent semantic drift from ambiguous rewrites |
| Tool text-processing hot path | `truncate_head`/`truncate_tail` use lazy line traversal and `memchr` line counting; normalization switched to a single-pass transform instead of chained string rewrites | Large file/tool outputs avoid unnecessary intermediate allocations and stay responsive |
| JS bridge regex and parser micro-paths | Frequent regex checks in the extension JS bridge are `OnceLock`-cached; hot bridge calls avoid repeated setup work | Cuts repeated per-call overhead in extension-heavy sessions |
| CLI/autocomplete process-spawn reduction | `fd`/`rg` availability checks are cached, and autocomplete file-index refresh runs in background with stale-update dropping | Keeps completion and command handling quick even in very large repositories |
| Session identity/index micro-optimizations | O(1) entry-id cache, single-pass metadata finalization, and append-path cleanups replace multi-pass/copy-heavy behavior | Reduces overhead on append/save/rebuild when sessions become large |
| Benchmark-driven rollback discipline | Candidate micro-optimizations are benchmarked and can be reverted when real workloads regress (for example, a newline-scan `memchr` swap in SSE was rolled back) | Prevents "optimizations" that look fast in theory but slow down real usage |

Optimization spans algorithms, execution lanes, memory movement, queue discipline, storage layout, and validation policy as one system.

### Release Profile and Binary Size Gate

The shipping release profile is tuned to keep release artifacts inside the
binary-size budget while preserving cross-crate optimization:

```toml
[profile.release]
opt-level = "z"      # Optimize generated code for size
lto = true           # Link-time optimization across all crates
codegen-units = 1    # Single codegen unit (slower compile, better optimization)
panic = "abort"      # No unwinding machinery
strip = true         # Remove symbol tables
```

Binary size is explicitly budgeted in CI via `binary_size_release`, with a target
threshold of `22.0 MiB` (the harness computes bytes / 1024 / 1024). A fresh
`v0.2.0` release measurement is required before reporting the achieved size.
Default release builds keep heavyweight extras opt-in; use `--features full`
when you need the image, clipboard, wasm, jemalloc, and syntax-highlighting
extras in one build.

### Benchmark Evidence vs Shipping Artifacts

Pi separates performance evidence artifacts from distributable release artifacts:

- **Benchmark evidence artifacts** (PERF-3X and certification inputs) are produced by
  `scripts/perf/orchestrate.sh` and `scripts/bench_extension_workloads.sh`, and must carry
  run provenance (`correlation_id`) plus profile labels (for example `build_profile=perf`).
- **Shipping artifacts** (end-user binaries) are built with Cargo `release` profile and
  distributed via GitHub Releases + installer paths.

Policy implication: release/size artifacts alone are not valid evidence for global performance
claims. Performance claims must cite benchmark evidence bundles with reproducible provenance.
See `docs/testing-policy.md` and `docs/releasing.md` for normative policy details.

Current checked-in performance evidence state:
- Run output: `tests/perf/reports/` (budget_summary.json, PERF_BUDGETS.md)
- The current budget summary is a blocker artifact, not claim support: without
  strict, source-bound, same-run lineage it records every declared budget as
  `NO_DATA` and does not run the artifact data-contract evaluation. Its empty
  failure list therefore means "not evaluated," not "contracts passed."
- Before spending time on a definitive refresh, run
  `python3 scripts/perf/preflight_budget_inputs.py` to list missing budget
  inputs, expected artifact paths, and RCH-only refresh commands.
- Orchestrated perf runs also write
  `results/perf_budget_preflight_before_refresh.json` before any
  `budget_summary.json` refresh and `results/perf_artifact_staging_manifest.json`
  after collection. The staging manifest records each required artifact's source
  path, RCH remote-source prefix when supplied via `PERF_REMOTE_TARGET_DIR`,
  retrieved local path, schema, size, mtime, checksum, and explicit blocker
  status.
- Perf evidence cache entries live under
  `PERF_EVIDENCE_CACHE_DIR` (default:
  `$CARGO_TARGET_DIR/perf/evidence_cache`) with schema
  `pi.perf.evidence_cache.v1`. Preflight and staging may reuse cached evidence
  only when the entry's schema, command, git commit, build profile, run
  ID/correlation ID, host/toolchain provenance, checksum, and TTL validate; reused
  artifacts are labeled `source_kind=cache`/`evidence_source=cache` in the JSON
  outputs. Override the cache lifetime with
  `PI_PERF_EVIDENCE_CACHE_TTL_HOURS`.
- `env_fingerprint.json` records cgroup-aware host topology with schema
  `pi.perf.host_topology_fingerprint.v1`: cgroup v2 CPU quota, cpuset size,
  memory limits, NUMA node count, caveats, and a constrained `budget_profile`
  for containerized hosts.
- Regenerate the perf evidence bundle before adding release-facing speed,
  throughput, memory, or startup numbers to this README.

Historical certification/evidence refresh (`2026-05-15` progress SLO closeout; `2026-05-15` extension gate; `2026-05-14` full-suite reports; `2026-05-18` drop-in certification verdict). These results do not certify the current source revision or `v0.2.0`:
- Unified evidence bundle: `29/29` sections present, `0` missing, `0` invalid *(from tests/evidence_bundle/index.json)*
- Full-suite gate: `20/20` gates passed, including `14/14` blocking gates *(from tests/full_suite_gate/full_suite_verdict.json)*
- Historical drop-in result: `22/22` certification gates passed, overall verdict `CERTIFIED` for source `52e9fbfb24352045985b59df9d7ea63f1f8f2ef8` only *(from the verdict blob stored at Git revision `2fc4b8c0b77ded267cf5e0f517f4b6fa87f45e91`, not from the live verdict file)*
- Historical extension gate: `123/123` then-observed must-pass extensions passed; stretch set `100/101` passed with only non-blocking stretch failures *(from tests/ext_conformance/reports/gate/must_pass_gate_verdict.json)*
- Context-intelligence closeout gate: `pass`, with child Beads mapped to code, tests, docs/evidence, validation commands, pushed commits, redaction posture, perf-budget evidence, README freshness, staged UBS, and Beads ledger reconciliation *(from docs/evidence/context-intelligence-closeout-gate.json)*
- Progress SLO closeout gate: `pass`, with child Beads mapped to code, tests, docs/evidence, validation commands, pushed commits, source-boundary checks, stress-budget evidence, README freshness, staged UBS, and Beads ledger reconciliation *(from docs/evidence/swarm-progress-slo-closeout-gate.json)*
- Runtime-intelligence closeout gate: `pass`, with child Beads mapped to compaction admission, tool-output artifacts, provider routing, scheduler fairness, frame-budget telemetry, cancellation cleanup, extension safety provenance, docs/evidence, source-boundary checks, pushed commits, staged UBS, and Beads ledger reconciliation *(from docs/evidence/runtime-intelligence-closeout-gate.json)*

### Fast Loop vs Definitive Benchmarks

For day-to-day implementation, use targeted checks to keep iteration fast. Reserve definitive
benchmark conclusions for integration boundaries where full evidence is regenerated.

- **Fast loop (non-authoritative):** file-scoped `cargo fmt --check` and targeted test replays (`rch exec -- cargo test --test ...` when compilation is non-trivial).
- **Definitive pass (authoritative):** offload heavy runs with strict remote gating (`rch exec -- ...` or script wrappers with `--require-rch`), then require
  updated evidence artifacts:
  - `tests/perf/reports/phase1_matrix_validation.json`
  - `tests/full_suite_gate/full_suite_verdict.json`
  - `tests/full_suite_gate/certification_verdict.json`
  - `tests/full_suite_gate/extension_remediation_backlog.json`

This keeps inner loops responsive while preserving strict claim-integrity at release time.

### Extension Workload Hotspot Profiling

Pi includes a dedicated workload harness for extension runtime bottlenecks:

```bash
cargo run --example ext_workloads -- \
  --out artifacts/perf/ext_workloads.jsonl \
  --matrix-out artifacts/perf/ext_hostcall_hotspot_matrix.json \
  --trace-out artifacts/perf/ext_hostcall_bridge_trace.jsonl
```

This harness does more than raw timing:

- Breaks hostcall cost into six stages: `marshal`, `queue`, `schedule`, `policy`, `execute`, `io`
- Produces a hotspot matrix (`pi.ext.hostcall_hotspot_matrix.v1`) for quick bottleneck ranking
- Produces bridge trace events (`pi.ext.hostcall_trace.v1`) for per-call debugging
- Measures how stage pairs interact and verifies full stage-pair coverage
- Generates a VOI scheduler plan (`pi.ext.voi_scheduler.v1`) to recommend the next highest-value experiments under a fixed overhead budget

In plain terms: it helps answer "what should we optimize next?" with data, not guesswork.

For multi-agent operator handoff, `scripts/build_swarm_operator_runpack.py` can
also project a diagnostic-only `bottleneck_attribution` dashboard from the tail
latency report, swarm flight recorder report, swarm resource preflight,
hostcall admission swarm profile, session recovery swarm profile, concurrent
RPC swarm E2E evidence, and RCH artifact-sync preflight. The dashboard is
operator evidence only; release-facing speed, drop-in, or performance claims
still require the claim-integrity gates below to pass.

The runpack also embeds `predictive_telemetry_ledger`
(`pi.swarm.predictive_telemetry_ledger.v1`) and can write it separately with
`--out-predictive-telemetry-ledger-json` or print it with
`--print-predictive-telemetry-ledger`. The ledger is read-only advisory
evidence: it joins existing RCH, Agent Mail, Beads, turn-pressure,
bottleneck-attribution, and claim-readiness signals into ordered pressure
observations and next-bottleneck hypotheses, but it does not mutate the
scheduler, Agent Mail, RCH, Beads, git, or release/capacity claims.

The runpack also embeds `validation_scheduler_plan`
(`pi.swarm.validation_scheduler_plan.v1`) and can write it separately with
`--out-validation-scheduler-plan-json` or print it with
`--print-validation-scheduler-plan`. This is an advisory RCH-aware simulator:
it ranks fast script checks, evidence regeneration, focused tests,
E2E/conformance, `cargo check --all-targets`, and clippy from the current git
change profile, predictive telemetry, RCH admission, remote proof, and target
cache evidence. It preserves exact command strings and required RCH env, but it
does not execute cargo, mutate RCH, reserve Agent Mail, claim Beads, delete temp
artifacts, or allow heavy cargo to fail open into a local build.

If validation-broker status or plan JSON is supplied with
`--validation-broker-json`, the runpack projects source status, slot counts,
stale slot warnings, duplicate-gate opportunities, and recommended next actions
as advisory handoff data. This projection does not replace RCH, Doctor, Beads,
Agent Mail, CI, UBS, `cargo_headroom.sh`, or release-claim gates. Operator
workflow, privacy, and degraded-data guidance lives in
[docs/swarm-operations-runbook.md#validation-broker-operator-workflow](docs/swarm-operations-runbook.md#validation-broker-operator-workflow).

If progress SLO JSON is supplied with `--progress-slo-json`, the runpack
projects the current swarm progress posture, confidence, saturation summary,
source freshness, redaction posture, and next actions as advisory handoff data.
This projection does not replace Beads, Agent Mail, RCH, Doctor, validation
gates, git, or release-claim gates. Operator workflow, privacy, degraded-source
handling, stale-Beads handling, RCH pressure interpretation, and no-open-work
convergence guidance lives in
[docs/swarm-operations-runbook.md#progress-slo-operator-workflow](docs/swarm-operations-runbook.md#progress-slo-operator-workflow).

The same runpack command can emit a dry-run swarm autopilot input pack and plan
beside the handoff bundle. When those companion artifacts are requested, the
runpack JSON/Markdown includes an `autopilot_handoff` summary
(`pi.swarm.autopilot_handoff.v1`) with
`pi.swarm.autopilot_input_pack.v1` and `pi.swarm.autopilot_plan.v1` schema
references, selected advisory action, artifact paths, and source provenance.
The autopilot never mutates ownership or replaces Doctor, Beads, Agent Mail,
RCH, git, or the source artifacts; it only turns those inputs into reproducible
operator next-action guidance. Its work-admission gate includes a read-only
dry-run executor that classifies plan items as `would_execute`, `blocked`,
`requires_operator`, or `never_execute` so deletion requests, Agent Mail/RCH
mutation, local heavyweight Cargo, and Beads ownership bypasses are rejected
without executing anything.

For degraded-coordination handoff proof, the runpack script also has a no-mock
E2E mode (`--run-degraded-coordination-e2e`) that feeds real temporary Beads
state plus fixture-captured Agent Mail and RCH failures through the runpack
builder. The emitted evidence stays operator-only: it proves Beads soft-lock
fallback, degraded validation, and no cleanup/deletion-command output without
supporting release-facing speed or drop-in claims.

Fourth-wave swarm self-healing guidance layers a read-only stale-evidence
renewal queue, dry-run action plan, work admission gate, budget lease
simulation, turn pressure ledger, extension quarantine rehearsal, and redacted
handoff summaries onto that runpack flow. These artifacts can recommend
`renew_stale_evidence`, `wait_for_pressure`, `use_beads_soft_lock`, or similar
operator actions, but they do not mutate Beads, Agent Mail, extension
configuration, git, evidence files, RCH jobs, or release claims. The full
workflow and safe handoff wording live in
[docs/swarm-operations-runbook.md#fourth-wave-self-healing-workflow](docs/swarm-operations-runbook.md#fourth-wave-self-healing-workflow).

The fourth-wave closeout gate emits
`pi.swarm.fourth_wave_self_healing.closeout_gate.v1`. It maps the
`bd-63x3v.7` child Beads to implementation artifacts, contracts, fixtures,
operator docs, validation commands, pushed refs, source-boundary checks, and
remaining advisory limits. It is governed by
`docs/contracts/fourth-wave-self-healing-closeout-gate-contract.json`; the
current closeout artifact is
`docs/evidence/fourth-wave-self-healing-closeout-gate.json`.

Offline swarm replay traces can be previewed before handoff with
`pi swarm-replay-preview --trace <trace.json> --format json`. The command is
read-only, emits `pi.swarm.replay_preview.v1` JSON or concise text, refuses to
overwrite requested output files, and can feed
`scripts/build_swarm_operator_runpack.py --swarm-replay-preview-json <preview.json>`
so runpacks carry the replay policy comparison without treating the runpack as
source-of-truth replay evidence. Operator workflow, privacy, and degraded-data
guidance lives in [docs/swarm-replay-operator-workflow.md](docs/swarm-replay-operator-workflow.md).

The autopilot closeout gate emits `pi.swarm.autopilot_decision_gate.v1` to audit
the shipped input pack, planner, work partitions, failure actions, budget drift
watcher, E2E/logging evidence, runpack handoff, safety guards, pushed commits,
and quality gates before the swarm-autopilot epic is closed.

Context intelligence has its own operator guide and closeout gate. See
[docs/context-intelligence.md](docs/context-intelligence.md) for configuration,
preview workflows, failure modes, privacy posture, examples, troubleshooting,
and final closeout evidence. The context-intelligence closeout gate emits
`pi.context_intelligence.closeout_gate.v1` and is governed by
`docs/contracts/context-intelligence-closeout-gate-contract.json`; the current
closeout artifact is `docs/evidence/context-intelligence-closeout-gate.json`.

The progress-SLO closeout gate emits `pi.swarm.progress_slo.closeout_gate.v1`.
It audits the shipped contract/source inventory, deterministic evaluator,
read-only CLI, Doctor/runpack projection, no-mock E2E evidence, large-host
stress budgets, operator docs, pushed child commits, source-boundary checks,
staged UBS, and Beads ledger reconciliation. It is governed by
`docs/contracts/swarm-progress-slo-closeout-gate-contract.json`; the current
closeout artifact is `docs/evidence/swarm-progress-slo-closeout-gate.json`.

The runtime-intelligence closeout gate emits
`pi.runtime_intelligence.closeout_gate.v1`. It maps the `bd-h66tp` child Beads
to compaction admission, tool-output artifacts, provider routing, scheduler
fairness, frame-budget telemetry, cancellation cleanup, extension safety
provenance, docs/evidence, source-boundary checks, pushed refs, and quality
gates. It is governed by
`docs/contracts/runtime-intelligence-closeout-gate-contract.json`; the current
artifact is `docs/evidence/runtime-intelligence-closeout-gate.json`. The gate
is advisory closeout evidence only and does not replace Beads, git, RCH, UBS,
CI, claim-integrity gates, or source artifacts.

The sixth-wave validation-hardening closeout gate emits
`pi.swarm.validation_hardening.closeout_gate.v1`. It maps the `bd-63x3v.9`
child Beads to RCH workspace-shadow detection, Agent Mail degraded Beads
soft-lock evidence, validation proof replay, temp artifact inventory, stale
claim heuristics, degraded-coordination E2E proof, cgroup/NUMA/RCH budget
modeling, provider/RPC/TUI backpressure budgets, operator explanations, pushed
refs, source-boundary checks, and quality gates. The provider/RPC/TUI contract
now includes a synthetic fairness stress gate for balanced load, one slow
provider stream, RPC output flood, and TUI frame-budget pressure, with fail-closed
negative controls when one surface reports success while another starves. It is governed by
`docs/contracts/sixth-wave-validation-hardening-closeout-gate-contract.json`;
the current artifact is
`docs/evidence/sixth-wave-validation-hardening-closeout-gate.json`. The gate is
advisory closeout evidence only and does not replace Beads, git, RCH, UBS, CI,
claim-integrity gates, or source artifacts.

The seventh-wave runtime-autonomy closeout gate emits
`pi.swarm.runtime_autonomy.closeout_gate.v1`. It maps the `bd-63x3v.10`
child Beads to effect-aware tool batching, fail-closed validation-proof reuse,
cgroup/NUMA lane placement, large-session replay acceleration, provider/RPC/TUI
fairness, work-admission dry-run execution, hostcall QoS starvation evidence,
source-boundary checks, pushed refs, and quality gates. It is governed by
`docs/contracts/seventh-wave-runtime-autonomy-closeout-gate-contract.json`; the
current artifact is
`docs/evidence/seventh-wave-runtime-autonomy-closeout-gate.json`. The gate is
advisory closeout evidence only and does not replace Beads, git, RCH, UBS, CI,
claim-integrity gates, child evidence, or source artifacts.

The proof-carrying swarm test-fabric closeout gate emits
`pi.swarm.proof_carrying_test_fabric.closeout_gate.v1`. It maps the `bd-zeccr`
child Beads to no-mock lifecycle E2E evidence, cross-surface conformance,
operator evidence goldens, structure-aware fuzz/property coverage, metamorphic
replay equivalence, source-boundary checks, pushed refs, negative controls, and
quality gates. It is governed by
`docs/contracts/proof-carrying-swarm-test-fabric-closeout-gate-contract.json`;
the current artifact is
`docs/evidence/proof-carrying-swarm-test-fabric-closeout-gate.json`. The gate is
advisory closeout evidence only and does not replace Beads, git, RCH, Agent
Mail, UBS, CI, claim-integrity gates, child evidence, or source artifacts.

The predictive-operations closeout gate emits
`pi.swarm.predictive_operations.closeout_gate.v1`. It maps the
`bd-63x3v.11` child Beads to predictive telemetry fusion, RCH-aware validation
scheduling, semantic compaction quality, extension hostcall cost attribution,
operator-perceived latency, redundant-agent-work detection, source-boundary
checks, pushed refs, staged UBS, and Beads ledger reconciliation. It is
governed by
`docs/contracts/predictive-operations-closeout-gate-contract.json`; the current
artifact is `docs/evidence/predictive-operations-closeout-gate.json`. The gate
is advisory closeout evidence only and does not replace Beads, git, RCH, Agent
Mail, UBS, CI, claim-integrity gates, child evidence, or source artifacts.

The ninth-wave incident replay and proof-memory closeout gate emits
`pi.swarm.incident_replay_proof_memory.closeout_gate.v1`. It maps the
`bd-9yq7i` child Beads to incident corpus, incident replay, validation
proof-memory, operator work recommendation, operator smoothness SLO, extension
resource firewall matrix, and no-mock incident replay E2E evidence. It is
governed by
`docs/contracts/ninth-wave-incident-replay-proof-memory-closeout-gate-contract.json`;
the current artifact is
`docs/evidence/ninth-wave-incident-replay-proof-memory-closeout-gate.json`. The
gate is advisory closeout evidence only and does not replace Beads, git, RCH,
Agent Mail, UBS, CI, claim-integrity gates, child evidence, generated target/perf
outputs, prior-wave evidence, or source artifacts.

The closeout evidence registry at
`docs/contracts/closeout-evidence-registry.json` is the machine-readable index
of current advisory closeout-gate artifacts. It binds each current artifact to
its governing contract, decision schema, source bead or epic, Markdown
references, and advisory boundary, including current artifacts that are not
expanded above in prose: `docs/evidence/adaptive-execution-closeout-gate.json`,
`docs/evidence/extension-compatibility-closeout-gate.json`, and
`docs/evidence/swarm-replay-closeout-gate.json`. The registry is not a release
manifest, performance evidence bundle, Beads source of truth, Agent Mail
authority, RCH authority, CI/UBS replacement, drop-in certification gate, or
permission to delete or mutate files.

The operator-perceived latency trace emits
`pi.operator.perceived_latency_trace.v1`. It joins deterministic provider,
RPC, TUI, tool-update, and operator-visible fixture timelines so operators can
see when semantic output became visible while low-value updates were coalesced.
It is governed by
`docs/contracts/operator-perceived-latency-trace-contract.json`; the current
fixture artifact is `docs/evidence/operator-perceived-latency-trace.json`.
The trace is advisory only and does not authorize benchmark, capacity, release
performance, strict drop-in, or backpressure-budget replacement claims.

The operator smoothness SLO emits `pi.operator.smoothness_slo.v1`. It uses
deterministic high-volume fixtures for provider stream deltas, RPC output
pressure, TUI frame rendering, tool-update coalescing, and session-write
pressure. It is governed by
`docs/contracts/operator-smoothness-slo-contract.json`; the current fixture
artifact is `docs/evidence/operator-smoothness-slo.json`. Its p50/p95/p99
counters are engineering fixture counters only, proving semantic milestones
remain visible while low-value updates can be coalesced; they are not release
benchmark, capacity, performance, or strict drop-in evidence.

The extension resource firewall matrix emits `pi.ext.resource_firewall_matrix.v1`
from the deterministic extension stress fixture. It covers cheap-read floods,
large payload emission, denied capability churn, slow hostcalls, repeated
failure, and steady-peer progress while preserving payload redaction and
existing extension capability boundaries. The contract is
`docs/contracts/extension-resource-firewall-matrix-contract.json`; focused test
runs write `resource_firewall_matrix.json` under the configured `target/perf`
directory. This matrix is advisory stress evidence only and does not replace
runtime enforcement, hostcall cost attribution, RCH validation, Agent Mail,
Beads, UBS, CI, or benchmark/capacity/release claims.

The swarm incident corpus emits `pi.swarm.incident_corpus.v1`. It records
deterministic degraded-source fixtures for Agent Mail schema corruption, RCH
saturation with local-fallback denial, stale evidence, duplicate work risk,
dirty worktree admission denial, malformed sources, and deletion or live
mutation rejection. It is governed by
`docs/contracts/swarm-incident-corpus-contract.json`; the current fixture
artifact is `docs/evidence/swarm-incident-corpus.json`. The corpus is operator
evidence only and does not replace release performance, drop-in certification,
Agent Mail, RCH, Beads, git, source artifacts, or destructive-action authority.

The swarm incident replay harness emits `pi.swarm.incident_replay.v1`. It
consumes the checked-in incident corpus and replays healthy, degraded Agent Mail,
saturated RCH, duplicate work, stale evidence, malformed source, dirty worktree,
and deletion-request scenarios into ordered phases, per-step assertions,
redacted excerpts, selected safe actions, and follow-up recommendations. It is
governed by `docs/contracts/swarm-incident-replay-contract.json`; the current
fixture artifact is `docs/evidence/swarm-incident-replay.json`. Replay output is
read-only operator evidence and cannot replace source systems or authorize live
Agent Mail, RCH, Beads, git, deletion, release, benchmark, or drop-in claims.

The swarm incident replay E2E harness emits `pi.swarm.incident_replay_e2e.v1`
plus `pi.swarm.incident_replay_e2e.event.v1` JSONL events. It joins real
temporary Beads and git workspaces with fixture-captured degraded Agent Mail and
RCH inputs, then verifies healthy replay, Beads soft-lock fallback, RCH proof
refresh backoff, duplicate-work risk, dirty worktree denial, stale proof-memory
refresh, extension resource firewall failure, and smoothness SLO failure. It is
governed by `docs/contracts/swarm-incident-replay-e2e-contract.json`; the
current fixture artifact is `docs/evidence/swarm-incident-replay-e2e.json`.
This evidence is advisory only and does not authorize live source mutation,
local heavyweight Cargo fallback, release, benchmark, capacity, or drop-in
claims.

The validation proof-memory index emits `pi.validation.proof_memory_index.v1`.
It indexes existing remote validation proof fixtures by command fingerprint,
git head, touched paths, RCH provenance, artifact retrieval hash, freshness, and
reuse eligibility. The index is governed by
`docs/contracts/validation-proof-memory-index-contract.json`; the current
fixture artifact is `docs/evidence/validation-proof-memory-index.json`. It is
advisory operator evidence only: stale refs, missing artifacts, local fallback,
dirty worktree mismatch, command mismatch, uncovered paths, or
non-authoritative coverage all fail closed and require fresh validation.

The operator work recommender emits
`pi.swarm.operator_work_recommendation.v1`. It consumes the incident replay and
proof-memory artifacts, then ranks read-only next-work decisions for ready Beads,
no ready work, Agent Mail corruption, RCH saturation, stale proof refresh,
duplicate-work risk, and dirty-worktree admission denial. It is governed by
`docs/contracts/operator-work-recommendation-contract.json`; the current
fixture artifact is `docs/evidence/operator-work-recommendation.json`. Its
recommendations are advisory only and never claim, reserve, launch RCH, run
cargo, mutate git, delete files, or replace source systems.

For the full launch, throttling, recovery, and handoff workflow for large
multi-agent runs, see [docs/swarm-operations-runbook.md](docs/swarm-operations-runbook.md).

### Claim-Integrity Gates for Performance Reporting

Pi's perf pipeline includes strict evidence checks so global speed claims cannot be based on partial or stale data.

**v0.2.0 performance-claim status: NOT authorized.** Its checked-in
`pi.perf.budget_summary.v2` artifact is an explicit blocked/NO_DATA result, so
this release may ship only without quantitative or global performance claims.
That blocked state is not a passing benchmark result.

- `scripts/perf/orchestrate.sh` generates artifacts tied to a shared `correlation_id` for the same run.
- `scripts/e2e/run_all.sh` validates required schemas, freshness, and `correlation_id` alignment before considering claims valid.
- `tests/release_evidence_gate.rs` and `scripts/release_gate.sh` always reject malformed schemas, a non-canonical budget-inventory digest, reordered/duplicate/missing results, mismatched comparison semantics, inconsistent counts/statuses, or forged readiness fields. A coherent `blocked` summary is only a release warning when `RELEASE_GATE_REQUIRE_PERFORMANCE_CLAIM_READY=0`; it still cannot authorize performance copy.
- A `claim_ready` summary must come from strict mode with data for every declared budget, no declared budget failures or data-contract failures, complete CI coverage, one matching `run_id`/`correlation_id`, and fresh source-bound evidence. The aggregate `budget_data_missing` and `budget_failed` blockers prevent non-CI informational budgets from being silently excluded from the global authorization boolean. The gate also proves its exact canonical strict test was listed once, executed once, and was not ignored; that test freshly recomputes and deep-compares the checked-in definitions, results, failures, counts, and readiness instead of accepting an unrelated passing benchmark run.
- `scripts/e2e/run_all.sh` emits an evidence-adjudication matrix and only treats evidence as canonical when freshness and lineage checks both pass.
- Key release-facing artifacts include:
  - `pi.perf.extension_benchmark_stratification.v1`
  - `pi.perf.phase1_matrix_validation.v1`
  - `pi.claim_integrity.evidence_adjudication_matrix.v1`

If the evidence set is incomplete or contradictory, the claim-integrity gate stays closed and reports exactly why.

### Allocator Strategy for Benchmarks

Default release builds use the platform allocator to stay inside the shipping
binary-size budget. FreeBSD and MSVC builds also always use the platform
allocator; FreeBSD libc already uses jemalloc internally, and mixing allocator
domains across libc/pthread and C dependencies can turn heap corruption into
thread-spawn crashes. For allocator experiments, Pi supports explicit `system`
and `jemalloc` benchmark variants:

```bash
# System allocator baseline + jemalloc variant in one repeatable run
BENCH_ALLOCATORS_CSV=system,jemalloc \
  ./scripts/bench_extension_workloads.sh
```

The benchmark harness records both requested and effective allocator metadata in
its JSONL output (`allocator_requested`, `allocator_effective`,
`allocator_fallback_reason`) via `PI_BENCH_ALLOCATOR`.

- `system`: build with the explicit benchmark feature set except `jemalloc`
- `jemalloc`: build with `--features jemalloc` where supported by the target
- `auto`: prefer `jemalloc`, fall back to `system` if build fails

If `jemalloc` is requested but unavailable for the current build, the run fails
closed to the compiled allocator and includes a fallback reason in artifacts.

### Memory Usage

Rust's ownership model avoids a garbage-collected application runtime. Current
memory figures are intentionally withheld until the `v0.2.0` evidence inputs
are regenerated with matching source and run provenance.

### Streaming Architecture

Responses move incrementally from the API to the terminal with bounded buffering
for incomplete network and SSE event frames:

```
API Server → TCP → SSE Parser → Event Handler → Terminal
     │                              │
     └──── incremental bounded buffering ────┘
```

The SSE parser processes events as they arrive rather than waiting for complete responses.

### TUI Rendering Performance

The interactive TUI has a 60fps design target, with several optimization layers:

**Frame timing telemetry**: Every render cycle is instrumented. Slow frames (>16ms) are tracked and classifiable by phase: viewport sync, message encoding, markdown rendering. This data feeds the internal performance monitor.

**Message render cache**: Markdown-to-ANSI conversion involves syntax highlighting, table layout, and link detection. Pi caches the rendered output per message and invalidates only on theme change or terminal resize. During streaming, only the actively-changing message is re-rendered; all prior messages hit the cache.

**Pre-allocated render buffers**: The `RenderBuffers` struct is reused across render cycles. Rather than allocating new `String` buffers each frame, Pi writes into pre-sized buffers and clears them before reuse, avoiding repeated small allocations during streaming.

**Memory pressure monitoring**: A `MemoryMonitor` samples process heap size and classifies it into three tiers:

| Tier | Threshold | Action |
|------|-----------|--------|
| **Normal** | <80% of budget | No action |
| **Pressure** | 80-95% of budget | Collapse tool output displays, hide thinking blocks |
| **Critical** | >95% of budget | Truncate older messages, force compaction |

Progressive degradation keeps Pi responsive during long sessions with accumulated tool output.

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
| **Not all provider APIs** | Built-in support is backed by 11 native provider implementation modules: Anthropic, OpenAI Chat, OpenAI Responses/Codex Responses, Gemini, Cohere, Azure OpenAI, Bedrock, Vertex AI, GitHub Copilot, GitLab Duo, and Cursor; some ecosystem-specific APIs are still TBD |
| **No web browsing** | Use bash with curl |
| **No GUI** | Terminal-only by design |
| **Some extensions need npm stubs** | Common stubs are provided; unlisted npm packages still require a stub. See docs/planning/EXTENSIONS.md §8.1 |
| **English-centric** | Works but not optimized for other languages |
| **Pinned Rust toolchain** | Releases are validated with `nightly-2026-07-05`; locked dependencies require Rust 1.95+ |

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

The Rust implementation can be validated against the TypeScript original without coupling to implementation details.

### Extension System

Pi supports legacy JS/TS extensions via an embedded QuickJS runtime. Unlike
traditional plugin systems, extensions run in a **sandboxed, capability-gated**
environment with no ambient OS access:

1. **No Node/Bun required**: QuickJS + Pi-provided shims for common Node APIs
2. **Capability-based security**: each host connector call is policy-checked and logged
3. **Conformance-tested**: status is tracked in `docs/ext-compat.md` and `tests/ext_conformance/reports/pipeline/`
4. **Measured load paths**: cold and warm extension loads are instrumented; fresh release measurements are pending

Legacy extension behavior is automatic:
- Existing `.js/.ts` extensions run directly (no manual conversion step).
- `*.native.json` descriptors are optional and mainly useful for native-rust runtime workflows.
- One session currently uses one runtime family at a time (JS/TS or native descriptor).

Policy preset quick-start:

```bash
# Inspect current effective policy
pi --explain-extension-policy

# Switch profile for one command (safe | balanced | permissive)
pi --extension-policy balanced --explain-extension-policy

# Legacy alias is still accepted:
pi --extension-policy standard --explain-extension-policy

# Narrow dangerous-capability opt-in (preferred over permissive)
PI_EXTENSION_ALLOW_DANGEROUS=1 pi --extension-policy balanced --explain-extension-policy
```

Operator rollout playbook (compatibility-first local defaults + explicit lock-down):

```bash
# 1) Baseline: verify defaults are compatibility-first (`permissive`)
pi --explain-extension-policy

# 2) Staging: use balanced prompting, dangerous caps still denied by default
pi --extension-policy balanced --explain-extension-policy

# 3) Explicit lock-down for strict local/CI runs
pi --extension-policy safe --explain-extension-policy

# 4) Narrow opt-in for dangerous capabilities (preferred path)
PI_EXTENSION_ALLOW_DANGEROUS=1 pi --extension-policy balanced --explain-extension-policy

# 5) Explicit permissive mode when you want to be unambiguous
pi --extension-policy permissive --explain-extension-policy
```

`settings.json` baseline for local/dev:

```json
{
  "extensionPolicy": {
    "defaultPermissive": true
  }
}
```

Use this to restore the stricter fallback without CLI flags:

```json
{
  "extensionPolicy": {
    "defaultPermissive": false
  }
}
```

Interactive TUI: open `/settings` and toggle `extensionPolicy.defaultPermissive`.

CI guidance:

```bash
# CI default: keep dangerous capabilities disabled
pi --extension-policy safe --explain-extension-policy

# CI opt-in job (only where required), keep explicit and auditable
PI_EXTENSION_ALLOW_DANGEROUS=1 pi --extension-policy balanced --explain-extension-policy
```

Rollback rule: remove `PI_EXTENSION_ALLOW_DANGEROUS`, set `extensionPolicy.profile`
back to `safe` or set `extensionPolicy.defaultPermissive` to `false`, and re-run
`pi --explain-extension-policy` to confirm deny decisions.

See [EXTENSIONS.md](docs/planning/EXTENSIONS.md) for the full architecture, runtime contract,
and conformance results.

### Unsafe Forbidden

The `#![forbid(unsafe_code)]` directive is project-wide and non-negotiable. Rationale:

- **Attack surface**: Pi executes user-provided shell commands and reads arbitrary files
- **Memory bugs = security bugs**: Buffer overflows or use-after-free in this context could be exploitable
- **No safety-for-speed trade**: The project forbids unsafe Rust regardless of workload profile
- **Dependency boundary is explicit**: `forbid(unsafe_code)` governs this
  project's Rust code; transitive dependencies remain subject to separate
  review and supply-chain checks

The safe Rust subset provides all necessary functionality without compromising security.

---

## FAQ

**Q: What's the relationship to the original Pi Agent?**
A: This is an authorized Rust port of [Pi Agent](https://github.com/badlogic/pi) by [Mario Zechner](https://github.com/badlogic), created with his blessing. The architecture differs significantly from the TypeScript original: it uses [asupersync](https://github.com/Dicklesworthstone/asupersync) for structured concurrency and [rich_rust](https://github.com/Dicklesworthstone/rich_rust) (a port of Will McGugan's [Rich](https://github.com/Textualize/rich) library) for terminal rendering. The goal is idiomatic Rust while preserving Pi Agent's UX.

**Q: Why rewrite in Rust?**
A: Startup time matters when you're in a terminal all day. Rust provides a native single-binary deployment without a managed application runtime. Fresh comparative measurements are required before this release makes a speed claim.

**Q: Can I use providers beyond Anthropic (OpenAI/Gemini/Cohere/Azure/Bedrock/Vertex/Copilot/GitLab/Cursor/Codex)?**
A: Yes. Pi has 11 native provider implementation modules: Anthropic, OpenAI Chat, OpenAI Responses/Codex Responses, Gemini (native + Gemini CLI + Antigravity routes), Cohere, Azure OpenAI, Amazon Bedrock, Vertex AI, GitHub Copilot, GitLab Duo, and Cursor. Pi also supports many OpenAI-compatible presets (for example Groq, OpenRouter, Mistral, Together, DeepSeek, Cerebras, DeepInfra, Alibaba/Qwen, and Moonshot/Kimi). Provider IDs and aliases are case-insensitive. Set credentials and choose via `--provider`/`--model`; run `pi --list-providers` to see canonical IDs, aliases, and env keys.

**Q: How do sessions work?**
A: By default, each session is a JSONL v3 file with message entries, parent references for branching, and compaction metadata. Builds include `sqlite-sessions` support by default, so configured deployments can use SQLite-backed session storage too; JSONL remains the default store unless configuration selects SQLite.

**Q: Why is unsafe forbidden?**
A: Memory safety is non-negotiable for a tool that executes arbitrary commands. The project does not trade that policy for performance.

**Q: How do I extend Pi?**
A: Pi has a full extension system with two runtime families: JS/TS entrypoints run in embedded QuickJS, and `*.native.json` descriptors run in the native-rust descriptor runtime. Both are capability-gated and audited through the same policy system. One session uses one runtime family at a time. Extensions can register tools, slash commands, event hooks, flags, and custom providers. See [EXTENSIONS.md](docs/planning/EXTENSIONS.md) for details. For built-in tool changes, implement the `Tool` trait in `src/tools.rs`.

**Q: Why isn't X feature included?**
A: Pi focuses on core coding assistance. Features like web browsing, image generation, etc. are out of scope. Use specialized tools for those.

**Q: How does compaction work?**
A: When a conversation exceeds the model's context window, Pi summarizes older messages using the LLM itself, storing the summary as a session entry. Recent messages are kept verbatim. The cut point is chosen at a turn boundary, and the summary includes a record of which files were read or modified so the model retains that awareness. Compaction runs automatically after each agent turn when needed, or manually via `/compact`.

**Q: Can I add a custom provider that Pi doesn't support natively?**
A: Yes. Create a `models.json` file in `~/.pi/agent/` or `.pi/` with entries specifying the model ID, base URL, and API type (usually `openai-completions` for OpenAI-compatible endpoints). Pi's compat config system handles field name differences and feature flag overrides. Extensions can also register entirely custom providers.

**Q: How does Pi decide which session to resume?**
A: Pi maintains a SQLite session metadata index sidecar with WAL/lock handling and stale-index reindexing. When you run `pi -c`, it queries that index for the most recently modified session whose working directory matches your current project, including JSONL sessions and configured SQLite-backed sessions. This avoids scanning the filesystem on every resume.

**Q: What happens if an extension tries to access something dangerous?**
A: Every hostcall from an extension is checked against the active capability policy before execution. Dangerous capabilities (`exec`, `env`) are denied by default under `safe` and `balanced` unless explicitly opted in (for example via `PI_EXTENSION_ALLOW_DANGEROUS=1`), and are available under `permissive`. For `exec`, Pi then applies command mediation before spawn: it classifies command+arg signatures and blocks critical classes by default (for example recursive delete, disk/device write, reverse shell), with strict/safe policy able to block high-tier classes as well (for example shutdown, process-kill, credential-file modification). Denied calls return errors to the extension Promise path, and denial events are recorded in redacted security-alert and exec-mediation audit artifacts. Sensitive env keys (API keys/tokens/secrets) remain filtered. If behavior escalates, you can kill-switch that extension into quarantined `killed` state immediately or force compatibility-lane routing as a containment step while investigating.

**Q: Does Pi work with self-hosted or proxied LLMs?**
A: Yes. Point any provider at a custom base URL via `models.json`. Pi normalizes URL paths per API type and applies compatibility overrides for field-name and feature differences. This works with vLLM, Ollama, LiteLLM, and similar OpenAI-compatible servers.

---

## Comparison

| Feature | Pi | Claude Code | Aider | Cursor |
|---------|-----|-------------|-------|--------|
| **Language** | Rust | TypeScript | Python | Electron |
| **Startup** | Fresh comparative measurement pending | Not measured here | Not measured here | Not measured here |
| **Memory** | Fresh comparative measurement pending | Not measured here | Not measured here | Not measured here |
| **Providers** | 11 native provider implementation modules + OpenAI-compatible presets | Anthropic | Many | Many |
| **Tools** | 9 built-in | Many | File-focused | IDE-integrated |
| **Sessions** | JSONL tree | Proprietary | Git-based | Proprietary |
| **Open source** | Yes | Yes | Yes | No |

---

## Development

### Building

```bash
./scripts/cargo_headroom.sh build           # Debug build (remote offload)
./scripts/cargo_headroom.sh build --release # Release build (optimized, remote offload)
./scripts/cargo_headroom.sh test --all-targets # Full test run with disk preflight
# Lint checks (remote-safe split to avoid rch clippy timeout fail-open)
./scripts/cargo_headroom.sh clippy --lib --bins -- -D warnings
./scripts/cargo_headroom.sh clippy --tests -- -D warnings
./scripts/cargo_headroom.sh clippy --benches -- -D warnings
./scripts/cargo_headroom.sh clippy --examples -- -D warnings
```

When those variables are unset, `cargo_headroom.sh` defaults `CARGO_TARGET_DIR`
and `TMPDIR` to a per-agent directory under `/data/tmp/pi_agent_rust_cargo/...`,
writes a `CACHEDIR.TAG`, rejects accidental repo-root target directories, and
fails before compilation if the target or temp mount has insufficient free
space. Set `PI_CARGO_RUNNER=local` for a local-only run,
`PI_CARGO_BUILD_ROOT=<dir>` for a different large volume, or
`PI_CARGO_HEADROOM_MIN_FREE_MB=<mb>` for smaller focused checks.

Before launching swarms or heavyweight all-target gates, run
`pi doctor --only swarm --format json`. The
`pi.doctor.swarm_resource_preflight.v1` result fails closed when
`CARGO_TARGET_DIR` or `TMPDIR` cannot prove enough scratch headroom, and its
`recommended_budgets` object gives conservative agent, tool, extension hostcall,
RCH fanout, queue-depth, and RSS budgets derived from the effective cgroup CPU,
cpuset, NUMA, and memory limits. The same object includes budget explanations,
local cargo/rustc pressure, and replayable RCH queue posture. For deterministic
replays, provide `PI_DOCTOR_LOCAL_BUILD_PROCESS_COUNT`,
`PI_DOCTOR_RCH_QUEUE_JSON`, or `PI_DOCTOR_RCH_QUEUE_JSON_PATH`; these inputs are
advisory budget controls, not release-facing performance claims. The same
finding also includes `lane_placement` (`pi.doctor.swarm_lane_placement.v1`),
which groups the current cpuset/NUMA topology into read-only operator lanes with
CPU affinity hints, per-lane `CARGO_TARGET_DIR`/`TMPDIR` roots under
`/data/tmp/pi_agent_rust_cargo/<agent>/`, and max agent/tool/hostcall/RCH fanout
recommendations. Doctor reports caveats such as unknown NUMA data, partial
cpusets, tight memory limits, or RCH queue pressure, but it never pins processes
or mutates OS/RCH state.

When `PI_VALIDATION_BROKER_STORE` points at a validation-broker slot JSONL
store, Doctor also emits `pi.doctor.validation_broker_posture.v1` with advisory
slot posture for runpacks and operator handoff. Missing broker configuration is
reported as optional and non-blocking; stale or degraded broker stores remain
visible instead of being promoted to green validation evidence.

### Cargo Feature Defaults

Default builds enable `sqlite-sessions` only. The `sqlite-sessions` feature is
therefore on for normal `cargo build`, `cargo test`, release, and installer
builds; JSONL remains the default session store unless configuration selects
SQLite storage. Heavyweight extras (`image-resize`, `jemalloc`, `clipboard`,
`wasm-host`, and syntax highlighting) are opt-in so the default release binary
stays under the size budget. To build all optional user-facing extras, use:

```bash
./scripts/cargo_headroom.sh build --features full
```

To build a smaller custom subset without SQLite session backend support, use
`--no-default-features` and explicitly re-enable only the features you need:

```bash
./scripts/cargo_headroom.sh build --no-default-features --features clipboard
```

### Testing

```bash
# Unified verification runner (recommended for deterministic evidence artifacts)
./scripts/e2e/run_all.sh --profile focused
./scripts/e2e/run_all.sh --profile ci
./scripts/e2e/run_all.sh --rerun-from tests/e2e_results/<timestamp>/summary.json --skip-unit

# Fast smoke/extension quality wrappers with strict remote enforcement
./scripts/smoke.sh --require-rch
./scripts/ext_quality_pipeline.sh --require-rch

# Multi-agent safety: with CODEX_THREAD_ID set, run_all defaults
# CARGO_TARGET_DIR to target/agents/<CODEX_THREAD_ID> unless overridden.
# Set CARGO_TARGET_DIR explicitly if you want a custom shared or isolated target.

# All tests
./scripts/cargo_headroom.sh test --all-targets

# Specific module
./scripts/cargo_headroom.sh test tools::tests
./scripts/cargo_headroom.sh test sse::tests

# Conformance tests
./scripts/cargo_headroom.sh test conformance
```

Focused validation tools:

```bash
# Dev-firstset gate before release build
rch exec -- cargo build --bin pi --bin ext_release_binary_e2e
PI_HTTP_REQUEST_TIMEOUT_SECS=0 rch exec -- \
  cargo run --example ext_release_binary_e2e -- \
  --pi-bin target/debug/pi \
  --provider ollama --model qwen2.5:0.5b \
  --jobs 10 --timeout-secs 600 --max-cases 20 --extension-policy balanced

# Full optimized release-binary run after gate passes
rch exec -- cargo build --release --bin pi --bin ext_release_binary_e2e
PI_HTTP_REQUEST_TIMEOUT_SECS=0 target/release/ext_release_binary_e2e \
  --pi-bin target/release/pi \
  --provider ollama --model qwen2.5:0.5b \
  --jobs 10 --timeout-secs 600 --extension-policy balanced

# Runtime risk ledger forensics (verify, replay, calibrate)
rch exec -- cargo run --example ext_runtime_risk_ledger -- verify --input path/to/runtime_risk_ledger.json
rch exec -- cargo run --example ext_runtime_risk_ledger -- replay --input path/to/runtime_risk_ledger.json
rch exec -- cargo run --example ext_runtime_risk_ledger -- calibrate --input path/to/runtime_risk_ledger.json --objective balanced_accuracy
```

- `ext_runtime_risk_ledger` operates on `pi.ext.runtime_risk_ledger.v1` artifacts (for example, from incident bundle exports).

### Release & Publishing

Releases are tag-driven and must align with `Cargo.toml` versions.

- Tag format: `vX.Y.Z` (pre-releases like `vX.Y.Z-rc.N` are allowed but skip crates.io publish).
- The tag version **must** match `package.version` in `Cargo.toml`.
- Publish order for dependencies: `asupersync` → `rich_rust` → `charmed-*` (lipgloss, bubbletea, bubbles, glamour) → `pi_agent_rust`.
- `.github/workflows/release.yml` owns the ordered release boundary: verified
  GitHub draft, exact stable crate publication/reconciliation on a fresh
  review-gated runner, then public GitHub release last. Pre-releases skip
  crates.io. `.github/workflows/publish.yml` is a manual dry-run diagnostic;
  it has no registry secret and never publishes.
- The no-Actions DSR lane is specified in [docs/releasing.md](docs/releasing.md).
  It is fail-closed. For v0.2.0, the audited preserved wrapper is authorized
  only to produce five raw binaries plus its aggregate build manifest; a
  separate source-bound deterministic stage creates the exact 12 public assets.
  Publication uses the checksum-gated Cargo credential provider materialized
  from the frozen release workflow; a plain `cargo:token` publish is forbidden.
  Immutable `v*` tag ruleset `20418963` was created and read back on 2026-08-04
  with update/deletion forbidden and no bypass actors. The manual lane
  re-verifies that live control before tagging and publication and stops on any
  drift; the automated lane remains disabled until its separate
  protected-environment governance is configured.

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

Selected core modules (non-exhaustive):

```
src/
├── main.rs                # CLI entry point
├── lib.rs                 # Library exports
├── app.rs                 # Startup/model selection helpers
├── agent.rs               # Agent loop + event orchestration
├── agent_cx.rs            # asupersync capability context wiring
├── cli.rs                 # Argument parsing
├── config.rs              # Configuration
├── auth.rs                # API key/OAuth/AWS credential storage
├── model.rs               # Message/content/stream event types
├── provider.rs            # Provider trait
├── provider_metadata.rs   # Canonical provider IDs + routing defaults
├── models.rs              # Model registry + models.json overrides
├── providers/
│   ├── anthropic.rs        # Anthropic Messages API
│   ├── openai.rs           # OpenAI Chat Completions
│   ├── openai_responses.rs # OpenAI Responses API
│   ├── gemini.rs           # Gemini API
│   ├── cohere.rs           # Cohere Chat API
│   ├── azure.rs            # Azure OpenAI
│   ├── bedrock.rs          # Amazon Bedrock Converse
│   ├── vertex.rs           # Google Vertex AI
│   ├── copilot.rs          # GitHub Copilot backend
│   ├── gitlab.rs           # GitLab Duo backend
│   └── mod.rs              # Provider factory + extension bridge
├── tools.rs                # Built-in tool implementations
├── sse.rs                  # Streaming SSE parser
├── http/
│   ├── client.rs           # asupersync-backed HTTP client
│   ├── sse.rs              # HTTP SSE helpers
│   └── mod.rs
├── session.rs              # JSONL session persistence/tree ops
├── session_index.rs        # SQLite session metadata index/cache
├── session_sqlite.rs       # Default-enabled sqlite-sessions backend support
├── compaction.rs           # Context compaction algorithm
├── interactive.rs          # Interactive TUI app loop/state
├── interactive/            # Bubble Tea-style TUI submodules
├── rpc.rs                  # RPC/stdio mode
├── extensions.rs           # Stable extension facade + manager/lifecycle
├── extensions/
│   ├── protocol.rs         # Wire validation + hostcall reactor internals
│   ├── fs_connector.rs     # Capability-scoped filesystem implementation
│   ├── exec_mediation.rs   # Dangerous-command + secret policy logic
│   ├── permission_drift.rs # Permission snapshot drift evidence
│   ├── event_coalescer_impl.rs # Coalesced event dispatch
│   ├── extension_manager_impl.rs # Manager lifecycle + event orchestration
│   ├── native_runtime_duplicate_scaffold.rs # Active native descriptor runtime
│   ├── wasm_host.rs        # Feature-gated Wasmtime component host
│   └── tests/              # Behavior-domain characterization suites
├── extensions_js.rs        # QuickJS runtime bridge + hostcalls
├── extension_dispatcher.rs # Hostcall/tool dispatch plumbing
├── extension_preflight.rs  # Extension compatibility scanner
├── extension_validation.rs # Extension validation pipeline glue
├── resources.rs            # Skills/prompt/theme/extension loading
└── tui.rs                  # Terminal UI rendering helpers
```

---

## Documentation Index

This index intentionally lists the load-bearing docs, not every generated
snapshot under `docs/`. Historical planning and review material lives in the
[planning archive](docs/planning/); see
[the docs classification record](docs/docs-classification-bd-we34i1.md) for the
broader inventory.

| Area | Primary docs |
|---|---|
| Getting started and operations | [development](docs/development.md), [terminal setup](docs/terminal-setup.md), [settings](docs/settings.md), [models](docs/models.md), [keybindings](docs/keybindings.md), [packages](docs/packages.md), [troubleshooting](docs/troubleshooting.md), [releasing](docs/releasing.md) |
| Swarm operations and replay | [operations runbook](docs/swarm-operations-runbook.md), [replay operator workflow](docs/swarm-replay-operator-workflow.md), [activity ledger](docs/swarm-activity-ledger.md), [flight recorder](docs/swarm-flight-recorder.md) |
| Core runtime surfaces | [session](docs/session.md), [tree](docs/tree.md), [TUI](docs/tui.md), [RPC](docs/rpc.md), [SDK](docs/sdk.md), [skills](docs/skills.md), [prompt templates](docs/prompt-templates.md), [streaming hostcalls](docs/streaming-hostcalls.md), [context intelligence](docs/context-intelligence.md) |
| Drop-in certification and migration | [certification contract](docs/contracts/dropin-certification-contract.json), [certification verdict](docs/evidence/dropin-certification-verdict.json), [parity gap ledger](docs/evidence/dropin-parity-gap-ledger.json), [differential evidence suite](docs/evidence/dropin-differential-evidence-suite.json), [feature inventory](docs/evidence/dropin-feature-inventory-matrix.json), [migration playbook](docs/integrator-migration-playbook.md), [parity snapshot](docs/parity-certification.json), [program governance](docs/program-governance.md) |
| Extensions | [architecture](docs/extension-architecture.md), [compatibility guide](docs/ext-compat.md), [compatibility matrix](docs/extension-compatibility-matrix.md), [conformance plan](docs/extension-conformance-test-plan.json), [runtime threat model](docs/extension-runtime-threat-model.md), [troubleshooting](docs/extension-troubleshooting.md), [registry](docs/extension-registry.md), [WIT ABI](docs/wit/extension.wit) |
| Providers | [provider guide](docs/providers.md), [auth troubleshooting checked crosswalk](docs/provider-auth-troubleshooting.md), [config examples](docs/provider-config-examples.md), [canonical ID policy](docs/provider-canonical-id-policy.md), [onboarding playbook](docs/provider-onboarding-playbook.md), [test obligations](docs/provider-test-obligations.md), [upstream catalog snapshot](docs/provider-upstream-catalog-snapshot.md), [historical 2026-02-13 support baseline](docs/provider-support-baseline-audit.md) |
| QA and evidence | [QA runbook](docs/qa-runbook.md), [testing policy](docs/testing-policy.md), [conformance playbook](docs/conformance-operator-playbook.md), [coverage matrix](docs/TEST_COVERAGE_MATRIX.md), [evidence schema](docs/evidence-contract-schema.json), [coverage baseline map](docs/coverage-baseline-map.json), [E2E scenario matrix](docs/e2e_scenario_matrix.json), [non-mock rubric](docs/non-mock-rubric.json) |
| Security | [baseline audit](docs/security/baseline-audit.md), [threat model](docs/security/threat-model.md), [security invariants](docs/security/invariants.md), [operator handbook](docs/security/operator-handbook.md), [operator quick reference](docs/security/operator-quick-reference.md), [incident response](docs/security/incident-response-runbook.md), [runtime hostcall telemetry](docs/security/runtime-hostcall-telemetry.md), [security SLOs](docs/security/security-slos.md) |
| Schemas and machine contracts | [extension manifest schema](docs/schema/extension_manifest.json), [extension protocol schema](docs/schema/extension_protocol.json), [session store v2 contract](docs/schema/session_store_v2_contract.json), [semantic workspace graph contract](docs/contracts/semantic-workspace-graph-contract.json), [semantic context graph contract](docs/contracts/semantic-context-graph-contract.json), [swarm replay trace contract](docs/contracts/swarm-replay-trace-contract.json), [swarm replay preview schema](docs/schema/swarm_replay_preview.json), [remote validation proof ledger contract](docs/contracts/remote-validation-proof-ledger-contract.json), [remote validation proof reuse gate contract](docs/contracts/remote-validation-proof-reuse-gate-contract.json), [validation proof-memory index contract](docs/contracts/validation-proof-memory-index-contract.json), [validation proof-memory index evidence](docs/evidence/validation-proof-memory-index.json), [operator work recommendation contract](docs/contracts/operator-work-recommendation-contract.json), [operator work recommendation evidence](docs/evidence/operator-work-recommendation.json), [operator smoothness SLO contract](docs/contracts/operator-smoothness-slo-contract.json), [operator smoothness SLO evidence](docs/evidence/operator-smoothness-slo.json), [extension resource firewall matrix contract](docs/contracts/extension-resource-firewall-matrix-contract.json), [validation broker contract](docs/contracts/validation-broker-contract.json), [validation broker closeout gate contract](docs/contracts/validation-broker-closeout-gate-contract.json), [validation broker closeout gate evidence](docs/evidence/validation-broker-closeout-gate.json), [context-intelligence closeout gate contract](docs/contracts/context-intelligence-closeout-gate-contract.json), [context-intelligence closeout gate evidence](docs/evidence/context-intelligence-closeout-gate.json), [progress SLO closeout gate contract](docs/contracts/swarm-progress-slo-closeout-gate-contract.json), [progress SLO closeout gate evidence](docs/evidence/swarm-progress-slo-closeout-gate.json), [runtime intelligence closeout gate contract](docs/contracts/runtime-intelligence-closeout-gate-contract.json), [runtime intelligence closeout gate evidence](docs/evidence/runtime-intelligence-closeout-gate.json), [sixth-wave validation hardening closeout gate contract](docs/contracts/sixth-wave-validation-hardening-closeout-gate-contract.json), [sixth-wave validation hardening closeout gate evidence](docs/evidence/sixth-wave-validation-hardening-closeout-gate.json), [seventh-wave runtime autonomy closeout gate contract](docs/contracts/seventh-wave-runtime-autonomy-closeout-gate-contract.json), [seventh-wave runtime autonomy closeout gate evidence](docs/evidence/seventh-wave-runtime-autonomy-closeout-gate.json), [predictive swarm telemetry ledger contract](docs/contracts/predictive-swarm-telemetry-ledger-contract.json), [predictive swarm telemetry ledger evidence](docs/evidence/predictive-swarm-telemetry-ledger.json), [test evidence logging contract](docs/schema/test_evidence_logging_contract.json), [runtime hostcall telemetry schema](docs/schema/runtime_hostcall_telemetry.json), [mock spec schema](docs/schema/mock_spec.json), [CLI surface diff schema](docs/schema/cli-surface-diff.json), [security traceability matrix](docs/sec_traceability_matrix.md) |

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---

## License

MIT License (with OpenAI/Anthropic Rider). See [LICENSE](LICENSE) for details.

---

<p align="center">
  <sub>Built with Rust, for developers who live in the terminal.</sub>
</p>
