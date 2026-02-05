# Performance Benchmarks

> **Purpose:** Track and validate performance budgets for pi_agent_rust.

## Quick Start

```bash
# Run all benchmarks
cargo bench

# Run specific benchmark
cargo bench "truncation"
cargo bench "sse_parsing"
cargo bench "ext_policy"
cargo bench "ext_js_runtime"

# Run with baseline comparison
cargo bench -- --save-baseline main
cargo bench -- --baseline main
```

## Performance Budgets

These are the target performance metrics. Regressions beyond these thresholds should be investigated.

### Core Metrics (Hard Budgets)

| Benchmark | Budget | Current | Status |
|-----------|--------|---------|--------|
| **startup/version** | <100ms (p95) | ~11ms | ✅ |
| **startup/help** | <150ms (p95) | ~15ms | ✅ |
| **startup/list_models** | <200ms (p95) | ~25ms | ✅ |
| **binary/size_mb** | <20MB | ~7.6MB | ✅ |
| **memory/version_peak** | <50MB RSS | TBD | ⬜ |

### Micro-Benchmarks

| Benchmark | Budget | Current | Status |
|-----------|--------|---------|--------|
| truncate_head (10K lines) | <1ms | ~250μs | ✅ |
| truncate_tail (10K lines) | <1ms | ~250μs | ✅ |
| sse_parse (100 events) | <100μs | ~50μs | ✅ |
| ext_policy/evaluate | <1μs | ~20ns | ✅ |
| ext_dispatch/decision | <10μs | ~100ns | ✅ |
| ext_protocol/parse | <100μs | ~5μs | ✅ |
| ext_js_runtime/cold_start | <200ms | ~315μs | ✅ |
| ext_js_runtime/warm_eval_noop | <25ms | ~3.48μs | ✅ |
| ext_js_runtime/tool_call_roundtrip | <500μs | TBD | ⬜ |

### Extension Runtime (Planned)

| Benchmark | Budget | Current | Status |
|-----------|--------|---------|--------|
| ext_runtime_cold_start (no-op extension) | p95 < 200ms (p99 < 400ms) | TBD | ⬜ |
| ext_runtime_warm_start (no-op extension) | p95 < 25ms (p99 < 50ms) | TBD | ⬜ |
| ext_tool_hook_overhead (no-op extension) | p95 < 500μs (p99 < 1ms) | TBD | ⬜ |
| ext_hostcall_dispatch (single call) | p95 < 50μs (p99 < 100μs) | TBD | ⬜ |

### Extension Runtime Budget Definitions

These budgets target **extension overhead**, not end-to-end LLM latency.

- **Cold start:** first time an extension runtime is created/initialized for a process (cold caches).
- **Warm start:** extension runtime is already initialized (warm caches); measures steady-state overhead.
- **Hook overhead:** incremental latency added by routing a tool call through a no-op extension hook.
- **Hostcall dispatch:** cost to invoke a single hostcall across the connector boundary (no-op payload).

### Measurement Methodology (bd-1ii)

- **Hardware class:** GitHub Actions `ubuntu-latest` runner (x86_64). Treat numbers as *CI budgets*; local machines will vary.
- **Percentiles:** budgets are specified as **p95/p99** to avoid overfitting to median-only results on shared CI runners.
- **Benchmarks:** extension benchmarks will live under `benches/extensions.rs` (planned) and should report:
  - cold vs warm timings separately
  - a baseline (no extension) vs no-op extension delta for hook overhead
  - enough samples to make percentile reporting meaningful on CI

## Benchmark Results

### Truncation Performance

Processing throughput for text truncation operations:

```
truncation/head/1000    time:   [32 µs]     thrpt:  [2.3 GiB/s]
truncation/head/10000   time:   [251 µs]    thrpt:  [3.0 GiB/s]
truncation/head/100000  time:   [2.3 ms]    thrpt:  [3.3 GiB/s]

truncation/tail/1000    time:   [~32 µs]    thrpt:  [~2.3 GiB/s]
truncation/tail/10000   time:   [~251 µs]   thrpt:  [~3.0 GiB/s]
truncation/tail/100000  time:   [~2.3 ms]   thrpt:  [~3.3 GiB/s]
```

**Key observations:**
- Throughput is consistent at 2.3-3.3 GiB/s regardless of input size
- Head and tail truncation have similar performance
- Well within the 1ms budget for typical file sizes (10K lines)

### SSE Parsing Performance

Server-Sent Events parsing throughput:

```
sse_parsing/parse/100   time:   [50.129 µs 50.315 µs 50.504 µs]
                         thrpt:  [1.9800 Melem/s 1.9875 Melem/s 1.9949 Melem/s]

sse_parsing/parse/1000  time:   [495.54 µs 495.96 µs 496.40 µs]
                         thrpt:  [2.0145 Melem/s 2.0163 Melem/s 2.0180 Melem/s]
```

## Benchmark Structure

```
benches/
├── tools.rs          # Core operation benchmarks
│   ├── truncation    # Text truncation (head/tail)
│   └── sse_parsing   # SSE event parsing
├── extensions.rs     # Connector dispatch + policy / protocol parsing
│   ├── ext_policy
│   ├── ext_required_capability
│   ├── ext_dispatch
│   └── ext_protocol
│   └── ext_js_runtime     # QuickJS cold/warm start + no-op eval
└── system.rs         # System-level benchmarks (process spawn)
    ├── startup       # Startup time (version, help, list_models)
    ├── memory        # RSS memory measurement
    └── binary        # Binary size tracking
```

