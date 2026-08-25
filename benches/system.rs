//! System-level benchmarks: startup time, memory usage, binary size.
//!
// Allow some clippy lints that are acceptable in benchmarks
#![allow(clippy::cast_precision_loss)] // u64 -> f64 for size calculations is fine
#![allow(clippy::cmp_owned)] // PathBuf comparison with "pi" requires owned
#![allow(clippy::too_many_lines)]
//!
//! Run with:
//! - `cargo bench --bench system`
//! - `cargo bench startup`
//! - `cargo bench memory`
//!
//! These benchmarks measure real-world performance by spawning the actual binary.
//! They complement the micro-benchmarks in tools.rs and extensions.rs.
//!
//! Performance budgets:
//! - Startup time (--version): <100ms (p95), 11.2ms typical
//! - Startup time (cold, full agent): <200ms (p95)
//! - Idle memory: <50MB RSS
//! - Binary size (release): <20MB

#[path = "bench_env.rs"]
mod bench_env;

use std::env;
use std::fs;
use std::hint::black_box;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

fn criterion_config() -> Criterion {
    match idle_rss_raw_artifact_path() {
        Ok(Some(raw_path)) => {
            if let Err(error) = generate_idle_rss_raw_artifact(&raw_path) {
                eprintln!("failed to generate canonical idle-RSS raw artifact: {error}");
                std::process::exit(2);
            }
            // This opt-in mode is an evidence producer, not a Criterion timing run.
            // Exit only after every sampled child is reaped and the artifact is
            // atomically published, so unrelated benchmark setup cannot contaminate
            // the declared idle boundary.
            std::process::exit(0);
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("invalid idle-RSS evidence configuration: {error}");
            std::process::exit(2);
        }
    }
    bench_env::criterion_config_system()
}

const IDLE_RSS_RAW_PATH_ENV: &str = "PI_IDLE_RSS_RAW_RELATIVE_PATH";
const IDLE_RSS_SOURCE_COMMIT_ENV: &str = "PI_IDLE_RSS_SOURCE_COMMIT";
const IDLE_RSS_SOURCE_DIRTY_ENV: &str = "PI_IDLE_RSS_SOURCE_DIRTY";
const IDLE_RSS_CORRELATION_ID_ENV: &str = "PI_IDLE_RSS_CORRELATION_ID";
const IDLE_RSS_SAMPLE_COUNT: usize = 5;
const IDLE_RSS_SETTLE_MS: u64 = 1_000;

#[derive(Debug, Clone, Serialize)]
struct IdleRssSample {
    pid: u32,
    process_name: String,
    rss_bytes: u64,
}

// Fields mirror `BenchEnvMeasurementControl` in `src/perf_build.rs`; both
// producer and verifier hash the normalized JSON object.
#[derive(Debug, Serialize)]
struct IdleRssBenchEnv {
    os: String,
    arch: &'static str,
    cpu_brand: String,
    cpu_cores: usize,
    mem_total_mb: u64,
    governor: String,
    turbo_boost: String,
    aslr: String,
    thp: String,
    noise_score: u8,
    config_hash: String,
}

fn invalid_idle_rss_input(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, detail.into())
}

fn idle_rss_raw_artifact_path() -> io::Result<Option<PathBuf>> {
    let Some(raw) = env::var_os(IDLE_RSS_RAW_PATH_ENV) else {
        return Ok(None);
    };
    let relative = PathBuf::from(raw);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(invalid_idle_rss_input(format!(
            "{IDLE_RSS_RAW_PATH_ENV} must be a non-empty normalized relative path"
        )));
    }
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            invalid_idle_rss_input(format!("{IDLE_RSS_RAW_PATH_ENV} requires CARGO_TARGET_DIR"))
        })?;
    Ok(Some(target_dir.join(relative)))
}

