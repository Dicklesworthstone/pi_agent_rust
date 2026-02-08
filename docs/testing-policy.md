# Testing Policy: Suite Classification and Enforcement

This document defines Pi's test suite boundaries, classification criteria, and enforcement rules.

## Suites

All tests belong to exactly one of three suites:

### Suite 1: Unit (no-mock, no-fixture)

**What it tests:** Pure logic, data transformations, parsing, serialization, state machines.

**Rules:**
- No VCR cassettes, no fixture files, no HTTP servers (real or mock).
- No `MockHttp*`, `RecordingSession`, `RecordingHostActions`, `DummyProvider`, or any struct whose name
  starts with `Mock`, `Fake`, or `Stub` (enforced by CI).
- Temporary filesystem via `tempfile` is permitted (real I/O, not a mock).
- Custom test-only types (e.g. `DeterministicClock`, `SharedBufferWriter`) are permitted when they
  exercise real logic with controlled inputs rather than replacing a dependency.
- `NullSession` and `NullUiHandler` are **not permitted** in this suite (they are no-op stubs
  that suppress real behavior).

**How to run:**
```bash
cargo test --all-targets --lib          # inline #[cfg(test)] modules only
cargo test --all-targets --test model_serialization --test config_precedence \
  --test session_conformance --test error_types     # curated integration subset
```

**Identifying tests in this suite:** Tests live in `#[cfg(test)]` modules inside `src/*.rs` or in
`tests/` files listed in the `[suite.unit]` section of `tests/suite_classification.toml`.

### Suite 2: VCR / Fixture Replay

**What it tests:** Provider streaming, HTTP client behavior, protocol conformance, extension
registration against recorded or pre-built data.

**Rules:**
- VCR cassettes (`VcrRecorder`, `VcrMode::Playback`) are the primary data source.
- JSON fixture files (conformance comparators, extension logs) are permitted.
- `MockHttpServer` is permitted only when VCR cannot represent the test data (e.g. raw invalid
  UTF-8 byte injection). Each use must be documented in the allowlist below.
- `RecordingSession` and `RecordingHostActions` are permitted for session/extension API surface
  testing where a full session is unnecessary.
- Tests must be deterministic: same cassette/fixture, same result. Flaky tests are bugs.

**How to run:**
```bash
cargo test --all-targets                          # default: includes VCR-backed tests
cargo test --features ext-conformance             # + extension conformance
VCR_MODE=playback cargo test --all-targets        # force playback (CI default)
```

**Identifying tests:** Files listed in `[suite.vcr]` of `tests/suite_classification.toml`, or any
test file that imports from `pi::vcr` / references `cassette_root()` / loads JSON fixtures.

### Suite 3: Live E2E

**What it tests:** Full system behavior with real providers, real network, real terminal (tmux).

**Rules:**
- Requires live API keys, network access, and/or tmux.
- Tests must gate on availability: skip gracefully if providers/tools are missing.
- Must emit JSONL logs and artifact indices (per bd-4u9).
- Cost budget: each test run must stay under configurable token/dollar limits.

**How to run:**
```bash
# With live providers (requires API keys)
PI_E2E=1 cargo test --test e2e_cli --test e2e_tui --test e2e_tools

# VCR-backed E2E (deterministic, no API keys needed)
VCR_MODE=playback cargo test --test e2e_provider_streaming --test agent_loop_vcr
```

**Identifying tests:** Files listed in `[suite.e2e]` of `tests/suite_classification.toml`, or any
test file prefixed with `e2e_`.

---

## Definitions

| Term | Definition | Permitted in Suite 1? |
|------|------------|----------------------|
| **Mock** | Object that replaces a dependency with programmable behavior and optional call verification. Identifiers matching `Mock*`, `Fake*`, `Stub*`. | No |
| **VCR cassette** | Recorded HTTP interaction replayed during tests. | No |
| **Fixture file** | Pre-built JSON/text data loaded from disk. | No |
| **Stub type** | No-op or minimal implementation of a trait (`NullSession`, `NullUiHandler`). | No |
| **Test helper** | Controlled-input type that exercises real logic (`DeterministicClock`, `SharedBufferWriter`). | Yes |
| **Tempfile** | Real filesystem I/O via `tempfile` crate. | Yes |
| **Real TCP** | Local `TcpListener` for testing HTTP client code. | Suite 2 only |

