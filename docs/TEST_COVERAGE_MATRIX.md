## Test Coverage Matrix (No-Mock Audit)

This document inventories test coverage for **all `src/` modules** and **all `tests/` files**, flags mock usage, and lists prioritized gaps.

> Last updated: 2026-02-05

### Legend
- **Unit**: `#[cfg(test)]` tests inside the module file.
- **Integration**: tests under `tests/`.
- **Conformance**: fixture-based behavior verification against legacy expectations.
- **E2E**: end-to-end CLI, real provider flows, or full tool roundtrips (VCR-backed or deterministic).
- **JSONL**: test emits JSONL logs + artifact index per bd-4u9.

---

## 1) Module Coverage Matrix (all `src/`)

| Module | Unit | Integration | Conformance | E2E | JSONL | Notes / Mocks |
|---|---|---|---|---|---|---|
| `src/agent.rs` | ✅ | `tests/rpc_mode.rs`, `tests/agent_loop_vcr.rs` | ❌ | ✅ (VCR) | ✅ | RPC + agent loop VCR tests. |
| `src/auth.rs` | ✅ | `tests/auth_oauth_refresh_vcr.rs` | ❌ | ✅ (VCR) | ❌ | OAuth refresh via VCR cassettes. |
| `src/cli.rs` | ✅ | `tests/e2e_cli.rs`, `tests/main_cli_selection.rs` | ✅ | ✅ | ✅ | CLI parsing + offline E2E with JSONL logs; npm/git stubs for package flows (bd-27t/bd-2fz9/bd-2z22/bd-1ub). |
| `src/compaction.rs` | ❌ | `tests/compaction.rs` | ❌ | ❌ | ❌ | Scripted provider + session compaction coverage. |
| `src/config.rs` | ✅ | `tests/config_precedence.rs` | ❌ | ❌ | ❌ | Config parsing + precedence tests. |
| `src/error.rs` | ❌ | `tests/error_types.rs`, `tests/error_handling.rs` | ❌ | ❌ | ❌ | Error formatting + hint + handling coverage. |
| `src/extensions.rs` | ✅ | `tests/extensions_manifest.rs`, `tests/ext_conformance_artifacts.rs`, `tests/ext_conformance.rs`, `tests/extensions_registration.rs`, `tests/e2e_extension_registration.rs` | 🔶 | 🔶 | ✅ | Registration E2E with JSONL logging (bd-nh33); message/session control uses RecordingHostActions/RecordingSession stubs (bd-m9rk); full runtime E2E tracked by bd-1gl. |
| `src/extensions_js.rs` | ✅ | `tests/event_loop_conformance.rs`, `tests/js_runtime_ordering.rs`, `tests/extensions_provider_streaming.rs`, `tests/e2e_message_session_control.rs` | ✅ | 🔶 | ❌ | PiJS deterministic scheduler + Promise hostcall bridge; E2E message/session control uses RecordingHostActions/RecordingSession stubs. |
| `src/extension_tools.rs` | ❌ | `tests/e2e_extension_registration.rs` | ❌ | ✅ | ✅ | Extension tool wrappers tested via registration E2E. |
| `src/http/client.rs` | ❌ | `src/http/test_api.rs`, `src/http/test_asupersync.rs` | ❌ | ❌ | ❌ | Minimal API smoke only. |
| `src/http/mod.rs` | ❌ | — | ❌ | ❌ | ❌ | Re-export layer only. |
| `src/http/sse.rs` | ✅ | `tests/repro_sse_flush.rs` | ❌ | ❌ | ❌ | Unit tests + SSE flush repro. |
| `src/interactive.rs` | ✅ | `tests/tui_snapshot.rs`, `tests/tui_state.rs`, `tests/session_picker.rs`, `tests/e2e_tui.rs` | ❌ | ✅ | ✅ | TUI state + snapshot + tmux E2E with JSONL artifacts (bd-3hp; VCR playback coverage in bd-dvgl). |
| `src/lib.rs` | ❌ | ❌ | ❌ | ❌ | ❌ | Re-exports only. |
| `src/main.rs` | ❌ | `tests/e2e_cli.rs`, `tests/main_cli_selection.rs` | ✅ | ✅ | ✅ | Headless CLI + tmux interactive E2E with JSONL artifacts; offline npm/git stubs for package flows (bd-27t/bd-2fz9/bd-2z22/bd-1ub). |
| `src/model.rs` | ❌ | `tests/model_serialization.rs` | ❌ | ❌ | ❌ | Message/content serialization. |
| `src/models.rs` | ❌ | `tests/model_registry.rs` | ❌ | ❌ | ❌ | Registry parsing + defaults. |
| `src/package_manager.rs` | ✅ | `tests/package_manager.rs` | ❌ | ❌ | ❌ | Unit + integration coverage. |
| `src/provider.rs` | ❌ | `tests/provider_factory.rs` | ❌ | ❌ | ❌ | Provider factory tests. |
| `src/providers/anthropic.rs` | ✅ | `tests/provider_streaming/anthropic.rs`, `tests/e2e_provider_streaming.rs` | ✅ (VCR) | ✅ | ✅ | Full VCR playback (21 scenarios) with artifact logging. |
| `src/providers/azure.rs` | ✅ | `tests/provider_streaming.rs` | ✅ (VCR) | ❌ | ❌ | VCR-backed streaming fixtures. |
| `src/providers/gemini.rs` | ✅ | `tests/provider_streaming.rs` | ✅ (VCR) | ❌ | ❌ | VCR-backed streaming fixtures. |
| `src/providers/openai.rs` | ✅ | `tests/provider_streaming.rs` | ✅ (VCR) | ❌ | ❌ | VCR-backed streaming fixtures. |
| `src/providers/mod.rs` | ❌ | `tests/provider_factory.rs` | ❌ | ❌ | ❌ | ExtensionStreamSimpleProvider + create_provider. |
| `src/resources.rs` | ✅ | `tests/resource_loader.rs` | ❌ | ❌ | ❌ | Resource loader tests. |
| `src/rpc.rs` | ❌ | `tests/rpc_mode.rs`, `tests/rpc_protocol.rs` | ❌ | ✅ (VCR) | ✅ | VCR-backed RPC tests, no MockProvider (bd-17o). |
| `src/session.rs` | ✅ | `tests/session_conformance.rs`, `tests/e2e_message_session_control.rs`, `tests/extensions_message_session.rs` | ✅ | ❌ | ❌ | Session JSONL conformance + message/session control. |
| `src/session_index.rs` | ❌ | `tests/session_index_tests.rs`, `tests/session_sqlite.rs` | ❌ | ❌ | ❌ | Indexing + SQLite storage. |
| `src/sse.rs` | ✅ | ❌ | ❌ | ❌ | ❌ | Unit coverage for SSE parser. |
| `src/tools.rs` | ✅ | `tests/tools_conformance.rs`, `tests/e2e_tools.rs` | ✅ | ✅ | ✅ | Best-covered: conformance fixtures + E2E roundtrip with artifact logging (bd-2xyv). |
| `src/tui.rs` | ✅ | `tests/tui_snapshot.rs`, `tests/e2e_tui.rs` | ❌ | ✅ | ✅ | tmux E2E capture + JSONL artifacts (bd-3hp). |
| `src/vcr.rs` | ✅ | `tests/provider_streaming.rs`, `tests/rpc_mode.rs`, `tests/auth_oauth_refresh_vcr.rs` | ✅ (VCR) | ✅ | ❌ | VCR playback/record infrastructure. |
| `src/session_picker.rs` | ✅ | `tests/session_picker.rs` | ❌ | ❌ | ❌ | Session picker UI state coverage. |

