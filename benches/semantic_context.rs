//! Focused Criterion inputs for semantic context intelligence budgets.
//!
//! The benchmarks build a deterministic large workspace on real filesystem
//! storage and emit Criterion artifacts consumed by `tests/perf_budgets.rs`.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

#[path = "bench_env.rs"]
mod bench_env;

use std::fmt::Write as _;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use pi::semantic_workspace_graph::{
    ContextBundleBudget, ContextBundleRequest, SemanticContextBundle, SemanticContextBundlePlanner,
    SemanticWorkspaceGraph, SemanticWorkspaceGraphBuilder,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const MODULE_COUNT: usize = 120;
const TEST_COUNT: usize = 60;
const DOC_COUNT: usize = 24;
const EVIDENCE_COUNT: usize = 24;
const LARGE_WORKSPACE_CASE: &str = "large_workspace";
const PERF_BUDGET_SCHEMA: &str = "pi.semantic_context.performance_budget.v1";

#[derive(Debug, Clone, Copy)]
enum FixtureOrder {
    Forward,
    Reverse,
}

struct LargeWorkspaceFixture {
    _temp: TempDir,
    root: PathBuf,
}

impl LargeWorkspaceFixture {
    fn new() -> Self {
        Self::new_with_order(FixtureOrder::Forward)
    }

    fn new_with_order(order: FixtureOrder) -> Self {
        let temp = tempfile::tempdir().expect("create semantic context bench workspace");
        let root = temp.path().to_path_buf();

        write_file(
            &root,
            "README.md",
            "# Semantic Context Bench\n\nThis large workspace cites docs/evidence/context_000.json for release-facing context planning.\n",
        );
        write_file(&root, ".beads/issues.jsonl", &beads_fixture_content());

        for index in ordered_indices(MODULE_COUNT, order) {
            write_file(
                &root,
                &format!("src/module_{index:03}.rs"),
                &module_content(index, 0),
            );
        }
        for index in ordered_indices(TEST_COUNT, order) {
            write_file(
                &root,
                &format!("tests/context_fixture_{index:03}.rs"),
                &test_content(index),
            );
        }
        for index in ordered_indices(DOC_COUNT, order) {
            write_file(
                &root,
                &format!("docs/context_{index:03}.md"),
                &doc_content(index),
            );
        }
        for index in ordered_indices(EVIDENCE_COUNT, order) {
            write_file(
                &root,
                &format!("docs/evidence/context_{index:03}.json"),
                &evidence_content(index),
            );
        }

        Self { _temp: temp, root }
    }

    fn build_graph(&self) -> SemanticWorkspaceGraph {
        SemanticWorkspaceGraphBuilder::new(&self.root)
            .build()
            .expect("build semantic workspace graph")
    }

    fn rewrite_module(&self, iteration: usize) {
        let index = iteration % MODULE_COUNT;
        write_file(
            &self.root,
            &format!("src/module_{index:03}.rs"),
            &module_content(index, iteration.saturating_add(1)),
        );
    }
}

fn criterion_config() -> Criterion {
    bench_env::criterion_config()
}

fn ordered_indices(count: usize, order: FixtureOrder) -> Vec<usize> {
    let mut indices = (0..count).collect::<Vec<_>>();
    if matches!(order, FixtureOrder::Reverse) {
        indices.reverse();
    }
    indices
}

fn write_file(root: &Path, relative_path: &str, content: &str) {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create semantic context bench parent");
    }
    std::fs::write(path, content).expect("write semantic context bench fixture");
}

fn measure_ms(action: impl FnOnce()) -> f64 {
    let started_at = Instant::now();
    action();
    started_at.elapsed().as_secs_f64() * 1000.0
}

fn resolved_target_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || manifest_dir.join("target"),
        |raw| {
            let path = PathBuf::from(raw);
            if path.is_absolute() {
                path
            } else {
                manifest_dir.join(path)
            }
        },
    )
}

