# AGENTS.md — pi_agent_rust (Pi CLI Coding Agent)

> Guidelines for AI coding agents working in this Rust codebase.

---

## RULE 0 - THE FUNDAMENTAL OVERRIDE PREROGATIVE

If I tell you to do something, even if it goes against what follows below, YOU MUST LISTEN TO ME. I AM IN CHARGE, NOT YOU.

---

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created, such as a test code file. You have a horrible track record of deleting critically important files or otherwise throwing away tons of expensive work. As a result, you have permanently lost any and all rights to determine that a file or folder should be deleted.

**YOU MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.**

---

## Irreversible Git & Filesystem Actions — DO NOT EVER BREAK GLASS

1. **Absolutely forbidden commands:** `git reset --hard`, `git clean -fd`, `rm -rf`, or any command that can delete or overwrite code/data must never be run unless the user explicitly provides the exact command and states, in the same message, that they understand and want the irreversible consequences.
2. **No guessing:** If there is any uncertainty about what a command might delete or overwrite, stop immediately and ask the user for specific approval. "I think it's safe" is never acceptable.
3. **Safer alternatives first:** When cleanup or rollbacks are needed, request permission to use non-destructive options (`git status`, `git diff`, `git stash`, copying to backups) before ever considering a destructive command.
4. **Mandatory explicit plan:** Even after explicit user authorization, restate the command verbatim, list exactly what will be affected, and wait for a confirmation that your understanding is correct. Only then may you execute it—if anything remains ambiguous, refuse and escalate.
5. **Document the confirmation:** When running any approved destructive command, record (in the session notes / final response) the exact user text that authorized it, the command actually run, and the execution time. If that record is absent, the operation did not happen.

---

## Git Branch: ONLY Use `main`, NEVER `master`

**The default branch is `main`. The `master` branch exists only for legacy URL compatibility.**

- **All work happens on `main`** — commits, PRs, feature branches all merge to `main`
- **Never reference `master` in code or docs** — if you see `master` anywhere, it's a bug that needs fixing
- **The `master` branch must stay synchronized with `main`** — after pushing to `main`, also push to `master`:
  ```bash
  git push origin main:master
  ```

---

## Toolchain: Rust & Cargo

We only use **Cargo** in this project, NEVER any other package manager.

- **Edition:** Rust 2024 (nightly required — see `rust-toolchain.toml`)
- **Dependency versions:** Explicit versions for stability
- **Configuration:** Cargo.toml only
- **Unsafe code:** Forbidden (`#![forbid(unsafe_code)]`)

### Key Dependencies

| Crate | Purpose |
|-------|---------|
| `asupersync` | Structured concurrency async runtime |
| `rich_rust` | Terminal UI rendering with markup syntax |
| `serde` + `serde_json` | JSON serialization for API/session formats |
| `clap` | CLI argument parsing with derive macros |
| `crossterm` | Low-level terminal control |
| `thiserror` | Error type definitions |

### Release Profile

The default release build optimizes for shipping size while retaining LTO:

```toml
[profile.release]
opt-level = "z"     # Optimize generated code for size
lto = true          # Link-time optimization
codegen-units = 1   # Single codegen unit for better optimization
panic = "abort"     # Smaller binary, no unwinding overhead
strip = true        # Remove debug symbols
```

jemalloc is opt-in via `--features jemalloc` for allocation-heavy benchmark variants.

---

## Code Editing Discipline

### No Script-Based Changes

**NEVER** run a script that processes/changes code files in this repo. Brittle regex-based transformations create far more problems than they solve.

- **Always make code changes manually**, even when there are many instances
- For many simple changes: use parallel subagents
- For subtle/complex changes: do them methodically yourself

### No File Proliferation

If you want to change something or add a feature, **revise existing code files in place**.

**NEVER** create variations like:
- `mainV2.rs`
- `main_improved.rs`
- `main_enhanced.rs`

New files are reserved for **genuinely new functionality** that makes zero sense to include in any existing file. The bar for creating new files is **incredibly high**.

---

## Backwards Compatibility

We do not care about backwards compatibility—we're in early development with no users. We want to do things the **RIGHT** way with **NO TECH DEBT**.

- Never create "compatibility shims"
- Never create wrapper functions for deprecated APIs
- Just fix the code directly

---

## Drop-In Claim Messaging Guardrail

