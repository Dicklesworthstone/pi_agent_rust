## Test Coverage Matrix (No‑Mock Audit)

This document inventories test coverage for **all `src/` modules** and **all `tests/` files**, flags mock usage, and lists prioritized gaps.

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
| `src/compaction.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: no unit/integration coverage. |
| `src/config.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |
| `src/error.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: no tests. |
| `src/extensions.rs` | ✅ | ❌ | ❌ | ❌ | Protocol tests only; **missing hostcall/WASM conformance**. |
| `src/http/client.rs` | ❌ | `src/http/test_api.rs`, `src/http/test_asupersync.rs` | ❌ | ❌ | Minimal API smoke only. |
| `src/http/mod.rs` | ❌ | `src/http/test_api.rs`, `src/http/test_asupersync.rs` | ❌ | ❌ | Re-export layer only. |
| `src/http/sse.rs` | ✅ | ❌ | ❌ | ❌ | Unit tests for SSE parsing. |
| `src/http/test_api.rs` | ✅ | ❌ | ❌ | ❌ | API smoke test only. |
| `src/http/test_asupersync.rs` | ✅ | ❌ | ❌ | ❌ | Import smoke test only. |
| `src/interactive.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |
| `src/lib.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: no tests (re‑exports). |
| `src/main.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: no CLI E2E. |
| `src/model.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: message/content serialization untested here. |
| `src/models.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: no tests. |
| `src/package_manager.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |
| `src/provider.rs` | ❌ | ❌ | ❌ | ❌ | Covered indirectly via provider impl tests. |
| `src/providers/anthropic.rs` | ✅ | ❌ | ❌ | ❌ | Unit tests only; no VCR/streaming fixtures. |
| `src/providers/azure.rs` | ✅ | ❌ | ❌ | ❌ | Unit tests only; no VCR/streaming fixtures. |
| `src/providers/gemini.rs` | ✅ | ❌ | ❌ | ❌ | Unit tests only; no VCR/streaming fixtures. |
| `src/providers/openai.rs` | ✅ | ❌ | ❌ | ❌ | Unit tests only; no VCR/streaming fixtures. |
| `src/providers/mod.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: no tests. |
| `src/resources.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |
| `src/rpc.rs` | ❌ | `tests/rpc_mode.rs` | ❌ | ❌ | **Uses MockProvider** in RPC tests. |
| `src/session.rs` | ✅ | `tests/session_conformance.rs` | ❌ | ❌ | Session JSONL conformance coverage. |
| `src/session_index.rs` | ❌ | ❌ | ❌ | ❌ | **Gap**: no tests. |
| `src/sse.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage for SSE parser. |
| `src/tools.rs` | ✅ | `tests/tools_conformance.rs` | ✅ (`tests/conformance_fixtures.rs` + fixtures) | ❌ | Best‑covered module. |
| `src/tui.rs` | ✅ | ❌ | ❌ | ❌ | **Gap**: snapshot/regression tests missing. |
| `src/vcr.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |
| `src/session_picker.rs` | ✅ | ❌ | ❌ | ❌ | Unit coverage only. |

---

## 2) Test Suite Inventory (all `tests/`)

| Test File | Type | Modules Covered | Notes / Mocks |
|---|---|---|---|
| `tests/tools_conformance.rs` | Integration | `src/tools.rs` | Direct tool execution tests. |
| `tests/conformance_fixtures.rs` | Conformance | `src/tools.rs`, truncation | Fixture runner for tool parity. |
| `tests/session_conformance.rs` | Conformance | `src/session.rs` | JSONL session format v3. |
| `tests/rpc_mode.rs` | Integration | `src/rpc.rs`, `src/agent.rs`, `src/session.rs` | **MockProvider** used. |
| `tests/conformance/mod.rs` | Conformance infra | Fixture schema | Not a test on its own. |
| `tests/conformance/fixture_runner.rs` | Conformance infra | Fixtures execution | Not a test on its own. |
| `tests/common/harness.rs` | Test infra | Harness utilities | Real FS, no mocks. |
| `tests/common/logging.rs` | Test infra | Logging helpers | Real logging only. |
| `tests/common/mod.rs` | Test infra | Re-exports | — |
| `tests/conformance/fixtures/*.json` | Fixtures | Tools + truncation | Source of parity expectations. |

---

## 3) Mock / Fake / Stub Audit (No‑Mock Policy)

**Found mock usage:**
- `tests/rpc_mode.rs`: `MockProvider` (custom Provider impl returning canned stream events).

**Recommendation:** replace with VCR‑backed real provider (Anthropic/OpenAI/Gemini) once `bd-1pf` (VCR infra) is complete, or use a deterministic local test provider that exercises the real streaming parser without mocking internal APIs.

---

## 4) Prioritized Coverage Gaps (Backlog Feed)

1. **Provider streaming VCR (P0)**  
   Add VCR‑backed streaming tests for Anthropic/OpenAI/Gemini/Azure providers.  
   _Bead: `bd-1pf` (in progress)._

2. **CLI E2E flows (P0/P1)**  
   Real CLI runs covering: interactive session, `--continue`, `--print`, tool execution, and session persistence (no mocks).  
   _No bead yet; should create one or attach to coverage workstream._

3. **Extension runtime conformance (P1)**  
   WASM hostcall + policy decisions + audit logging fixtures.  
   _Beads: `bd-3d1`, `bd-1uj`, `bd-nom`._

4. **Session index + compaction + models (P1)**  
   Unit + integration tests for `src/session_index.rs`, `src/compaction.rs`, `src/model.rs`, `src/models.rs`.  
   _No bead yet; should create coverage tasks._

5. **TUI snapshot/regression tests (P1)**  
   Snapshot tests for `src/tui.rs` and session picker.  
   _Bead: `bd-1d3`._

6. **HTTP client integration (P2)**  
   Replace minimal API smoke tests with real request/response fixtures or VCR playback.  
   _Tie into `bd-1pf` once VCR is ready._

---

## 5) Notes

- Conformance suite is strongest for built‑in tools (fixtures + direct tests).
- E2E automation is currently missing; all end‑to‑end runs are manual.
- No‑mock policy is currently violated only by `MockProvider` in RPC tests.
