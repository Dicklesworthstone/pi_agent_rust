#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use futures::executor::block_on;
use pi::extension_popularity::{CandidateItem, CandidatePool, CandidateSource};
use pi::extension_validation::{ValidationStatus, classify_source_content};
use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::{PiJsRuntimeConfig, RepairMode};
use pi::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const SUPPORTED_EXTS: &[&str] = &["ts", "tsx", "jsx", "js", "mjs", "cjs", "mts", "cts"];

#[derive(Parser, Debug)]
#[command(name = "ext_unvendored_fetch_run")]
#[command(about = "Fetch and runtime-probe unvendored extension candidates")]
struct Cli {
    #[command(subcommand)]
    command: CommandMode,
}

#[derive(Subcommand, Debug)]
enum CommandMode {
    /// Fetch and probe a corpus of candidates.
    RunAll(RunAllArgs),
    /// Probe one entrypoint with the real `QuickJS` extension runtime.
    ProbeOne(ProbeOneArgs),
}

#[derive(Args, Debug, Clone)]
struct RunAllArgs {
    /// Candidate pool containing vendored + unvendored entries.
    #[arg(long, default_value = "docs/extension-candidate-pool.json")]
    candidate_pool: PathBuf,

    /// Optional priority ranking file (used for ordering and reporting rank).
    #[arg(long, default_value = "docs/extension-priority.json")]
    priority_json: PathBuf,

    /// Optional code-search summary (repo->entrypoint hints).
    #[arg(long, default_value = "docs/extension-code-search-summary.json")]
    code_search_summary: PathBuf,

    /// Output JSON report.
    #[arg(
        long,
        default_value = "tests/ext_conformance/reports/pipeline/unvendored_fetch_probe_report.json"
    )]
    out_json: PathBuf,

    /// Output JSONL event stream (incremental per-candidate writes).
    #[arg(
        long,
        default_value = "tests/ext_conformance/reports/pipeline/unvendored_fetch_probe_events.jsonl"
    )]
    out_jsonl: PathBuf,

    /// Cache directory for fetched sources.
    #[arg(long, default_value = ".tmp-codex-unvendored-cache")]
    cache_dir: PathBuf,

    /// Number of worker threads.
    #[arg(long, default_value_t = 4)]
    workers: usize,

    /// Optional hard limit on number of candidates to process.
    #[arg(long)]
    limit: Option<usize>,

    /// Include vendored candidates too (default: only unvendored).
    #[arg(long, default_value_t = false)]
    include_vendored: bool,

    /// Fetch command timeout per candidate.
    #[arg(long, default_value_t = 120)]
    fetch_timeout_secs: u64,

    /// Probe subprocess timeout per candidate.
    #[arg(long, default_value_t = 20)]
    probe_timeout_secs: u64,

    /// Max files to scan when locating an entrypoint.
    #[arg(long, default_value_t = 5000)]
    max_scan_files: usize,

    /// Max bytes to read per source file while scanning.
    #[arg(long, default_value_t = 1_500_000)]
    max_file_bytes: u64,

    /// Disable runtime probe and only fetch + detect entrypoints.
    #[arg(long, default_value_t = false)]
    no_probe: bool,

    /// Restrict run to explicit candidate ids.
    #[arg(long = "only-id")]
    only_ids: Vec<String>,
}

#[derive(Args, Debug)]
struct ProbeOneArgs {
    #[arg(long)]
    entry: PathBuf,

    #[arg(long)]
    cwd: PathBuf,
}

#[derive(Debug, Deserialize)]
struct PriorityDoc {
    #[serde(default)]
    items: Vec<PriorityItem>,
}

#[derive(Debug, Deserialize)]
struct PriorityItem {
    id: String,
    rank: usize,
}

#[derive(Debug, Deserialize)]
struct CodeSearchSummary {
    #[serde(default)]
    repos: Vec<CodeSearchRepoEntry>,
}