---

## 2) Test Suite Inventory (all `tests/`)

| Test File | Type | Modules Covered | JSONL | Notes |
|---|---|---|---|---|
| `tests/tools_conformance.rs` | Integration + E2E | `src/tools.rs` | ✅ | Direct tool execution + E2E roundtrip with artifact logging (bd-2xyv). Gates on rg/fd availability. |
| `tests/e2e_tools.rs` | E2E | `src/tools.rs` | ❌ | Additional tool E2E coverage (artifact logging lives in `tests/tools_conformance.rs`, bd-2xyv). |
| `tests/conformance_fixtures.rs` | Conformance | `src/tools.rs`, truncation | ❌ | Fixture runner for tool parity. |
| `tests/session_conformance.rs` | Conformance | `src/session.rs` | ❌ | JSONL session format v3. |
| `tests/rpc_mode.rs` | Integration | `src/rpc.rs`, `src/agent.rs`, `src/session.rs` | ✅ | VCR-backed OpenAI stream for RPC prompt path. No MockProvider (bd-17o). |
| `tests/rpc_protocol.rs` | Integration | `src/rpc.rs` | ❌ | RPC protocol conformance. |
| `tests/provider_streaming.rs` | Conformance | `src/providers/*`, `src/vcr.rs` | ❌ | VCR-backed streaming fixtures (multi-provider). |
| `tests/e2e_provider_streaming.rs` | E2E | `src/providers/anthropic.rs` | ✅ | Anthropic VCR scenarios with artifact logging. |
| `tests/provider_factory.rs` | Integration | `src/providers/mod.rs` | ❌ | Provider creation + ExtensionStreamSimpleProvider. |
| `tests/provider_error_paths.rs` | Integration | `src/providers/*` | ❌ | Provider error handling: VCR-only (HTTP 500 × 4 providers, malformed SSE × 4 providers). One allowlisted `MockHttpServer` test for invalid UTF-8 injection (VCR cannot represent raw bytes). (bd-2x78 complete.) |
| `tests/e2e_cli.rs` | E2E | `src/main.rs`, `src/cli.rs` | ✅ | Offline CLI runs with JSONL logs + artifact index; npm/git stubs for package flows (bd-27t/bd-2fz9/bd-2z22/bd-1ub). |
| `tests/main_cli_selection.rs` | Integration | `src/main.rs` | ❌ | CLI flag/arg selection. |
| `tests/e2e_tui.rs` | E2E | `src/interactive.rs`, `src/tui.rs` | ✅ | tmux-driven interactive E2E with JSONL artifacts (bd-3hp; VCR playback coverage in bd-dvgl). |
| `tests/tui_snapshot.rs` | Integration | `src/tui.rs`, `src/interactive.rs` | ❌ | insta snapshot coverage. |
| `tests/tui_state.rs` | Integration | `src/interactive.rs` | ❌ | Interactive model state transitions. |
| `tests/session_picker.rs` | Integration | `src/session_picker.rs` | ❌ | Session picker UI state. |
| `tests/e2e_extension_registration.rs` | E2E | `src/extensions.rs`, `src/extensions_js.rs` | ✅ | Full registration lifecycle with JSONL logging + artifacts (bd-nh33). |
| `tests/extensions_registration.rs` | Integration | `src/extensions.rs` | ❌ | Extension registration API tests. |
| `tests/extensions_manifest.rs` | Integration | `src/extensions.rs` | ❌ | Protocol/schema + validation. |
| `tests/ext_conformance.rs` | Conformance | `src/extensions.rs` | ❌ | Extension conformance testing. |
| `tests/ext_conformance_artifacts.rs` | Integration | `src/extensions.rs` | ❌ | Pinned legacy artifacts + compat ledger. |
| `tests/ext_conformance_fixture_schema.rs` | Conformance | `src/extensions.rs` | ❌ | Fixture schema validation. |
| `tests/ext_proptest.rs` | Property | `src/extensions.rs` | ❌ | Property-based extension tests. |
| `tests/extensions_provider_streaming.rs` | Integration | `src/extensions_js.rs`, `src/providers/mod.rs` | ❌ | Extension provider streamSimple tests. |
| `tests/extensions_message_session.rs` | Integration | `src/session.rs`, `src/extensions.rs` | ❌ | Extension message/session API using RecordingSession stub (bd-m9rk). |
| `tests/e2e_message_session_control.rs` | E2E | `src/session.rs`, `src/extensions_js.rs`, `src/extensions.rs` | ❌ | Message + session control E2E using RecordingHostActions/RecordingSession stubs (bd-m9rk). |
| `tests/event_loop_conformance.rs` | Conformance | `src/extensions_js.rs` | ❌ | Fixture-driven scheduler ordering/determinism. |
| `tests/js_runtime_ordering.rs` | Integration | `src/extensions_js.rs` | ❌ | JS runtime execution ordering. |
| `tests/agent_loop_vcr.rs` | Integration | `src/agent.rs` | ❌ | Agent loop with VCR playback; records session/timeline JSONL artifacts. |
| `tests/auth_oauth_refresh_vcr.rs` | Integration | `src/auth.rs` | ❌ | OAuth token refresh via VCR cassettes. |
| `tests/model_serialization.rs` | Integration | `src/model.rs` | ❌ | Message/content serialization. |
| `tests/model_registry.rs` | Integration | `src/models.rs` | ❌ | Registry parsing + defaults. |
| `tests/config_precedence.rs` | Integration | `src/config.rs` | ❌ | Config file precedence rules. |
| `tests/error_types.rs` | Integration | `src/error.rs` | ❌ | Error type formatting. |
| `tests/error_handling.rs` | Integration | `src/error.rs`, `src/providers/*`, `src/tools.rs` | ❌ | Fully VCR-based: HTTP 400/401/403/429/500/529 × 4 providers, malformed SSE, SSE error events, empty-body, error hints taxonomy, tool error paths. No `MockHttpServer`. (bd-2x78 complete.) |
| `tests/session_index_tests.rs` | Integration | `src/session_index.rs` | ❌ | Indexing + retrieval. |
| `tests/session_sqlite.rs` | Integration | `src/session_index.rs` | ❌ | SQLite storage backend. |
| `tests/compaction.rs` | Integration | `src/compaction.rs` | ❌ | Session compaction. |
| `tests/resource_loader.rs` | Integration | `src/resources.rs` | ❌ | Resource loading. |
| `tests/package_manager.rs` | Integration | `src/package_manager.rs` | ❌ | Package manager. |
| `tests/repro_sse_flush.rs` | Repro | `src/http/sse.rs` | ❌ | SSE flush reproduction. |
| `tests/repro_config_error.rs` | Repro | `src/config.rs` | ❌ | Config error reproduction. |