## Adding New Benchmarks

1. Add benchmark function to `benches/tools.rs`:

```rust
fn bench_new_operation(c: &mut Criterion) {
    let mut group = c.benchmark_group("new_operation");

    // Test with different input sizes
    for size in [100, 1000, 10000] {
        let input = generate_input(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(
            BenchmarkId::new("name", size),
            &input,
            |b, input| {
                b.iter(|| pi::module::function(black_box(input)));
            },
        );
    }

    group.finish();
}

// Add to criterion_group!
criterion_group!(benches, ..., bench_new_operation);
```

2. Add performance budget to this document
3. Run benchmark: `cargo bench new_operation`

## CI Integration

Performance regression detection in GitHub Actions:

```yaml
# .github/workflows/bench.yml
name: Benchmarks
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always

jobs:
  benchmark:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly

      - name: Cache cargo registry
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-bench-${{ hashFiles('**/Cargo.lock') }}

      - name: Build release binary (for system benchmarks)
        run: cargo build --release

      - name: Run benchmarks
        run: |
          # Run all benchmarks, save results
          cargo bench -- --noplot --save-baseline current

          # Check binary size budget
          SIZE_MB=$(stat --printf="%s" target/release/pi | awk '{printf "%.2f", $1/1024/1024}')
          echo "Binary size: ${SIZE_MB}MB"
          if (( $(echo "$SIZE_MB > 20" | bc -l) )); then
            echo "::error::Binary size ${SIZE_MB}MB exceeds 20MB budget"
            exit 1
          fi

      - name: Upload benchmark results
        uses: actions/upload-artifact@v4
        with:
          name: benchmark-results
          path: target/criterion/
          retention-days: 30
```

### Regression Detection (Manual)

Compare against a known good baseline:

```bash
# Save baseline on main branch
cargo bench -- --save-baseline main

# After changes, compare
cargo bench -- --baseline main

# Look for regressions > 10%
```

### Variance Handling

System benchmarks spawn real processes, so variance is higher than micro-benchmarks:

- **Micro-benchmarks** (tools.rs, extensions.rs): Use criterion defaults (100+ samples)
- **System benchmarks** (system.rs): Use 20 samples, 10s measurement time
- **CI runners**: Expect 2-3x variance vs local machines; focus on relative changes
- **Percentiles**: Report p95/p99 for budgets, not just mean

## Profiling Tips

### bd-1pb: Profile-Driven Optimization Loop

This workstream uses a strict **baseline → profile → prove → implement → verify** loop.

#### 1) Baseline

- Use Criterion for stable micro-bench artifacts: `cargo bench --bench extensions -- ext_js_runtime`.
- Use `hyperfine` for end-to-end CLI paths (if installed):

```bash
hyperfine --warmup 3 --runs 10 'target/release/pi --version'
```

- Use the PiJS workload harness for deterministic extension roundtrips:

```bash
scripts/bench_extension_workloads.sh
```

#### 2) Profile

- CPU hotspots: `cargo flamegraph --bench extensions` (requires `cargo install flamegraph`).
- Allocations: `heaptrack cargo bench --bench extensions` (Linux).

#### 3) Prove (No “silent regressions”)

- Keep outputs reproducible: record environment (`[bench-env] ... config_hash=...` emitted by `benches/extensions.rs`).
- Store benchmark artifacts in `target/criterion/` (Criterion JSON + reports).
- Use `--save-baseline` / `--baseline` comparisons for regression detection.

#### 4) Opportunity Matrix (Template)

| Opportunity | Evidence | Expected impact | Confidence | Effort | Score | Notes |
|-------------|----------|-----------------|------------|--------|-------|-------|
| (fill) | (bench/flamegraph link) | (ms/μs) | 1–5 | 1–5 | impact×confidence/effort | (guardrails) |

### CPU Profiling with perf

```bash
# Record profile
cargo bench -- --profile-time 10
perf record -g target/release/deps/tools-*

# Analyze
perf report
```

### Memory Profiling with heaptrack

```bash
heaptrack cargo bench
heaptrack_gui heaptrack.tools.*.gz
```

### Flame Graphs

```bash
cargo install flamegraph
cargo flamegraph --bench tools
```

## Comparison with TypeScript

Target metrics for Rust vs TypeScript:

| Operation | TypeScript | Rust Target | Rust Actual |
|-----------|------------|-------------|-------------|
| Startup | ~200ms | <100ms | 11.2ms ✅ |
| 10K line truncate | ~10ms | <1ms | 250μs ✅ |
| 100 SSE events | ~5ms | <100μs | 50.3μs ✅ |
| Binary size | N/A (Node) | <20MB | 7.6MB ✅ |
| Memory (idle) | ~80MB | <50MB | TBD |

## Notes

- Benchmarks run in release mode with LTO enabled
- Times measured on standard CI hardware (GitHub Actions)
- Throughput measured in GiB/s or elements/sec
- Use `--save-baseline` and `--baseline` for regression detection
