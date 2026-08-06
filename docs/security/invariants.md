# Extension Security Invariants and Policy Precedence

Status: Active  
Primary bead: `bd-2ezm9`  
Last updated: 2026-02-14  
Related baseline: `docs/security/threat-model.md`

## 1. Purpose and Scope

This document defines non-negotiable invariants for extension hostcall security in `pi_agent_rust`.
It is normative for:

- capability derivation and hostcall validation
- policy precedence and conflict resolution
- prompt decision resolution
- runtime risk overlay behavior
- explicit fail-open exceptions and their justification

## 2. Canonical Decision Pipeline

The authoritative flow is implemented in:

- `Config::resolve_extension_policy_with_metadata` (`src/config.rs`)
- `validate_host_call` (`src/extensions/protocol.rs`)
- `required_capability_for_host_call_static` (`src/extensions.rs`)
- `ExtensionPolicy::evaluate_for` (`src/extensions.rs`)
- `resolve_shared_policy_prompt` (`src/extensions.rs`)
- `dispatch_host_call_shared` (`src/extensions.rs`)
- `evaluate_runtime_risk` / `record_runtime_risk_outcome` (`src/extensions/extension_manager_impl.rs`)

### 2.1 Stage A: Policy and Risk Settings Resolution

Extension policy profile resolution precedence:

1. CLI override (`--extension-policy`)
2. `PI_EXTENSION_POLICY` env var
3. config `extension_policy.profile`
4. default `safe`

Normalization and fail-closed semantics:

- `balanced` and legacy `standard` map to `PolicyProfile::Standard`.
- Unknown profile values fail closed to `safe`.
- Dangerous capabilities (`exec`, `env`) are denied by default.
- Dangerous caps are only removed from deny list when `allow_dangerous` is explicitly enabled.

Runtime-risk settings resolution precedence:

1. `PI_EXTENSION_RISK_*` env vars
2. config `extension_risk.*`
3. `RuntimeRiskConfig::default()`

### 2.2 Stage B: Hostcall Validation and Capability Canonicalization

`validate_host_call` must pass before any dispatch:

- non-empty `call_id`
- `params` is an object
- non-empty declared `capability`
- non-empty `method`
- required capability is derivable from method + params
- declared capability equals required capability

If any condition fails: return `invalid_request` and do not execute hostcall.

### 2.3 Stage C: Deterministic Static Policy Evaluation Order

`ExtensionPolicy::evaluate_for` resolves conflicts with this fixed chain:

1. per-extension deny (`extension_deny`)
2. global `deny_caps` (`deny_caps`)
3. per-extension allow (`extension_allow`)
4. global `default_caps` (`default_caps`)
5. mode fallback (`strict`/`prompt`/`permissive`)

Effective mode is per-extension override mode if present, else global mode.

### 2.4 Stage D: Prompt Resolution

For `PolicyDecision::Prompt`:

1. check `(extension_id, capability)` cache (`prompt_cache_allow` / `prompt_cache_deny`)
2. if manager is unavailable: deny (`shutdown`)
3. otherwise prompt once; map to `prompt_user_allow` / `prompt_user_deny`
4. cache the decision for future calls

### 2.5 Stage E: Runtime Risk Overlay

`dispatch_host_call_shared` runs runtime risk only after final policy decision is `Allow`.

- policy deny is terminal
- runtime risk can tighten via `harden`, `deny`, `terminate`
- runtime risk never upgrades policy deny into allow

## 3. Precedence Truth Tables

### 3.1 Static Policy Layers

| Case | extension deny | global deny | extension allow | in default caps | effective mode | Decision | Reason |
|---|---|---|---|---|---|---|---|
| P1 | yes | any | any | any | any | Deny | `extension_deny` |
| P2 | no | yes | any | any | any | Deny | `deny_caps` |
| P3 | no | no | yes | any | any | Allow | `extension_allow` |
| P4 | no | no | no | yes | any | Allow | `default_caps` |
| P5 | no | no | no | no | `strict` | Deny | `not_in_default_caps` |
| P6 | no | no | no | no | `prompt` | Prompt | `prompt_required` |
| P7 | no | no | no | no | `permissive` | Allow | `permissive` |

