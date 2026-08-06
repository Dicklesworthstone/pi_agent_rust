# Security Maintenance Playbook

> SEC-7.3 (bd-2kle2) -- Procedures for ongoing security system maintenance, calibration, and operations.

---

## Table of Contents

1. [Scanner Rule Maintenance](#scanner-rule-maintenance)
2. [Risk Controller Calibration](#risk-controller-calibration)
3. [Policy Profile Updates](#policy-profile-updates)
4. [Secret Broker Maintenance](#secret-broker-maintenance)
5. [Exec Mediation Maintenance](#exec-mediation-maintenance)
6. [CI Security Gate Maintenance](#ci-security-gate-maintenance)
7. [Waiver Lifecycle Management](#waiver-lifecycle-management)
8. [Extension Conformance Monitoring](#extension-conformance-monitoring)
9. [Troubleshooting Guide](#troubleshooting-guide)

---

## Scanner Rule Maintenance

### Adding a Forbidden Pattern

The compatibility scanner detects dangerous imports and code patterns at extension load time.

**When to add a pattern:**
- A new evasion technique was discovered (see INC-4 in [Incident Response Runbook](incident-response-runbook.md))
- A new dangerous API surface was identified
- The scanner's detection rate (SLO-02) drops below 95%

**Steps:**

1. Identify the pattern to add (e.g., `require('node:child_process')`)
2. Add the rule to the appropriate compatibility-scanner classifier in
   `src/extensions/compatibility.rs` (`classify_import`,
   `scan_flagged_apis_in_line`, or `scan_forbidden_patterns_in_line`)
3. Add a regression test in `tests/install_time_security_scanner.rs`
4. Verify:
   ```bash
   cargo test --test install_time_security_scanner -- --nocapture
   ```
5. Check for false positives against the extension corpus:
   ```bash
   cargo test --test ext_conformance_generated --features ext-conformance -- --nocapture
   ```

### Reviewing Scanner Effectiveness

**Periodic review (monthly):**

1. Run the full conformance suite to measure current detection rates
2. Compare against SLO-02 (>= 95% detection) and SLO-03 (<= 5% false positives)
3. Review any scanner-bypass incidents in the past period
4. Update patterns if new evasion techniques are documented

---

## Risk Controller Calibration

### When to Recalibrate

- After deploying new extensions with different call patterns
- When false-positive rate (SLO-10) exceeds 10%
- When false-negative rate (SLO-11) exceeds 5%
- After significant changes to the hostcall dispatch pipeline

### Calibration Procedure

**1. Baseline in shadow mode:**
```json
{
  "extensionRisk": {
    "enabled": true,
    "enforce": false
  }
}
```

Run for a representative workload period (at least 100 hostcall decisions).

**2. Analyze results:**
```bash
cargo test --test accuracy_performance_sec63 -- --nocapture
cargo test --test runtime_risk_quantile_validation -- --nocapture
```

Check:
- False positive rate from benign traces
- False negative rate from adversarial traces
- Latency distribution (SLO-06: p99 <= 5ms)

**3. Tune parameters:**

| Symptom | Adjustment |
|---------|------------|
| Too many false positives | Decrease `alpha` (e.g., 0.01 -> 0.005) |
| Missing real threats | Increase `alpha` (e.g., 0.01 -> 0.02) |
| Slow decisions | Decrease `windowSize` or increase `decisionTimeoutMs` |
| Memory pressure from ledger | Decrease `ledgerLimit` |

**4. Enable enforcement:**
```json
{
  "extensionRisk": {
    "enabled": true,
    "enforce": true
  }
}
```

**5. Verify:**
```bash
cargo test --test ledger_calibration_sec35 -- --nocapture
cargo test --test baseline_modeling_evidence -- --nocapture
```

### Golden Fixture Validation

The risk scorer has golden fixtures that validate scoring determinism:
```bash
cargo test --test risk_scorer_golden_fixtures -- --nocapture
```

After calibration changes, update golden fixtures if the scoring algorithm changed.

---

## Policy Profile Updates

### Adding a New Default Capability

If a new non-dangerous capability is introduced (e.g., `analytics`):

1. Add to `PolicyProfile::Safe.to_policy().default_caps`
2. Add to `PolicyProfile::Standard` default policy
3. Update `Capability` enum if needed
4. Update the compatibility test matrix:
   ```bash
   cargo test --test security_conformance_benign -- --nocapture
   ```
5. Update `BENIGN_CAPABILITIES` in the test if the capability should be tested
6. Verify the compatibility dashboard shows the new capability

### Adding a New Dangerous Capability

If a new capability should be classified as dangerous:

1. Add to `Capability::is_dangerous()` check
2. Add to `Capability::dangerous_list()`
3. Add to `deny_caps` in Safe and Standard profiles
4. Update tests:
   ```bash
   cargo test --test policy_profile_hardening -- --nocapture
   cargo test --test capability_denial_matrix -- --nocapture
   ```
5. Verify invariant INV-008 (dangerous caps default-deny)

### Modifying the Precedence Chain

The 5-layer precedence chain is an invariant (INV-001). Modifications require:

1. Review against the threat model (T3: capability escalation)
2. Update `evaluate_for()` in `ExtensionPolicy`
3. Update all precedence tests:
   ```bash
   cargo test --test capability_policy_model -- --nocapture
   cargo test --test capability_policy_scoped -- --nocapture
   ```
4. Update the operator handbook documentation

---

## Secret Broker Maintenance

### Adding a Secret Pattern

**Exact name** (highest priority):
Add to the `secret_exact` list in `SecretBrokerPolicy::default()`.

**Suffix pattern** (catches `*_API_KEY`, `*_SECRET`, etc.):
Add to `secret_suffixes`.

**Prefix pattern** (catches `AWS_SECRET_*`, etc.):
Add to `secret_prefixes`.

**Verification:**
```bash
cargo test --test security_budgets -- secret_broker --nocapture
```

### Allowlisting a Variable

If a variable matches a secret pattern but should be disclosed:
Add to `disclosure_allowlist` in the policy config.

Document why the variable is safe to expose.

---

## Exec Mediation Maintenance

### Adding a Deny Pattern

The exec mediation layer filters shell commands after the `exec` capability is granted.

1. Add the pattern to `ExecMediationPolicy.deny_patterns`
2. Set the appropriate `deny_threshold` (Low/Medium/High/Critical)
3. Verify:
   ```bash
   cargo test --test exec_mediation_integration -- --nocapture
   ```

### Adding an Allow Pattern

For known-safe commands that might match deny patterns:
Add to `allow_patterns`. Allow patterns are checked before deny patterns.

---

## CI Security Gate Maintenance

### Gate Overview

The Linux full-suite CI gate (`ci_full_suite_gate.rs`) includes 20 sub-gates,
14 of them blocking. Security-relevant gates:

| Gate ID | Name | Blocking | Artifact |
|---------|------|----------|----------|
| `sec_conformance` | SEC-6.4 security compatibility conformance | YES | `tests/full_suite_gate/sec_conformance_verdict.json` |
| `conformance_regression` | Conformance regression | YES | `tests/ext_conformance/reports/regression_verdict.json` |
| `ext_must_pass` | Extension must-pass (authoritative inclusion list) | YES | `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json` |
| `non_mock_unit` | Non-mock compliance | YES | `docs/non-mock-rubric.json` |
| `waiver_lifecycle` | Waiver lifecycle | YES | `tests/full_suite_gate/waiver_audit.json` |

### When a Gate Fails

1. **Read the gate detail.** Each gate produces a detail message explaining the failure.
2. **Run the reproduction command.** Each gate includes a `reproduce_command`.
3. **Check for regressions.** Compare against the last known-good state.
4. **Fix or waive.** Either fix the underlying issue or create a time-bounded waiver.

### Updating Gate Thresholds

Gate thresholds are configured in `ci.yml`:

```yaml
CI_GATE_MIN_PASS_RATE_PCT: "80.0"  # Minimum conformance pass rate
CI_GATE_MAX_FAIL_COUNT: "36"       # Maximum failures
CI_GATE_MAX_NA_COUNT: "170"        # Maximum N/A count
```

These values govern the broader conformance promotion gate. They do not weaken
the release-blocking extension must-pass corpus selected by
`docs/extension-inclusion-list.json`, which requires exact coverage and passing
results for every included entry. To adjust a promotion threshold, update the
GitHub variable and document the justification.

---

## Waiver Lifecycle Management

### Creating a Waiver

Waivers provide time-bounded CI gate bypass. Add to `tests/suite_classification.toml`:

```toml
[waiver.sec_conformance]
owner = "YourName"
created = "2026-02-14"
expires = "2026-02-28"
bead = "bd-XXXX"
reason = "Scanner update pending for new evasion pattern"
scope = "full"
remove_when = "Scanner update deployed and all compatibility tests pass"
```

Reproduce this aggregate gate with
`cargo test --test sec_compatibility_conformance -- --nocapture`. The
`tests/security_compat/security_compat_dashboard.json` artifact described below
is additional monitoring evidence, not the aggregate gate's waiver ID or
verdict artifact.

**Required fields:** owner, created, expires, bead, reason, scope, remove_when.

**Constraints:**
- Maximum duration: 30 days
- Valid scopes: `full`, `preflight`, `both`
- Must link to a bead tracking the fix
- Expired waivers cause CI failure

### Monitoring Waivers

```bash
cargo test --test ci_full_suite_gate -- waiver_lifecycle_audit --nocapture --exact
```

This validates all waivers and produces `tests/full_suite_gate/waiver_audit.json` with:
- Active/expired/expiring-soon/invalid counts
- Per-waiver validation details
- Days remaining for active waivers

### Renewing a Waiver

Update `expires` and document why more time is needed. Maximum duration from `created` remains 30 days. If more time is needed, you must set a new `created` date and justify the extension.

---

## Extension Conformance Monitoring

### Daily Monitoring

The compatibility dashboard tracks benign extension behavior under hardened policy:

```bash
cargo test --test security_conformance_benign -- generate_compat_dashboard_artifact --nocapture --exact
```

Produces `tests/security_compat/security_compat_dashboard.json` with:
- Per-profile pass rates (Safe, Standard)
- Individual check results (24 compatibility checks)
- Regression detection flag

### Regression Response

If the dashboard shows `regression_detected: true`:

1. Check which specific checks failed (see `checks` array)
2. Determine if a policy change or code change caused the regression
3. Fix the regression or document it as intentional (with waiver if needed)
4. Re-run to confirm the dashboard shows `regression_detected: false`

### Extension Corpus Updates

When new extensions are added to the conformance corpus:

1. Update and validate `docs/extension-inclusion-list.json`.
2. Run the full conformance suite.
3. Require every entry in the authoritative must-pass selection to be exercised
   and pass; the broader-corpus 80% SLO is not a substitute for this release gate.
4. Update the conformance baseline if the new extensions are expected to pass.
5. File beads for any new failures that need investigation.

---

## Troubleshooting Guide

### Problem: Extension Fails to Load

**Check:**
1. Is the extension in the compatibility scanner's blocklist?
   - Scanner may flag dangerous patterns
   - Check scanner results for the extension
2. Is the policy too restrictive?
   - Run `pi --explain-extension-policy` to see effective policy
3. Is the QuickJS runtime healthy?
   - Check for module resolution errors in extension logs

### Problem: False Positive Alerts

**Check:**
1. Is the risk controller alpha too aggressive?
   - Default `alpha: 0.01` may be too sensitive for some workloads
   - Try `alpha: 0.005` in shadow mode first
2. Is the extension's behavior pattern unusual but benign?
   - Add to the risk controller's baseline if confirmed benign
3. Is the secret broker matching non-secret variables?
   - Add to `disclosure_allowlist`

### Problem: CI Gate Fails After Dependency Update

**Check:**
1. Did the dependency change any security-relevant behavior?
2. Run the specific failing gate's reproduction command
3. Update conformance baselines if the change is expected
4. File a waiver if the fix requires time

### Problem: Risk Controller Latency Exceeds SLO

**Target:** SLO-06 requires p99 <= 5ms.

**Actions:**
1. Reduce `windowSize` (fewer entries to evaluate per decision)
2. Increase `decisionTimeoutMs` only as last resort (allows more time but increases latency)
3. Check for I/O contention in the ledger write path
4. Profile with:
   ```bash
   cargo bench --bench system -- risk_decision
   ```

### Problem: Ledger Growing Too Large

**Default:** `ledgerLimit: 2048` entries in memory.

**Actions:**
1. Reduce `ledgerLimit` to a smaller value
2. Export and archive evidence bundles periodically
3. The oldest entries are automatically evicted when the limit is reached
4. Chain integrity is preserved even after eviction (entries reference hashes, not indices)