When editing docs, release notes, or user-facing copy:

- Do not describe Pi Rust as a strict drop-in replacement unless `docs/contracts/dropin-certification-contract.json` hard gates are satisfied.
- Treat `docs/evidence/dropin-certification-verdict.json` as the release claim gate: strict replacement language requires `overall_verdict = CERTIFIED`.
- Treat `docs/parity-certification.json` as informational progress evidence only; it does not override release-gate policy.

---

## Compiler Checks (CRITICAL)

**After any substantive code changes, you MUST verify no errors were introduced:**

```bash
# Check for compiler errors and warnings
cargo check --all-targets

# Check for clippy lints (pedantic + nursery are enabled)
cargo clippy --all-targets -- -D warnings

# Verify formatting
cargo fmt --check
```

For heavyweight local runs (especially `--all-targets`) in multi-agent environments, set both build artifacts and test temp files to a high-capacity tmpfs to avoid `No space left on device` failures:

```bash
export CARGO_TARGET_DIR="/data/tmp/pi_agent_rust_cargo/${USER:-agent}/target"
export TMPDIR="/data/tmp/pi_agent_rust_cargo/${USER:-agent}/tmp"
mkdir -p "$CARGO_TARGET_DIR" "$TMPDIR"
```

Use an agent-specific suffix (for example `/data/tmp/pi_agent_rust_cargo/topazfalcon`) to avoid collisions across concurrent agents.

If you see errors, **carefully understand and resolve each issue**. Read sufficient context to fix them the RIGHT way.

---

## Testing

### Unit Tests

The test suite covers all core functionality:

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run specific test module
cargo test sse::tests
cargo test tools::tests
cargo test conformance
```

### Test Categories

| Area | Coverage | Purpose |
|------|----------|---------|
| `model` | Serialization + conversion tests | Message/content type correctness |
| `provider_streaming` | Multi-provider fixtures | Streaming + tool-call parity across providers |
| `tools` | Built-in + conformance fixtures | File/shell/search behavior and truncation/process safety |
| `session` | JSONL/tree/index/sqlite tests | Persistence, branching, and replay correctness |
| `extensions` | Runtime/policy/shim/conformance suites | QuickJS extension compatibility and capability controls |
| `e2e` | End-to-end scenario tests | CLI/agent/rpc workflows and regression coverage |

---

## Pi Agent — This Project

**This is the project you're working on.** Pi is a high-performance AI coding agent CLI, a Rust port of the Pi Agent TypeScript CLI. It provides an interactive terminal interface for AI-assisted coding with streaming responses, tool execution, and session persistence.

### Architecture

```
CLI (clap) → main/app/config/resources → Agent Session
                         ↓
Provider Layer (11 native provider implementation modules + extension providers)
                         ↓
Tool Registry (built-ins + extension tools) ↔ Extension Runtime (QuickJS + capability policy)
                         ↓
Surfaces: Interactive TUI + RPC/stdin modes
                         ↓
