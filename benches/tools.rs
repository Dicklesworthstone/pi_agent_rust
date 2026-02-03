//! Benchmarks for `pi_agent_rust` core operations.
//!
//! Run with: cargo bench
//! Run specific: cargo bench -- truncate
//!
//! Performance budgets (targets):
//! - `truncate_head_10k_lines`: <1ms
//! - `truncate_tail_10k_lines`: <1ms
//! - `sse_parse_100_events`: <100μs

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::fmt::Write as _;
use std::hint::black_box;

// ============================================================================
// Test Data Builders
// ============================================================================

fn build_lines(line_count: usize, line_len: usize) -> String {
    let mut s = String::with_capacity(line_count.saturating_mul(line_len.saturating_add(1)));
    for i in 0..line_count {
        if i > 0 {
            s.push('\n');
        }
        s.extend(std::iter::repeat_n('a', line_len));
    }
    s
}

fn build_sse_data(event_count: usize) -> String {
    let mut s = String::new();
    for i in 0..event_count {
        let _ = write!(
            s,
            "event: message\ndata: {{\"type\": \"content_block_delta\", \"index\": {i}, \"delta\": {{\"type\": \"text_delta\", \"text\": \"Hello world \"}}}}\n\n"
        );
    }
    s.push_str("data: [DONE]\n\n");
    s
}

// ============================================================================
// Truncation Benchmarks
// ============================================================================

fn bench_truncate_head(c: &mut Criterion) {
    let mut group = c.benchmark_group("truncation");

    for line_count in [1_000, 10_000, 100_000] {
        let content = build_lines(line_count, 80);
        group.throughput(Throughput::Bytes(content.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("head", line_count),
            &content,
            |b, content| {
                b.iter(|| {
                    pi::tools::truncate_head(
                        black_box(content),
                        pi::tools::DEFAULT_MAX_LINES,
                        pi::tools::DEFAULT_MAX_BYTES,
                    )
                });
            },
        );
    }

    group.finish();
}

fn bench_truncate_tail(c: &mut Criterion) {
    let mut group = c.benchmark_group("truncation");

    for line_count in [1_000, 10_000, 100_000] {
        let content = build_lines(line_count, 80);
        group.throughput(Throughput::Bytes(content.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("tail", line_count),
            &content,
            |b, content| {
                b.iter(|| {
                    pi::tools::truncate_tail(
                        black_box(content),
                        pi::tools::DEFAULT_MAX_LINES,
                        pi::tools::DEFAULT_MAX_BYTES,
                    )
                });
            },
        );
    }

    group.finish();
}

// ============================================================================
// SSE Parsing Benchmarks
// ============================================================================

fn bench_sse_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("sse_parsing");

    for event_count in [10, 100, 1000] {
        let data = build_sse_data(event_count);
        group.throughput(Throughput::Elements(event_count as u64));
        group.bench_with_input(BenchmarkId::new("parse", event_count), &data, |b, data| {
            b.iter(|| {
                let mut parser = pi::sse::SseParser::new();
                let events = parser.feed(black_box(data));
                black_box(events)
            });
        });
    }

    group.finish();
}

// ============================================================================
// Criterion Groups
// ============================================================================

criterion_group!(
    benches,
    bench_truncate_head,
    bench_truncate_tail,
    bench_sse_parsing,
);
criterion_main!(benches);
