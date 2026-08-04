//! Benchmarks for `pi_agent_rust` core operations.
//!
//! Run with: cargo bench
//! Run specific: cargo bench -- truncate
//!
//! Performance budgets (targets):
//! - `truncate_head_10k_lines`: <1ms
//! - `truncate_tail_10k_lines`: <1ms
//! - `sse_parse_100_events`: <100μs

#[path = "bench_env.rs"]
mod bench_env;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::StreamExt as _;
use std::fmt::Write as _;
use std::hint::black_box;
use std::time::Duration;

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

fn chunk_bytes(input: &[u8], chunk_size: usize) -> Vec<Vec<u8>> {
    input.chunks(chunk_size).map(<[u8]>::to_vec).collect()
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
                        black_box(content).clone(),
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
                        black_box(content).clone(),
                        pi::tools::DEFAULT_MAX_LINES,
                        pi::tools::DEFAULT_MAX_BYTES,
                    )
                });
            },
        );
    }

    group.finish();
}

/// Benchmarks the no-truncation fast path: content fits within limits, so the
/// owned String is moved directly into the result (zero-copy).
fn bench_truncate_no_truncation(c: &mut Criterion) {
    let mut group = c.benchmark_group("truncation");

    // 500 lines × 80 chars = ~40.5KB — under both DEFAULT_MAX_LINES (2000)
    // and DEFAULT_MAX_BYTES (50KB).
    let content = build_lines(500, 80);
    group.throughput(Throughput::Bytes(content.len() as u64));

    group.bench_function("head_no_trunc_500", |b| {
        b.iter(|| {
            pi::tools::truncate_head(
                black_box(&content).clone(),
                pi::tools::DEFAULT_MAX_LINES,
                pi::tools::DEFAULT_MAX_BYTES,
            )
        });
    });

    group.bench_function("tail_no_trunc_500", |b| {
        b.iter(|| {
            pi::tools::truncate_tail(
                black_box(&content).clone(),
                pi::tools::DEFAULT_MAX_LINES,
                pi::tools::DEFAULT_MAX_BYTES,
            )
        });
    });

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
// SSE Streaming Benchmarks (SseStream)
// ============================================================================

fn bench_sse_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("sse_stream");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));

    let event_count = 1000;
    let data = build_sse_data(event_count);
    let data_bytes = data.as_bytes();

    for chunk_size in [64usize, 1024, 4096] {
        let chunks = chunk_bytes(data_bytes, chunk_size);

        group.throughput(Throughput::Elements(event_count as u64));
        group.bench_function(BenchmarkId::new("parse", chunk_size), |b| {
            b.iter(|| {
                let stream = futures::stream::iter(chunks.iter().cloned().map(Ok));
                let mut sse = pi::sse::SseStream::new(stream);
                let parsed = futures::executor::block_on(async move {
                    let mut count = 0usize;
                    while let Some(event) = sse.next().await {
                        let _event = event.expect("ok");
                        count += 1;
                    }
                    count
                });
                black_box(parsed);
            });
        });
    }

    group.finish();
}

// ============================================================================
// Streaming Clone Benchmark — Arc<AssistantMessage> vs deep clone
// ============================================================================

/// Simulates the streaming hot path: accumulate text via `push_str`, then share
/// with event consumers via `Arc::clone` (O(1)) instead of deep clone (O(n)).
fn bench_streaming_arc(c: &mut Criterion) {
    use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage};
    use std::sync::Arc;

    let mut group = c.benchmark_group("streaming_clone");
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));

    // Simulate 500 tokens building up a ~10KB response
    let token_count = 500usize;
    let token_text = "Hello world "; // ~12 bytes per token

    // Benchmark: Arc::make_mut + Arc::clone pattern (current, optimized)
    group.bench_function("arc_make_mut_clone_500tok", |b| {
        b.iter(|| {
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(""))],
                api: "anthropic".into(),
                provider: "anthropic".into(),
                model: "claude-sonnet-4".into(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            let mut arc = Arc::new(msg);

            for _ in 0..token_count {
                // Mutate via make_mut (O(1) when refcount=1)
                let m = Arc::make_mut(&mut arc);
                if let Some(ContentBlock::Text(t)) = m.content.first_mut() {
                    t.text.push_str(token_text);
                }
                // Share via clone (O(1) refcount bump)
                let shared = Arc::clone(&arc);
                // Simulate two event consumers
                black_box(&shared);
                let shared2 = Arc::clone(&arc);
                black_box(&shared2);
                // Both dropped before next iteration → refcount returns to 1
            }

            black_box(&arc);
        });
    });

    // Benchmark: Deep clone pattern (old, unoptimized)
    group.bench_function("deep_clone_500tok", |b| {
        b.iter(|| {
            let mut msg = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(""))],
                api: "anthropic".into(),
                provider: "anthropic".into(),
                model: "claude-sonnet-4".into(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };

            for _ in 0..token_count {
                // Mutate directly
                if let Some(ContentBlock::Text(t)) = msg.content.first_mut() {
                    t.text.push_str(token_text);
                }
                // Deep clone to share with events (O(accumulated_text))
                let ev = msg.clone();
                black_box(&ev);
                let ev2 = msg.clone();
                black_box(&ev2);
            }

            black_box(&msg);
        });
    });

    group.finish();
}

// ============================================================================
// Criterion Groups
// ============================================================================

criterion_group! {
    name = benches;
    config = bench_env::criterion_config();
    targets =
        bench_truncate_head,
        bench_truncate_tail,
        bench_truncate_no_truncation,
        bench_sse_parsing,
        bench_sse_stream,
        bench_streaming_arc,
}
criterion_main!(benches);