Session persistence + index (JSONL, default-enabled SQLite backend support)
```

### Key Files

| File | Purpose |
|------|---------|
| `src/main.rs` | CLI entry point and subcommands |
| `src/agent.rs` | Agent loop with tool iteration |
| `src/provider.rs` | Provider trait definition |
| `src/providers/anthropic.rs` | Anthropic API implementation |
| `src/providers/openai.rs` | OpenAI API implementation |
| `src/providers/openai_responses.rs` | OpenAI Responses API implementation |
| `src/providers/gemini.rs` | Gemini API implementation |
| `src/providers/cohere.rs` | Cohere API implementation |
| `src/providers/azure.rs` | Azure OpenAI API implementation |
| `src/providers/mod.rs` | Provider factory and extension stream-simple bridge |
| `src/tools.rs` | 9 built-in tools (`subagent` is opt-in) |
| `src/interactive.rs` | Interactive TUI application state and event loop |
| `src/rpc.rs` | RPC/stdin server mode |
| `src/extensions.rs` | Stable extension facade, public contracts, manager state, and shared entry points |
| `src/extensions/` | Focused manager, protocol, policy, connector, runtime, and behavior-domain test modules |
| `src/extensions_js.rs` | QuickJS runtime bridge and hostcalls |
| `src/resources.rs` | Skills/prompt/theme/extension resource loading |
| `src/models.rs` | Built-in and `models.json` registry resolution |
| `src/model.rs` | Message/content types |
| `src/session.rs` | JSONL session persistence |
| `src/session_index.rs` | Session indexing and metadata cache |
| `src/sse.rs` | SSE parser for streaming |
| `src/tui.rs` | Terminal UI rendering helpers |
| `src/config.rs` | Configuration loading |
| `src/error.rs` | Error types |

### Core Components

**Provider Layer:**
- Abstract `Provider` trait for LLM backends
- Anthropic implementation with streaming + extended thinking
- OpenAI Chat Completions + OpenAI Responses implementations
- Gemini implementation with streaming + tool use
- Cohere implementation with streaming + tool use
- Azure OpenAI implementation (requires resource/deployment config)
- Extension-provided providers via stream-simple bridge
- Tool definitions with JSON Schema

**Built-in Tools:**
- `read` - Read files with line numbers, image support
- `write` - Create/overwrite files
- `edit` - String replacement editing
- `bash` - Shell command execution with timeout
- `grep` - Content search with context
- `find` - File discovery with glob patterns
- `ls` - Directory listing
- `hashline_edit` - Precise edits using `LINE#HASH` tags from `read`/`grep` with `hashline=true`
- `subagent` - Native isolated Rust Pi child-agent delegation (opt-in via `--tools ...subagent`)

**Session Management:**
- JSONL format (version 3)
- Tree structure for branching
- Entry types: Message, ModelChange, ThinkingLevel, Compaction, etc.
- Per-project session directories + session index metadata
- SQLite session backend support is enabled by default via `sqlite-sessions`; opt out with `--no-default-features` and re-enable only the features you need

### Migration Strategy

This port uses two key libraries from sibling projects:

1. **asupersync** (`../asupersync`) - Structured concurrency runtime
   - Async runtime (structured concurrency)
   - Provides HTTP client, TLS, SQLite
   - Enables deterministic testing with LabRuntime
   - Explicit capability context (Cx)

2. **rich_rust** (`../rich_rust`) - Terminal UI library
   - Console with markup syntax `[bold red]text[/]`
   - Tables, Panels, Progress bars
   - Markdown rendering
   - Theme support

**Current Status:**
- asupersync powers runtime + HTTP/TLS + cancellation + optional SQLite integration
- rich_rust/charmed_rust stack powers the interactive terminal UI
- Provider layer has 11 native provider implementation modules in `src/providers/`: Anthropic, OpenAI Chat, OpenAI Responses/Codex Responses, Gemini, Cohere, Azure OpenAI, Bedrock, Vertex AI, GitHub Copilot, GitLab Duo, and Cursor
- Extension runtime, capability policy, and conformance harness are integrated

### Performance Targets

| Metric | Target | Notes |
|--------|--------|-------|
| Startup time | <100ms | No heavy initialization |
| Binary size (release) | <22 MiB | CI size budget with LTO + strip enabled |
| TUI framerate | 60fps | Differential rendering |
| Memory (idle) | <50MB | No leaks on long sessions |

---

## Conformance Testing

The port uses fixture-based conformance tests to validate behavior matches expectations.

### Fixture Structure

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

### Running Conformance Tests

```bash
# All conformance tests
cargo test conformance

# Specific tool
cargo test conformance::test_read
cargo test conformance::test_bash
```

---

## Third-Party Library Usage

If you aren't 100% sure how to use a third-party library, **SEARCH ONLINE** to find the latest documentation and current best practices.

---

## MCP Agent Mail — Multi-Agent Coordination

A mail-like layer that lets coding agents coordinate asynchronously via MCP tools and resources. Provides identities, inbox/outbox, searchable threads, and advisory file reservations with human-auditable artifacts in Git.

### Why It's Useful

- **Prevents conflicts:** Explicit file reservations (leases) for files/globs
- **Token-efficient:** Messages stored in per-project archive, not in context
- **Quick reads:** `resource://inbox/...`, `resource://thread/...`

### Same Repository Workflow

1. **Register identity:**
   ```
   ensure_project(project_key=<abs-path>)
   register_agent(project_key, program, model)
   ```