### Test Infrastructure

| File | Purpose |
|---|---|
| `tests/common/harness.rs` | TestHarness, MockHttpServer, TestEnv — real FS/TCP, no mocking frameworks. |
| `tests/common/logging.rs` | TestLogger with JSONL output, artifact index, redaction (bd-3ml, bd-4u9). |
| `tests/common/mod.rs` | Re-exports + `run_async()` helper. |
| `tests/common/tmux.rs` | Tmux session driver for interactive E2E. |
| `tests/fixtures/vcr/*.json` | VCR cassettes (32+ files) for Anthropic, OpenAI, OAuth, RPC scenarios. |
| `tests/provider_streaming/` | Per-provider streaming test modules (Anthropic with 21 VCR scenarios). |

---

## 3) Mock / Fake / Stub Audit (No-Mock Policy)

**Found mock usage:** none (mock frameworks), but there are allowlisted stubs.

**Allowlisted exceptions (audited):**
- `tests/common/harness.rs`: `MockHttp{Server,Request,Response}` — real local TCP server. Used by: (1) `tests/provider_error_paths.rs::openai_invalid_utf8_in_sse_is_reported` for raw byte injection (VCR cassettes store `body_chunks: Vec<String>` and cannot represent invalid UTF-8; this is the only legitimate use in provider tests); (2) `tests/extensions_provider_oauth.rs` for OAuth token-exchange flows. `tests/error_handling.rs` is fully VCR — no `MockHttpServer` usage. (bd-2x78 complete; bd-3kl0 closed.)
- `tests/e2e_cli.rs`: `PackageCommandStubs` (npm/git) for offline package-manager E2E; logs to `npm-invocations.jsonl` / `git-invocations.jsonl` (bd-27t/bd-2fz9/bd-2z22).
- `tests/e2e_message_session_control.rs`: `RecordingHostActions` + `RecordingSession` stubs (bd-m9rk).
- `tests/extensions_message_session.rs`: `RecordingSession` stub (bd-m9rk).
- `src/extensions.rs` unit tests: `MockHostActions` for sendMessage/sendUserMessage (bd-m9rk).

