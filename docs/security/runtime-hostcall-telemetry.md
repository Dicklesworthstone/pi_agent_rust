# Runtime Hostcall Telemetry and Feature Extraction (SEC-3.1)

This document defines the canonical runtime hostcall telemetry contract and the deterministic feature extraction path used by the runtime risk scorer.

## Scope

The telemetry event captures:
- extension identity (`extension_id`)
- capability and method (`capability`, `method`)
- argument shape hash (`args_shape_hash`) and canonical params hash (`params_hash`)
- resource target class (`resource_target_class`)
- policy decision context (`policy_profile`, `policy_reason`)
- risk score (`risk_score`)
- latency and outcome (`latency_ms`, `outcome`, `outcome_error_code`)
- deterministic sequence context (`sequence`)
- deterministic feature vector (`features`)
- deterministic explanation payload (`explanation_level`, `explanation_summary`, `top_contributors`, `budget_state`)

## Schema

- Artifact schema: `pi.ext.hostcall_telemetry.v1`
- Feature schema: `pi.ext.hostcall_feature_vector.v1`
- JSON Schema: `docs/schema/runtime_hostcall_telemetry.json`

The Rust artifact export is `ExtensionManager::runtime_hostcall_telemetry_artifact()`.

## Sequence Context Semantics

Each event includes a pre-call sequence snapshot:
- `sequence_id`: monotonic per-extension sequence number (starting at 1)
- `previous_*`: previous call identity tuple
- `burst_count_1s` / `burst_count_10s`: call volume in recent windows
- `recent_error_count` / `recent_window_count`: short-horizon outcome history
- `prior_failure_streak`: consecutive failures before current call

## Feature Extraction

Feature extraction is deterministic and O(1) per call.

Current vector fields:
- `base_score`
- `recent_mean_score`
- `recent_error_rate`
- `burst_density_1s`
- `burst_density_10s`
- `prior_failure_streak_norm`
- `dangerous_capability`
- `timeout_requested`
- `policy_prompt_bias`

Extraction budget target:
- `RUNTIME_HOSTCALL_FEATURE_BUDGET_US = 250`

Each event records:
- `extraction_latency_us`
- `extraction_budget_us`
- `extraction_budget_exceeded`

## Explanation Payload (SEC-3.3A)

Each event includes deterministic runtime-risk explanation metadata:
- `explanation_level`: one of `compact`, `standard`, `full`
- `explanation_summary`: stable human-readable action summary
- `top_contributors`: contribution terms sorted by descending `magnitude`, tie-broken by `code`
- `budget_state`: strict explanation budget status:
  - `time_budget_ms`
  - `elapsed_ms`
  - `term_budget`
  - `terms_emitted`
  - `exhausted`
  - `fallback_mode`

Budget-exhaustion behavior is fail-closed for explanation generation:
- on exhaustion, emit conservative deterministic summary payload
- include explicit `budget_state.exhausted=true` and `budget_state.fallback_mode=true`
- avoid speculative contributor terms in fallback mode

## Redaction and Safety

No raw hostcall params are emitted in telemetry artifacts.
Only hashes are emitted:
- `params_hash`
- `args_shape_hash`

`redaction_summary` must describe this policy for downstream audit tooling.

## Determinism and Compatibility Guarantees

- Identical traces produce identical feature vectors.
- Telemetry events are version-tagged.
- Deserialization is backward-readable via serde defaults for additive fields.

Coverage is enforced by unit + integration tests in:
- `src/extensions/tests/core.rs` hostcall feature-classification tests
- `src/extensions/tests/reactor.rs` runtime telemetry recording tests
- `src/extensions/tests/shared_dispatch.rs` feature-vector and telemetry-schema tests
- `tests/e2e_runtime_risk_telemetry.rs`
