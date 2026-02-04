## Test Coverage Matrix (No‑Mock Audit)

This document inventories test coverage for **all `src/` modules** and **all `tests/` files**, flags mock usage, and lists prioritized gaps.

> Last updated: 2026-02-04

### Legend
- **Unit**: `#[cfg(test)]` tests inside the module file.
- **Integration**: tests under `tests/`.
- **Conformance**: fixture‑based behavior verification against legacy expectations.
- **E2E**: end‑to‑end CLI or real provider flows (none currently automated).

---

## 1) Module Coverage Matrix (all `src/`)

| Module | Unit | Integration | Conformance | E2E | Notes / Mocks |
|---|---|---|---|---|---|
| `src/agent.rs` | ✅ | `tests/rpc_mode.rs` | ❌ | ❌ | RPC tests exercise agent loop indirectly. |
| `src/auth.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |
| `src/cli.rs` | ✅ | ❌ | ❌ | ❌ | CLI parsing lacks CLI‑level E2E. |
| `src/compaction.rs` | ❌ | `tests/compaction.rs` | ❌ | ❌ | Scripted provider + session compaction coverage. |
| `src/config.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |
| `src/error.rs` | ❌ | `tests/error_types.rs` | ❌ | ❌ | Error formatting + hint coverage. |
| `src/extensions.rs` | ✅ | `tests/extensions_manifest.rs`, `tests/ext_conformance_artifacts.rs` | 🔶 | ❌ | Protocol/schema + compat scanner + connector hardening; runtime dispatch is still stubbed. |
| `src/extensions_js.rs` | ✅ | `tests/event_loop_conformance.rs` | ✅ (`tests/fixtures/event_loop_conformance.json`) | ❌ | PiJS deterministic scheduler + Promise hostcall bridge. |
| `src/http/client.rs` | ❌ | `src/http/test_api.rs`, `src/http/test_asupersync.rs` | ❌ | ❌ | Minimal API smoke only. |
| `src/http/mod.rs` | ❌ | `src/http/test_api.rs`, `src/http/test_asupersync.rs` | ❌ | ❌ | Re-export layer only. |
| `src/http/sse.rs` | ✅ | ❌ | ❌ | ❌ | Unit tests for SSE parsing. |
| `src/http/test_api.rs` | ✅ | ❌ | ❌ | ❌ | API smoke test only. |
| `src/http/test_asupersync.rs` | ✅ | ❌ | ❌ | ❌ | Import smoke test only. |
| `src/interactive.rs` | ✅ | `tests/tui_snapshot.rs`, `tests/tui_state.rs`, `tests/session_picker.rs` | ❌ | ❌ | Interactive TUI state + snapshot coverage. |
| `src/lib.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: no tests (re‑exports). |
| `src/main.rs` | ❌ | `tests/e2e_cli.rs`, `tests/main_cli_selection.rs` | ✅ (`tests/conformance/fixtures/cli_flags.json`) | 🔶 | Headless CLI E2E + CLI flag fixtures; interactive E2E still manual. |
| `src/model.rs` | ❌ | `tests/model_serialization.rs` | ❌ | ❌ | Message/content serialization coverage lives in integration tests. |
| `src/models.rs` | ❌ | `tests/model_registry.rs` | ❌ | ❌ | Registry parsing + defaults. |
| `src/package_manager.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |
| `src/provider.rs` | ❌ | ❌ | ❌ | ❌ | Covered indirectly via provider impl tests. |
| `src/providers/anthropic.rs` | ✅ | `tests/provider_streaming.rs` | ✅ (VCR) | ❌ | Streaming is covered via VCR playback fixtures. |
| `src/providers/azure.rs` | ✅ | `tests/provider_streaming.rs` | ✅ (VCR) | ❌ | Streaming is covered via VCR playback fixtures. |
| `src/providers/gemini.rs` | ✅ | `tests/provider_streaming.rs` | ✅ (VCR) | ❌ | Streaming is covered via VCR playback fixtures. |
| `src/providers/openai.rs` | ✅ | `tests/provider_streaming.rs` | ✅ (VCR) | ❌ | Streaming is covered via VCR playback fixtures. |
| `src/providers/mod.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: no tests. |
| `src/resources.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |
| `src/rpc.rs` | ❌ | `tests/rpc_mode.rs` | ❌ | ❌ | Uses a deterministic `TestProvider` in RPC tests. |
| `src/session.rs` | ✅ | `tests/session_conformance.rs` | ❌ | ❌ | Session JSONL conformance coverage. |
| `src/session_index.rs` | ❌ | `tests/session_index_tests.rs` | ❌ | ❌ | Indexing + retrieval coverage. |
| `src/sse.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage for SSE parser. |
| `src/tools.rs` | ✅ | `tests/tools_conformance.rs` | ✅ (`tests/conformance_fixtures.rs` + fixtures) | ❌ | Best‑covered module. |
| `src/tui.rs` | ✅ | `tests/tui_snapshot.rs` | ❌ | ❌ | Snapshot/regression coverage via insta. |
| `src/vcr.rs` | ✅ | `tests/provider_streaming.rs` | ✅ (VCR) | ❌ | VCR playback/record infra. |
| `src/session_picker.rs` | ✅ | `tests/session_picker.rs` | ❌ | ❌ | Session picker UI state coverage. |

---

## 2) Test Suite Inventory (all `tests/`)

| Test File | Type | Modules Covered | Notes / Mocks |
|---|---|---|---|
| `tests/tools_conformance.rs` | Integration | `src/tools.rs` | Direct tool execution tests. |
| `tests/conformance_fixtures.rs` | Conformance | `src/tools.rs`, truncation | Fixture runner for tool parity. |
| `tests/session_conformance.rs` | Conformance | `src/session.rs` | JSONL session format v3. |
| `tests/rpc_mode.rs` | Integration | `src/rpc.rs`, `src/agent.rs`, `src/session.rs` | Uses a deterministic `TestProvider`. |
| `tests/provider_streaming.rs` | Conformance | `src/providers/*`, `src/vcr.rs` | VCR-backed streaming fixtures. |
| `tests/e2e_cli.rs` | Integration | `src/main.rs`, `src/app.rs` | Headless CLI smoke (no interactive TUI). |
| `tests/tui_snapshot.rs` | Integration | `src/tui.rs`, `src/interactive.rs` | insta snapshot coverage. |
| `tests/tui_state.rs` | Integration | `src/interactive.rs` | Interactive model state transitions. |
| `tests/event_loop_conformance.rs` | Conformance | `src/extensions_js.rs` | Fixture-driven scheduler ordering/determinism. |
| `tests/extensions_manifest.rs` | Integration | `src/extensions.rs` | Protocol/policy parsing + validation. |
| `tests/ext_conformance_artifacts.rs` | Integration | `src/extensions.rs` | Pinned legacy artifacts + compat ledger snapshots. |
| `tests/conformance/mod.rs` | Conformance infra | Fixture schema | Not a test on its own. |
| `tests/conformance/fixture_runner.rs` | Conformance infra | Fixtures execution | Not a test on its own. |
| `tests/common/harness.rs` | Test infra | Harness utilities | Real FS, no mocks. |
| `tests/common/logging.rs` | Test infra | Logging helpers | Real logging only. |
| `tests/common/mod.rs` | Test infra | Re-exports | — |
| `tests/conformance/fixtures/*.json` | Fixtures | Tools + truncation | Source of parity expectations. |

---

## 3) Mock / Fake / Stub Audit (No‑Mock Policy)

**Found mock usage:** none.

**Allowlisted exceptions (audited):**
- `tests/common/harness.rs`: `MockHttp{Server,Request,Response}` — real local TCP server used for deterministic offline test infra (name contains `Mock*`, but it is not a mocking framework).

**Enforcement:** CI fails if `Mock*` / `Fake*` / `Stub*` identifiers are introduced in `tests/` outside the allowlist (see `.github/workflows/ci.yml`, step `No-mock code guard`).

**Recommendation:** prefer VCR‑backed real provider fixtures (Anthropic/OpenAI/Gemini) once `bd-1pf` (VCR infra) is complete, or build deterministic local providers that exercise real parsing/IO without mocking internal APIs.

---

## 4) Prioritized Coverage Gaps (Backlog Feed)

1. **CLI E2E flows (P0/P1)**  
   Real CLI runs covering: interactive session, `--continue`, `--print`, tool execution, and session persistence (no mocks).  
   _No bead yet; should create one or attach to coverage workstream._

2. **Extension runtime execution (P1)**  
   WASM hostcall + policy decisions + audit logging fixtures.  
   _Beads: `bd-3d1`, `bd-1uj`, `bd-nom`._

3. **Session index + models polish (P1)**  
   Continue expanding coverage for `src/session_index.rs` and model registry edge cases.  
   _Tie into existing model/session test workstreams._

4. **HTTP client integration (P2)**  
   Replace minimal API smoke tests with real request/response fixtures or VCR playback.  
   _Tie into `bd-1pf` once VCR is ready._

---

## 5) Notes

- Conformance suite is strongest for built‑in tools (fixtures + direct tests).
- E2E automation is currently missing; all end‑to‑end runs are manual.
- No‑mock policy violations are prevented via CI guardrails; the only current allowlist entry is the `MockHttp*` test harness types.

---

## 6) Coverage Tooling

Coverage reports are generated with `cargo-llvm-cov` (see the **Coverage** section in `README.md`).

Baseline (2026-02-03): **31.07% line coverage** from `cargo llvm-cov --all-targets --workspace --summary-only`.
CI currently gates on **>= 30% line coverage** (see `.github/workflows/ci.yml`).

CI runs llvm-cov in VCR playback mode (`VCR_MODE=playback`) and uploads artifacts (summary + LCOV + HTML) via `.github/workflows/ci.yml`.