2. **Reserve files before editing:**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)
   ```

3. **Communicate with threads:**
   ```
   send_message(..., thread_id="FEAT-123")
   fetch_inbox(project_key, agent_name)
   acknowledge_message(project_key, agent_name, message_id)
   ```

4. **Quick reads:**
   ```
   resource://inbox/{Agent}?project=<abs-path>&limit=20
   resource://thread/{id}?project=<abs-path>&include_bodies=true
   ```

### Macros vs Granular Tools

- **Prefer macros for speed:** `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`
- **Use granular tools for control:** `register_agent`, `file_reservation_paths`, `send_message`, `fetch_inbox`, `acknowledge_message`

### Common Pitfalls

- `"from_agent not registered"`: Always `register_agent` in the correct `project_key` first
- `"FILE_RESERVATION_CONFLICT"`: Adjust patterns, wait for expiry, or use non-exclusive reservation
- **Auth errors:** If JWT+JWKS enabled, include bearer token with matching `kid`

---

## Beads (br) — Dependency-Aware Issue Tracking

Beads provides a lightweight, dependency-aware issue database and CLI (`br` - beads_rust) for selecting "ready work," setting priorities, and tracking status. It complements MCP Agent Mail's messaging and file reservations.

**Important:** `br` is non-invasive—it NEVER runs git commands automatically. You must manually commit changes after `br sync --flush-only`.

### Conventions

- **Single source of truth:** Beads for task status/priority/dependencies; Agent Mail for conversation and audit
- **Shared identifiers:** Use Beads issue ID (e.g., `br-123`) as Mail `thread_id` and prefix subjects with `[br-123]`
- **Reservations:** When starting a task, call `file_reservation_paths()` with the issue ID in `reason`

### Typical Agent Flow

1. **Pick ready work (Beads):**
   ```bash
   br ready --json  # Choose highest priority, no blockers
   ```

2. **Reserve edit surface (Mail):**
   ```
   file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true, reason="br-123")
   ```

3. **Announce start (Mail):**
   ```
   send_message(..., thread_id="br-123", subject="[br-123] Start: <title>", ack_required=true)
   ```

4. **Work and update:** Reply in-thread with progress

5. **Complete and release:**
   ```bash
   br close 123 --reason "Completed"
   br sync --flush-only  # Export to JSONL (no git operations)
   ```
   ```
   release_file_reservations(project_key, agent_name, paths=["src/**"])
   ```
   Final Mail reply: `[br-123] Completed` with summary

### Mapping Cheat Sheet

| Concept | Value |
|---------|-------|
| Mail `thread_id` | `br-###` |
| Mail subject | `[br-###] ...` |
| File reservation `reason` | `br-###` |
| Commit messages | Include `br-###` for traceability |

---

## bv — Graph-Aware Triage Engine

bv is a graph-aware triage engine for Beads projects (`.beads/beads.jsonl`). It computes PageRank, betweenness, critical path, cycles, HITS, eigenvector, and k-core metrics deterministically.

**Scope boundary:** bv handles *what to work on* (triage, priority, planning). For agent-to-agent coordination (messaging, work claiming, file reservations), use MCP Agent Mail.

**CRITICAL: Use non-interactive flags (`--robot-*`, `--recipe`, `--as-of`, `--diff-since`, `--export-md`) only. Bare `bv` launches an interactive TUI that blocks your session.**

### The Workflow: Start With Triage

Use this order of operations:

```bash
bv --robot-plan          # Primary triage surface (tracks + highest-impact summary)
bv --robot-priority      # Priority sanity check and suggested re-ranking
bv --robot-insights      # Deep graph metrics when needed
br ready --json          # Ground-truth actionable issues from Beads
```

If your local `bv` build supports `--robot-triage`, you can still use it. If not, `--robot-plan` + `br ready --json` is the required fallback.

**CRITICAL Tombstone Guard:** `bv` output can include `status = tombstone` items in some versions. Tombstones are deleted/merged issues and are **never actionable**.

Before claiming work from `bv`, always verify status with `br`:

```bash
br show <issue-id> --json | jq -r '.[0].status'
# Only proceed if status is open/in_progress and the issue is not deleted/tombstoned.
```

If `br ready --json` is empty and `bv` only surfaces tombstones, do not claim tombstoned items. Create or refine a real bead and proceed.

### Command Reference