---

## Allowlisted Exceptions

Each mock/stub usage outside Suite 1 must be explicitly allowlisted here with rationale:

| Identifier | Location | Suite | Rationale |
|------------|----------|-------|-----------|
| `MockHttpServer` | `tests/common/harness.rs` | 2 | Real local TCP; name is misleading (it's a real server). Used for raw byte injection that VCR cannot represent. |
| `MockHttpRequest` | `tests/common/harness.rs` | 2 | Request builder for `MockHttpServer`. |
| `MockHttpResponse` | `tests/common/harness.rs` | 2 | Response builder for `MockHttpServer`. |
| `PackageCommandStubs` | `tests/e2e_cli.rs` | 3 | Offline npm/git stubs for CLI E2E; logged to JSONL. |
| `RecordingSession` | `tests/extensions_message_session.rs` | 2 | Session API surface testing. Cleanup tracked by bd-m9rk. |
| `RecordingHostActions` | `tests/e2e_message_session_control.rs` | 2 | Extension host action recording. Cleanup tracked by bd-m9rk. |
| `MockHostActions` | `src/extensions.rs` (unit tests) | 2 | In-module stub for sendMessage/sendUserMessage. Cleanup tracked by bd-m9rk. |

**Process for adding new exceptions:** Open a bead with rationale. Get review. Add to this table
with the bead ID. Update the CI allowlist regex in `.github/workflows/ci.yml`.

---

## CI Enforcement

### Existing Guards (ci.yml)

1. **No-mock dependency guard:** Fails if `mockall`, `mockito`, or `wiremock` appear in
   `Cargo.toml` or `Cargo.lock`.

2. **No-mock code guard:** Fails if `Mock*`, `Fake*`, or `Stub*` identifiers appear in `tests/`
   outside the allowlist regex.

### New Guards (this policy)

3. **Suite classification guard:** Fails if any `tests/*.rs` file is not listed in
   `tests/suite_classification.toml`. Ensures every test file has an explicit suite assignment.

4. **VCR leak guard:** Fails if Suite 1 tests import `VcrRecorder`, `VcrMode`, `cassette_root`,
   or load files from `tests/fixtures/vcr/`.

5. **Mock leak guard:** Enhanced version of guard #2 that also checks Suite 1 `src/` test modules
   for `NullSession`, `NullUiHandler`, `DummyProvider`.

### CI Gate Promotion Runbook (bd-k5q5.5.7)

The Linux CI lane includes a promotion gate step after `./scripts/e2e/run_all.sh --profile ci`.
This gate is intentionally **blocking by default** and evaluates the newest
`tests/e2e_results/**/summary.json` alongside:

- `tests/e2e_results/**/evidence_contract.json`
- `tests/ext_conformance/reports/conformance_summary.json`

The step writes a structured verdict at:

- `tests/e2e_results/**/ci_gate_promotion_v1.json`

#### Versioned threshold controls

Thresholds are configured in `.github/workflows/ci.yml` with a version marker.
Defaults are provided in-repo and can be overridden via repository variables.

| Variable | Default | Purpose |
|----------|---------|---------|
| `CI_GATE_PROMOTION_MODE` | `strict` | `strict` blocks merges; `rollback` emits warnings without blocking. |
| `CI_GATE_THRESHOLD_VERSION` | `2026-02-08.v1` | Auditable threshold set version. |
| `CI_GATE_MIN_PASS_RATE_PCT` | `80.0` | Minimum allowed conformance pass rate. |
| `CI_GATE_MAX_FAIL_COUNT` | `36` | Maximum allowed conformance failure count. |
| `CI_GATE_MAX_NA_COUNT` | `170` | Maximum allowed conformance N/A count. |

#### Rollback procedure (emergency, short-lived)

1. Set repository variable `CI_GATE_PROMOTION_MODE=rollback`.
2. Re-run CI and confirm `ci_gate_promotion_v1.json` reports `status=rollback_warning`.
3. Triage failures using:
   - `tests/e2e_results/**/ci_gate_promotion_v1.json`
   - `tests/e2e_results/**/evidence_contract.json`
   - `tests/ext_conformance/reports/conformance_summary.json`
4. Fix root cause or adjust thresholds with a **new** `CI_GATE_THRESHOLD_VERSION`.
5. Restore `CI_GATE_PROMOTION_MODE=strict` and verify the gate returns `status=pass`.

#### Gate behavior self-test

The CI step includes inline assertions that verify mode semantics every run:

- `strict + failures` must fail the job.
- `rollback + failures` must remain non-blocking.
- `strict + no failures` must pass.

---

## Suite Classification File

`tests/suite_classification.toml` maps every test file to its suite:

```toml
[suite.unit]
# Pure logic tests — no mocks, no fixtures, no VCR, no network.
files = [
    "model_serialization",
    "config_precedence",
    "session_conformance",
    "error_types",
    "bench_schema",
    "compaction",
    "compaction_bug",
    "extension_scoring",
    "mock_spec_validation",
    "mock_spec_schema",
    "perf_budgets",
    "perf_comparison",
    "performance_comparison",
]

[suite.vcr]
# VCR cassettes, fixture files, or allowlisted stubs.
files = [
    "provider_streaming",
    "agent_loop_vcr",
    "auth_oauth_refresh_vcr",
    "provider_error_paths",
    "error_handling",
    "http_client",
    "rpc_mode",
    "rpc_protocol",
    "tools_conformance",
    "conformance_fixtures",
    "conformance_comparator",
    "conformance_mock",
    "conformance_report",
    "ext_conformance",
    "ext_conformance_artifacts",
    "ext_conformance_diff",
    "ext_conformance_generated",
    "ext_conformance_guard",
    "ext_conformance_scenarios",
    "ext_conformance_fixture_schema",
    "ext_entry_scan",
    "ext_proptest",
    "ext_load_time_benchmark",
    "extensions_manifest",
    "extensions_registration",
    "extensions_event_wiring",
    "extensions_event_cancellation",
    "extensions_message_session",
    "extensions_policy_negative",
    "extensions_provider_streaming",
    "extensions_provider_oauth",
    "extensions_stress",
    "event_loop_conformance",
    "event_dispatch_latency",
    "js_runtime_ordering",
    "streaming_hostcall",
    "lab_runtime_extensions",
    "session_index_tests",
    "session_sqlite",
    "session_picker",
    "model_registry",
    "package_manager",
    "provider_factory",
    "resource_loader",
    "capability_prompt",
    "tui_state",
    "tui_snapshot",
    "main_cli_selection",
    "repro_sse_flush",
    "repro_config_error",
    "repro_edit_encoding",
    "sse_strict_compliance",
    "repro_sse_newline",
]

[suite.e2e]
# Full system: real providers, real network, real terminal, or tmux.
files = [
    "e2e_cli",
    "e2e_tui",
    "e2e_tools",
    "e2e_provider_streaming",
    "e2e_library_integration",
    "e2e_extension_registration",
    "e2e_message_session_control",
    "e2e_ts_extension_loading",
    "e2e_live",
    "e2e_live_harness",
]
```

---

## Migration Checklist

For tests currently in Suite 2 that should migrate to Suite 1:

1. [ ] Remove VCR imports and cassette references.
2. [ ] Replace `MockHttp*` with real local TCP + deterministic response.
3. [ ] Replace `NullSession` / `NullUiHandler` with real (possibly minimal) implementations.
4. [ ] Replace fixture file loads with inline test data construction.
5. [ ] Verify test passes without `VCR_MODE` environment variable.
6. [ ] Move file entry from `[suite.vcr]` to `[suite.unit]` in classification file.
7. [ ] Run suite classification guard to confirm.

For VCR-heavy tests claiming "live" coverage:

1. [ ] Verify the test actually exercises the code path (not just replaying a canned response).
2. [ ] Add a live E2E variant that runs against real providers (gated on `PI_E2E=1`).
3. [ ] Ensure VCR cassettes are regenerated periodically to catch API changes.
4. [ ] Document the cassette regeneration process in the test file header.
