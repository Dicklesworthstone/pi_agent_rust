# Testing Policy: Suite Classification and Enforcement

This document defines Pi's test suite boundaries, classification criteria, and enforcement rules.

## Parity Test + Logging Contract (DROPIN-171)

This policy document is the normative home of `pi.parity.test_logging_contract.v1`.

The contract binds three things together:
- test suite taxonomy (`unit`, `vcr`, `e2e`)
- structured logging/event artifacts used by those suites
- required failure-triage metadata for deterministic replay/debugging

### Contracted Schemas

| Domain | Schema ID | Source |
|--------|-----------|--------|
| Test log JSONL records | `pi.test.log.v2` | `tests/common/logging.rs` |
| Artifact index JSONL records | `pi.test.artifact.v1` | `tests/common/logging.rs` |
| Evidence contract bundle | `pi.qa.evidence_contract.v1` | `docs/evidence-contract-schema.json` |
| Per-suite failure digest | `pi.e2e.failure_digest.v1` | `docs/evidence-contract-schema.json` |
| Replay bundle | `pi.e2e.replay_bundle.v1` | `tests/e2e_replay_bundles.rs` + run artifacts |
| Extension remediation backlog | `pi.qa.extension_remediation_backlog.v1` | `tests/qa_certification_dossier.rs` + `tests/full_suite_gate/extension_remediation_backlog.json` |
| User-perceived SLI + UX matrix | `pi.perf.sli_ux_matrix.v1` | `docs/perf_sli_matrix.json` |

### Correlation Model

| Field | Scope | Requirement |
|-------|-------|-------------|
| `correlation_id` | Run-level aggregate artifacts | Required in evidence/replay summaries |
| `trace_id` | Per-suite/per-test log stream | Required in `pi.test.log.v2` records |
| `span_id` | Nested operation traces | Optional but must be string when present |
| `parent_span_id` | Span hierarchy | Optional but must be string when present |
| `ci_correlation_id` | Cross-shard CI linkage | Optional but must be string when present |

### Failure Triage Metadata Requirements

For every failed suite entry in evidence artifacts:
- `root_cause_class` must be one of the declared taxonomy values
- `first_failing_assertion` must be recorded as a non-empty string
- `remediation_pointer.replay_command` should be emitted
- `suite_replay_command` and `targeted_test_replay_command` should be emitted when available

### Suite Binding Requirements

| Suite | Minimum Contract Binding |
|-------|--------------------------|
| `unit` | Must preserve schema-valid JSONL logging when test harness logging is used |
| `vcr` | Must preserve deterministic replay + schema-valid log/artifact records |
| `e2e` | Must emit evidence/replay/failure-digest artifacts that satisfy the schema set above, and each workflow must map to one or more user-facing SLIs via `docs/perf_sli_matrix.json` |
| `certification` | Must emit certification dossier artifacts and extension remediation backlog artifacts in lock-step when certification is regenerated |

### Schema Evolution Policy

`pi.parity.test_logging_contract.v1` uses additive, versioned evolution with strict fail-closed validation:

- `pi.test.log.v2` is the current required log schema for new test output.
- `pi.test.log.v1` remains readable only for historical/backfill validation and is rejected by `validate_jsonl_v2_only`.
- `pi.test.artifact.v1` remains the canonical artifact-index schema until a successor is explicitly ratified.
- New schema versions must ship with:
  - validator updates in `tests/common/logging.rs`
  - regression tests covering old/new acceptance and rejection boundaries
  - runbook/policy updates in this document and `docs/qa-runbook.md`
- Cross-run comparison tooling must use stable-field projection (schema/type/level/category/message/context) and scenario/component filtering to avoid false diffs from timing/correlation fields.

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

# Opt-in loom model checks for hostcall queue concurrency invariants.
rch exec -- cargo test --features loom-tests --test hostcall_queue_loom
rch exec -- cargo test --features loom-tests --lib hostcall_queue::tests::loom_
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