**Planning:**
| Command | Returns |
|---------|---------|
| `--robot-plan` | Parallel execution tracks with `unblocks` lists |
| `--robot-priority` | Priority misalignment detection with confidence |
| `--robot-recipes` | Available recipe filters for scoped triage |

**Graph Analysis:**
| Command | Returns |
|---------|---------|
| `--robot-insights` | Full metrics: PageRank, betweenness, HITS, eigenvector, critical path, cycles, k-core, articulation points, slack |

**History & Change Tracking:**
| Command | Returns |
|---------|---------|
| `--robot-diff --diff-since <ref>` | Changes since ref: new/closed/modified issues, cycles |

**Other:**
| Command | Returns |
|---------|---------|
| `--recipe <name>` | Apply recipe filters (for example `actionable`, `high-impact`) |
| `--export-md <file.md>` | Markdown status/export report |

### Scoping & Filtering

```bash
bv --robot-plan --as-of HEAD~30              # Historical point-in-time
bv --recipe actionable --robot-plan          # Pre-filter: ready to work
bv --recipe high-impact --robot-plan         # Pre-filter: top-impact set
bv --robot-priority                          # Cross-check priority drift
bv --robot-recipes                           # Discover installed recipe names
```

### Understanding Robot Output

**All robot JSON includes:**
- `data_hash` — Fingerprint of source beads.jsonl
- `status` — Per-metric state: `computed|approx|timeout|skipped` + elapsed ms
- `as_of` / `as_of_commit` — Present when using `--as-of`

**Two-phase analysis:**
- **Phase 1 (instant):** degree, topo sort, density
- **Phase 2 (async, 500ms timeout):** PageRank, betweenness, HITS, eigenvector, cycles

### jq Quick Reference

```bash
bv --robot-plan | jq '.plan.summary.highest_impact'        # Best unblock target
bv --robot-plan | jq '.plan.tracks[0].items[0]'            # First candidate in first track
bv --robot-priority | jq '.recommendations[0]'             # Top priority recommendation
bv --robot-insights | jq '.status'                         # Check metric readiness
bv --robot-insights | jq '.Cycles'                         # Circular deps (must fix!)
```

---

## UBS — Ultimate Bug Scanner

**Golden Rule:** `ubs --staged --only=rust .` before every commit. Exit 0 = safe. Exit >0 = fix & re-run.

### Commands

```bash
ubs --staged --only=rust .              # Staged Rust changes — USE THIS
python3 scripts/check_ubs_staged_delta.py # Changed-line gate when staged UBS is noisy
ubs --diff --only=rust .                # Unstaged+staged Rust changes vs HEAD
ubs --only=rust,toml .                  # Language-filtered project scan
ubs --ci --fail-on-warning .            # CI mode — before PR
timeout 60s ubs --staged --only=rust .  # Bounded pre-commit run if UBS stalls
ubs .                                   # Whole project (ignores target/, Cargo.lock)
```

### Output Format

```
⚠️  Category (N errors)
    file.rs:42:5 – Issue description
    💡 Suggested fix
Exit code: 1
```

Parse: `file:line:col` → location | 💡 → how to fix | Exit 0/1 → pass/fail

### Noisy Staged-File Baseline

UBS scans whole staged Rust files, so large modules can report old findings on
unchanged lines. If raw `ubs --staged --only=rust .` is non-actionable because
it is dominated by unchanged-file baseline noise, run:

```bash
python3 scripts/check_ubs_staged_delta.py
```

That gate still runs UBS on the staged Rust files, then fails only when warning
or critical findings land on lines added or modified in the staged diff. If it
passes, treat remaining whole-file findings as baseline inventory and file or
link owner beads for that inventory instead of blocking the current patch on
unrelated legacy findings.

### Fix Workflow

1. Read finding → category + fix suggestion
2. Navigate `file:line:col` → view context
3. Verify real issue (not false positive)
4. Fix root cause (not symptom)
5. Re-stage and re-run `ubs --staged --only=rust .`; if it is noisy from
   unchanged baseline findings, run `python3 scripts/check_ubs_staged_delta.py`
   and fix any changed-line failures
6. Commit

### Bug Severity

- **Critical (always fix):** Memory safety, use-after-free, data races, SQL injection
- **Important (production):** Unwrap panics, resource leaks, overflow checks
- **Contextual (judgment):** TODO/FIXME, println! debugging

---

## Beads Ledger Reconciliation — Invariant

