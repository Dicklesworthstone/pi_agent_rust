# SEC Workstream Unit-Test Traceability Matrix

Version: 1.0.0 | Counts generated: 2026-02-14 | Locator ownership refreshed: 2026-08-06 | Bead: bd-2jkio (SEC-6.5)

## Overview

This matrix maps every SEC implementation bead to its concrete unit and integration test targets.
A machine-readable version is available at `docs/sec_traceability_matrix.json`.

**Grand total: 1,155 SEC-related tests** across 18 beads and 29 integration test files. The certified counts remain the 2026-02-14 baseline; source-level test owners in the machine-readable matrix were refreshed after the extension-runtime decomposition.

---

## WS2: Supply-Chain and Provenance

| Bead | Title | Unit | Integration | Total | Categories |
|------|-------|------|-------------|-------|------------|
| SEC-2.1 (bd-f0huc) | Extension manifest v2 | 12 | 23 | 35 | success, failure, edge-case |
| SEC-2.2 (bd-3br2a) | Lockfile + provenance | 8 | 0 | 8 | success, failure |
| SEC-2.3 (bd-21vng) | Install-time scanner | 15 | 49 | 64 | success, failure, edge-case, determinism |
| SEC-2.4 (bd-21nj4) | Quarantine-to-trust | 10 | 37 | 47 | success, failure, edge-case |

**Integration test files:**
- `tests/ext_preflight_analyzer.rs` (7 tests) - SEC-2.1
- `tests/e2e_workflow_preflight.rs` (16 tests) - SEC-2.1
- `tests/install_time_security_scanner.rs` (49 tests) - SEC-2.3
- `tests/extension_trust_promotion.rs` (37 tests) - SEC-2.4

---

## WS3: Runtime Anomaly Detection

| Bead | Title | Unit | Integration | Total | Categories |
|------|-------|------|-------------|-------|------------|
| SEC-3.1 (bd-2a9ll) | Hostcall telemetry | 20 | 1 | 21 | success, edge-case |
| SEC-3.2 (bd-153pv) | Baseline modeling | 15 | 14 | 29 | success, edge-case, determinism |
| SEC-3.3 (bd-3f1ab) | Risk scorer | 30 | 17 | 47 | success, failure, edge-case, determinism |
| SEC-3.4 (bd-3tb30) | Enforcement state machine | 29 | 16 | 45 | success, anti-flapping, determinism |
| SEC-3.5 (bd-3i9da) | Hash-chained ledger | 25 | 15 | 40 | success, determinism, tamper-detection |

**Integration test files:**
- `tests/e2e_runtime_risk_telemetry.rs` (1 test) - SEC-3.1
- `tests/runtime_risk_quantile_validation.rs` (8 tests) - SEC-3.2
- `tests/runtime_risk_quantile_evidence.rs` (6 tests) - SEC-3.2
- `tests/risk_scorer_golden_fixtures.rs` (7 tests) - SEC-3.3
- `tests/accuracy_performance_sec63.rs` (10 tests) - SEC-3.3/6.3
- `tests/enforcement_state_machine_sec34.rs` (16 tests) - SEC-3.4
- `tests/ledger_calibration_sec35.rs` (15 tests) - SEC-3.5

### SEC-3.4 Assertion Checklist

- [x] State ordering (`Allow < Harden < Prompt < Deny < Terminate`)
- [x] Display impl roundtrip
- [x] `EnforcementState` <-> `RuntimeRiskAction` mapping
- [x] Per-profile score band classification (safe/balanced/permissive)
- [x] Immediate escalation on high scores
- [x] Multi-level escalation jumps
- [x] Hysteresis prevents immediate de-escalation
- [x] Cooldown required before de-escalation
- [x] One-level-at-a-time de-escalation
- [x] Terminate is terminal
- [x] Cooldown resets on escalation
- [x] No flapping under borderline scores (10-evaluation jitter test)
- [x] Serde roundtrip for all types
- [x] Deterministic sequence reproduction
- [x] Profile comparison (safe vs permissive)

