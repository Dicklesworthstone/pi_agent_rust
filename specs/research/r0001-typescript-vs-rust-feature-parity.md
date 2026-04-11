# r0001 Feature Parity: TypeScript (pi-mono) vs Rust Port

> Research document comparing the original TypeScript coding agent
> (`legacy_pi_mono_code/pi-mono/packages/coding-agent/`) with the current
> Rust codebase (`src/`).
>
> Date: 2026-04-11 | Rust LOC: ~568k | TypeScript LOC: ~42k (129 files)

---

## Scale Comparison

| Metric | TypeScript | Rust |
|--------|-----------|------|
| Source files | 129 `.ts` | 73 `.rs` + 37 binaries/helpers |
| Lines of code | ~42,000 | ~568,000 |
| Test count | (vitest suite) | 6,160+ unit tests |

The Rust port is substantially larger due to inline provider implementations,
extensive test suites, and features that go beyond the TypeScript original
(see "Rust-only features" below).

---

## 1. Core Tools

| Tool | TypeScript | Rust | Notes |
|------|-----------|------|-------|
| bash | `core/tools/bash.ts` | `src/tools.rs` (BashTool) | Both support timeouts, output limits |
| read | `core/tools/read.ts` | `src/tools.rs` (ReadTool) | Both handle text + images |
| write | `core/tools/write.ts` | `src/tools.rs` (WriteTool) | Equivalent |
| edit | `core/tools/edit.ts` | `src/tools.rs` (EditTool) | Equivalent |
| grep | `core/tools/grep.ts` | `src/tools.rs` (GrepTool) | Both wrap ripgrep (`rg`) |
| find | `core/tools/find.ts` | `src/tools.rs` (FindTool) | Equivalent |
| ls | `core/tools/ls.ts` | `src/tools.rs` (LsTool) | Equivalent |
| hashline_edit | — | `src/tools.rs` (HashlineEditTool) | **Rust-only** |
| edit-diff | `core/tools/edit-diff.ts` | — | **TS-only** visual diff rendering |
| file-mutation-queue | `core/tools/file-mutation-queue.ts` | — | **TS-only** serialized file ops |
| truncate | `core/tools/truncate.ts` | — | **TS-only** (truncation is inline in Rust) |

**Verdict**: Core tool parity is high. Rust adds `hashline_edit`. TypeScript has
some helper abstractions (edit-diff, mutation queue) not directly ported but
whose functionality is incorporated inline.

---

## 2. LLM Providers

| Provider | TypeScript | Rust | Notes |
|----------|-----------|------|-------|
| Anthropic | via `@mariozechner/pi-ai` | `src/providers/anthropic.rs` | Both |
| OpenAI (completions) | via `@mariozechner/pi-ai` | `src/providers/openai.rs` | Both |
| OpenAI (responses) | via `@mariozechner/pi-ai` | `src/providers/openai_responses.rs` | Both |
| Google Gemini | via `@mariozechner/pi-ai` | `src/providers/gemini.rs` | Both |
| Google Vertex | via `@mariozechner/pi-ai` | `src/providers/vertex.rs` | Both |
| Azure OpenAI | via `@mariozechner/pi-ai` | `src/providers/azure.rs` | Both |
| AWS Bedrock | `bun/register-bedrock.ts` | `src/providers/bedrock.rs` | Both |
| OpenRouter | model registry | `src/provider.rs` (KnownProvider) | Both |
| Groq | model registry | `src/provider.rs` (KnownProvider) | Both |
| Mistral | model registry | `src/provider.rs` (KnownProvider) | Both |
| Cohere | — | `src/providers/cohere.rs` | **Rust-only** |
| Copilot (GitHub) | — | `src/providers/copilot.rs` | **Rust-only** |
| GitLab | — | `src/providers/gitlab.rs` | **Rust-only** |
| Codex responses | — | `src/providers/openai_responses.rs` | **Rust-only** variant |
| Google Gemini CLI | — | `src/provider.rs` | **Rust-only** variant |

**Verdict**: Rust has **more** provider backends. TypeScript delegates to the
shared `@mariozechner/pi-ai` package; Rust implements each provider natively
with dedicated SSE parsing.

---

## 3. Session Management

| Feature | TypeScript | Rust |
|---------|-----------|------|
| JSONL session storage | `core/session-manager.ts` | `src/session.rs` |
| Session branching/forking | `core/session-manager.ts` | `src/session.rs` (ForkPlan) |
| Branch summarization | `core/compaction/branch-summarization.ts` | `src/compaction.rs` |
| Context compaction | `core/compaction/compaction.ts` | `src/compaction.rs`, `src/compaction_worker.rs` |
| Session picker UI | `cli/session-picker.ts` | `src/session_picker.rs` |
| Session search | `cli/session-picker.ts` | `src/session_picker.rs` |
| SQLite session index | — | `src/session_sqlite.rs`, `src/session_index.rs` | **Rust-only** |
| Session metrics | — | `src/session_metrics.rs` | **Rust-only** |
| Session store v2 | — | `src/session_store_v2.rs` | **Rust-only** |
| Migrations | `migrations.ts` | `src/migrations.rs` | Both |

