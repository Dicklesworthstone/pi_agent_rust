# Security Operator Handbook

> SEC-7.3 (bd-2kle2) -- Reference guide for operators managing Pi's extension security system.

---

## Table of Contents

1. [System Overview](#system-overview)
2. [Policy Configuration](#policy-configuration)
3. [Runtime Risk Controller](#runtime-risk-controller)
4. [Exec Mediation](#exec-mediation)
5. [Secret Broker](#secret-broker)
6. [Quota Management](#quota-management)
7. [Alert Triage](#alert-triage)
8. [Incident Handling](#incident-handling)
9. [Postmortem Workflow](#postmortem-workflow)
10. [Operator Commands Reference](#operator-commands-reference)
11. [Related Documents](#related-documents)

---

## System Overview

Pi's extension security system provides defense-in-depth through five interposition layers:

| Layer | Component | Purpose |
|-------|-----------|---------|
| 1 | Compatibility Scanner | Blocks malicious code at install/load time |
| 2 | Capability Policy Engine | Gates each hostcall against a 5-layer precedence chain |
| 3 | Exec Mediation | Filters shell commands after `exec` capability is granted |
| 4 | Secret Broker | Redacts environment variable values matching secret patterns |
| 5 | Runtime Risk Controller | Bayesian anomaly detection with graduated enforcement |

Every decision is recorded in a hash-chained ledger for tamper-evident audit.

### Trust Boundaries

```
Extension source  ──[B1: scanner]──>  QuickJS sandbox
                                        │
                                    [B2: hostcall ABI]
                                        │
                                    Policy engine
                                        │
                                    [B3: capability gate]
                                        │
                                    Risk controller
                                        │
                                    [B4: enforcement]
                                        │
                                    Connectors (fs/shell/env/http)
                                        │
                                    [B5: asset access]
                                        │
                                    Audit ledger ──[B6: integrity]──> Evidence bundle
```

---

## Policy Configuration

### Profiles

| Profile | Mode | Dangerous Caps | Use Case |
|---------|------|----------------|----------|
| **Safe** | Strict | Denied (`exec`, `env`) | Default. Production workloads. |
| **Standard** | Prompt | Denied (require explicit opt-in) | Development with user approval. |
| **Permissive** | Permissive | Allowed | Trusted extensions only. |

### Setting the Policy

**CLI flag (highest priority):**
```bash
pi --extension-policy safe
pi --extension-policy standard
pi --extension-policy permissive
```

**Environment variable:**
```bash
export PI_EXTENSION_POLICY=safe
```

**Config file** (`~/.config/pi/settings.json`):
```json
{
  "extensionPolicy": {
    "profile": "safe",
    "allowDangerous": false
  }
}
```

**Resolution order:** CLI > env var > config file > default (`safe`).

Unknown profile names fail closed to `safe` (invariant INV-006).

### Inspecting Effective Policy

```bash
pi --explain-extension-policy
```

Outputs the resolved policy with per-capability decisions, showing which layer in the precedence chain determined each decision.

### Per-Extension Overrides

Config file supports per-extension rules:

```json
{
  "extensionPolicy": {
    "profile": "safe",
    "perExtension": {
      "trusted-logging-ext": {
        "allow": ["read", "write"],
        "deny": [],
        "mode": null
      }
    }
  }
}
```

**Precedence chain (5 layers):**

1. Per-extension deny (highest) -- `extension_deny`
2. Global deny_caps -- `deny_caps`
3. Per-extension allow -- `extension_allow`
4. Global default_caps -- `default_caps`
5. Mode fallback (lowest) -- Strict=Deny, Prompt=Prompt, Permissive=Allow

Layer 2 always overrides layer 3. An extension cannot escalate to dangerous capabilities via per-extension allow when global deny_caps includes them.

### Dangerous Capability Opt-In

Dangerous capabilities (`exec`, `env`) are denied by default in Safe and Standard profiles. To allow them:

1. Set `allowDangerous: true` in config
2. This removes `exec`/`env` from `deny_caps`
3. An audit entry (`DangerousOptInAuditEntry`) is recorded

This should only be done for trusted, audited extensions.

---

## Runtime Risk Controller

### Configuration

```json
{
  "extensionRisk": {
    "enabled": true,
    "enforce": true,
    "alpha": 0.01,
    "windowSize": 128,
    "ledgerLimit": 2048,
    "decisionTimeoutMs": 50,
    "failClosed": true
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Master switch for the risk controller |
| `enforce` | `true` | `true` = block risky calls; `false` = shadow mode (score only) |
| `alpha` | `0.01` | Type-I error budget (false positive rate target) |
| `windowSize` | `128` | Sliding window for drift detection |
| `ledgerLimit` | `2048` | Max in-memory ledger entries |
| `decisionTimeoutMs` | `50` | Budget per risk decision; exceeded = fail-closed |
| `failClosed` | `true` | Deny on controller errors |

### Enforcement Actions

| Action | Trigger | Effect |
|--------|---------|--------|
| **Allow** | Risk below threshold | Call proceeds normally |
| **Harden** | Moderate risk signal | Extra logging, quota enforcement |
| **Deny** | High risk | Call blocked, alert raised |
| **Terminate** | Critical risk | Extension killed, quarantined |

### Shadow Mode

Set `enforce: false` to run the risk controller in observation mode. All scoring and ledger recording occurs, but no calls are blocked. Use this to:

- Calibrate thresholds before enforcement
- Validate false-positive rates in production
- Establish baseline risk profiles for new extensions

### Tuning Alpha

`alpha` controls the false-positive tolerance of the sequential detector:

| alpha | Meaning | When to Use |
|-------|---------|-------------|
| 0.001 | Very conservative (low FP, higher FN) | Production with high-value extensions |
| 0.01 | Default balance | General use |
| 0.05 | Aggressive detection (higher FP, low FN) | Development/testing |

---

## Exec Mediation

Exec mediation is a second-stage filter that classifies shell commands after the
`exec` capability is granted. It blocks known dangerous command patterns.

### Dangerous Command Classes

| Class | Risk Tier | Examples |
|-------|-----------|----------|
| `RecursiveDelete` | Critical | `rm -rf /` |
| `DeviceWrite` | Critical | `dd`, `mkfs` |
| `ForkBomb` | Critical | `:(){ :\|:& };:` |
| `ReverseShell` | Critical | `bash -i >& /dev/tcp/...` |
| `CredentialExfil` | Critical | `cat ~/.ssh/id_rsa \| curl` |
| `PipeToShell` | High | `curl \| sh` |
| `SystemShutdown` | High | `reboot`, `shutdown` |
| `DataExfil` | High | `tar czf - / \| nc` |

### Configuration

```json
{
  "extensionPolicy": {
    "execMediation": {
      "enabled": true,
      "denyThreshold": "high",
      "denyPatterns": ["rm -rf /"],
      "allowPatterns": ["rm -rf ./node_modules"],
      "auditAllClassified": true
    }
  }
}
```

Decisions are recorded in `ExecMediationLedgerEntry` with `command_hash`
(never raw command), `command_class`, `risk_tier`, and `decision`.

See the [Maintenance Playbook](maintenance-playbook.md) for tuning procedures.

---

## Secret Broker

The secret broker redacts environment variable values that match secret
patterns, preventing credential exposure through extensions.

### Default Patterns

- **Suffixes**: `_KEY`, `_SECRET`, `_TOKEN`, `_PASSWORD`
- **Prefixes**: `SECRET_`, `AUTH_`, `PRIVATE_`
- **Exact**: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `AWS_SECRET_ACCESS_KEY`

### Configuration

```json
{
  "extensionPolicy": {
    "secretBroker": {
      "enabled": true,
      "redactionPlaceholder": "[REDACTED]",
      "disclosureAllowlist": ["HOME", "PATH", "TERM"]
    }
  }
}
```

Decisions are recorded in `SecretBrokerLedgerEntry` with `name_hash`
(never raw name), `redacted` flag, and `reason`.

See the [Maintenance Playbook](maintenance-playbook.md) for pattern management.

---

## Quota Management

Extension resource quotas prevent resource exhaustion. Quotas are configured
per-extension in the policy.

### Parameters

| Quota | Description |
|-------|-------------|
| `max_hostcalls_per_second` | Rate limit per second |
| `max_hostcalls_per_minute` | Rate limit per minute |
| `max_hostcalls_total` | Lifetime call limit |
| `max_subprocesses` | Concurrent subprocess limit |
| `max_write_bytes` | Total write bytes limit |
| `max_http_requests` | Total HTTP request limit |

### Configuration

```json
{
  "extensionPolicy": {
    "perExtension": {
      "untrusted-ext": {
        "quota": {
          "maxHostcallsPerSecond": 10,
          "maxHostcallsPerMinute": 100
        }
      }
    }
  }
}
```

When a quota is exceeded, a `QuotaBreachEvent` is recorded and a security
alert with category `QuotaBreach` is raised.

---

## Alert Triage

### Alert Severity Levels

| Severity (enum) | Meaning | Response SLA |
|----------|---------|-------------|
| `Critical` | Extension quarantined/terminated | Immediate (minutes) |
| `Error` | Action was blocked (Deny) | Same session |
| `Warning` | User should review (Harden/Prompt) | Next operator check |
| `Info` | Informational, no action blocked | Batch review |

### Alert Categories

| Category (enum) | Source |
|----------|--------|
| `PolicyDenial` | Capability denied by static policy |
| `AnomalyDenial` | Denied by runtime risk scorer |
| `ExecMediation` | Shell command blocked by exec filter |
| `SecretBroker` | Secret detected/redacted by secret broker |
| `QuotaBreach` | Resource quota exceeded |
| `Quarantine` | Extension quarantined/terminated |
| `ProfileTransition` | Policy profile transition attempt |

### Triage Procedure

1. **Check alert severity and category.** Critical/High alerts require immediate investigation.

2. **Identify the extension.**
   ```bash
   pi --explain-extension-policy  # View current policy state
   ```

3. **Query related alerts.**
   The security alert system supports filtering by extension ID, category, severity, and time range.

4. **Examine the risk ledger.** Check the runtime risk ledger for the extension's recent activity pattern:
   - Look for risk score trends (increasing = concerning)
   - Check posterior probabilities (Suspicious/Unsafe values)
   - Verify ledger chain integrity

5. **Determine response:**
   - **False positive:** Document and adjust alpha/thresholds
   - **True positive, low impact:** Restrict extension capabilities
   - **True positive, high impact:** Quarantine extension, escalate to incident

### Quick Alert Queries

Export the current security state:
```bash
# View resolved policy with explanations
pi --explain-extension-policy

# Run security tests to verify invariants
cargo test --test security_budgets -- --nocapture
cargo test --test security_alert_integration -- --nocapture
```

---

## Incident Handling

### Incident Classification

| Class | Description | Example |
|-------|-------------|---------|
| **SEC-INC-1** | Unauthorized capability use | Extension accessed `exec` despite Safe policy |
| **SEC-INC-2** | Secret exposure | API key leaked through extension output |
| **SEC-INC-3** | Ledger tampering | Hash chain verification failed |
| **SEC-INC-4** | Scanner bypass | Malicious code loaded despite compatibility scan |
| **SEC-INC-5** | Quarantine escape | Quarantined extension resumed without promotion |

### Incident Response Steps

**1. Contain**
- Kill the offending extension session
- Verify containment: no other extensions affected (isolation check)

**2. Collect evidence**
- Export the incident evidence bundle (see [Incident Response Runbook](incident-response-runbook.md))
- The bundle includes: risk ledger, security alerts, exec mediation log, secret broker log, hostcall telemetry, quota breaches

**3. Verify integrity**
- Run `verify_incident_evidence_bundle()` on the exported bundle
- Check `bundle_hash` for tamper detection
- Verify ledger chain hashes

**4. Analyze**
- Replay the risk ledger to reconstruct the decision timeline
- Identify the root cause: policy misconfiguration, scanner gap, or genuine attack
- Cross-reference with the threat model (T1-T8 in `threat-model.md`)

**5. Remediate**
- Update policy/scanner rules as needed
- File a bead for any security gap discovered
- Link remediation to the relevant invariant (INV-001 through INV-012)

**6. Postmortem**
- See [Postmortem Workflow](#postmortem-workflow)

---

## Postmortem Workflow

### Template

Every security incident requires a structured postmortem:

```markdown
## Incident ID: SEC-INC-YYYY-NNN

### Summary
One-line description of what happened.

### Timeline
| Time | Event |
|------|-------|
| T+0  | Alert triggered |
| T+1m | Operator acknowledged |
| T+5m | Extension isolated |
| T+15m | Evidence bundle exported |
| T+30m | Root cause identified |

### Root Cause
What failed and why. Reference specific controls:
- Policy evaluation: which layer, which decision
- Risk controller: score, posterior, action taken
- Scanner: what was missed and why

### Impact
- Users affected: N
- Data exposed: Y/N (details)
- Duration of exposure: Xm

### Remediation
- Immediate: [actions taken]
- Short-term: [bead IDs for fixes]
- Long-term: [systemic improvements]

### Verification
- [ ] Evidence bundle exported and hash-verified
- [ ] Ledger chain verified intact
- [ ] Remediation tested in shadow mode
- [ ] Regression test added (test name, file)
- [ ] SLO impact assessed (which SLOs affected)

### Artifacts
- Evidence bundle: `<path>`
- Dashboard: `<path>`
- Related beads: `<IDs>`
```

### Postmortem Checklist

1. Write the postmortem within 24 hours of incident resolution
2. Review with at least one other operator
3. File remediation beads with priority matching incident severity
4. Add regression tests covering the incident scenario
5. Update threat model if a new threat was discovered
6. Update SLOs if thresholds need adjustment
7. Archive the postmortem and evidence bundle

---

## Operator Commands Reference

### Policy Inspection

| Command | Purpose |
|---------|---------|
| `pi --explain-extension-policy` | Show resolved policy with per-capability decisions |
| `pi --extension-policy safe` | Override policy for this session |

### Testing and Verification

| Command | Purpose |
|---------|---------|
| `cargo test --test security_conformance_benign -- --nocapture` | Benign extension compatibility under hardened policy |
| `cargo test --test security_budgets -- --nocapture` | Resource quota enforcement |
| `cargo test --test security_fs_escape -- --nocapture` | Filesystem escape prevention |
| `cargo test --test security_http_policy -- --nocapture` | HTTP request policy enforcement |
| `cargo test --test security_alert_integration -- --nocapture` | Security alert system |
| `cargo test --test policy_profile_hardening -- --nocapture` | Policy profile hardening |
| `cargo test --test exec_mediation_integration -- --nocapture` | Command-level mediation |
| `cargo test --test install_time_security_scanner -- --nocapture` | Manifest scanner |

### CI Gates

| Gate | Blocking | Artifact |
|------|----------|----------|
| `sec_conformance` — SEC-6.4 security compatibility conformance | YES | `tests/full_suite_gate/sec_conformance_verdict.json` |
| Conformance regression | YES | `tests/ext_conformance/reports/regression_verdict.json` |
| Extension must-pass (authoritative inclusion list) | YES | `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json` |
| Non-mock compliance | YES | `docs/non-mock-rubric.json` |
| Suite classification | YES | `tests/suite_classification.toml` |
| Waiver lifecycle | YES | `tests/full_suite_gate/waiver_audit.json` |

### Evidence Artifacts

| Artifact | Schema | Purpose |
|----------|--------|---------|
| `security_compat_dashboard.json` | `pi.security.compat_dashboard.v1` | Compatibility pass rates under hardened policy |
| `full_suite_verdict.json` | `pi.ci.full_suite_gate.v1` | Aggregate CI gate verdict |
| `conformance_summary.json` | `pi.ext.conformance_summary.v2` | Extension conformance pass rates |
| `waiver_audit.json` | `pi.ci.waiver_audit.v1` | Gate waiver lifecycle status |

---

## Related Documents

| Document | Purpose |
|----------|---------|
| [Threat Model](threat-model.md) | Trust boundaries and threat taxonomy (T1-T8) |
| [Security Invariants](invariants.md) | Non-negotiable invariants (INV-001 to INV-012) |
| [Security SLOs](security-slos.md) | 14 measurable security objectives |
| [Baseline Audit](baseline-audit.md) | Audit findings and security gaps (G-1 to G-7) |
| [Incident Response Runbook](incident-response-runbook.md) | Step-by-step incident response procedures |
| [Maintenance Playbook](maintenance-playbook.md) | Scanner rules, recalibration, rollout controls |
| [CI Operator Runbook](../ci-operator-runbook.md) | CI failure triage and replay |
| [QA Runbook](../qa-runbook.md) | Testing procedures and suite classification |