---

## WS4: Capability Policy and Mediation

| Bead | Title | Unit | Integration | Total | Categories |
|------|-------|------|-------------|-------|------------|
| SEC-4.1 (bd-b1d7o) | Resource quotas | 20 | 19 | 39 | success, failure, edge-case |
| SEC-4.2 (bd-wzzp4) | FS/network allowlists | 25 | 169 | 194 | success, failure, path-traversal |
| SEC-4.3 (bd-zh0hj) | Exec + secret mediation | 20 | 68 | 88 | success, failure, edge-case |
| SEC-4.4 (bd-2vbax) | Policy profile hardening | 23 | 36 | 59 | success, edge-case, audit |

**Integration test files:**
- `tests/security_budgets.rs` (19 tests) - SEC-4.1
- `tests/security_fs_escape.rs` (40 tests) - SEC-4.2
- `tests/security_http_policy.rs` (28 tests) - SEC-4.2
- `tests/capability_policy_scoped.rs` (75 tests) - SEC-4.2
- `tests/capability_denial_matrix.rs` (26 tests) - SEC-4.2
- `tests/exec_mediation_integration.rs` (68 tests) - SEC-4.3
- `tests/policy_profile_hardening.rs` (36 tests) - SEC-4.4

### SEC-4.4 Assertion Checklist

- [x] `explain_effective_policy()` returns correct decisions per profile
- [x] Dangerous capabilities blocked by default in safe/standard
- [x] Dangerous capabilities enabled only in permissive profile
- [x] `DangerousOptInAuditEntry` records unblocked capabilities
- [x] `is_valid_downgrade()` correctly identifies strictness changes
- [x] Config integration: audit trail populated on `allow_dangerous`
- [x] Policy explanation serializes to JSON

---

## WS5: Security UX, Alerts, and Incident Response

| Bead | Title | Unit | Integration | Total | Categories |
|------|-------|------|-------------|-------|------------|
| SEC-5.1 (bd-qudx1) | Security alerts | 21 | 30 | 51 | success, filtering, serde |
| SEC-5.2 (bd-ww5br) | Kill-switch + trust | 27 | 13 | 40 | success, failure, lifecycle, audit |
| SEC-5.3 (bd-11mqo) | Incident evidence bundle | 5 | 43 | 48 | success, determinism, redaction |

**Integration test files:**
- `tests/security_alert_integration.rs` (30 tests) - SEC-5.1
- `tests/trust_onboarding_killswitch_sec52.rs` (13 tests) - SEC-5.2
- `tests/incident_evidence_bundle.rs` (30 tests) - SEC-5.3
- `tests/incident_evidence_bundle_sec53.rs` (13 tests) - SEC-5.3

### SEC-5.1 Assertion Checklist

- [x] Factory methods for 6 alert sources (policy denial, exec mediation, secret redaction, anomaly, quarantine, enforcement transition)
- [x] `SecurityAlertAction::from_enforcement()` roundtrip
- [x] `SecurityAlertAction::as_str()` for all variants
- [x] Alert serialization/deserialization roundtrip
- [x] Filter by category, severity, extension, timestamp
- [x] Category and severity count aggregation
- [x] `emit_security_alert()` records + emits tracing
- [x] `sha256_short()` determinism

### SEC-5.2 Assertion Checklist

