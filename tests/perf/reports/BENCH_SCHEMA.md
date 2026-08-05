# Benchmark JSONL Schema Reference

> Auto-generated. Do not edit manually.

## Registered Schemas

| Schema | Description |
|---|---|
| `pi.bench.protocol.v1` | Canonical benchmark protocol contract (partitions, datasets, metadata, replay inputs) |
| `pi.ext.rust_bench.v1` | Rust QuickJS extension benchmark event (load, tool call, event hook) |
| `pi.ext.legacy_bench.v1` | Legacy pi-mono (Node.js) extension benchmark event |
| `pi.perf.workload.v1` | PiJS workload harness output (tool call throughput) |
| `pi.perf.budget.v1` | Performance budget check result |
| `pi.perf.budget_summary.v2` | Strict provenance-bound budget summary with per-budget results and claim readiness |
| `pi.ext.conformance_report.v2` | Per-extension conformance report event |
| `pi.ext.conformance_summary.v2` | Aggregate conformance summary with per-tier breakdowns |
| `pi.perf.extension_benchmark_stratification.v1` | Layered extension benchmark artifact linking cold-load, per-call, and full E2E evidence with claim-integrity guards |
| `pi.perf.phase1_matrix_validation.v1` | Phase-1 realistic/matched-state matrix validation with stage attribution and release-gate readiness |
| `pi.resource_governor.admission.v1` | Host-scale resource-governor admission decision telemetry for swarm pressure control |

## Environment Fingerprint

Every benchmark record SHOULD include an `env` object with:

| Field | Type | Description |
|---|---|---|
| `os` | string | Operating system name and version |
| `arch` | string | CPU architecture (x86_64, aarch64) |
| `cpu_model` | string | CPU model string from /proc/cpuinfo or sysinfo |
| `cpu_cores` | integer | Logical CPU core count |
| `mem_total_mb` | integer | Total system memory in megabytes |
| `build_profile` | string | Cargo build profile: debug or release |
| `git_commit` | string | Short git commit hash of the build |
| `features` | string[] | Active Cargo feature flags |
| `config_hash` | string | SHA-256 of env fields for dedup |

## Required Fields by Schema

### `pi.ext.rust_bench.v1`

| Field | Type | Description |
|---|---|---|
| `schema` | string | Always `"pi.ext.rust_bench.v1"` |
| `runtime` | string | Always `"pi_agent_rust"` |
| `scenario` | string | Benchmark scenario (e.g., `ext_load_init/load_init_cold`) |
| `extension` | string | Extension ID being benchmarked |
| `runs` | integer | Number of runs (load scenarios) |
| `iterations` | integer | Number of iterations (throughput scenarios) |
| `summary` | object | `{count, min_ms, p50_ms, p95_ms, p99_ms, max_ms}` |
| `elapsed_ms` | float | Total elapsed time in milliseconds |
| `per_call_us` | float | Per-call latency in microseconds |
| `calls_per_sec` | float | Throughput (calls per second) |

### `pi.ext.legacy_bench.v1`

Same structure as `pi.ext.rust_bench.v1` with:
- `runtime` = `"legacy_pi_mono"`
- `node` object: `{version, platform, arch}`

### `pi.perf.workload.v1`

| Field | Type | Description |
|---|---|---|
| `scenario` | string | Workload scenario name |
| `iterations` | integer | Number of outer iterations |
| `tool_calls_per_iteration` | integer | Tool calls per iteration |
| `total_calls` | integer | Total tool calls executed |
| `elapsed_ms` | number | Total elapsed milliseconds |
| `per_call_us` | number | Per-call latency in microseconds |
| `calls_per_sec` | number | Throughput (calls per second) |

### `pi.perf.budget_summary.v2`

Each `budgets` entry requires `name`, `category`, `metric`, `unit`, `threshold`, `comparison`, `ci_enforced`, and `methodology`. `comparison` is the exact enum `maximum` (`actual <= threshold`) or `minimum` (`actual >= threshold`); consumers must never infer direction from a budget name. Blanket performance claims are authorized only when strict, source-bound, same-run evidence gives every declared budget data and PASS status with zero data-contract failures; aggregate `budget_data_missing` and `budget_failed` blockers prevent non-CI results from escaping that rule. Incomplete lineage produces a canonical all-`NO_DATA` blocked sentinel without inspecting ambient artifacts, target paths, or mtimes. Inventory SHA-256 uses compact JSON in producer declaration order and the listed field order, with every threshold rendered using exactly six decimal places. The canonical v0.2.0 digest is `96e3147ef23e1c634d56265581975a2b619ac9a701f4839ef6f3f4b3987226ad`.

### `pi.bench.protocol.v1`