**Verdict**: Rust has a more advanced session layer with SQLite indexing,
metrics tracking, and a v2 store abstraction not present in TypeScript.

---

## 4. Interactive TUI

| Feature | TypeScript | Rust |
|---------|-----------|------|
| Terminal UI framework | Ink (React-based) | Bubbletea (charmed-bubbles) |
| Model selector | `components/model-selector.ts` | `src/interactive/model_selector_ui.rs` |
| Thinking level selector | `components/thinking-selector.ts` | `src/interactive/commands.rs` |
| Session selector | `components/session-selector.ts` | `src/session_picker.rs` |
| Extension selector | `components/extension-selector.ts` | `src/interactive/ext_session.rs` |
| Theme selector | `components/theme-selector.ts` | `src/theme.rs` |
| Settings selector | `components/settings-selector.ts` | — |
| Tree view (branches) | `components/tree-selector.ts` | `src/interactive/tree.rs`, `tree_ui.rs` |
| Diff display | `components/diff.ts` | `src/interactive/tool_render.rs` |
| Keybinding hints | `components/keybinding-hints.ts` | `src/interactive/keybindings.rs` |
| Custom editor | `components/custom-editor.ts` | — |
| Login dialog | `components/login-dialog.ts` | `src/auth.rs` (CLI-based) |
| OAuth selector | `components/oauth-selector.ts` | `src/auth.rs` |
| Footer | `components/footer.ts` | `src/interactive/view.rs` |
| Config selector | `components/config-selector.ts` | — |
| Countdown timer | `components/countdown-timer.ts` | — |
| Image display selector | `components/show-images-selector.ts` | — |
| Terminal image rendering | — | `src/terminal_images.rs` | **Rust-only** inline images |
| Share/export | — | `src/interactive/share.rs` | **Rust-only** |

**Verdict**: Both have rich TUIs. TypeScript uses React/Ink components;
Rust uses a Bubbletea-style model. TypeScript has more UI selector widgets.
Rust adds inline terminal image rendering and share functionality.

---

## 5. Extensions & Plugins

| Feature | TypeScript | Rust |
|---------|-----------|------|
| Extension loader | `core/extensions/loader.ts` | `src/extensions.rs` |
| Extension runner | `core/extensions/runner.ts` | `src/extensions_js.rs` |
| Extension types | `core/extensions/types.ts` | `src/extension_tools.rs` |
| Extension wrapper | `core/extensions/wrapper.ts` | `src/extension_dispatcher.rs` |
| WASM runtime | — | `src/pi_wasm.rs` (wasmtime) | **Rust-only** |
| Extension validation | — | `src/extension_validation.rs` | **Rust-only** |
| Extension scoring | — | `src/extension_scoring.rs` | **Rust-only** |
| Extension licensing | — | `src/extension_license.rs` | **Rust-only** |
| Extension popularity | — | `src/extension_popularity.rs` | **Rust-only** |
| Extension conformance | — | `src/extension_conformance_matrix.rs` | **Rust-only** |
| Extension preflight | — | `src/extension_preflight.rs` | **Rust-only** |
| Extension inclusion | — | `src/extension_inclusion.rs` | **Rust-only** |
| Extension replay | — | `src/extension_replay.rs` | **Rust-only** |
| Extension index | — | `src/extension_index.rs` | **Rust-only** |
| MCP support | minimal (vendor highlight.js only) | `src/extensions.rs` | Both minimal |

**Verdict**: The Rust port has a **dramatically larger** extension ecosystem
infrastructure — validation, scoring, licensing, conformance matrices, WASM
sandboxing — that does not exist in the TypeScript original. This is the
single biggest area of divergence.

---

## 6. Modes of Operation

| Mode | TypeScript | Rust |
|------|-----------|------|
| Interactive (TUI) | `modes/interactive/interactive-mode.ts` | `src/interactive.rs` |
| Print (non-interactive) | `modes/print-mode.ts` | `src/app.rs`, `src/cli.rs` |
| RPC (JSON over stdio) | `modes/rpc/rpc-mode.ts` | `src/rpc.rs` |
| ACP (Zed editor) | — | `src/acp.rs` | **Rust-only** |
| SDK (library) | `core/sdk.ts` | `src/sdk.rs` | Both |