- [x] `kill_switch()` sets trust state to Killed
- [x] `kill_switch()` quarantines in runtime risk controller
- [x] `kill_switch()` emits Critical security alert
- [x] `kill_switch()` records audit entry with provenance
- [x] `kill_switch()` is idempotent (no-op when already killed)
- [x] `kill_switch()` works from Acknowledged and Trusted states
- [x] `lift_kill_switch()` restores to Acknowledged
- [x] `lift_kill_switch()` clears quarantine + consecutive_unsafe counter
- [x] `lift_kill_switch()` emits Info-level alert
- [x] `lift_kill_switch()` records deactivation audit entry
- [x] `lift_kill_switch()` fails if not currently killed
- [x] `is_killed()` returns correct state through transitions
- [x] Default trust state is Pending for unknown extensions
- [x] Trust onboarding accept -> Acknowledged
- [x] Trust onboarding reject -> Killed + quarantine
- [x] Trust onboarding records decision with risk level
- [x] `promote_trust()` from Acknowledged -> Trusted
- [x] `promote_trust()` no-op from Pending or Killed
- [x] Full lifecycle: Pending -> Acknowledged -> Trusted -> Killed -> Acknowledged -> Trusted
- [x] Kill-switch audit preserves operator provenance
- [x] Multiple extensions have independent trust states
- [x] `ExtensionTrustState` Display impl
- [x] Alert sequence IDs are monotonically increasing
- [x] Kill -> lift -> kill again cycle works correctly

---

## WS6: Validation and Determinism Testing

| Bead | Title | Unit | Integration | Total | Categories |
|------|-------|------|-------------|-------|------------|
| SEC-6.4 (bd-1a2cu) | Compatibility conformance + CI gates | 0 | 31 | 31 | success, conformance, regression |

**Integration test files:**
- `tests/sec_compatibility_conformance.rs` (31 tests) - SEC-6.4

### SEC-6.4 Assertion Checklist

- [x] Benign capabilities (read/write/http/events/session) allowed in all profiles
- [x] Dangerous capabilities (exec/env) denied in safe/standard, allowed in permissive
- [x] Per-extension override cannot bypass deny_caps
- [x] Per-extension deny overrides default allow
- [x] Policy explanation covers all capabilities with reasons
- [x] Profile transition validation (downgrade/upgrade detection)
- [x] Compatibility scanner: benign extensions pass, dangerous flagged
- [x] Trust lifecycle: Pending → Acknowledged → Trusted → Killed
- [x] Kill-switch emits security alert and audit entry
- [x] Lift kill-switch emits additional alert
- [x] Onboarding accept/reject records decision
- [x] Waiver format and duration validation
- [x] Cross-profile consistency matrix
- [x] Serde roundtrip for all profiles
- [x] CI gate artifact (`sec_conformance_verdict.json`) generated with 95% threshold

---

## WS7: Rollout and Operations

| Bead | Title | Unit | Integration | Total | Categories |
|------|-------|------|-------------|-------|------------|
| SEC-7.1 (bd-2teqs) | Shadow mode | 3 | 0 | 3 | success, failure |

---

## Cross-Cutting Test Files

These files cover multiple SEC beads:

| File | Tests | Covers |
|------|-------|--------|
| `tests/capability_policy_model.rs` | 34 | SEC-4.2, SEC-4.4 |
| `tests/capability_prompt.rs` | 46 | SEC-4.2, SEC-4.3 |
| `tests/extensions_policy_negative.rs` | 38 | SEC-4.2, SEC-4.3, SEC-4.4 |
| `tests/e2e_high_risk_workflows.rs` | 23 | SEC-3.3, SEC-3.4, SEC-5.1 |
| `tests/qa_docs_policy_validation.rs` | 61 | SEC-4.2, SEC-4.3, SEC-4.4 |

---

## Maintenance Protocol

1. **When adding a new SEC bead**: Add an entry to both `sec_traceability_matrix.json` and this file.
2. **When adding/removing tests**: Update the test count and assertion checklist.
3. **When changing behavior**: Verify that the corresponding test assertions still match.
4. **Naming convention**: Integration test files should include the SEC ID where possible (e.g., `_sec34.rs`, `_sec52.rs`).
5. **Golden fixtures**: Owned by SEC-3.3 (`risk_scorer_golden_fixtures.rs`). Changes require re-validation.