#[derive(Debug, Deserialize)]
struct CodeSearchRepoEntry {
    repo: String,
    #[serde(default)]
    entrypoint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum FetchState {
    Cached,
    Fetched,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProbeState {
    Pass,
    Fail,
    Timeout,
    NotAttempted,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateProbeResult {
    id: String,
    source_tier: String,
    source_type: String,
    status: String,
    rank: Option<usize>,
    fetch_state: FetchState,
    fetch_error: Option<String>,
    local_root: Option<String>,
    entrypoint: Option<String>,
    entry_score: Option<i32>,
    scanned_files: usize,
    probe_state: ProbeState,
    probe_error: Option<String>,
    registered_commands: usize,
    registered_tools: usize,
    registered_flags: usize,
    registered_providers: usize,
    duration_ms: u64,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportCounts {
    total_selected: usize,
    cached: usize,
    fetched: usize,
    fetch_failed: usize,
    no_entrypoint: usize,
    probe_pass: usize,
    probe_fail: usize,
    probe_timeout: usize,
    probe_not_attempted: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunReport {
    schema: String,
    generated_at: String,
    candidate_pool: String,
    priority_json: String,
    code_search_summary: String,
    counts: ReportCounts,
    results: Vec<CandidateProbeResult>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeOneResult {
    status: String,
    error: Option<String>,
    registered_commands: usize,
    registered_tools: usize,
    registered_flags: usize,
    registered_providers: usize,
    duration_ms: u64,
}

#[derive(Debug, Clone)]
struct SharedRunConfig {
    repo_root: PathBuf,
    cache_dir: PathBuf,
    fetch_timeout_secs: u64,
    probe_timeout_secs: u64,
    max_scan_files: usize,
    max_file_bytes: u64,
    no_probe: bool,
    rank_map: Arc<HashMap<String, usize>>,
    repo_entry_hints: Arc<HashMap<String, String>>,
    exe_path: PathBuf,
}

#[derive(Debug)]
struct DetectResult {
    entrypoint: Option<PathBuf>,
    score: Option<i32>,
    scanned_files: usize,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CommandMode::RunAll(args) => run_all(args),
        CommandMode::ProbeOne(args) => run_probe_one(args),
    }
}

fn run_probe_one(args: ProbeOneArgs) -> Result<()> {
    let started = Instant::now();

    let mut out = ProbeOneResult {
        status: "fail".to_string(),
        error: None,
        registered_commands: 0,
        registered_tools: 0,
        registered_flags: 0,
        registered_providers: 0,
        duration_ms: 0,
    };

    let probe_res = probe_entry_with_runtime(&args.entry, &args.cwd);
    match probe_res {
        Ok((commands, tools, flags, providers)) => {
            out.status = "pass".to_string();
            out.registered_commands = commands;
            out.registered_tools = tools;
            out.registered_flags = flags;
            out.registered_providers = providers;
        }
        Err(err) => {
            out.status = "fail".to_string();
            out.error = Some(err.to_string());
        }
    }

    out.duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn run_all(args: RunAllArgs) -> Result<()> {
    if args.workers == 0 {
        bail!("--workers must be > 0");
    }

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let pool_path = repo_root.join(&args.candidate_pool);
    let pool: CandidatePool = read_json(&pool_path)
        .with_context(|| format!("read candidate pool: {}", pool_path.display()))?;

    let rank_map = load_rank_map(&repo_root.join(&args.priority_json)).unwrap_or_default();
    let repo_entry_hints =
        load_repo_entry_hints(&repo_root.join(&args.code_search_summary)).unwrap_or_default();

    fs::create_dir_all(repo_root.join(&args.cache_dir))
        .with_context(|| format!("create cache dir: {}", args.cache_dir.display()))?;

    let mut selected = pool
        .items
        .into_iter()
        .filter(|item| args.include_vendored || item.status.eq_ignore_ascii_case("unvendored"))
        .collect::<Vec<_>>();

    if !args.only_ids.is_empty() {
        let set = args
            .only_ids
            .iter()
            .map(|id| id.to_ascii_lowercase())
            .collect::<std::collections::BTreeSet<_>>();
        selected.retain(|item| set.contains(&item.id.to_ascii_lowercase()));
    }

    selected.sort_by(|a, b| {
        rank_map
            .get(&a.id)
            .copied()
            .unwrap_or(usize::MAX)
            .cmp(&rank_map.get(&b.id).copied().unwrap_or(usize::MAX))
            .then_with(|| a.id.cmp(&b.id))
    });

    if let Some(limit) = args.limit {
        selected.truncate(limit);
    }

    let counts_total = selected.len();

    let queue = Arc::new(Mutex::new(selected));
    let (tx, rx) = mpsc::channel::<CandidateProbeResult>();

    let shared = SharedRunConfig {
        repo_root: repo_root.clone(),
        cache_dir: repo_root.join(&args.cache_dir),
        fetch_timeout_secs: args.fetch_timeout_secs,
        probe_timeout_secs: args.probe_timeout_secs,
        max_scan_files: args.max_scan_files,
        max_file_bytes: args.max_file_bytes,
        no_probe: args.no_probe,
        rank_map: Arc::new(rank_map),
        repo_entry_hints: Arc::new(repo_entry_hints),
        exe_path: std::env::current_exe().context("resolve current executable path")?,
    };

    for _ in 0..args.workers {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        let shared = shared.clone();
        thread::spawn(move || {
            loop {
                let next = {
                    let Ok(mut guard) = queue.lock() else {
                        return;
                    };
                    guard.pop()
                };

                let Some(item) = next else {
                    return;
                };

                let result = process_candidate(&shared, &item);
                if tx.send(result).is_err() {
                    return;
                }
            }
        });
    }
    drop(tx);

    if let Some(parent) = args.out_json.parent() {
        fs::create_dir_all(repo_root.join(parent)).ok();
    }
    if let Some(parent) = args.out_jsonl.parent() {
        fs::create_dir_all(repo_root.join(parent)).ok();
    }

    let out_jsonl_path = repo_root.join(&args.out_jsonl);
    let out_json_path = repo_root.join(&args.out_json);

    let jsonl_file = File::create(&out_jsonl_path)
        .with_context(|| format!("create {}", out_jsonl_path.display()))?;
    let mut jsonl_writer = BufWriter::new(jsonl_file);

    let mut results = Vec::with_capacity(counts_total);
    for idx in 0..counts_total {
        let result = rx
            .recv()
            .context("worker channel closed before all results were produced")?;
        let line = serde_json::to_string(&result).context("serialize jsonl result")?;
        writeln!(jsonl_writer, "{line}").context("write jsonl result")?;
        results.push(result);
        let processed = idx + 1;
        if processed % 25 == 0 || processed == counts_total {
            let pct_tenths = processed
                .saturating_mul(1000)
                .checked_div(counts_total)
                .unwrap_or(0);
            let pct_whole = pct_tenths / 10;
            let pct_frac = pct_tenths % 10;
            eprintln!(
                "[ext_unvendored_fetch_run] progress {processed}/{counts_total} ({pct_whole}.{pct_frac}%)"
            );
        }
    }
    jsonl_writer.flush().context("flush jsonl output")?;

    results.sort_by(|a, b| {
        a.rank
            .unwrap_or(usize::MAX)
            .cmp(&b.rank.unwrap_or(usize::MAX))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut counts = ReportCounts {
        total_selected: results.len(),
        ..ReportCounts::default()
    };
    for result in &results {
        match result.fetch_state {
            FetchState::Cached => counts.cached += 1,
            FetchState::Fetched => counts.fetched += 1,
            FetchState::Failed => counts.fetch_failed += 1,
            FetchState::Skipped => {}
        }
        if result.entrypoint.is_none() {
            counts.no_entrypoint += 1;
        }
        match result.probe_state {
            ProbeState::Pass => counts.probe_pass += 1,
            ProbeState::Fail => counts.probe_fail += 1,
            ProbeState::Timeout => counts.probe_timeout += 1,
            ProbeState::NotAttempted => counts.probe_not_attempted += 1,
        }
    }

    let report = RunReport {
        schema: "pi.ext.unvendored_fetch_probe.v1".to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        candidate_pool: args.candidate_pool.display().to_string(),
        priority_json: args.priority_json.display().to_string(),
        code_search_summary: args.code_search_summary.display().to_string(),
        counts,
        results,
    };

    let report_json = serde_json::to_string_pretty(&report).context("serialize report")?;
    fs::write(&out_json_path, format!("{report_json}\n"))
        .with_context(|| format!("write {}", out_json_path.display()))?;

    eprintln!(
        "[ext_unvendored_fetch_run] wrote report: {}",
        out_json_path.display()
    );
    eprintln!(
        "[ext_unvendored_fetch_run] wrote events: {}",
        out_jsonl_path.display()
    );

    Ok(())
}

fn process_candidate(shared: &SharedRunConfig, item: &CandidateItem) -> CandidateProbeResult {
    let started = Instant::now();

    let mut out = CandidateProbeResult {
        id: item.id.clone(),
        source_tier: item.source_tier.clone(),
        source_type: source_type_label(&item.source).to_string(),
        status: item.status.clone(),
        rank: shared.rank_map.get(&item.id).copied(),
        fetch_state: FetchState::Skipped,
        fetch_error: None,
        local_root: None,
        entrypoint: None,
        entry_score: None,
        scanned_files: 0,
        probe_state: ProbeState::NotAttempted,
        probe_error: None,
        registered_commands: 0,
        registered_tools: 0,
        registered_flags: 0,
        registered_providers: 0,
        duration_ms: 0,
    };

    let fetched = fetch_candidate_source(shared, item);
    match fetched {
        Ok((state, root)) => {
            out.fetch_state = state;
            out.local_root = Some(display_rel_or_abs(&shared.repo_root, &root));

            let hint = repo_hint_for_item(&shared.repo_entry_hints, item).map(str::to_string);
            let detect = detect_entrypoint(
                &root,
                hint.as_deref(),
                shared.max_scan_files,
                shared.max_file_bytes,
            );
            out.scanned_files = detect.scanned_files;
            if let Some(entry) = detect.entrypoint {
                out.entrypoint = Some(display_rel_or_abs(&shared.repo_root, &entry));
                out.entry_score = detect.score;

                if !shared.no_probe {
                    match run_probe_subprocess(shared, &entry, &root) {
                        Ok(probe) => {
                            let status = probe.status.to_ascii_lowercase();
                            if status == "pass" {
                                out.probe_state = ProbeState::Pass;
                            } else {
                                out.probe_state = ProbeState::Fail;
                                out.probe_error.clone_from(&probe.error);
                            }
                            out.registered_commands = probe.registered_commands;
                            out.registered_tools = probe.registered_tools;
                            out.registered_flags = probe.registered_flags;
                            out.registered_providers = probe.registered_providers;
                        }
                        Err(err) => {
                            if err.contains("timeout") {
                                out.probe_state = ProbeState::Timeout;
                            } else {
                                out.probe_state = ProbeState::Fail;
                            }
                            out.probe_error = Some(err);
                        }
                    }
                }
            }
        }
        Err(err) => {
            out.fetch_state = FetchState::Failed;
            out.fetch_error = Some(err);
            out.probe_state = ProbeState::NotAttempted;
        }
    }

    out.duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    out
}

fn run_probe_subprocess(
    shared: &SharedRunConfig,
    entry: &Path,
    cwd: &Path,
) -> std::result::Result<ProbeOneResult, String> {
    let args = vec![
        "probe-one".to_string(),
        "--entry".to_string(),
        entry.display().to_string(),
        "--cwd".to_string(),
        cwd.display().to_string(),
    ];

    let output = run_command_with_timeout(
        shared.probe_timeout_secs.max(1),
        &shared.exe_path,
        &args,
        Some(&shared.repo_root),
    )
    .map_err(|e| e.to_string())?;

    if output.timed_out {
        return Err("probe timeout".to_string());
    }
    if !output.success {
        let stderr = output.stderr.trim();
        return Err(if stderr.is_empty() {
            format!("probe subprocess exit {:?}", output.exit_code)
        } else {
            stderr.to_string()
        });
    }

    let parsed: ProbeOneResult =
        serde_json::from_str(output.stdout.trim()).map_err(|e| format!("probe json parse: {e}"))?;
    Ok(parsed)
}

fn probe_entry_with_runtime(entry: &Path, cwd: &Path) -> Result<(usize, usize, usize, usize)> {
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], cwd, None));

    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        repair_mode: RepairMode::AutoStrict,
        ..Default::default()
    };

    let runtime = block_on(async {
        JsExtensionRuntimeHandle::start(js_config, Arc::clone(&tools), manager.clone()).await
    })
    .context("start JS runtime")?;

    manager.set_js_runtime(runtime);

    let spec = JsExtensionLoadSpec::from_entry_path(entry)
        .with_context(|| format!("build load spec from {}", entry.display()))?;

    let load_result = block_on(async { manager.load_js_extensions(vec![spec]).await });

    let shutdown_ok = block_on(async { manager.shutdown(Duration::from_secs(2)).await });
    if !shutdown_ok {
        tracing::warn!("extension manager shutdown did not complete within budget");
    }

    load_result?;

    let commands = manager.list_commands().len();
    let tools_count = manager.extension_tool_defs().len();
    let flags = manager.list_flags().len();
    let providers = manager.extension_providers().len();

    Ok((commands, tools_count, flags, providers))
}

fn fetch_candidate_source(
    shared: &SharedRunConfig,
    item: &CandidateItem,
) -> std::result::Result<(FetchState, PathBuf), String> {
    match &item.source {
        CandidateSource::Npm {
            package, version, ..
        } => {
            let id_dir = shared
                .cache_dir
                .join("npm")
                .join(sanitize_for_fs(&format!("{}-{}", item.id, version)));
            if id_dir.exists() && id_dir.join(".fetched").exists() {
                return Ok((FetchState::Cached, id_dir));
            }

            if let Some(parent) = id_dir.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let tarball_dir = shared.cache_dir.join("npm_tarballs");
            fs::create_dir_all(&tarball_dir).map_err(|e| e.to_string())?;

            let spec = format!("{package}@{version}");
            let pack_args = vec![
                "pack".to_string(),
                spec,
                "--pack-destination".to_string(),
                tarball_dir.display().to_string(),
                "--silent".to_string(),
            ];
            let packed = run_command_with_timeout(
                shared.fetch_timeout_secs,
                Path::new("npm"),
                &pack_args,
                Some(&shared.repo_root),
            )
            .map_err(|e| format!("npm pack failed: {e}"))?;

            if packed.timed_out {
                return Err("npm pack timeout".to_string());
            }
            if !packed.success {
                return Err(format!(
                    "npm pack exit {:?}: {}",
                    packed.exit_code,
                    truncate(&packed.stderr, 600)
                ));
            }

            let tar_name = packed
                .stdout
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(str::trim)
                .ok_or_else(|| "npm pack did not emit tarball name".to_string())?;

            let tar_path = tarball_dir.join(tar_name);
            if !tar_path.exists() {
                return Err(format!("packed tarball not found: {}", tar_path.display()));
            }

            if id_dir.exists() {
                fs::remove_dir_all(&id_dir).map_err(|e| e.to_string())?;
            }
            fs::create_dir_all(&id_dir).map_err(|e| e.to_string())?;

            let extract_args = vec![
                "-xzf".to_string(),
                tar_path.display().to_string(),
                "-C".to_string(),
                id_dir.display().to_string(),
                "--strip-components=1".to_string(),
            ];
            let extracted = run_command_with_timeout(
                shared.fetch_timeout_secs,
                Path::new("tar"),
                &extract_args,
                Some(&shared.repo_root),
            )
            .map_err(|e| format!("tar extract failed: {e}"))?;
            if extracted.timed_out {
                return Err("tar extract timeout".to_string());
            }
            if !extracted.success {
                return Err(format!(
                    "tar extract exit {:?}: {}",
                    extracted.exit_code,
                    truncate(&extracted.stderr, 600)
                ));
            }

            fs::write(
                id_dir.join(".fetched"),
                format!("npm:{package}@{version}\n"),
            )
            .map_err(|e| e.to_string())?;
            Ok((FetchState::Fetched, id_dir))
        }
        CandidateSource::Url { url } => {
            let repo_slug = github_repo_slug_from_url(url)
                .ok_or_else(|| format!("unsupported non-GitHub URL source: {url}"))?;
            fetch_github_repo(shared, url, &repo_slug)
        }
        CandidateSource::Git { repo, path } => {
            let repo_slug =
                github_repo_slug_from_url(repo).unwrap_or_else(|| sanitize_for_fs(repo));
            let (state, root) = fetch_github_repo(shared, repo, &repo_slug)?;
            if let Some(rel) = path
                && let Some(adjusted) = resolve_candidate_subpath(&root, rel)
                && adjusted.exists()
            {
                return Ok((state, adjusted));
            }
            Ok((state, root))
        }
    }
}

fn fetch_github_repo(
    shared: &SharedRunConfig,
    url: &str,
    repo_slug: &str,
) -> std::result::Result<(FetchState, PathBuf), String> {
    let id_dir = shared
        .cache_dir
        .join("github")
        .join(sanitize_for_fs(repo_slug));
    if id_dir.exists() && id_dir.join(".fetched").exists() {
        return Ok((FetchState::Cached, id_dir));
    }

    if id_dir.exists() {
        fs::remove_dir_all(&id_dir).map_err(|e| e.to_string())?;
    }
    if let Some(parent) = id_dir.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let clone_url = normalize_clone_url(url);
    let clone_args = vec![
        "clone".to_string(),
        "--depth".to_string(),
        "1".to_string(),
        "--filter=blob:none".to_string(),
        clone_url.clone(),
        id_dir.display().to_string(),
    ];
    let mut cloned = run_command_with_timeout(
        shared.fetch_timeout_secs,
        Path::new("git"),
        &clone_args,
        Some(&shared.repo_root),
    )
    .map_err(|e| format!("git clone failed: {e}"))?;

    if !cloned.success && !cloned.timed_out {
        // Retry without blob filter for older servers.
        let retry_args = vec![
            "clone".to_string(),
            "--depth".to_string(),
            "1".to_string(),
            clone_url,
            id_dir.display().to_string(),
        ];
        cloned = run_command_with_timeout(
            shared.fetch_timeout_secs,
            Path::new("git"),
            &retry_args,
            Some(&shared.repo_root),
        )
        .map_err(|e| format!("git clone retry failed: {e}"))?;
    }

    if cloned.timed_out {
        return Err("git clone timeout".to_string());
    }
    if !cloned.success {
        return Err(format!(
            "git clone exit {:?}: {}",
            cloned.exit_code,
            truncate(&cloned.stderr, 600)
        ));
    }

    fs::write(id_dir.join(".fetched"), format!("git:{url}\n")).map_err(|e| e.to_string())?;
    Ok((FetchState::Fetched, id_dir))
}

fn detect_entrypoint(
    root: &Path,
    repo_hint: Option<&str>,
    max_scan_files: usize,
    max_file_bytes: u64,
) -> DetectResult {
    let mut scanned_files = 0usize;
    let mut best_path: Option<PathBuf> = None;
    let mut best_score: i32 = i32::MIN;

    // Package.json hints first.
    let mut hinted = package_json_hints(root);
    if let Some(hint) = repo_hint {
        let p = root.join(hint);
        if p.exists() {
            hinted.push(p);
        }
    }
    hinted.sort();
    hinted.dedup();

    for hint in hinted {
        if !is_supported_source_file(&hint) {
            continue;
        }
        if let Ok(Some(score)) = score_source_file(&hint, max_file_bytes) {
            let score = score + 100;
            if score > best_score {
                best_score = score;
                best_path = Some(hint);
            }
        }
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if should_skip_dir(&name) {
                    continue;
                }
                stack.push(path);
                continue;
            }

            if scanned_files >= max_scan_files {
                break;
            }
            if !is_supported_source_file(&path) {
                continue;
            }

            scanned_files += 1;
            if let Ok(Some(score)) = score_source_file(&path, max_file_bytes)
                && score > best_score
            {
                best_score = score;
                best_path = Some(path);
            }
        }

        if scanned_files >= max_scan_files {
            break;
        }
    }

    DetectResult {
        entrypoint: best_path,
        score: if best_score == i32::MIN {
            None
        } else {
            Some(best_score)
        },
        scanned_files,
    }
}

fn score_source_file(path: &Path, max_file_bytes: u64) -> Result<Option<i32>> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.len() > max_file_bytes {
        return Ok(None);
    }

    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let (status, evidence) = classify_source_content(&text);

    let mut score = 0i32;
    if status == ValidationStatus::TrueExtension {
        score += 35;
    }
    if evidence.has_api_import {
        score += 15;
    }
    if evidence.has_export_default {
        score += 10;
    }
    if let Ok(registration_count) = i32::try_from(evidence.registrations.len()) {
        score = score.saturating_add(8_i32.saturating_mul(registration_count));
    }
    if text.contains("pi.") {
        score += 2;
    }

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        file_name.as_str(),
        "index.ts" | "index.js" | "extension.ts" | "extension.js" | "main.ts" | "main.js"
    ) {
        score += 5;
    }
    if path
        .to_string_lossy()
        .to_ascii_lowercase()
        .contains("extension")
    {
        score += 3;
    }