| Field | Type | Description |
|---|---|---|
| `schema` | string | Always `"pi.bench.protocol.v1"` |
| `version` | string | Protocol version used by all benchmark harnesses |
| `partition_tags` | string[] | Must include `matched-state` and `realistic` |
| `realistic_session_sizes` | integer[] | Canonical matrix: 100k, 200k, 500k, 1M, 5M |
| `matched_state_scenarios` | object[] | `cold_start`, `warm_start`, `tool_call`, `event_dispatch` with replay inputs |
| `required_metadata_fields` | string[] | `runtime`, `build_profile`, `host`, `scenario_id`, `correlation_id` |
| `evidence_labels` | object | `evidence_class` (`measured/inferred`) + `confidence` (`high/medium/low`) |
| `regression_gate_admission` | object | Generic additive metadata policy only: measured, high-confidence wall-clock evidence at an enumerated production boundary with a positive sample count, exact cache-policy tokens, a clean source commit, and a verified canonical perf executable/config fingerprint |
| `pijs_regression_gate_admission` | object | PiJS-specific release gate layered on the generic policy: exactly 2000 path-verified perf-profile QuickJS iterations through the production extension manager, with 1-call mean-latency and 10-call throughput lanes |
| `partition_weighting` | object | Machine-readable partition weights (`realistic` + `matched-state`) with explicit sum-to-one contract |
| `partition_interpretation` | object | Primary/secondary partition roles and release guardrail forbidding single-partition conclusions |
| `user_perceived_sli_catalog` | object[] | Versioned user-facing SLI targets with UX interpretation guidance |
| `scenario_sli_matrix` | object[] | Canonical mapping from benchmark scenarios to user-perceived SLIs and consuming validation beads |

## User-Perceived SLI Catalog

| SLI ID | Unit | Target | UX Guidance |
|---|---|---|---|
| `interactive_turn_p95_ms` | `ms` | `<= 1200` | Feels responsive for normal coding dialogue. |
| `resume_session_p95_ms` | `ms` | `<= 1800` | Project/session restore feels immediate after launch. |
| `extension_dispatch_p95_ms` | `ms` | `<= 350` | Extension-backed actions feel near-instant. |
| `tool_roundtrip_p95_ms` | `ms` | `<= 900` | Tool invocation and result handoff feel tight. |
| `tail_stability_p99_over_p50_ratio` | `ratio` | `<= 4.0` | Latency is predictable with low surprise spikes. |

## Protocol Matrix

| Partition | Scenario ID | Replay Input | SLI IDs | UX Outcome |
|---|---|---|---|---|
| `matched-state` | `cold_start` | `{"extension_fixture_set":["hello","pirate","diff"],"runs":5}` | `interactive_turn_p95_ms, resume_session_p95_ms, tail_stability_p99_over_p50_ratio` | First interaction after startup feels responsive. |
| `matched-state` | `warm_start` | `{"extension_fixture_set":["hello","pirate","diff"],"runs":5}` | `interactive_turn_p95_ms, tail_stability_p99_over_p50_ratio` | Steady-state turn latency remains consistently snappy. |
| `matched-state` | `tool_call` | `{"extension_fixture_set":["hello","pirate","diff"],"iterations":500}` | `tool_roundtrip_p95_ms, interactive_turn_p95_ms, tail_stability_p99_over_p50_ratio` | Tool-assisted coding loop stays fluid. |
| `matched-state` | `event_dispatch` | `{"event_name":"before_agent_start","iterations":500}` | `extension_dispatch_p95_ms, tail_stability_p99_over_p50_ratio` | Extension events execute without perceptible stalls. |
| `realistic` | `realistic/session_100000` | `{"mode":"replay","seed":7,"transcript_fixture":"tests/artifacts/perf/session_100000.jsonl"}` | `interactive_turn_p95_ms, resume_session_p95_ms, tail_stability_p99_over_p50_ratio` | Large-session operations remain usable for humans under realistic transcript load. |
| `realistic` | `realistic/session_200000` | `{"mode":"replay","seed":7,"transcript_fixture":"tests/artifacts/perf/session_200000.jsonl"}` | `interactive_turn_p95_ms, resume_session_p95_ms, tail_stability_p99_over_p50_ratio` | Large-session operations remain usable for humans under realistic transcript load. |
| `realistic` | `realistic/session_500000` | `{"mode":"replay","seed":7,"transcript_fixture":"tests/artifacts/perf/session_500000.jsonl"}` | `interactive_turn_p95_ms, resume_session_p95_ms, tail_stability_p99_over_p50_ratio` | Large-session operations remain usable for humans under realistic transcript load. |
| `realistic` | `realistic/session_1000000` | `{"mode":"replay","seed":7,"transcript_fixture":"tests/artifacts/perf/session_1000000.jsonl"}` | `interactive_turn_p95_ms, resume_session_p95_ms, tail_stability_p99_over_p50_ratio` | Large-session operations remain usable for humans under realistic transcript load. |
| `realistic` | `realistic/session_5000000` | `{"mode":"replay","seed":7,"transcript_fixture":"tests/artifacts/perf/session_5000000.jsonl"}` | `interactive_turn_p95_ms, resume_session_p95_ms, tail_stability_p99_over_p50_ratio` | Large-session operations remain usable for humans under realistic transcript load. |

## Determinism Requirements

1. **Stable key ordering**: JSON keys are sorted alphabetically within each record
2. **No floating point in keys**: Use string or integer identifiers
3. **Timestamps**: canonical RFC 3339 UTC; release-facing v2 summaries and PiJS records use millisecond precision (`2026-02-06T01:00:00.000Z`)
4. **Config hash**: SHA-256 of concatenated env fields for dedup
5. **One record per line**: Standard JSONL (newline-delimited JSON)