fn resolved_tmpdir() -> PathBuf {
    std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

fn bundle_signature(bundle: &SemanticContextBundle) -> Value {
    json!({
        "selected": bundle.selected_items.iter().map(|item| {
            json!({
                "path": &item.source_path,
                "title": &item.title,
                "reason": &item.reason,
            })
        }).collect::<Vec<_>>(),
        "excluded": bundle.excluded_items.iter().map(|item| {
            json!({
                "path": &item.source_path,
                "reason": &item.reason,
            })
        }).collect::<Vec<_>>(),
        "commands": &bundle.suggested_validation_commands,
        "estimated_bytes": bundle.estimated_bytes,
    })
}

fn randomized_order_replay_matches(request: &ContextBundleRequest) -> bool {
    let forward = LargeWorkspaceFixture::new_with_order(FixtureOrder::Forward);
    let reverse = LargeWorkspaceFixture::new_with_order(FixtureOrder::Reverse);

    let forward_graph = forward.build_graph();
    let reverse_graph = reverse.build_graph();
    let forward_bundle = SemanticContextBundlePlanner::new(&forward_graph).plan(request);
    let reverse_bundle = SemanticContextBundlePlanner::new(&reverse_graph).plan(request);

    let forward_signature = bundle_signature(&forward_bundle);
    let reverse_signature = bundle_signature(&reverse_bundle);
    match (
        serde_json::to_vec(&forward_signature),
        serde_json::to_vec(&reverse_signature),
    ) {
        (Ok(forward_bytes), Ok(reverse_bytes)) => forward_bytes.eq(&reverse_bytes),
        _ => false,
    }
}

fn write_context_budget_artifact(
    graph_fixture: &LargeWorkspaceFixture,
    request: &ContextBundleRequest,
    bundle: &SemanticContextBundle,
) {
    let cold_ms = measure_ms(|| {
        let fixture = LargeWorkspaceFixture::new();
        black_box(fixture.build_graph());
    });
    let warm_ms = measure_ms(|| {
        black_box(graph_fixture.build_graph());
    });
    let incremental_fixture = LargeWorkspaceFixture::new();
    let incremental_ms = measure_ms(|| {
        incremental_fixture.rewrite_module(1);
        black_box(incremental_fixture.build_graph());
    });
    let graph = graph_fixture.build_graph();
    let planner = SemanticContextBundlePlanner::new(&graph);
    let planning_ms = measure_ms(|| {
        black_box(planner.plan(black_box(request)));
    });
    let serialization_ms = measure_ms(|| {
        black_box(
            serde_json::to_vec(black_box(bundle)).expect("serialize semantic context bundle"),
        );
    });

    let target_dir = resolved_target_dir();
    let output_root = bench_env::criterion_output_directory()
        .unwrap_or_else(|| target_dir.join("criterion"));
    let output_path = output_root.join("context_intelligence/perf_budget.json");
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).expect("create semantic context perf budget dir");
    }
    let correlation_id = std::env::var("CI_CORRELATION_ID")
        .unwrap_or_else(|_| "standalone-unclaimable".to_string());
    let source_commit = std::env::var("VERGEN_GIT_SHA")
        .unwrap_or_else(|_| "unknown".to_string());
    let source_dirty = std::env::var("VERGEN_GIT_DIRTY")
        .ok()
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(true);
    let payload = json!({
        "schema": PERF_BUDGET_SCHEMA,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "run_id": &correlation_id,
        "correlation_id": correlation_id,
        "source_commit": source_commit,
        "source_dirty": source_dirty,
        "environment": {
            "cargo_target_dir": target_dir.display().to_string(),
            "tmpdir": resolved_tmpdir().display().to_string(),
        },
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "determinism": {
            "randomized_file_order_checked": true,
            "matched": randomized_order_replay_matches(request),
        },
        "cache_hit_miss": {
            "cold_graph_build": "fresh_large_workspace_fixture",
            "warm_graph_build": "same_workspace_rebuild_after_warmup",
            "incremental_update": "single_file_rebuild_current_path",
        },
        "metrics": {
            "context_graph_build_cold_ms": {"value_ms": cold_ms},
            "context_graph_build_warm_ms": {"value_ms": warm_ms},
            "context_incremental_update_ms": {"value_ms": incremental_ms},
            "context_planning_ms": {"value_ms": planning_ms},
            "context_bundle_serialization_ms": {"value_ms": serialization_ms},
            "context_bundle_estimated_bytes": {"bytes": bundle.estimated_bytes as f64},
        },
    });
    std::fs::write(
        &output_path,
        serde_json::to_string_pretty(&payload).expect("serialize context budget artifact"),
    )
    .expect("write semantic context perf budget artifact");
}

fn module_content(index: usize, revision: usize) -> String {
    format!(
        r#"pub struct ContextFixture{index:03};

pub fn context_symbol_{index:03}(input: usize) -> usize {{
    input.saturating_add({index}).saturating_add({revision})
}}

pub fn context_planner_surface_{index:03}() -> &'static str {{
    "semantic context planner performance budget"
}}
"#
    )
}

fn test_content(index: usize) -> String {
    format!(
        r"#[test]
fn context_fixture_{index:03}_validation() {{
    assert_eq!(2 + 2, 4);
}}
"
    )
}