    if score <= 0 {
        Ok(None)
    } else {
        Ok(Some(score))
    }
}

fn package_json_hints(root: &Path) -> Vec<PathBuf> {
    let package_json = root.join("package.json");
    let Ok(bytes) = fs::read(&package_json) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };

    let mut hints = Vec::new();
    for key in ["main", "module"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            let p = normalize_hint_path(root, s);
            hints.push(p);
        }
    }

    if let Some(exports) = v.get("exports") {
        collect_export_paths(exports, &mut hints, root);
    }

    hints
}

fn collect_export_paths(value: &Value, out: &mut Vec<PathBuf>, root: &Path) {
    match value {
        Value::String(s) => out.push(normalize_hint_path(root, s)),
        Value::Object(map) => {
            for v in map.values() {
                collect_export_paths(v, out, root);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_export_paths(v, out, root);
            }
        }
        _ => {}
    }
}

fn normalize_hint_path(root: &Path, s: &str) -> PathBuf {
    let trimmed = s.trim();
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    root.join(trimmed)
}

const fn source_type_label(source: &CandidateSource) -> &'static str {
    match source {
        CandidateSource::Npm { .. } => "npm",
        CandidateSource::Url { .. } => "url",
        CandidateSource::Git { .. } => "git",
    }
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".turbo"
            | ".idea"
            | ".vscode"
    )
}