**Verdict**: Rust adds ACP (Agent Client Protocol) for Zed editor integration,
which the TypeScript version does not have.

---

## 7. Auth & Configuration

| Feature | TypeScript | Rust |
|---------|-----------|------|
| API key auth | `core/auth-storage.ts` | `src/auth.rs` |
| OAuth login | `components/oauth-selector.ts` | `src/auth.rs` |
| Config file | `config.ts` | `src/config.rs` |
| Settings manager | `core/settings-manager.ts` | `src/config.rs` |
| Permissions store | — | `src/permissions.rs` | **Rust-only** |
| Keybindings | `core/keybindings.ts` | `src/keybindings.rs` |
| Model registry | `core/model-registry.ts` | `src/models.rs` |
| Model resolver | `core/model-resolver.ts` | `src/model_selector.rs` |

---

## 8. Features Only in Rust (not in TypeScript)

| Feature | Source | Description |
|---------|--------|-------------|
| ACP (Zed integration) | `src/acp.rs` | Agent Client Protocol for IDE embedding |
| SQLite session index | `src/session_sqlite.rs` | Fast session lookup/search |
| Terminal images | `src/terminal_images.rs` | Inline image rendering (iTerm2/Kitty/Sixel) |
| VCR test recording | `src/vcr.rs` | Record/replay HTTP streams for testing |
| Pi Doctor | `src/doctor.rs` | Environment health checker |
| Version check | `src/version_check.rs` | Background GitHub release polling |
| Permissions store | `src/permissions.rs` | Persistent tool approval/denial |
| Extension marketplace infra | `src/extension_*.rs` (10 files) | Scoring, licensing, validation, conformance |
| WASM sandbox | `src/pi_wasm.rs` | Wasmtime-based extension isolation |
| Flake classifier | `src/flake_classifier.rs` | Test flakiness detection |
| Hostcall system | `src/hostcall_*.rs` (7 files) | Advanced host-extension communication |
| Conformance testing | `src/conformance.rs` | Golden file validation framework |
| Provider metadata | `src/provider_metadata.rs` | Runtime provider capability discovery |
| Scheduler | `src/scheduler.rs` | Background task scheduling |
| SSE parser | `src/sse.rs` | Native SSE stream parser |
| Performance build | `src/perf_build.rs` | Build-time performance instrumentation |
| Copilot/GitLab providers | `src/providers/{copilot,gitlab,cohere}.rs` | Additional provider backends |

---

## 9. Features Only in TypeScript (not in Rust)

| Feature | Source | Description |
|---------|--------|-------------|
| HTML export | `core/export-html/` | Export sessions to styled HTML |
| ANSI-to-HTML converter | `core/export-html/ansi-to-html.ts` | Terminal color to HTML |
| Edit diff viewer | `core/tools/edit-diff.ts` | Visual diff for edit operations |
| File mutation queue | `core/tools/file-mutation-queue.ts` | Serialized file write coordination |
| Event bus | `core/event-bus.ts` | Internal pub/sub event system |
| Source info | `core/source-info.ts` | Source metadata tracking |
| Output guard | `core/output-guard.ts` | Output safety/sanitization |
| Skills system | `core/skills.ts` | Composable skill definitions |
| Prompt templates | `core/prompt-templates.ts` | Reusable prompt snippets |
| Resource loader | `core/resource-loader.ts` | Dynamic resource loading |
| Footer data provider | `core/footer-data-provider.ts` | Structured footer info |
| Timings | `core/timings.ts` | Operation timing tracking |
| Various UI selectors | `components/{config,countdown,images,...}` | Additional interactive widgets |
| Package manager CLI | `package-manager-cli.ts` | Standalone extension manager CLI |
| Photon image processing | `utils/photon.ts` | WASM-based image ops |
| EXIF orientation | `utils/exif-orientation.ts` | Image orientation correction |

---

## 10. Summary

The Rust port is **not a 1:1 translation** — it is a superset of the
TypeScript original with significant additions:

- **Provider coverage** is broader (Copilot, GitLab, Cohere, Codex)
- **Extension infrastructure** is vastly expanded (10+ modules for validation, scoring, licensing, WASM sandboxing)
- **Session storage** is more sophisticated (SQLite index, metrics, v2 store)
- **IDE integration** via ACP (Zed) is new
- **Testing infrastructure** is far more comprehensive (6,160+ tests, VCR recording, conformance framework, flake detection)

The TypeScript version retains a few features not yet ported:
- **HTML session export** (styled HTML output)
- **Several UI selector widgets** (config, countdown, images)
- **Event bus** abstraction
- **Skills system** (composable skill definitions)

Overall porting completion is estimated at **~85-90%** of the TypeScript
feature set, with the Rust port adding roughly **40% more functionality**
beyond what the TypeScript version offers.
