//! Search backend comparison benches (`bd-cv653.1.5`).
//!
//! Engineering measurement (not a release claim): compares the in-process
//! grep/find backends against the external `rg`/`fd` escape hatch on a
//! synthetic source tree. The external lanes are skipped silently when the
//! binaries are not installed.
//!
//! Run with: `cargo bench --bench search_backends`

#[path = "bench_env.rs"]
mod bench_env;

use criterion::{Criterion, criterion_group, criterion_main};
use pi::config::Config;
use pi::tools::ToolRegistry;
use std::fmt::Write as _;
use std::path::Path;

/// Lay out a synthetic tree: `dirs` directories x `files_per_dir` files, each
/// with `lines` lines, a needle on one line per file, plus a `.gitignore`
/// excluding one subtree.
fn build_fixture_tree(root: &Path, dirs: usize, files_per_dir: usize, lines: usize) {
    std::fs::write(root.join(".gitignore"), "ignored-dir/\n").expect("write gitignore");
    let ignored = root.join("ignored-dir");
    std::fs::create_dir_all(&ignored).expect("create ignored dir");
    std::fs::write(ignored.join("skip.rs"), "needle should not appear\n").expect("write ignored");

    for dir_index in 0..dirs {
        let dir = root.join(format!("mod_{dir_index:03}"));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        for file_index in 0..files_per_dir {
            let mut content = String::with_capacity(lines * 24);
            for line_index in 0..lines {
                if line_index == lines / 2 {
                    let _ = writeln!(content, "    let needle_{file_index} = {line_index};");
                } else {
                    let _ = writeln!(content, "    let filler_{line_index} = {line_index};");
                }
            }
            std::fs::write(dir.join(format!("file_{file_index:03}.rs")), content)
                .expect("write fixture file");
        }
    }
}

fn registry_for(root: &Path, backend: &str) -> ToolRegistry {
    let config: Config = serde_json::from_value(serde_json::json!({
        "search_backend": backend,
    }))
    .expect("backend config");
    ToolRegistry::new(&["grep", "find"], root, Some(&config))
}

/// Runs the tool once with a per-iteration `limit` nudge so the tool-output
/// cache key changes every call — the scan is what's being measured, not the
/// cache hit path.
fn run_tool(
    registry: &ToolRegistry,
    name: &str,
    mut input: serde_json::Value,
    iteration: &mut u64,
) {
    *iteration += 1;
    input["limit"] = serde_json::Value::Number(serde_json::Number::from(5000 + *iteration));
    let tool = registry.get(name).expect("tool registered");
    asupersync::test_utils::run_test(|| async {
        tool.execute("bench", input.clone(), None)
            .await
            .expect("tool run");
    });
}

fn external_binaries_present() -> bool {
    let have = |names: &[&str]| {
        names.iter().any(|name| {
            std::process::Command::new(name)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
        })
    };
    have(&["rg"]) && have(&["fd", "fdfind"])
}

fn bench_search_backends(c: &mut Criterion) {
    let tmp = tempfile::tempdir().expect("fixture tempdir");
    // ~2k files, ~60 lines each — big enough to exercise walking + matching,
    // small enough for CI.
    build_fixture_tree(tmp.path(), 40, 50, 60);
    let root = tmp.path();

    let grep_input = serde_json::json!({ "pattern": "needle_[0-9]+" });
    let find_input = serde_json::json!({ "pattern": "*.rs" });
    let mut iteration = 0u64;

    let mut group = c.benchmark_group("search_backends");
    group.sample_size(10);

    let inproc = registry_for(root, "inproc");
    group.bench_function("grep_inproc_2k_files", |b| {
        b.iter(|| run_tool(&inproc, "grep", grep_input.clone(), &mut iteration));
    });
    group.bench_function("find_inproc_2k_files", |b| {
        b.iter(|| run_tool(&inproc, "find", find_input.clone(), &mut iteration));
    });

    if external_binaries_present() {
        let external = registry_for(root, "external");
        group.bench_function("grep_external_2k_files", |b| {
            b.iter(|| run_tool(&external, "grep", grep_input.clone(), &mut iteration));
        });
        group.bench_function("find_external_2k_files", |b| {
            b.iter(|| run_tool(&external, "find", find_input.clone(), &mut iteration));
        });
    }

    group.finish();
}

criterion_group!(
    name = benches;
    config = bench_env::criterion_config();
    targets = bench_search_backends
);
criterion_main!(benches);