fn sample_interactive_idle_rss(
    binary_path: &Path,
    workspace: &Path,
    agent_dir: &Path,
    settle_ms: u64,
) -> io::Result<IdleRssSample> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| io::Error::other(format!("allocate idle-RSS PTY: {error}")))?;
    let mut command = CommandBuilder::new(binary_path);
    command.cwd(workspace);
    command.env("TERM", "xterm-256color");
    command.env("PI_NO_MOUSE_CAPTURE", "1");
    command.env("PI_WORKSPACE_TRUST", "trusted");
    command.env("PI_CODING_AGENT_DIR", agent_dir);
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| io::Error::other(format!("spawn interactive pi: {error}")))?;
    drop(pair.slave);
    let Some(pid) = child.process_id() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other(
            "interactive pi did not expose a process id",
        ));
    };
    let mut reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!(
                "clone idle-RSS PTY reader: {error}"
            )));
        }
    };
    let drain = thread::spawn(move || {
        let _ = io::copy(&mut reader, &mut io::sink());
    });

    thread::sleep(Duration::from_millis(settle_ms));
    let sample_result = (|| {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "interactive pi exited before idle sample: {status:?}"
            )));
        }
        let pid = sysinfo::Pid::from_u32(pid);
        let mut system = System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing().with_memory()),
        );
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        let process = system
            .process(pid)
            .ok_or_else(|| io::Error::other("interactive pi vanished before RSS refresh"))?;
        let process_name = process.name().to_string_lossy().into_owned();
        if process_name != "pi" {
            return Err(io::Error::other(format!(
                "idle-RSS sample resolved process name {process_name:?}, expected pi"
            )));
        }
        let rss_bytes = process.memory();
        if rss_bytes == 0 {
            return Err(io::Error::other("interactive pi reported zero RSS"));
        }
        Ok(IdleRssSample {
            pid: pid.as_u32(),
            process_name,
            rss_bytes,
        })
    })();

    let _ = child.kill();
    let _ = child.wait();
    drop(pair.master);
    let _ = drain.join();
    sample_result
}

fn generate_idle_rss_raw_artifact(output_path: &Path) -> io::Result<()> {
    let binary = resolve_pi_binary();
    if binary.kind != BinaryKind::Release {
        return Err(invalid_idle_rss_input(format!(
            "idle RSS requires target/release/pi, resolved {}",
            binary.path.display()
        )));
    }
    let binary_path = fs::canonicalize(&binary.path)?;
    if binary_path.file_name().and_then(|name| name.to_str()) != Some("pi") {
        return Err(invalid_idle_rss_input(
            "idle RSS release binary must be named pi",
        ));
    }
    let source_commit = env::var(IDLE_RSS_SOURCE_COMMIT_ENV)
        .map_err(|_| invalid_idle_rss_input(format!("missing {IDLE_RSS_SOURCE_COMMIT_ENV}")))?;
    let source_dirty = env::var(IDLE_RSS_SOURCE_DIRTY_ENV)
        .map_err(|_| invalid_idle_rss_input(format!("missing {IDLE_RSS_SOURCE_DIRTY_ENV}")))?;
    if source_dirty != "false" {
        return Err(invalid_idle_rss_input(
            "idle RSS release evidence requires source_dirty=false",
        ));
    }
    let correlation_id = env::var(IDLE_RSS_CORRELATION_ID_ENV)
        .map_err(|_| invalid_idle_rss_input(format!("missing {IDLE_RSS_CORRELATION_ID_ENV}")))?;
    if correlation_id.trim().is_empty() {
        return Err(invalid_idle_rss_input(
            "idle RSS correlation id must be non-empty",
        ));
    }

    let workspace = output_path
        .parent()
        .ok_or_else(|| invalid_idle_rss_input("idle RSS output path has no parent"))?;
    fs::create_dir_all(workspace)?;
    let mut samples = Vec::with_capacity(IDLE_RSS_SAMPLE_COUNT);
    for sample_index in 0..IDLE_RSS_SAMPLE_COUNT {
        // Isolate global settings, extensions, skills, and session state so a
        // worker's personal Pi installation cannot contaminate idle RSS.
        let agent_dir = workspace.join(format!("agent-sample-{}", sample_index + 1));
        fs::create_dir_all(&agent_dir)?;
        samples.push(sample_interactive_idle_rss(
            &binary_path,
            workspace,
            &agent_dir,
            IDLE_RSS_SETTLE_MS,
        )?);
    }
    let representative = samples
        .iter()
        .max_by_key(|sample| sample.rss_bytes)
        .cloned()
        .ok_or_else(|| io::Error::other("idle RSS produced no samples"))?;
    let min_rss_bytes = samples
        .iter()
        .map(|sample| sample.rss_bytes)
        .min()
        .ok_or_else(|| io::Error::other("idle RSS produced no minimum"))?;

    let fingerprint = bench_env::collect_fingerprint();
    let bench_env = IdleRssBenchEnv {
        os: fingerprint.os,
        arch: fingerprint.arch,
        cpu_brand: fingerprint.cpu_brand,
        cpu_cores: fingerprint.cpu_cores,
        mem_total_mb: fingerprint.mem_total_mb,
        governor: fingerprint.governor,
        turbo_boost: fingerprint.turbo_boost,
        aslr: fingerprint.aslr,
        thp: fingerprint.thp,
        noise_score: fingerprint.noise_score,
        config_hash: fingerprint.config_hash,
    };
    let bench_env_value = serde_json::to_value(&bench_env)
        .map_err(|error| io::Error::other(format!("normalize benchmark environment: {error}")))?;
    let bench_env_bytes = serde_json::to_vec(&bench_env_value)
        .map_err(|error| io::Error::other(format!("serialize benchmark environment: {error}")))?;
    let bench_env_sha256 = pi::package_manager::hex_encode(&Sha256::digest(&bench_env_bytes));
    let binary_sha256 = pi::perf_build::sha256_file(&binary_path)?;
    let payload = serde_json::json!({
        "schema": pi::perf_build::IDLE_RSS_MEASUREMENT_SCHEMA,
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "run_id": correlation_id,
        "correlation_id": correlation_id,
        "source_commit": source_commit,
        "source_dirty": false,
        "pid": representative.pid,
        "process_name": representative.process_name,
        "allocator": pi::perf_build::compiled_allocator().as_str(),
        "binary_path": binary_path,
        "binary_sha256": binary_sha256,
        "rss_bytes": representative.rss_bytes,
        "idle_state": "startup_before_user_input",
        "cargo_profile": "release",
        "build_command": "cargo build --bin pi --release",
        "sample_count": samples.len(),
        "samples": samples,
        "rss_spread_bytes": representative.rss_bytes.saturating_sub(min_rss_bytes),
        "settle_ms": IDLE_RSS_SETTLE_MS,
        "bench_env_source": "benches/bench_env.rs",
        "bench_env": bench_env,
        "bench_env_sha256": bench_env_sha256,
    });
    let mut encoded = serde_json::to_vec_pretty(&payload)
        .map_err(|error| io::Error::other(format!("serialize idle-RSS artifact: {error}")))?;
    encoded.push(b'\n');
    let temporary_path = output_path.with_extension("json.tmp");
    fs::write(&temporary_path, &encoded)?;
    fs::rename(&temporary_path, output_path)?;
    let transport_record = serde_json::to_string(&payload)
        .map_err(|error| io::Error::other(format!("serialize idle-RSS transport: {error}")))?;
    eprintln!("[idle-rss-control] {transport_record}");
    eprintln!(
        "[idle-rss] artifact={} samples={} max_bytes={} spread_bytes={} binary_sha256={} bench_env_sha256={}",
        output_path.display(),
        IDLE_RSS_SAMPLE_COUNT,
        representative.rss_bytes,
        representative.rss_bytes.saturating_sub(min_rss_bytes),
        payload["binary_sha256"].as_str().unwrap_or("invalid"),
        payload["bench_env_sha256"].as_str().unwrap_or("invalid"),
    );
    Ok(())
}

