# f-0008: CI Pipeline Hardening

> Epic: [Rust Investment Roadmap](../epics/rust-investment-roadmap.md) — Item 8
> Priority: Medium | Effort: 2-3 days

---

## Problem

CI workflows exist (`.github/workflows/{ci,release,bench,conformance,fuzz}.yml`)
but are configured for the upstream repo (`Dicklesworthstone/pi_agent_rust`).
The fork (`zoosky/pi_agent_rust`) needs these verified and adapted.

Additionally, with 6,187 unit tests and 250+ integration test files, test
execution must be tiered to keep PR feedback fast while maintaining thorough
validation on merge.

## Current CI Workflows

| Workflow | Trigger | What it does |
|----------|---------|-------------|
| `ci.yml` | PR + push to main | cargo test on linux/mac/windows |
| `release.yml` | tag push | cross-platform binary builds |
| `bench.yml` | ? | benchmark runs |
| `conformance.yml` | ? | extension conformance checks |
| `fuzz.yml` | ? | fuzzing runs |

## Features

### 8a. Fork CI Verification

**Actions:**
- Verify `ci.yml` runs on the fork (check `working-directory: pi_agent_rust`
  assumption — may need adjustment)
- Ensure all required secrets/vars are configured:
  - `CI_GATE_PROMOTION_MODE`
  - `CI_GATE_THRESHOLD_VERSION`
  - `CI_GATE_MIN_PASS_RATE_PCT`
  - `CI_GATE_MAX_FAIL_COUNT`
- Run `ci.yml` manually via `workflow_dispatch` to validate
- Fix any path assumptions that reference the upstream repo structure

### 8b. Tiered Test Execution

**Tier 1: PR checks (< 5 min)**
- `cargo check` — type checking
- `cargo fmt --check` — formatting
- `cargo clippy -- -D warnings` — lint
- `cargo test --lib` — unit tests only (6,187 tests, ~80s)

**Tier 2: Merge to main (< 15 min)**
- Everything in Tier 1
- `cargo test --test '*'` — integration tests
- Binary size check

**Tier 3: Nightly (< 60 min)**
- Everything in Tier 2
- `conformance.yml` — extension conformance
- `bench.yml` — performance benchmarks
- `fuzz.yml` — fuzzing runs
- E2e tests against live providers (with API keys from secrets)

**Implementation:**
- Add `tier` labels to test files via `#[cfg_attr(not(ci_tier2), ignore)]`
- Or simpler: use Cargo test filtering:
  ```yaml
  # Tier 1
  cargo test --lib
  # Tier 2
  cargo test --lib --test 'e2e_*' --test 'rpc_*'
  # Tier 3
  cargo test
  ```

### 8c. Flake Management

`src/flake_classifier.rs` exists for test flakiness detection.

**Implementation:**
- On CI failure: re-run failed tests 2x to classify flake vs real failure
- Log flaky tests to `tests/quarantine_report.json`
- Weekly job: report on flake rate trends
- Auto-quarantine tests that flake >3x in a week

### 8d. CI Dashboard

**What to track:**
- Test pass/fail trend over time
- Binary size trend
- Benchmark results trend
- Time-to-merge (PR open → merge)

**Implementation:**
- GitHub Actions summary with badges in README
- Upload test results as artifacts
- Use `bench.yml` results for size/perf tracking

## Acceptance Criteria

- `ci.yml` passes on the fork for all 3 OS targets
- PR feedback in <5 min (Tier 1 only)
- Merge gate runs full integration suite
- Flaky tests are identified and quarantined automatically
- README badges show CI status, test count, binary size