Canonical scenario coverage mapping for this suite lives in:

- `docs/e2e_scenario_matrix.json` (schema `pi.e2e.scenario_matrix.v2`)
- `docs/perf_sli_matrix.json` (schema `pi.perf.sli_ux_matrix.v1`)
- Drift and schema enforcement: `python3 scripts/check_traceability_matrix.py`

PERF-3X phase-validation and diagnostics flows must consume SLI outputs keyed by
`scenario_id` + `sli_id`; micro-benchmark-only summaries are insufficient.

### Live Provider Credential + Replay Policy (bd-1f42.2.7)

This policy applies to `tests/e2e_live_harness.rs` and shared helpers in
`tests/common/harness.rs` + `tests/common/logging.rs`.

#### Credential handling and redaction

- API key source precedence is strict: environment (`*_API_KEY`) -> auth store -> `models.json`.
- Credential **values** must never be written to logs, JSONL artifacts, or contract records.
- Live harness artifacts only include `credential_source` metadata (for example `env:OPENAI_API_KEY`).
- Sensitive request header values are force-redacted to `[REDACTED]` before writing run records.
- Every emitted JSONL artifact (`log`, `artifact index`, raw result, contract result, cost contract)
  must pass unredacted-key scans (`find_unredacted_keys`) plus header-pair redaction checks.

#### Quota/rate-limit budget and retry policy

- Cost budgets are enforced per provider via `default_cost_thresholds()` and `check_cost_budget()`:
  warn at soft threshold, fail at hard threshold.
- Live provider calls use deterministic retry policy:
  - `LIVE_E2E_MAX_ATTEMPTS=3`
  - `LIVE_E2E_RETRYABLE_HTTP_STATUS=[408,429,500,502,503,504,529]`
  - `LIVE_E2E_RETRY_BACKOFF_MS=[500,1500]` (ms, exponential-ish fixed schedule)
- Retries are only for transient failures (retryable HTTP status or transport timeout/reset class errors).
- Retry telemetry (`attempts`, `retry_backoff_ms`) is required in live provider result contracts.

#### Deterministic replay boundary and logging guarantees

- Live harness execution mode is always `live_record` (`VcrMode::Record` only).
- Boundary definition:
  - Live network call + live streaming events happen first.
  - Post-call trace extraction reads the latest interaction from the just-recorded cassette.
  - No VCR playback is allowed for this suite.
- Result contracts must include:
  - `execution_mode=live_record`
  - `replay_boundary=live_request_then_vcr_trace_extract`
  - `trace_origin=vcr_last_interaction`
- Normalized JSONL artifacts must still normalize timestamps/paths and preserve redaction.

---

## Test-Double Inventory Baseline (bd-1f42.8.1)

Machine-readable inventory artifact:

- `docs/test_double_inventory.json`

The report tags test-double usage by:

- `file`
- `suite` (`unit`, `vcr`, `e2e`, `unit-inline`, `unclassified`)
- `module`
- nearest `test_case`
- `double_identifier` and `double_type`
- `risk` and rationale

Current baseline snapshot (from `report_id=bd-1f42.8.1-test-double-inventory-v2`, generated `2026-02-13T04:24:50Z`; extension-decomposition locators refreshed `2026-08-06`):

- `entry_count`: 265
- `module_count`: 21
- suite distribution:
  - `unit-inline`: 114
  - `vcr`: 73
  - `unit`: 16
  - `e2e`: 26
  - `unclassified`: 36

Top risk clusters:

- `src/extension_dispatcher` (86 entries, high)
- `src/extensions/tests` (20 entries, high)
- `tests/extensions_provider_oauth` (28 entries, high)
- `tests/e2e_provider_scenarios` (23 entries, high)
- `tests/mock_spec_validation` (11 entries, high)
- `tests/provider_native_contract` (14 entries, high)
- `tests/provider_factory` (13 entries, high)
- `tests/common` (23 entries, high; helper module inventory, currently unclassified)