// ============================================================================
// Binary Path Resolution
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryKind {
    Release,
    Debug,
    Unknown,
}

#[derive(Debug, Clone)]
struct ResolvedBinary {
    path: PathBuf,
    kind: BinaryKind,
}

fn infer_binary_kind(path: &Path) -> BinaryKind {
    let mut saw_release = false;
    let mut saw_debug = false;
    for component in path.components() {
        let part = component.as_os_str().to_string_lossy();
        if part == "release" {
            saw_release = true;
        } else if part == "debug" {
            saw_debug = true;
        }
    }

    if saw_release {
        BinaryKind::Release
    } else if saw_debug {
        BinaryKind::Debug
    } else {
        BinaryKind::Unknown
    }
}

fn target_roots(manifest_dir: &Path) -> Vec<PathBuf> {
    let cargo_target_dir = env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
    target_roots_with(manifest_dir, cargo_target_dir.as_deref())
}

fn target_roots_with(manifest_dir: &Path, cargo_target_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(candidate) = cargo_target_dir {
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            manifest_dir.join(candidate)
        };
        roots.push(resolved);
    }

    let default_target = manifest_dir.join("target");
    if !roots.contains(&default_target) {
        roots.push(default_target);
    }

    roots
}

const fn binary_file_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["pi.exe", "pi"]
    }
    #[cfg(not(windows))]
    {
        &["pi"]
    }
}

