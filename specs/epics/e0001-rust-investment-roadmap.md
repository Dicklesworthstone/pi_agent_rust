# Epic e0001: Rust Port Investment Roadmap

> Strategic areas where investment in the Rust codebase yields the greatest
> advantage over the TypeScript original.
>
> Date: 2026-04-11 | Based on: [Feature Parity Research](../research/typescript-vs-rust-feature-parity.md)

---

## Thesis

The Rust port already exceeds the TypeScript original in provider coverage,
extension infrastructure, session durability, and test depth. The highest-ROI
investments are in areas where Rust's strengths (single binary, no runtime,
native performance, memory safety) create advantages that TypeScript
structurally cannot match.

---

## 1. Fix the 12 Failing Tests (Priority: Immediate)

Before any feature work, stabilize the existing test suite.

**Current failures (12):**

| Category | Count | Tests |
|----------|-------|-------|
| Branch-aware model state | 4 | `select_model_and_thinking_*`, `branch_without_overrides_*`, `navigation_clears_stale_*` |
| Model registry | 2 | `built_in_models_preserve_legacy_*`, `preserves_alias_equivalent_*` |
| Spill file handling | 2 | `rpc_spill_file_hard_limit_*`, `bash_hard_limit_*` |
| Autocomplete | 1 | `suggest_slash_alone_returns_all_builtins` |
| Extension dispatcher | 1 | `dispatcher_tool_find_discovers_files` |

**Why first:** These failures mask regressions. The branch-aware model state
tests (4 failures) suggest the recent `eaa865c0..69ac3dcd` upstream commits
introduced issues that need resolution before building on top.

**Effort:** ~1-2 days

---

## 2. Module Decomposition: Break Up God Files (Priority: High)

Three files dominate the codebase and are maintenance hazards:

| File | Lines | Concern |
|------|-------|---------|
| `src/extensions.rs` | 50,453 | Extension runtime, policy, sandbox |
| `src/extensions_js.rs` | 25,360 | JS/QuickJS extension bridge |
| `src/session.rs` | 11,386 | Session JSONL, branching, HTML export |
| `src/tools.rs` | 9,205 | All 8 tool implementations |

**Recommendation:**
- Split `extensions.rs` into `ext_runtime.rs`, `ext_policy.rs`, `ext_sandbox.rs`, `ext_hostcall.rs`
- Split `tools.rs` into per-tool modules: `tools/bash.rs`, `tools/read.rs`, `tools/edit.rs`, etc. (mirrors the TypeScript structure)
- Extract `session.rs` HTML export into `session_export.rs`

**Why:** These files are too large for effective code review, parallel
development, and incremental compilation. The TypeScript version's modular
tool structure (`core/tools/*.ts`) is actually better organized here.

**Effort:** ~3-5 days (mechanical refactor, low risk)

---

## 3. Single-Binary Distribution Advantage (Priority: High)

This is the Rust port's killer differentiator. The TypeScript agent requires
Node.js/Bun runtime + npm install. Invest in making the single binary
experience flawless:

**Areas to invest:**
- **Cross-compilation CI pipeline** — build for linux-x64, linux-arm64, darwin-x64, darwin-arm64, windows-x64
- **Static linking** — minimize dynamic library dependencies (musl on Linux)
- **Binary size optimization** — current target is <20MB per PLAN_TO_PORT_PI_TO_RUST.md; profile and strip
- **Self-update mechanism** — `version_check.rs` already polls GitHub releases; add `pi update` command
- **Shell completions** — ship bash/zsh/fish completions via `clap_complete` (already a dependency)

**Why:** Every friction point in installation is a user lost. `curl | sh` +
self-update makes adoption trivial.

**Effort:** ~1 week for CI, ~2-3 days for self-update

---

## 4. Extension WASM Sandbox Hardening (Priority: High)

The Rust port has `pi_wasm.rs` (wasmtime) + the entire hostcall system
(7 modules, ~121k LOC total in the extension subsystem). This is the
single largest investment area and represents functionality the TypeScript
version does not and cannot match.

**Areas to invest:**
- **Resource limits** — CPU time budgets, memory caps, filesystem sandboxing per extension
- **Capability-based permissions** — extensions declare needed capabilities; user approves at install
- **Extension marketplace API** — the scoring, licensing, and validation modules exist but need a registry endpoint
- **Hot-reload** — load/unload extensions without restarting the agent session

**Why:** Secure extension execution is a hard problem. TypeScript extensions
run in the same process with full access. WASM sandboxing is a genuine
security advantage that can become a platform differentiator.