### 3.2 Cross-Layer Conflict Resolution

| Conflict | Winner | Deterministic outcome |
|---|---|---|
| Static `Deny` vs runtime risk | Static policy | deny; runtime risk not evaluated |
| Prompt cache/user deny vs runtime risk | Prompt/static path | deny; runtime risk not evaluated |
| Prompt cache/user allow vs runtime risk deny/terminate | Runtime risk | deny/quarantine |
| Prompt mode with no manager/UI path | Fail closed | deny (`shutdown` or prompt-deny path) |
| Invalid hostcall shape or capability mismatch | Validation | `invalid_request` |
| Runtime-risk harden on dangerous capability | Runtime risk | deny |
| Runtime-risk harden on non-dangerous capability | Runtime risk policy | allow with hardening semantics |

## 4. Non-Negotiable Invariants

| ID | Invariant |
|---|---|
| `INV-001` | Required capability is derived from method/params, not trusted from caller declaration. |
| `INV-002` | Hostcalls with unknown method or declared/derived capability mismatch are rejected as `invalid_request` before execution. |
| `INV-003` | Static policy precedence order is deterministic and layer-stable. |
| `INV-004` | Global deny list cannot be bypassed by per-extension allow. |
| `INV-005` | Extension-scoped deny/allow rules affect only the matching extension identity. |
| `INV-006` | Default profile behavior is fail-closed (`safe`); unknown profiles resolve to `safe`. |
| `INV-007` | Dangerous capabilities (`exec`, `env`) are denied unless explicit operator opt-in (`allow_dangerous`). |
| `INV-008` | Prompt-required decisions fail closed when manager/UI is unavailable. |
| `INV-009` | Runtime risk is an overlay that can only preserve or tighten access, never relax a policy deny. |
| `INV-010` | Runtime risk evidence ledger is tamper-evident and replay-verifiable. |
| `INV-011` | Sensitive env access requires both name filtering and policy approval. |
| `INV-012` | Security event logging must use deterministic hashes/reason codes and avoid raw secret payload leakage. |

## 5. Explicit Fail-Open Exceptions (Justified)

These are intentionally configurable exceptions, not accidental bypasses:

1. `allow_dangerous=true` (config/env) removes `exec`/`env` from deny list by explicit operator choice.
2. `--extension-policy permissive` allows unknown capabilities by profile policy.
3. Runtime risk `enabled=false` disables only the runtime risk overlay and reverts to baseline static policy behavior.
4. Runtime risk timeout with `fail_closed=false` falls back to `Allow` by explicit operator choice.

No implicit fail-open path is permitted by default profile/settings.

## 6. Machine-Checkable Invariant Manifest

Canonical machine-readable artifact: `docs/security/invariants.machine.json`.

Embedded copy:

```json
{
  "schema_version": "1.0",
  "document": "docs/security/invariants.md",
  "issue_id": "bd-2ezm9",
  "invariants": [
    {
      "id": "INV-001",
      "tests": [
        "src/extensions/tests/core.rs::required_capability_for_host_call_maps_tools_and_fs_ops",
        "tests/extensions_policy_negative.rs::hostcall_exec_maps_to_exec_capability",
        "src/extensions/tests/ui_protocol.rs::protocol_adapter_capability_mismatch_returns_invalid_request"
      ]
    },
    {
      "id": "INV-002",
      "tests": [
        "tests/capability_policy_scoped.rs::empty_call_id_returns_invalid_request",
        "tests/capability_policy_scoped.rs::non_object_params_returns_invalid_request",
        "src/extensions/tests/shared_dispatch.rs::shared_dispatch_unsupported_method_returns_invalid_request"
      ]
    },
    {
      "id": "INV-003",
      "tests": [
        "tests/capability_policy_scoped.rs::scoped_deny_wins_over_scoped_allow_for_same_cap",
        "tests/capability_policy_scoped.rs::scoped_allow_cannot_bypass_global_deny_caps",
        "tests/extensions_policy_negative.rs::deny_caps_override_default_caps"
      ]
    },
    {
      "id": "INV-004",
      "tests": [
        "tests/capability_policy_scoped.rs::scoped_allow_cannot_bypass_global_deny_caps",
        "tests/extensions_policy_negative.rs::deny_caps_override_permissive_mode"
      ]
    },
    {
      "id": "INV-005",
      "tests": [
        "src/extensions/tests/shared_dispatch.rs::shared_dispatch_per_extension_deny_does_not_affect_other_extensions",
        "tests/capability_policy_scoped.rs::multiple_extensions_independent_scoping"
      ]
    },
    {
      "id": "INV-006",
      "tests": [
        "src/config.rs::extension_policy_metadata_unknown_profile_falls_back_to_safe",
        "tests/config_edge_cases.rs::extension_policy_unknown_profile_falls_back_to_safe",
        "tests/config_edge_cases.rs::extension_policy_default_permissive_toggle_false_restores_safe"
      ]
    },
    {
      "id": "INV-007",
      "tests": [
        "tests/extensions_policy_negative.rs::deny_caps_exec_denied_in_all_modes",
        "tests/extensions_policy_negative.rs::deny_caps_env_denied_in_all_modes",
        "tests/capability_policy_scoped.rs::allow_dangerous_removes_exec_env_from_deny_caps"
      ]
    },
    {
      "id": "INV-008",
      "tests": [
        "tests/capability_policy_scoped.rs::dispatch_prompt_without_manager_falls_to_deny",
        "tests/capability_policy_scoped.rs::dispatch_prompt_with_manager_but_no_ui_sender",
        "src/extensions/tests/ui_protocol.rs::shared_dispatch_ui_without_manager_returns_denied"
      ]
    },
    {
      "id": "INV-009",
      "tests": [
        "src/extensions/tests/shared_dispatch.rs::shared_dispatch_runtime_risk_disabled_is_isomorphic",
        "src/extensions/tests/shared_dispatch.rs::shared_dispatch_runtime_risk_hardens_exec_calls",
        "src/extensions/tests/shared_dispatch.rs::shared_dispatch_runtime_risk_quarantines_repeated_unsafe_attempts"
      ]
    },
    {
      "id": "INV-010",
      "tests": [
        "src/extensions/tests/shared_dispatch.rs::shared_dispatch_runtime_risk_ledger_is_tamper_evident",
        "src/extensions/tests/shared_dispatch.rs::shared_dispatch_runtime_risk_ledger_replay_reconstructs_decision_path",
        "src/extensions/tests/shared_dispatch.rs::shared_dispatch_runtime_risk_ledger_verifies_after_ring_buffer_truncation"
      ]
    },
    {
      "id": "INV-011",
      "tests": [
        "src/extensions_js.rs::pijs_env_get_honors_allowlist",
        "src/extensions/wasm_host.rs::wasm_host_env_requires_allowlist",
        "src/extensions/wasm_host.rs::wasm_host_env_denied_by_policy_even_when_allowlisted"
      ]
    },
    {
      "id": "INV-012",
      "tests": [
        "src/extensions/tests/core.rs::hostcall_params_hash_is_stable_for_key_ordering",
        "src/extensions/tests/core.rs::hostcall_ledger_start_redacts_params_and_includes_hash",
        "src/extensions/tests/core.rs::js_hostcall_prompt_policy_caches_user_allow_and_never_logs_raw_params"
      ]
    }
  ]
}
```

## 7. Change Notes for `bd-2ezm9`

- This bead defines normative policy/risk invariants and deterministic precedence semantics.
- Runtime behavior is not changed in this documentation step.
- E2E scenario/log contracts remain defined in `docs/security/threat-model.md` section 13.
- Deterministic artifact capture: `sha256sum docs/security/invariants.md`.