Interpretation notes:

- High counts in `unit-inline` represent strict audit hotspots and should be reviewed against no-mock policy intent.
- `tests/common` is intentionally helper-only and not part of direct `tests/*.rs` suite classification entries.
- Allowlisted exceptions in this document remain the policy source of truth; the JSON report is the searchable evidence index.
- The inventory now includes `extension_stub_placeholder_reconciliation` for `bd-8t27h.14`. That section separates test fixtures from production compatibility stubs, ties current extension non-pass rows to conformance artifacts, records the audited runtime API stubs resolved by `bd-8t27h.14.1`, and points remaining active runtime crash, packaging, and fixture gaps at child beads `bd-8t27h.14.2` through `bd-8t27h.14.4`.

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

| Identifier | Location | Suite | Rationale | Owner | Replacement Plan |
|------------|----------|-------|-----------|-------|------------------|
| `MockHttpServer` | `tests/common/harness.rs` | 2 | Real local TCP; name is misleading (it's a real server). Used for raw byte injection that VCR cannot represent (invalid UTF-8). | infra | Permanent: VCR stores UTF-8 strings and cannot represent raw invalid bytes. |
| `MockHttpRequest` | `tests/common/harness.rs` | 2 | Request builder for `MockHttpServer`. | infra | Same as `MockHttpServer` — permanent companion type. |
| `MockHttpResponse` | `tests/common/harness.rs` | 2 | Response builder for `MockHttpServer`. | infra | Same as `MockHttpServer` — permanent companion type. |
| `PackageCommandStubs` | `tests/e2e_cli.rs` | 3 | Offline npm/git stubs for CLI E2E; logged to JSONL. | infra | Permanent: real npm/git operations are non-deterministic. |
| `RecordingSession` | `tests/extensions_message_session.rs` | 2 | Session API surface testing. | bd-m9rk | Replace with `SessionHandle` (real session). Most usages already migrated. |
| `RecordingHostActions` | `tests/e2e_message_session_control.rs` | 2 | Extension host action recording; needed where agent loop provides host actions. | bd-m9rk | Evaluate if agent-loop integration test can replace recording. |
| `MockHostActions` | `src/extensions/tests/registration.rs` (unit tests) | 2 | In-module stub for `sendMessage`/`sendUserMessage`. | bd-m9rk | Replace with real session-based dispatch once full integration test exists. |

**Process for adding new exceptions:** Open a bead with rationale. Get review. Add to this table
with the bead ID. Add the `<path>:<Identifier>` entry to `.no-mock-allowlist` at repo root (the
no-mock CI gate in `.github/workflows/ci.yml` reads that file; new entries are not allowed
without a corresponding allowlist line).

### Ratified Non-Mock Standard (bd-1f42.1.3)

This section is the authoritative accepted/rejected matrix for test doubles.

Accepted (with explicit rationale and scope):

- Real local test infrastructure helpers that preserve real protocol behavior (`MockHttpServer` family).
- Recording doubles used to capture host/session side effects for contract assertions (`RecordingSession`, `RecordingHostActions`).
- CLI workflow stubs used in E2E to isolate external package managers while preserving end-user flow assertions (`PackageCommandStubs`).

Rejected:

- Any `Mock*`, `Fake*`, `Stub*`, `DummyProvider`, `NullSession`, or `NullUiHandler` in Suite 1 (`unit`) tests.
- Any new no-op trait implementation in Suite 1 that suppresses real behavior instead of exercising production logic.
- Any new allowlist entry without explicit owner, expiry, and replacement plan.

Mandatory exception template (required for temporary allowance):

- `bead_id`: tracking issue that justifies the exception.
- `owner`: single accountable owner.
- `expires_at`: hard expiration date (UTC).
- `replacement_plan`: concrete path to remove the double.
- `scope`: exact files/tests where the exception is permitted.
- `verification`: CI/tests proving behavior remains covered despite the temporary double.

Review checklist for exception approval:

- Is the double outside Suite 1?
- Is there a deterministic alternative (VCR/fixture/real local service) that was evaluated?
- Is owner + expiry + replacement plan documented?
- Is CI allowlist updated narrowly (no broad wildcard)?
- Is follow-up bead dependency linked to removal work?

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

### CI Gate Lanes (bd-1f42.8.8.1)

CI gates are organized into two evaluation lanes:

**Preflight fast-fail lane:** Evaluates only blocking gates, stops at first failure.
Used for fast PR feedback. Command:
```bash
cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact
```

**Full certification lane:** Evaluates all gates (blocking + non-blocking), generates
waiver audit, and produces a verdict with promotion rules and rerun guidance. Command:
```bash
cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact
```

**Extension must-pass authority:** The release-blocking `ext_must_pass` corpus is
the exact selection in `docs/extension-inclusion-list.json` (`tier1` plus
`tier1_review`). `tests/ext_conformance/VALIDATED_MANIFEST.json` supplies the
corresponding artifact metadata, but its numeric tier labels do not independently
define or shrink the release set. A passing extension gate is necessary release
evidence; it does not by itself authorize a strict drop-in claim.

**Drop-in contract gate (bd-35t7i):** strict drop-in release language is only
allowed when `docs/contracts/dropin-certification-contract.json` evaluates to
all hard gates `pass` and the emitted
`docs/evidence/dropin-certification-verdict.json` has
`overall_verdict = CERTIFIED` with `git_commit` naming the clean release source
commit. The final release ref must equal that commit or descend from it only
through allowlisted evidence-only commits.
Operational incident response for parity regressions is documented in
`docs/ci-operator-runbook.md` under **Parity Incident Response (DROPIN-162)**.

Artifacts:
- `tests/full_suite_gate/preflight_verdict.json` (schema `pi.ci.preflight_lane.v1`)
- `tests/full_suite_gate/certification_verdict.json` (schema `pi.ci.certification_lane.v1`)
- `tests/full_suite_gate/waiver_audit.json` (schema `pi.ci.waiver_audit.v1`)
- `tests/full_suite_gate/replay_bundle.json` (schema `pi.e2e.replay_bundle.v1`)

### Waiver Policy (bd-1f42.8.8.1)

CI gates can be temporarily bypassed with auditable waivers in `tests/suite_classification.toml`.
Each waiver requires: `owner`, `created`, `expires` (max 30 days), `bead`, `reason`, `scope`, `remove_when`.

Rules:
- Maximum waiver duration: 30 days (must renew or fix).
- Expired waivers cause CI hard failure via the `waiver_lifecycle` gate.
- Waivers expiring within 3 days trigger warnings.
- Each waiver `gate_id` must match a gate defined in `ci_full_suite_gate.rs`.

See `docs/qa-runbook.md` "Waiver Lifecycle" section for the full schema and examples.

### CI Gate Promotion Runbook (bd-k5q5.5.7)

The Linux CI lane includes a promotion gate step after `./scripts/e2e/run_all.sh --profile ci`.
This gate is intentionally **blocking by default** and evaluates the newest
`tests/e2e_results/**/summary.json` alongside:

- `tests/e2e_results/**/evidence_contract.json`
- `tests/ext_conformance/reports/conformance_summary.json`
- `tests/e2e_results/perf-ci-*/results/baseline_variance_confidence.json`
- `tests/e2e_results/perf-ci-*/results/extension_benchmark_stratification.json`

The runner-level evidence contract (`scripts/e2e/run_all.sh`) now enforces
claim-integrity fail-closed conditions from `docs/perf_sli_matrix.json#ci_enforcement`
when `CLAIM_INTEGRITY_REQUIRED=1` (set in Linux CI lane). This blocks stale,
partial, missing-partition, invalid-label, and microbench-only global claims.

The evidence-adjudication matrix artifact is also part of the claim-integrity
contract:

- JSON artifact: `tests/e2e_results/**/claim_integrity_evidence_adjudication_matrix.json`
- Markdown companion: `tests/e2e_results/**/claim_integrity_evidence_adjudication_matrix.md`
- Required schema id: `pi.claim_integrity.evidence_adjudication_matrix.v1`

Fail-closed summary invariants (must hold together):

1. `summary.conflict_count = summary.resolved_conflict_count + summary.unresolved_conflict_count`
2. `summary.total_claims = summary.pass_count + summary.warn_count + summary.fail_count + summary.missing_count + summary.unknown_count`
3. `summary.overall_status` must be `fail` whenever `summary.unresolved_conflict_count > 0`
4. `summary.observation_count >= summary.total_claims`

Row-level adjudication invariants:

- `claims[*].conflict_detected = true` implies `claims[*].observed_outcomes` has more than one unique value.
- `claims[*].unresolved_conflict = true` is valid only when canonical evidence is unavailable and must roll into `summary.unresolved_conflict_count`.
- `claims[*].adjudicated_confidence` must normalize to one of `high`, `medium`, `low`, `unknown`.
- `claims[*].adjudicated_outcome` must be one of `pass`, `warn`, `fail`, `missing`, `unknown`.

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
| `CLAIM_INTEGRITY_REQUIRED` | `1` (Linux CI lane) | Enables fail-closed claim-integrity evidence checks in `run_all.sh`. |

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

### Perf-vs-Size Artifact Policy and Shipping Strategy (bd-3ar8v.5.5)

Pi uses two distinct artifact classes with different policy roles:

| Artifact class | Primary producers | Required Cargo profile label | Allowed decision scope |
|----------------|-------------------|------------------------------|------------------------|
| Benchmark evidence artifacts | `scripts/perf/orchestrate.sh`, `scripts/bench_extension_workloads.sh`, PERF-3X CI matrix lanes | `perf` (or explicitly configured benchmark profile) | PERF-3X ratio claims, tuning decisions, certification evidence |
| Shipping/release artifacts | `.github/workflows/release.yml`, `cargo build --release`, installer/release binaries | `release` | Distribution integrity, binary-size/startup tradeoffs, rollout safety |

Normative rules:

1. PERF-3X and phase-certification claims must be backed by benchmark evidence artifacts, never by shipping-only binaries.
2. Shipping/release binaries remain the user distribution target and must not be re-labeled as benchmark evidence.
3. Every benchmark evidence bundle must carry profile/provenance labels sufficient for replay and attribution:
   - `build_profile`, `correlation_id`, `scenario_id`, `runtime`, `host`
   - CI linkage when present: `ci_correlation_id`
   - where applicable: `allocator_requested`, `allocator_effective`, allocator fallback field, `pgo_mode_requested`, `pgo_mode_effective`
4. Evidence ingestion for release/certification must fail closed when:
   - profile labels are missing,
   - profile labels conflict across records/manifests in the same run,
   - a global performance claim is sourced from release-only artifacts.
5. Phase-5 gate tasks must consume this policy explicitly:
   - `bd-3ar8v.6.1` opportunity matrix generation
   - `bd-3ar8v.6.2` parameter-sweep certification
   - `bd-3ar8v.6.3` extension conformance + perf stress certification
   - `bd-3ar8v.6.6` unified certification dossier lane

#### Practical-finish checkpoint policy (bd-3ar8v.6.9)

Release/certification decisions must apply a docs-last contract before final report wrap-up:

1. `practical_finish_checkpoint` must pass before declaring final PERF-3X completion.
2. `parameter_sweeps_integrity`, `extension_remediation_backlog`, and `conformance_stress_lineage` are co-required release gates.
3. Remaining open PERF-3X work is allowed only when the Beads `issue_type` is explicitly
   `docs`. Generic labels such as `docs-last`, `documentation`, `report`, or `runbook` do
   not prove that a task is documentation-only. For this contract, **docs/report scope**
   means exactly `issue_type = docs`; any technical open PERF-3X issue is fail-closed and
   blocks GO.

Required evidence artifacts for this policy:

- `tests/full_suite_gate/practical_finish_checkpoint.json` (`pi.perf3x.practical_finish_checkpoint.v1`)
- `tests/perf/reports/parameter_sweeps.json` (`pi.perf.parameter_sweeps.v1`)
- `tests/full_suite_gate/extension_remediation_backlog.json` (`pi.qa.extension_remediation_backlog.v1`)
- `tests/perf/reports/stress_triage.json` (`pi.ext.stress_triage.v1` with `run_id`, `correlation_id`)
- `tests/ext_conformance/reports/conformance_summary.json` (`pi.ext.conformance_summary.v2` with `run_id`, `correlation_id`)

Primary enforcement surfaces:

- `tests/ci_full_suite_gate.rs` (`practical_finish_checkpoint`, `parameter_sweeps_integrity`, `extension_remediation_backlog`, `conformance_stress_lineage`)
- `tests/release_readiness.rs` final certification gate aggregation
- `docs/qa-runbook.md` PERF-3X regression triage + replay procedure

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

## Fast Local Smoke Suite (bd-1f42.6.6)

Contributors can run a fast smoke check before pushing to catch common regressions without
waiting for full CI. The smoke suite targets under 60 seconds on a development machine.

**Command:**
```bash
./scripts/smoke.sh                    # lint + unit + VCR smoke targets
./scripts/smoke.sh --skip-lint        # skip cargo fmt/clippy (faster)
./scripts/smoke.sh --only unit        # only unit smoke targets
./scripts/smoke.sh --only vcr         # only VCR smoke targets
./scripts/smoke.sh --verbose          # show full cargo test output
./scripts/smoke.sh --json             # emit JSON summary to stdout
```

**What it covers:**

| Suite | Targets | Coverage Area |
|-------|---------|---------------|
| Unit | `model_serialization`, `config_precedence`, `session_conformance`, `error_types`, `compaction`, `security_budgets` | Core data model, config, session, error handling |
| VCR | `provider_streaming`, `error_handling`, `http_client`, `sse_strict_compliance`, `model_registry`, `provider_factory` | Provider layer, HTTP, SSE, model routing |

**Structured output:**
- `smoke_log.jsonl`: Per-event JSONL log (schema `pi.smoke.*.v1`)
- `smoke_summary.json`: Machine-readable pass/fail summary (schema `pi.smoke.summary.v1`)
- `<target>/output.log`: Per-target verbose output

**Design rationale:**
- Targets chosen to cover the critical path (model → provider → streaming → tools) with
  the fastest-running tests from each suite.
- No E2E targets: those require tmux/real providers and exceed the 60-second budget.
- `--skip-lint` option for inner-loop iteration where format is already checked.
- Exit code 0 = all pass, 1 = any failure (compatible with pre-commit hooks).

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

---

## Flaky-Test Quarantine and Escalation Policy (bd-1f42.6.3)

Flaky tests undermine CI signal and erode trust in the test suite. This section defines the
taxonomy, quarantine workflow, escalation rules, and auditable tracking for flaky tests.

### Flake Taxonomy

Every flaky test must be classified into exactly one category. Classification determines the
quarantine tier, auto-retry budget, and escalation timeline.

| Category | Code | Description | Retry Budget | Quarantine Tier |
|----------|------|-------------|-------------|-----------------|
| **Timing-dependent** | `FLAKE-TIMING` | Race conditions, sleep-based assertions, non-deterministic scheduling, CI load sensitivity. | 1 retry | 7-day fix window |
| **Environment-dependent** | `FLAKE-ENV` | Filesystem state, locale, timezone, OS-specific behavior, missing system deps. | 1 retry | 7-day fix window |
| **Network-dependent** | `FLAKE-NET` | DNS resolution, port conflicts, firewall rules, VPN state, proxy settings. | 1 retry | 14-day fix window |
| **Resource-dependent** | `FLAKE-RES` | OOM, disk full, file descriptor exhaustion, thread pool saturation. | 1 retry | 14-day fix window |
| **External-service** | `FLAKE-EXT` | Live API rate limits, provider downtime, auth token expiry, quota exhaustion. | 1 retry | 14-day fix window |
| **Non-deterministic logic** | `FLAKE-LOGIC` | Random seeds, hash ordering, floating-point comparison, concurrent data structures. | 1 retry | 7-day fix window |

**Hard limit:** Maximum quarantine window is **14 days** regardless of category. The CI guard
rejects entries with `expires - quarantined > 14`.

### Quarantine Lifecycle

```
Detection ──► Classification ──► Quarantine Entry ──► Fix/Workaround ──► Restore ──► Verify
    │              │                    │                   │                │           │
    ▼              ▼                    ▼                   ▼                ▼           ▼
  CI failure   Assign category    Add to TOML        Land fix PR      Remove from    3 clean
  + flake      + owner + tier     quarantine         or workaround    quarantine     CI runs
  evidence                        section                             section
```

#### Step 1: Detection

A test is suspected flaky when:
- It fails on CI but passes on retry (same commit, same runner OS).
- It passes locally but fails on CI intermittently.
- It fails with different error messages across runs on the same commit.

**Evidence requirement:** The detection claim must include:
- Commit SHA where the flake occurred.
- CI run URL or log excerpt showing the failure.
- At least one passing run on the same commit (proving non-determinism).
- Runner OS and relevant environment variables.

#### Step 2: Classification

Assign a flake category from the taxonomy above. Record:
- `category`: One of the `FLAKE-*` codes.
- `evidence_url`: Link to the CI failure log or artifact.
- `reproduction_command`: Exact command to attempt local reproduction.

#### Step 3: Quarantine Entry

Add the test to the `[quarantine]` section of `tests/suite_classification.toml`:

```toml
[quarantine]
# Each entry: test stem, category, owner, quarantine date, expiry date, bead ID.
# All 9 fields are required. CI rejects entries missing any field.

[quarantine.example_flaky_test]
category = "FLAKE-TIMING"
owner = "AgentName"
quarantined = "2026-02-10"
expires = "2026-02-17"          # Max 14 days from quarantined
bead = "bd-XXXX"                # Tracking bead for the fix
evidence = "https://ci.example.com/run/12345"
repro = "cargo test example_flaky_test -- --nocapture"
reason = "Intermittent timeout on CI due to thread scheduling variance"
remove_when = "Two consecutive green CI runs on Linux/macOS/Windows"
```

**What quarantine means:**
- The test is still compiled and run, but failures are **not blocking** in CI.
- Quarantined test failures are reported in a separate CI summary section.
- The test remains in its original suite classification (unit/vcr/e2e).
- Auto-retry up to the category's retry budget before marking as quarantine-fail.

#### Step 4: Fix or Workaround

The assigned owner must fix the root cause or apply a deterministic workaround within the
quarantine tier's fix window. Acceptable fixes:
- Eliminate the source of non-determinism (use deterministic seeds, mock time, pin ordering).
- Add proper synchronization (barriers, channels, condition variables instead of sleeps).
- Gate on environment availability (skip gracefully if resource is missing).
- Convert from live to VCR-backed (for `FLAKE-EXT` and `FLAKE-NET`).

#### Step 5: Restore

After the fix lands:
1. Remove the entry from `[quarantine]` in `tests/suite_classification.toml`.
2. Verify the test passes on 3 consecutive CI runs (tracked by the bead).
3. Close the tracking bead with a comment linking the fix commit and CI evidence.

#### Step 6: Expiry Enforcement

If a quarantined test is **not fixed** by its `expires` date:
- The quarantine entry turns into a CI **hard failure** (test must be fixed or removed).
- Escalation: the test owner must either extend with justification or disable the test.
- Extension requires a new bead with updated expiry (maximum one extension per test).

### Auto-Retry Policy

CI applies a uniform retry policy for quarantined tests before reporting failure:

| Setting | Value |
|---------|-------|
| Max auto-retries | 1 |
| Retry delay | 5 seconds |
| Retry scope | Failed target only |
| Second failure policy | Treated as deterministic failure |

Non-quarantined tests get **zero retries**. If a non-quarantined test fails, it is a real failure.

### CI Quarantine Guard

The quarantine guard runs as part of CI (`.github/workflows/ci.yml`) and:
1. Reads `[quarantine.*]` entries from `tests/suite_classification.toml`.
2. Validates all 9 required fields: `category`, `owner`, `quarantined`, `expires`, `bead`,
   `evidence`, `repro`, `reason`, `remove_when`.
3. Validates `category` is one of the 6 allowed `FLAKE-*` codes.
4. Validates quarantine span does not exceed 14 days (`expires - quarantined <= 14`).
5. Validates `evidence`, `repro`, and `remove_when` are non-empty.
6. Fails if any entry has expired (current date > `expires`).
7. Emits structured artifacts:
   - `tests/quarantine_report.json` (schema `pi.test.quarantine_report.v2`): active count,
     expiring-soon count, expired count, category breakdown, escalation actions.
   - `tests/quarantine_audit.jsonl` (schema `pi.test.quarantine_audit_entry.v1`): one line
     per quarantine entry for append-only audit trail.

### Escalation Workflow

```
┌─────────────────────────────────────────────────────────────────┐
│                    Flake Escalation Ladder                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Day 0: Detection + classification + quarantine entry           │
│         Owner assigned. Tracking bead created.                  │
│                                                                 │
│  Day 3 (Tier 1) / Day 7 (Tier 2-3): Mid-point check            │
│         Owner posts progress in bead thread.                    │
│         If no progress: escalate to project maintainer.         │
│                                                                 │
│  Expiry day: Fix must be landed and verified.                   │
│         If not fixed: CI hard-fails on the quarantine entry.    │
│         Owner must extend (1x max) or disable the test.         │
│                                                                 │
│  Expiry + 7 days (final deadline): Test is either:              │
│         (a) Fixed and restored, or                              │
│         (b) Removed from the suite with a rationale bead.       │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Quarantine Metrics

The quarantine system tracks:
- **Active quarantine count**: Target is zero. Any non-zero count is a debt signal.
- **Mean time to fix (MTTF)**: Average days from quarantine entry to restoration.
- **Escape rate**: Flaky tests that were restored but re-quarantined within 30 days.
- **Expiry violations**: Tests that hit their expiry deadline without a fix.

These metrics feed into `bd-1f42.6.2` (test health dashboards).

### Quarantine Decision Template

Every quarantine entry must be accompanied by a bead with this information:

```
Title: [FLAKE] <test_name>: <brief description>
Type: bug
Priority: P1 (Tier 1) or P2 (Tier 2-3)

Category: FLAKE-TIMING | FLAKE-ENV | FLAKE-NET | FLAKE-RES | FLAKE-EXT | FLAKE-LOGIC
Owner: <agent or person name>
Quarantined: <YYYY-MM-DD>
Expires: <YYYY-MM-DD> (max 14 days from quarantined)
Evidence: <CI run URL or artifact path>
Reproduction: <exact command>
Remove-when: <objective exit condition for quarantine removal>

Root cause analysis:
  <What makes this test non-deterministic?>

Proposed fix:
  <How will determinism be restored?>

Verification plan:
  <How will we confirm the fix works? (e.g., 3 clean CI runs)>
```
