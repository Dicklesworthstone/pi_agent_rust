# Flake Triage and Rerun Policy

## Purpose

Even deterministic test suites encounter environmental failures (resource
exhaustion, filesystem contention, timeout jitter). This policy defines how
the conformance CI classifies, retries, and triages such failures without
masking true regressions.

## Failure Classification

Every test failure is classified into one of three buckets:

| Bucket | Criteria | Action |
|--------|----------|--------|
| **Deterministic** | Reproduces locally on clean checkout | Fix immediately; block merge |
| **Transient** | Does not reproduce on retry; matches a known flake pattern | Auto-retry once; log for tracking |
| **Environmental** | Infrastructure-level (OOM, disk full, network timeout) | Retry with backoff; alert on repeat |

## Known Flake Patterns

The following patterns are recognized as transient and qualify for
automatic retry:

| Pattern | Regex | Category |
|---------|-------|----------|
| TS oracle timeout | `oracle.*timed?\s*out\|bun.*timed?\s*out` | `oracle_timeout` |
| Resource exhaustion | `out of memory\|ENOMEM\|Cannot allocate` | `resource_exhaustion` |
| Filesystem contention | `EBUSY\|ETXTBSY\|resource busy` | `fs_contention` |
| Port conflict | `EADDRINUSE\|address already in use` | `port_conflict` |
| Temp dir cleanup race | `No such file or directory.*tmp` | `tmpdir_race` |
| QuickJS GC pressure | `out of memory.*quickjs\|allocation failed` | `js_gc_pressure` |

## Retry Policy

### CI Retry Rules

1. **Max retries**: 1 automatic retry per test target per run.
2. **Retry scope**: Only the failed test target is retried, not the
   entire matrix row.
3. **Retry delay**: 5 seconds between attempts (avoids resource
   contention from immediate retry).
4. **Failure on second attempt**: Reported as a true failure.

### Local Retry (`scripts/e2e/run_all.sh`)

Use `--rerun-from <summary.json>` to re-execute only failed suites from
a previous run. This uses the same classification logic.

## Flake Budget

- **Per-target flake budget**: Each test target is allowed a maximum of
  3 flake occurrences per rolling 30-day window before requiring
  investigation.
- **Global flake budget**: Total flake count across all targets must
  stay below 5% of total test executions.
- **Budget exceeded**: When a target exceeds its flake budget, it is
  escalated from "transient" to "deterministic" and must be fixed or
  documented as a known limitation.

## Triage Workflow

1. **CI detects failure** in conformance job.
2. **Classifier** checks failure output against known flake patterns.
3. If match: retry once, log `flake_event` to JSONL.
4. If no match or retry also fails: mark as **deterministic failure**.
5. **Weekly review**: Aggregate flake events. If any target exceeds
   budget, create a bead for investigation.

## Evidence Artifacts

Every conformance run produces:

| Artifact | Format | Content |
|----------|--------|---------|
| `conformance-*.log` | Text | Full test output |
| `flake_events.jsonl` | JSONL | Classified flake events |
| `conformance_summary.json` | JSON | Pass/fail/skip counts |
| `retry_manifest.json` | JSON | Which targets were retried and outcome |

## Integration with Quality Pipeline

The `scripts/ext_quality_pipeline.sh` script does **not** perform
automatic retries. It reports failures as-is for deterministic local
feedback. The retry logic lives in CI only (`.github/workflows/conformance.yml`).

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `PI_CONFORMANCE_MAX_RETRIES` | `1` | Max automatic retries per target |
| `PI_CONFORMANCE_RETRY_DELAY` | `5` | Seconds between retry attempts |
| `PI_CONFORMANCE_FLAKE_BUDGET` | `3` | Per-target 30-day flake budget |
| `PI_CONFORMANCE_CLASSIFY_ONLY` | `0` | Set to `1` to classify without retrying |