fn find_profile_binary(root: &Path, profile: &str) -> Option<PathBuf> {
    let profile_dir = root.join(profile);
    for binary_name in binary_file_names() {
        let candidate = profile_dir.join(binary_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
#[allow(dead_code)]
fn run_resolution_regression_checks() {
    use std::path::{Path, PathBuf};

    // Binary kind inference
    let path = Path::new("/tmp/target/release/pi");
    assert_eq!(infer_binary_kind(path), BinaryKind::Release);
    let path = Path::new("/tmp/target/debug/pi");
    assert_eq!(infer_binary_kind(path), BinaryKind::Debug);
    let path = Path::new("/tmp/target/debug/release/pi");
    assert_eq!(infer_binary_kind(path), BinaryKind::Release);
    let path = Path::new("/tmp/pi");
    assert_eq!(infer_binary_kind(path), BinaryKind::Unknown);

    // Relative CARGO_TARGET_DIR is resolved from manifest dir.
    let manifest_dir = Path::new("/workspace/pi_agent_rust");
    let roots = target_roots_with(manifest_dir, Some(Path::new("target/agents/blackglen")));
    assert_eq!(roots.len(), 2);
    assert_eq!(roots[0], manifest_dir.join("target/agents/blackglen"));
    assert_eq!(roots[1], manifest_dir.join("target"));

    // Absolute CARGO_TARGET_DIR is preserved.
    let roots = target_roots_with(manifest_dir, Some(Path::new("/tmp/custom-target")));
    assert_eq!(roots.len(), 2);
    assert_eq!(roots[0], PathBuf::from("/tmp/custom-target"));
    assert_eq!(roots[1], manifest_dir.join("target"));

    // Default target root should not be duplicated.
    let roots = target_roots_with(manifest_dir, Some(Path::new("target")));
    assert_eq!(roots, vec![manifest_dir.join("target")]);

    // Platform-aware binary lookup candidate order.
    let names = binary_file_names();
    #[cfg(windows)]
    {
        assert_eq!(names.first().copied(), Some("pi.exe"));
        assert!(names.contains(&"pi"));
    }
    #[cfg(not(windows))]
    {
        assert_eq!(names, &["pi"]);
    }
}

fn resolve_pi_binary() -> ResolvedBinary {
    // Check for explicit override
    if let Ok(path) = env::var("PI_BENCH_BINARY") {
        let path = PathBuf::from(path);
        return ResolvedBinary {
            kind: infer_binary_kind(&path),
            path,
        };
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_roots = target_roots(&manifest_dir);

    // Look for release binary first (more realistic)
    for root in &target_roots {
        if let Some(release_path) = find_profile_binary(root, "release") {
            return ResolvedBinary {
                path: release_path,
                kind: BinaryKind::Release,
            };
        }
    }

    // Fall back to debug binary
    for root in &target_roots {
        if let Some(debug_path) = find_profile_binary(root, "debug") {
            return ResolvedBinary {
                path: debug_path,
                kind: BinaryKind::Debug,
            };
        }
    }

    // Last resort: hope it's in PATH
    ResolvedBinary {
        path: PathBuf::from("pi"),
        kind: BinaryKind::Unknown,
    }
}

fn binary_size_bytes(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

// ============================================================================
// Startup Time Benchmarks
// ============================================================================

/// Measure startup time for `pi --version` (minimal startup path)
fn bench_startup_version(c: &mut Criterion) {
    let binary = resolve_pi_binary();
    // Pre-flight check: verify the binary is actually runnable (handles
    // both missing file AND "pi" not in PATH).
    if Command::new(&binary.path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!(
            "[skip] bench_startup_version: binary not runnable at {}",
            binary.path.display()
        );
        return;
    }

    {
        let mut group = c.benchmark_group("startup");

        // Warm the filesystem cache
        for _ in 0..3 {
            let _ = Command::new(&binary.path)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }

        group.bench_function(BenchmarkId::new("version", "warm"), |b| {
            b.iter(|| {
                let start = Instant::now();
                let status = Command::new(&binary.path)
                    .arg("--version")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .expect("failed to execute pi");
                let elapsed = start.elapsed();
                assert!(status.success(), "pi --version failed");
                black_box(elapsed)
            });
        });

        group.finish();
    }

    // Log binary size for reference
    if let Some(size) = binary_size_bytes(&binary.path) {
        let size_mb = size as f64 / 1024.0 / 1024.0;
        eprintln!(
            "[info] binary_size={size_mb:.2}MB path={}",
            binary.path.display()
        );
    }
}

/// Measure startup time for `pi --help` (loads more code paths)
fn bench_startup_help(c: &mut Criterion) {
    let binary = resolve_pi_binary();
    if Command::new(&binary.path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!(
            "[skip] bench_startup_help: binary not runnable at {}",
            binary.path.display()
        );
        return;
    }

    {
        let mut group = c.benchmark_group("startup");

        // Warm the filesystem cache
        for _ in 0..3 {
            let _ = Command::new(&binary.path)
                .arg("--help")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }

        group.bench_function(BenchmarkId::new("help", "warm"), |b| {
            b.iter(|| {
                let start = Instant::now();
                let status = Command::new(&binary.path)
                    .arg("--help")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .expect("failed to execute pi");
                let elapsed = start.elapsed();
                assert!(status.success(), "pi --help failed");
                black_box(elapsed)
            });
        });

        group.finish();
    }
}

/// Measure startup time for `pi --list-models` (exercises provider listing)
fn bench_startup_list_models(c: &mut Criterion) {
    let binary = resolve_pi_binary();
    if Command::new(&binary.path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!(
            "[skip] bench_startup_list_models: binary not runnable at {}",
            binary.path.display()
        );
        return;
    }

    {
        let mut group = c.benchmark_group("startup");

        // Warm the filesystem cache
        for _ in 0..3 {
            let _ = Command::new(&binary.path)
                .arg("--list-models")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }

        group.bench_function(BenchmarkId::new("list_models", "warm"), |b| {
            b.iter(|| {
                let start = Instant::now();
                let status = Command::new(&binary.path)
                    .arg("--list-models")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .expect("failed to execute pi");
                let elapsed = start.elapsed();
                // list-models may fail without API key, just measure time
                black_box((elapsed, status))
            });
        });

        group.finish();
    }
}

// ============================================================================
// Memory Benchmarks
// ============================================================================

/// Measure RSS memory for `pi --version` (process exits immediately)
fn bench_memory_version(c: &mut Criterion) {
    let binary = resolve_pi_binary();
    if Command::new(&binary.path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!(
            "[skip] bench_memory_version: binary not runnable at {}",
            binary.path.display()
        );
        return;
    }

    let mut group = c.benchmark_group("memory");

    group.bench_function(BenchmarkId::new("version_peak", "spawn"), |b| {
        b.iter(|| {
            // Spawn process and immediately query its memory
            let mut child = Command::new(&binary.path)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("failed to spawn pi");

            let pid = sysinfo::Pid::from_u32(child.id());
            let mut system = System::new_with_specifics(
                RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing().with_memory()),
            );
            system.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing().with_memory(),
            );

            let memory_kb = system.process(pid).map_or(0, |p| p.memory() / 1024);

            // Wait for completion
            let _ = child.wait();

            black_box(memory_kb)
        });
    });

    group.finish();
}

// ============================================================================
// Binary Size Benchmark
// ============================================================================

/// Report binary size (not a timing benchmark, just records the value)
fn bench_binary_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("binary");
    let binary = resolve_pi_binary();

    if let Some(size) = binary_size_bytes(&binary.path) {
        let size_mb = size as f64 / 1024.0 / 1024.0;
        eprintln!(
            "[metric] binary_size_mb={size_mb:.2} path={} kind={:?}",
            binary.path.display(),
            binary.kind
        );

        if binary.kind == BinaryKind::Release {
            // Check release binary against budget.
            let budget_mb = 20.0;
            if size_mb > budget_mb {
                eprintln!("[WARN] binary size {size_mb:.2}MB exceeds budget {budget_mb:.2}MB");
            } else {
                eprintln!("[OK] binary size {size_mb:.2}MB within budget {budget_mb:.2}MB");
            }
        } else {
            eprintln!(
                "[info] skipping release-size budget check for non-release binary ({:?})",
                binary.kind
            );
        }

        // "Benchmark" that just records the size for criterion tracking
        group.bench_function("size_mb", |b| {
            b.iter(|| black_box(size_mb));
        });
    } else {
        eprintln!("[skip] bench_binary_size: could not read binary");
    }

    group.finish();
}

// ============================================================================
// Criterion Groups
// ============================================================================

criterion_group!(
    name = benches;
    config = criterion_config();
    targets =
        bench_startup_version,
        bench_startup_help,
        bench_startup_list_models,
        bench_memory_version,
        bench_binary_size
);
criterion_main!(benches);