fn is_supported_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            let ext = ext.to_ascii_lowercase();
            SUPPORTED_EXTS.iter().any(|allowed| *allowed == ext)
        })
}

fn load_rank_map(path: &Path) -> Result<HashMap<String, usize>> {
    let doc: PriorityDoc = read_json(path)?;
    let mut map = HashMap::new();
    for item in doc.items {
        map.insert(item.id, item.rank);
    }
    Ok(map)
}

fn load_repo_entry_hints(path: &Path) -> Result<HashMap<String, String>> {
    let doc: CodeSearchSummary = read_json(path)?;
    let mut map = HashMap::new();
    for repo in doc.repos {
        if let Some(entry) = repo.entrypoint
            && !entry.trim().is_empty()
        {
            map.insert(repo.repo.to_ascii_lowercase(), entry);
        }
    }
    Ok(map)
}

fn repo_hint_for_item<'a>(
    hints: &'a HashMap<String, String>,
    item: &CandidateItem,
) -> Option<&'a str> {
    let repo = match &item.source {
        CandidateSource::Url { url } => github_repo_slug_from_url(url),
        CandidateSource::Git { repo, .. } => github_repo_slug_from_url(repo),
        CandidateSource::Npm { .. } => item
            .repository_url
            .as_deref()
            .and_then(github_repo_slug_from_url),
    }?;

    hints
        .get(&repo.to_ascii_lowercase())
        .map(std::string::String::as_str)
}