**CRITICAL INVARIANT:** The beads ledger reconciliation script MUST pass before any commit. This prevents "completion illusion" where all beads appear closed but critical gaps remain untracked.

```bash
./scripts/reconcile_beads_ledger.sh
```

**Exit 0 = safe to commit.** **Exit 1 = orphan gaps found.**

### What It Checks

The script cross-references:
- **Active beads** (from `br list --json`, statuses `open` and `in_progress`)
- **Open critical/high gaps** (from `docs/evidence/dropin-parity-gap-ledger.json`)
- **Gap-tracking external refs** (`external_ref=<gap-id>`)

If any critical or high-severity gap lacks a corresponding active owner bead or active bead with `external_ref=<gap-id>`, the script fails and lists the orphan gap. If any active bead references a `gap-*` external ref that is not an active critical/high ledger gap, the script also fails so stale tracker work cannot outlive the ledger truth.

### Fix Workflow

1. **Run the script:** `./scripts/reconcile_beads_ledger.sh`
2. **If it passes:** Proceed with commit
3. **If it fails:** Create beads for each orphan gap:
   ```bash
   br create --title="Address gap-<id>" --type=task --priority=1
   ```
4. **Re-run until it passes:** The script must exit 0 before commit

### CI Integration

This check runs automatically in CI as the "Beads ledger reconciliation check" step. It will fail the build if orphan ledger gaps or stale active gap-tracking beads are detected.

**Why This Matters:** Without this invariant, teams can falsely believe all work is complete when critical gaps remain untracked, leading to incomplete drop-in certification or missed functionality gaps.

---

## RCH — Remote Compilation Helper

RCH offloads `cargo build`, `cargo test`, `cargo clippy`, and other compilation commands to a fleet of 8 remote Contabo VPS workers instead of building locally. This prevents compilation storms from overwhelming csd when many agents run simultaneously.

**RCH is installed at `~/.local/bin/rch` and is hooked into Claude Code's PreToolUse automatically.** Most of the time you don't need to do anything if you are Claude Code — builds are intercepted and offloaded transparently.

To manually offload a build:
```bash
rch exec -- cargo build --release
rch exec -- cargo test
rch exec -- cargo clippy
```

Quick commands:
```bash
rch doctor                    # Health check
rch workers probe --all       # Test connectivity to all 8 workers
rch status                    # Overview of current state
rch queue                     # See active/waiting builds
```

If rch or its workers are unavailable, it fails open — builds run locally as normal.

**Note for Codex/GPT-5.2:** Codex does not have the automatic PreToolUse hook, but you can (and should) still manually offload compute-intensive compilation commands using `rch exec -- <command>`. This avoids local resource contention when multiple agents are building simultaneously.

---

## ast-grep vs ripgrep

**Use `ast-grep` when structure matters.** It parses code and matches AST nodes, ignoring comments/strings, and can **safely rewrite** code.

- Refactors/codemods: rename APIs, change import forms
- Policy checks: enforce patterns across a repo
- Editor/automation: LSP mode, `--json` output

**Use `ripgrep` when text is enough.** Fastest way to grep literals/regex.

- Recon: find strings, TODOs, log lines, config values
- Pre-filter: narrow candidate files before ast-grep

### Rule of Thumb

- Need correctness or **applying changes** → `ast-grep`
- Need raw speed or **hunting text** → `rg`
- Often combine: `rg` to shortlist files, then `ast-grep` to match/modify

### Rust Examples

```bash
# Find structured code (ignores comments)
ast-grep run -l Rust -p 'fn $NAME($$$ARGS) -> $RET { $$$BODY }'

# Find all unwrap() calls
ast-grep run -l Rust -p '$EXPR.unwrap()'

# Quick textual hunt
rg -n 'println!' -t rust

# Combine speed + precision
rg -l -t rust 'unwrap\(' | xargs ast-grep run -l Rust -p '$X.unwrap()' --json
```

---

## Morph Warp Grep — AI-Powered Code Search

**Use `mcp__morph-mcp__warp_grep` for exploratory "how does X work?" questions.** An AI agent expands your query, greps the codebase, reads relevant files, and returns precise line ranges with full context.

**Use `ripgrep` for targeted searches.** When you know exactly what you're looking for.

**Use `ast-grep` for structural patterns.** When you need AST precision for matching/rewriting.