**Effort:** ~2-3 weeks

---

## 5. Performance: Startup and Memory (Priority: Medium)

The PLAN_TO_PORT_PI_TO_RUST.md sets targets: startup <100ms, binary <20MB.

**Areas to invest:**
- **Startup profiling** — measure cold start time; lazy-init provider connections and extension loading
- **Memory-mapped session files** — for large sessions, mmap the JSONL instead of loading into RAM
- **Streaming compaction** — compact in a background thread without blocking the UI (the `compaction_worker.rs` exists but may need optimization)
- **SQLite session index** — already implemented; ensure it's indexed properly for fast session search across thousands of sessions

**Why:** Noticeable performance is the primary user-facing advantage of a
native binary. If startup feels instant and memory stays flat during long
sessions, users stay.

**Effort:** ~1 week profiling + targeted fixes

---

## 6. ACP / IDE Integration Expansion (Priority: Medium)

`src/acp.rs` (1,509 lines) implements Zed editor integration via Agent
Client Protocol. This is Rust-only and a strong differentiator.

**Areas to invest:**
- **VS Code extension** — ACP or LSP-based integration for the most popular editor
- **Neovim plugin** — Lua wrapper around the ACP stdio interface
- **JetBrains gateway** — plugin for IntelliJ-family IDEs
- **ACP protocol documentation** — formalize the protocol spec so third-party editors can integrate

**Why:** IDE integration is where coding agents become sticky. The TypeScript
version has no IDE protocol at all.

**Effort:** ~1-2 weeks per editor (VS Code first)

---

## 7. Provider Reliability Layer (Priority: Medium)

The Rust port has 11 provider backends (vs TypeScript's ~8 via pi-ai).
Each is independently implemented, which means each can independently break.

**Areas to invest:**
- **Provider health monitoring** — track error rates, latency P50/P99 per provider
- **Automatic failover** — if primary provider returns 5xx, fall back to secondary
- **Rate limit handling** — per-provider rate limit tracking with exponential backoff
- **Cost tracking** — token counting per session with provider-specific pricing
- **VCR test coverage** — `vcr.rs` exists; record golden cassettes for each provider's streaming format

**Why:** Multi-provider support only matters if it's reliable. One flaky
provider shouldn't break the entire agent.

**Effort:** ~1-2 weeks

---

## 8. CI / Release Pipeline (Priority: Medium)

The `tests/` directory has 250+ test files including e2e, conformance,
security, and performance suites. But there's no visible CI configuration.

**Areas to invest:**
- **GitHub Actions workflow** — `cargo test`, `cargo clippy`, `cargo fmt --check`
- **Tiered test execution** — fast unit tests on every PR, e2e/security on merge to main
- **Release automation** — tag-triggered cross-platform binary builds + GitHub release
- **Test flake management** — `flake_classifier.rs` exists; wire it into CI reporting

**Why:** 6,187 tests are worthless if nobody runs them. CI makes the test
investment pay off continuously.

**Effort:** ~2-3 days

---

## 9. Session Portability and Collaboration (Priority: Low)

The Rust port has richer session infrastructure (SQLite index, metrics,
v2 store) but sessions are still local files.

**Areas to invest:**
- **Session export formats** — HTML export exists (`to_html()`); add Markdown export
- **Session sharing** — `share.rs` and `DEFAULT_SHARE_VIEWER_URL` exist; flesh out the upload flow
- **Session import** — resume a shared session locally
- **Multi-user sessions** — real-time collaborative sessions via WebSocket relay

**Why:** Shareable sessions make the agent useful in team contexts. This is
a product differentiator, not a technical one.

**Effort:** ~1-2 weeks

---

## Priority Summary

| Priority | Epic | Effort | Impact |
|----------|------|--------|--------|
| Immediate | Fix 12 failing tests | 1-2 days | Foundation stability |
| High | Module decomposition | 3-5 days | Developer velocity |
| High | Single-binary distribution | 1-2 weeks | User adoption |
| High | WASM sandbox hardening | 2-3 weeks | Security moat |
| Medium | Performance profiling | 1 week | User experience |
| Medium | IDE integration expansion | 1-2 weeks/editor | Stickiness |
| Medium | Provider reliability | 1-2 weeks | Robustness |
| Medium | CI pipeline | 2-3 days | Quality assurance |
| Low | Session portability | 1-2 weeks | Collaboration |

**Recommended first sprint:** Fix tests + CI pipeline + module decomposition.
These are low-risk, high-leverage investments that make everything else easier.
