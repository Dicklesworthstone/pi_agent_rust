# Performance Benchmarks

> **Purpose:** Track and validate performance budgets for pi_agent_rust.

## Quick Start

```bash
# Run all benchmarks
cargo bench

# Run specific benchmark
cargo bench "truncation"
cargo bench "sse_parsing"

# Run with baseline comparison
cargo bench -- --save-baseline main
cargo bench -- --baseline main
```

## Performance Budgets

These are the target performance metrics. Regressions beyond these thresholds should be investigated.

| Benchmark | Budget | Current | Status |
|-----------|--------|---------|--------|
| truncate_head (10K lines) | <1ms | ~250μs | ✅ |
| truncate_tail (10K lines) | <1ms | ~250μs | ✅ |
| sse_parse (100 events) | <100μs | ~50μs | ✅ |
| ext_runtime_cold_start (no-op extension) | p95 < 200ms (p99 < 400ms) | TBD | ⬜ |
| ext_runtime_warm_start (no-op extension) | p95 < 25ms (p99 < 50ms) | TBD | ⬜ |
| ext_tool_hook_overhead (no-op extension) | p95 < 500μs (p99 < 1ms) | TBD | ⬜ |
| ext_hostcall_dispatch (single call) | p95 < 50μs (p99 < 100μs) | TBD | ⬜ |
| Binary startup | <100ms | 11.2ms (`pi --version`) | ✅ |
| Binary size (release) | <20MB | 7.6MB | ✅ |

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
└── tools.rs          # Core operation benchmarks
    ├── truncation    # Text truncation (head/tail)
    └── sse_parsing   # SSE event parsing
```

Planned:
```
benches/
└── extensions.rs     # Connector dispatch + event-loop overhead
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
on: [push, pull_request]

jobs:
  benchmark:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
      - name: Run benchmarks
        run: cargo bench -- --noplot
      # Compare against baseline in future
```

## Profiling Tips

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