fn github_repo_slug_from_url(url: &str) -> Option<String> {
    let u = url.trim().trim_start_matches("git+");

    if let Some(rest) = u.strip_prefix("git@github.com:") {
        let slug = rest.trim_end_matches(".git").trim_matches('/');
        if slug.contains('/') {
            return Some(slug.to_string());
        }
    }

    let parsed = if u.contains("://") {
        url::Url::parse(u).ok()?
    } else {
        url::Url::parse(&format!("https://{u}")).ok()?
    };

    if !parsed.host_str()?.eq_ignore_ascii_case("github.com") {
        return None;
    }

    let mut segs = parsed.path_segments()?.filter(|s| !s.is_empty());
    let owner = segs.next()?;
    let repo = segs.next()?;
    Some(format!("{}/{}", owner, repo.trim_end_matches(".git")))
}

fn normalize_clone_url(url: &str) -> String {
    let trimmed = url.trim().trim_start_matches("git+");
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        if has_git_extension_case_insensitive(trimmed) {
            trimmed.to_string()
        } else {
            format!("{trimmed}.git")
        }
    } else if trimmed.starts_with("git@") {
        trimmed.to_string()
    } else {
        let host_hint = trimmed.split('/').next().unwrap_or_default();
        let as_https = if host_hint.contains('.') {
            format!("https://{trimmed}")
        } else {
            format!("https://github.com/{trimmed}")
        };
        if has_git_extension_case_insensitive(&as_https) {
            as_https
        } else {
            format!("{as_https}.git")
        }
    }
}

fn has_git_extension_case_insensitive(value: &str) -> bool {
    Path::new(value)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
}

fn sanitize_for_fs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

fn resolve_candidate_subpath(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    for comp in rel_path.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Some(root.join(rel_path))
}

#[derive(Debug)]
struct TimedOutput {
    success: bool,
    timed_out: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_command_with_timeout(
    timeout_secs: u64,
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<TimedOutput> {
    let mut cmd = Command::new("timeout");
    cmd.arg(format!("{}s", timeout_secs.max(1)))
        .arg(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    let output = cmd
        .output()
        .with_context(|| format!("spawn timeout {}", program.display()))?;

    let exit_code = output.status.code();
    let timed_out = exit_code == Some(124);
    let success = output.status.success();

    Ok(TimedOutput {
        success,
        timed_out,
        exit_code,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head = s.chars().take(max_chars).collect::<String>();
        format!("{head}…")
    }
}

fn display_rel_or_abs(repo_root: &Path, path: &Path) -> String {
    path.strip_prefix(repo_root)
        .map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
}