**Enforcement:** CI fails if `Mock*` / `Fake*` / `Stub*` identifiers are introduced in `tests/` outside the allowlist (see `.github/workflows/ci.yml`, step `No-mock code guard`).

**VCR-first strategy:** All provider streaming tests use VCR playback cassettes. RPC tests use VCR-backed OpenAI streams (bd-17o). No MockProvider remains in test code. Remaining no-mock cleanup is tracked under `bd-26s` / `bd-102`.

---

## 4) JSONL Logging Coverage (bd-4u9)

Tests with JSONL log + artifact index output:

| Test File | Artifacts Captured |
|---|---|
| `tests/tools_conformance.rs` (e2e_* tests) | Tool inputs, outputs, details JSON, truncation metadata, tool_call_id |
| `tests/e2e_extension_registration.rs` | Extension source, registration payloads (commands/shortcuts/flags/providers), model entries |
| `tests/e2e_cli.rs` | JSONL logs + artifact index; npm/git stub invocation logs |
| `tests/e2e_tui.rs` | `tui-steps.jsonl`, `tui-log.jsonl`, `tui-artifacts.jsonl`, tmux pane captures |
| `tests/rpc_mode.rs` | VCR cassette path, event timeline, session stats |
| `tests/e2e_provider_streaming.rs` | VCR cassette, stream events, scenario parameters |

