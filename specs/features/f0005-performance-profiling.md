# f-0005: Performance Profiling & Optimization

> Epic: [Rust Investment Roadmap](../epics/rust-investment-roadmap.md) — Item 5
> Priority: Medium | Effort: 1 week

---

## Problem

The project defines performance targets (startup <100ms, binary <20MB) in
`PLAN_TO_PORT_PI_TO_RUST.md` but has no automated measurement or regression
detection. Performance is the primary user-facing advantage of a native
binary — it must be measured to be managed.

## Current State

- `src/perf_build.rs` — build-time instrumentation exists
- `src/interactive/perf.rs` — runtime perf monitoring exists
- `tests/perf/` — performance test directory exists
- `benches/` — benchmark directory exists with session_save, tools, tui_perf
- `.github/workflows/bench.yml` — benchmark CI exists
- No published baseline numbers

## Features

### 5a. Startup Time Profiling

**Measurements needed:**
- Cold start to first prompt (no session)
- Cold start with session resume (1k message session)
- Cold start with extensions loaded (5 extensions)

**Implementation:**
- Add `--timing` flag to CLI that prints phase timings:
  ```
  config:      3ms
  auth:        8ms
  providers:   12ms
  extensions:  45ms
  session:     22ms
  tui_init:    15ms
  total:       105ms
  ```
- Instrument each init phase with `std::time::Instant` markers
- Add CI check: fail if cold start exceeds 200ms on GitHub Actions runner

### 5b. Memory Profiling for Long Sessions

**Problem areas to investigate:**
- Session entry accumulation (JSONL entries kept in memory)
- Extension state growth
- Provider connection pooling

**Implementation:**
- Add RSS tracking to `src/interactive/perf.rs` (partially exists)
- Implement memory budget: warn at 500MB, trigger compaction at 1GB
- Memory-mapped JSONL for session read-back (mmap instead of `read_to_string`)
- Profile with `jemalloc` stats (tikv-jemallocator already a dependency)

### 5c. Binary Size Budget

**Current binary size:** (needs measurement)

**Implementation:**
- Add CI step to `release.yml`: measure stripped binary size per target
- Track sizes in release notes
- Fail CI if stripped Linux musl binary exceeds 25MB
- Investigate size contributors: `cargo bloat --release --crates`
- Key suspects: wasmtime (~5MB), image crate, swc_ecma_* crates

### 5d. Benchmark Baselines

**What exists:** `benches/` directory with benchmarks, `bench.yml` CI.

**What to build:**
- Establish baseline numbers for key operations:
  - Tool execution latency (bash, read, edit, grep)
  - Session save/load for 1k, 10k, 100k entries
  - Provider SSE parse throughput
  - Extension hostcall round-trip time
- Store baselines in `benches/baselines/` as JSON
- CI comparison: warn on >10% regression, fail on >25%

## Acceptance Criteria

- `pi --timing` prints phase-by-phase startup breakdown
- Cold start <100ms measured on clean config (no extensions)
- Binary size tracked in CI with 25MB ceiling
- Benchmark baselines established for top-5 hot paths
- RSS stays under 500MB during a 10k-message session