fn doc_content(index: usize) -> String {
    let evidence_index = index % EVIDENCE_COUNT;
    format!(
        "# Context Doc {index:03}\n\nPlanner performance evidence cites docs/evidence/context_{evidence_index:03}.json and src/module_{index:03}.rs.\n\n## Validation\n\nUse cargo test --test context_fixture_{index:03} context_fixture_{index:03}_validation.\n"
    )
}

fn evidence_content(index: usize) -> String {
    format!(
        r#"{{
  "schema": "pi.context_intelligence.perf_fixture.v1",
  "generated_at": "2026-05-13T00:00:00Z",
  "fixture_index": {index},
  "overall_verdict": "CERTIFIED"
}}
"#
    )
}

fn beads_fixture_content() -> String {
    let mut lines = String::new();
    for index in 0..64 {
        let _ = writeln!(
            lines,
            r#"{{"id":"bd-context-{index:03}","title":"Context planner fixture {index:03}","status":"open","priority":2,"type":"task","external_ref":"docs/evidence/context_{evidence_index:03}.json"}}"#,
            evidence_index = index % EVIDENCE_COUNT,
        );
    }
    lines
}

fn planner_request() -> ContextBundleRequest {
    ContextBundleRequest {
        query: Some(
            "semantic context planner provider module performance budget context_042".to_string(),
        ),
        bead_id: Some("bd-context-042".to_string()),
        changed_paths: vec![
            "src/module_042.rs".to_string(),
            "tests/context_fixture_042.rs".to_string(),
            "docs/context_018.md".to_string(),
        ],
        failing_command: Some(
            "cargo test --test context_fixture_042 context_fixture_042_validation".to_string(),
        ),
        workspace_id: Some("semantic-context-large-workspace".to_string()),
        branch: Some("main".to_string()),
        session_id: Some("semantic-context-bench-session".to_string()),
        generated_at_utc: Some("2026-05-13T00:00:00Z".to_string()),
        cache_ttl_seconds: 900,
        budget: ContextBundleBudget {
            max_items: 32,
            max_bytes: 64 * 1024,
        },
    }
}

fn bench_graph_builds(c: &mut Criterion) {
    let mut group = c.benchmark_group("semantic_context");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function(
        BenchmarkId::new("graph_build_cold", LARGE_WORKSPACE_CASE),
        |b| {
            b.iter_batched(
                LargeWorkspaceFixture::new,
                |fixture| black_box(fixture.build_graph()),
                BatchSize::SmallInput,
            );
        },
    );

    let warm_fixture = LargeWorkspaceFixture::new();
    let _warmed = warm_fixture.build_graph();
    group.bench_function(
        BenchmarkId::new("graph_build_warm", LARGE_WORKSPACE_CASE),
        |b| {
            b.iter(|| black_box(warm_fixture.build_graph()));
        },
    );

    let incremental_fixture = LargeWorkspaceFixture::new();
    let mut iteration = 0_usize;
    group.bench_function(
        BenchmarkId::new("incremental_update", LARGE_WORKSPACE_CASE),
        |b| {
            b.iter_batched(
                || {
                    iteration = iteration.wrapping_add(1);
                    iteration
                },
                |current_iteration| {
                    incremental_fixture.rewrite_module(current_iteration);
                    black_box(incremental_fixture.build_graph())
                },
                BatchSize::SmallInput,
            );
        },
    );

    group.finish();
}

fn bench_planning_and_serialization(c: &mut Criterion) {
    let fixture = LargeWorkspaceFixture::new();
    let graph = fixture.build_graph();
    let planner = SemanticContextBundlePlanner::new(&graph);
    let request = planner_request();
    let bundle = planner.plan(&request);
    write_context_budget_artifact(&fixture, &request, &bundle);

    let mut group = c.benchmark_group("semantic_context");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function(BenchmarkId::new("planning", LARGE_WORKSPACE_CASE), |b| {
        b.iter(|| black_box(planner.plan(black_box(&request))));
    });

    group.bench_function(
        BenchmarkId::new("bundle_serialization", LARGE_WORKSPACE_CASE),
        |b| {
            b.iter(|| {
                black_box(
                    serde_json::to_vec(black_box(&bundle))
                        .expect("serialize semantic context bundle"),
                )
            });
        },
    );

    group.finish();
}

criterion_group!(
    name = benches;
    config = criterion_config();
    targets = bench_graph_builds, bench_planning_and_serialization
);
criterion_main!(benches);
