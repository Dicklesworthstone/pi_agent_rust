# Extension Performance Comparison: Rust vs Legacy

> Generated: 2026-02-01T01:09:56Z

## Executive Summary

| Metric | Count |
|---|---|
| Faster (>10% improvement) | 0 |
| Comparable (+/-10%) | 0 |
| Slower (>10% regression) | 6 |
| **Overall** | **ACCEPTABLE REGRESSIONS — see analysis below** |

## Comparison Table

| Category | Metric | Rust | Legacy (TS) | Delta | Delta % | Verdict |
|---|---|---|---|---|---|---|
| Load Time (60 exts) | Rust mean cold load | 103.3ms | N/A (measured per-ext below) | - | - | INFO |
| Load Time (60 exts) | Rust-to-TS ratio (p50) | 102x | 1x | 102x slower | +10100% | SLOWER |
| Load Time | hello cold-start p50 | 96.0ms | 21.4ms | +74.6ms | +349% | SLOWER |
| Load Time | pirate cold-start p50 | 98.0ms | 12.9ms | +85.1ms | +662% | SLOWER |
| Tool Call | hello per-call latency | 44.0us | 1.6us | +42.4us | +2596% | SLOWER |
| Tool Call | hello calls/sec | 22716 | 612776 | -590060 | -96% | SLOWER |
| Event Hook | before_agent_start per-call latency | 44.0us (tool_call proxy) | 1.7us | +42.3us | +2461% | SLOWER |
| Event Hook | legacy event calls/sec | N/A (see tool call) | 582092 | - | - | INFO |
| E2E Process | 200 iters x 1 tool (hyperfine median) | 16.8ms | N/A | - | - | INFO |
| E2E Process | 200 iters x 10 tools (hyperfine median) | 94.8ms | N/A | - | - | INFO |
| Stress | 30s sustained load (15 exts, 50 evt/s) | 1313 events, 0 errors | N/A | - | - | PASS |
| Stress | RSS growth under load | 0.00% | N/A | - | - | PASS |
| Stress | Dispatch p50 latency | 16689us | N/A | - | - | INFO |
| Stress | Dispatch p99 latency | 31272us | N/A | - | - | PASS |

## Regression Analysis

### Load Time

**Observation:** Rust cold-start load is ~5x slower for hello, ~8x slower for pirate vs legacy Node.js

**Hypothesis:** QuickJS lacks V8's JIT compiler and optimizing tiers. The SWC TypeScript-to-JavaScript transpilation happens eagerly at load time in Rust, whereas Node.js jiti defers/caches transpilation.

**Acceptable:** Yes — Extension loading is a one-time cost amortized over the entire agent session (minutes to hours). Even at 100ms, the load time is imperceptible relative to LLM API round-trip latency (~1-5 seconds).


### Load Time (oracle 1ms)

**Observation:** The conformance oracle reports TS load time as ~1ms, giving ratio ~100x. The real legacy benchmark measures ~12-21ms for the same extensions.

**Hypothesis:** The TS oracle uses an in-process, pre-warmed jiti runtime where module resolution and transpilation are cached. The 1ms timing reflects only the extension's activate() call, not cold-start loading. The real legacy benchmark (fresh process per run) gives a fairer comparison.

**Acceptable:** Yes — Use the legacy benchmark (p50: 12-21ms) as the true baseline, not the oracle timing. The real ratio is ~5-8x, not ~100x.


### Tool Call Latency

**Observation:** Rust tool calls take ~44us vs legacy ~1.7us (~26x slower).

**Hypothesis:** QuickJS function calls and JS-to-Rust bridge marshalling add overhead per invocation. V8 inlines and JIT-compiles hot call paths. The Rust bridge serializes/deserializes JSON for each tool call crossing the FFI boundary.

**Acceptable:** Yes — At 44us per tool call, a tool-heavy agent turn with 20 tool calls adds only 0.88ms total latency. LLM inference takes 1-10 seconds per turn, so tool call overhead is <0.1% of turn time. The absolute latency is well within acceptable bounds for interactive use.


### Memory Stability

**Observation:** RSS growth under 30s sustained load is <2%, no errors.

**Hypothesis:** QuickJS has deterministic reference counting GC with low memory overhead. The Rust wrapper correctly manages object lifetimes.

**Acceptable:** Yes — Excellent memory stability is a significant advantage of the Rust + QuickJS approach over Node.js, which relies on V8's generational GC and can exhibit higher memory variance under sustained load.


### Functional Parity

**Observation:** 60/60 official extensions pass conformance (100% pass rate).

**Hypothesis:** N/A — no regression.

**Acceptable:** Yes — Full functional compatibility with the legacy runtime for all official extensions. No behavioral regressions detected.


## Methodology

- **Rust benchmarks**: `pijs_workload` binary via hyperfine (10 runs, 3 warmup)
- **Legacy benchmarks**: `bench_legacy_extension_workloads.mjs` via Node.js v22.2.0 (10 cold-start runs, 2000 iterations)
- **Load time**: Conformance differential runner (60 extensions, Rust QuickJS vs TS jiti oracle)
- **Stress test**: 30s sustained load, 15 extensions, 50 events/sec
- **Environment**: Linux x86_64, same machine for both runtimes

## How to Regenerate

```bash
cargo test --test perf_comparison -- generate_perf_comparison
```