### When to Use What

| Scenario | Tool | Why |
|----------|------|-----|
| "How is streaming implemented?" | `warp_grep` | Exploratory; don't know where to start |
| "Where is the Anthropic provider?" | `warp_grep` | Need to understand architecture |
| "Find all uses of `serde_json::from_str`" | `ripgrep` | Targeted literal search |
| "Find files with `println!`" | `ripgrep` | Simple pattern |
| "Replace all `unwrap()` with `expect()`" | `ast-grep` | Structural refactor |

### warp_grep Usage

```
mcp__morph-mcp__warp_grep(
  repoPath: "/path/to/pi_agent_rust",
  query: "How does the SSE parser handle streaming events?"
)
```

Returns structured results with file paths, line ranges, and extracted code snippets.

### Anti-Patterns

- **Don't** use `warp_grep` to find a specific function name → use `ripgrep`
- **Don't** use `ripgrep` to understand "how does X work" → wastes time with manual reads
- **Don't** use `ripgrep` for codemods → risks collateral edits

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

**Important:** `br` is non-invasive—it NEVER executes git commands. After `br sync --flush-only`, you must manually run `git add .beads/ && git commit`.

### Essential Commands

```bash
# View issues (launches TUI - avoid in automated sessions)
bv

# CLI commands for agents (use these instead)
br ready              # Show issues ready to work (no blockers)
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br create --title="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason "Completed"
br close <id1> <id2>  # Close multiple issues at once
br sync --flush-only  # Export to JSONL (NO git operations)
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Run `br sync --flush-only` then manually commit

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers, not words)
- **Types**: task, bug, feature, epic, question, docs
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads to JSONL
git add .beads/         # Stage beads changes
git commit -m "..."     # Commit everything together
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always `br sync --flush-only && git add .beads/` before ending session

---

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   br sync --flush-only    # Export beads to JSONL (no git ops)
   git add .beads/         # Stage beads changes
   git add <other files>   # Stage code changes
   git commit -m "..."     # Commit everything
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds


---

## cass — Cross-Agent Session Search

`cass` indexes prior agent conversations (Claude Code, Codex, Cursor, Gemini, ChatGPT, etc.) so we can reuse solved problems.

**Rules:** Never run bare `cass` (TUI). Always use `--robot` or `--json`.

### Examples

```bash
cass health
cass search "async runtime" --robot --limit 5
cass view /path/to/session.jsonl -n 42 --json
cass expand /path/to/session.jsonl -n 42 -C 3 --json
cass capabilities --json
cass robot-docs guide
```

### Tips

- Use `--fields minimal` for lean output
- Filter by agent with `--agent`
- Use `--days N` to limit to recent history

stdout is data-only, stderr is diagnostics; exit code 0 means success.

Treat cass as a way to avoid re-solving problems other agents already handled.

---

## Note for Codex/GPT-5.2

You constantly bother me and stop working with concerned questions that look similar to this:

```
Unexpected changes (need guidance)

- Working tree still shows edits I did not make in Cargo.toml, Cargo.lock, src/cli/commands/upgrade.rs, src/storage/sqlite.rs, tests/conformance.rs, tests/storage_deps.rs. Please advise whether to keep/commit/revert these before any further work. I did not touch them.

Next steps (pick one)

1. Decide how to handle the unrelated modified files above so we can resume cleanly.
2. Triage beads_rust-orko (clippy/cargo warnings) and beads_rust-ydqr (rustfmt failures).
3. If you want a full suite run later, fix conformance/clippy blockers and re‑run cargo test --all.
```

NEVER EVER DO THAT AGAIN. The answer is literally ALWAYS the same: those are changes created by the potentially dozen of other agents working on the project at the same time. This is not only a common occurence, it happens multiple times PER MINUTE. The way to deal with it is simple: you NEVER, under ANY CIRCUMSTANCE, stash, revert, overwrite, or otherwise disturb in ANY way the work of other agents. Just treat those changes identically to changes that you yourself made. Just fool yourself into thinking YOU made the changes and simply don't recall it for some reason.

---

## Note on Built-in TODO Functionality

Also, if I ask you to explicitly use your built-in TODO functionality, don't complain about this and say you need to use beads. You can use built-in TODOs if I tell you specifically to do so. Always comply with such orders.
