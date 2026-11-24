# Must-Pass Extension CI Gate Report

> Generated: 2026-11-24T13:54:22Z
> Run ID: local-20261124T135356906Z
> Correlation ID: must-pass-gate-local-20261124T135356906Z
> Git commit: `f44c9187cdd5b1f0f49229f57a9f25740f1c1434`
> Source tree SHA-256: `59961e3343ff6481f1e7625ef8b65587490fed91dbd971c5951c96ce158717a1`
> Inclusion-list SHA-256: `8801e2f1193105778cd0f88a0892c4699f2fbf1ff42fc87590b8e7fa88f51fe4`
> Manifest SHA-256: `833ff01d20c5096075850249f1bc2dc1810f3b57b6690c51c19a722f81b5bac6`
> Mode: strict

## Gate Verdict

**Status: FAIL**

| Check | Actual | Threshold | Result |
|-------|--------|-----------|--------|
| Pass rate | 99.5% | >=100.0% | FAIL |
| Failure count | 1 | <=0 | FAIL |
| Complete coverage | 208/208 | 208/208 | PASS |

## Canonical Must-Pass Set (Tier-1 + Tier-1 Review)

| Metric | Value |
|--------|-------|
| Total | 208 |
| Tested | 208 |
| Passed | 207 |
| Failed | 1 |
| Skipped | 0 |
| Pass rate | 99.5% |

## Blocking Failures

### community/nicobailon-subagents

- **Tier:** 3
- **Reason:** Load error: Extension error: ERR_FEATURE_UNAVAILABLE: fs.watch native notifications are unavailable; use fs.watchFile for stat polling
    at watch (node:fs:1759:21)
    at registerSubagentExtension (/data/projects/pi_agent_rust/tests/ext_conformance/artifacts/community/nicobailon-subagents/index.ts:96:30)
    at <anonymous> (__pi_bridge_prelude__:752:20)

- **Category:** ExtensionLoadError
- **Duration:** 73ms
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_community_nicobailon_subagents --nocapture --exact
  ```

## Manifest Entries Outside the Canonical Set — Non-Blocking

| Metric | Value |
|--------|-------|
| Total | 19 |
| Tested | 19 |
| Passed | 17 |
| Failed | 2 |
| Skipped | 0 |