**Planned (workstream `bd-c4q` under `bd-26s`):** finish VCR-backed interactive E2E (bd-dvgl), extension runtime E2E (bd-1gl), RPC JSONL script (bd-kh2), and remaining CLI scenarios (bd-1o4, bd-idw).

---

## 5) Prioritized Coverage Gaps (Backlog Feed)

1. **Interactive E2E with VCR playback (P1)**  
   Deterministic tmux E2E that exercises full interactive loop + tool call.  
   _Bead: `bd-dvgl` (workstream `bd-c4q`)._

2. **CLI E2E scenario coverage (P1)**  
   Fill remaining CLI flows: tool enable/disable + error paths, full session lifecycle.  
   _Beads: `bd-27t`, `bd-2fz9`, `bd-2z22`, `bd-1o4`, `bd-idw` (workstream `bd-c4q`)._

3. **Extension runtime E2E + conformance (P1/P2)**  
   WASM host + QuickJS runtime parity with fixture-based conformance.  
   _Beads: `bd-1gl`, `bd-nom`, `bd-1f5`, `bd-6vcm`._

4. **No-mock replacements (P1)**
   Replace MockHttpServer + RecordingSession/HostActions with real-path/VCR tests.
   _Beads: ~~`bd-2x78`~~ (complete), ~~`bd-3kl0`~~ (closed), `bd-m9rk` (workstream `bd-102`)._

5. **JSONL logging expansion (P2)**  
   Extend JSONL logs + artifact index to remaining test files as part of `bd-c4q` / `bd-26s`.

---

## 6) Notes

- Conformance suite is strongest for built-in tools (fixtures + direct tests + E2E roundtrip).
- VCR-backed E2E tests now cover: Anthropic streaming (21 scenarios), RPC mode, OAuth refresh, agent loop.
- E2E tool tests gate on `rg`/`fd` availability with clear skip messages (bd-2xyv).
- No-mock policy violations are prevented via CI guardrails; allowlisted stubs include `MockHttp*`, `PackageCommandStubs`, and `RecordingSession`/`RecordingHostActions` (cleanup tracked by `bd-102`/`bd-m9rk`).

---

## 7) Coverage Tooling

Coverage reports are generated with `cargo-llvm-cov` (see the **Coverage** section in `README.md`).

Baseline (2026-02-03): **31.07% line coverage** from `cargo llvm-cov --all-targets --workspace --summary-only`.
CI currently gates on **>= 30% line coverage** (see `.github/workflows/ci.yml`).

CI runs llvm-cov in VCR playback mode (`VCR_MODE=playback`) and uploads artifacts (summary + LCOV + HTML) via `.github/workflows/ci.yml`.
