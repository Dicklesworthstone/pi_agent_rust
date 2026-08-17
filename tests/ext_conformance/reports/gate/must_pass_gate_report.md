# Must-Pass Extension CI Gate Report

> Generated: 2026-08-17T21:08:06Z
> Run ID: local-20260817T210741621Z
> Correlation ID: must-pass-gate-local-20260817T210741621Z
> Git commit: `d2752c4f671b0df8d3124ecfb727e5398923ff3f`
> Source tree SHA-256: `288d0607a5b6555241b2f532f3bc426861ae980795a6acfa1535a285e829fec2`
> Inclusion-list SHA-256: `8801e2f1193105778cd0f88a0892c4699f2fbf1ff42fc87590b8e7fa88f51fe4`
> Manifest SHA-256: `ee22b7fe9635c708881c29f5b70cec4998be6bda4056e367e33ea858a9a4b1bd`
> Mode: strict

## Gate Verdict

**Status: FAIL**

| Check | Actual | Threshold | Result |
|-------|--------|-----------|--------|
| Pass rate | 99.0% | >=100.0% | FAIL |
| Failure count | 2 | <=0 | FAIL |
| Complete coverage | 208/208 | 208/208 | PASS |

## Canonical Must-Pass Set (Tier-1 + Tier-1 Review)

| Metric | Value |
|--------|-------|
| Total | 208 |
| Tested | 208 |
| Passed | 206 |
| Failed | 2 |
| Skipped | 0 |
| Pass rate | 99.0% |

## Blocking Failures

### npm/marckrenn-pi-sub-bar

- **Tier:** 3
- **Reason:** Extension 'npm/marckrenn-pi-sub-bar': event-handler identities differ from the manifest; expected={"model_select", "session_branch", "session_shutdown", "session_start", "session_switch", "sub-core:action", "sub-core:ready", "sub-core:request", "sub-core:settings:patch", "sub-core:settings:updated", "sub-core:update-all", "sub-core:update-current", "tool_result", "turn_end", "turn_start"}, actual={"model_select", "session_shutdown", "session_start", "sub-core:ready", "sub-core:settings:updated", "sub-core:update-all", "sub-core:update-current"}
- **Category:** Unknown
- **Duration:** 106ms
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_npm_marckrenn_pi_sub_bar --nocapture --exact
  ```

### third-party/marckrenn-pi-sub

- **Tier:** 3
- **Reason:** Extension 'third-party/marckrenn-pi-sub': event-handler identities differ from the manifest; expected={"model_select", "session_branch", "session_shutdown", "session_start", "session_switch", "sub-core:action", "sub-core:ready", "sub-core:request", "sub-core:settings:patch", "sub-core:settings:updated", "sub-core:update-all", "sub-core:update-current", "tool_result", "turn_end", "turn_start"}, actual={"model_select", "session_shutdown", "session_start", "sub-core:ready", "sub-core:settings:updated", "sub-core:update-all", "sub-core:update-current"}
- **Category:** Unknown
- **Duration:** 102ms
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_third_party_marckrenn_pi_sub --nocapture --exact
  ```

## Manifest Entries Outside the Canonical Set — Non-Blocking

| Metric | Value |
|--------|-------|
| Total | 19 |
| Tested | 19 |
| Passed | 10 |
| Failed | 9 |
| Skipped | 0 |

