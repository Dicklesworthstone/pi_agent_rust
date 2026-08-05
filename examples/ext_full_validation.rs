#![forbid(unsafe_code)]
#![allow(
    clippy::cast_precision_loss,
    clippy::struct_excessive_bools,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use clap::Parser;
use pi::extension_popularity::CandidatePool;
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(name = "ext_full_validation")]
#[command(about = "Run and aggregate end-to-end extension validation in one command")]
struct Args {
    /// Candidate pool containing vendored + unvendored extension candidates.
    #[arg(long, default_value = "docs/extension-candidate-pool.json")]
    candidate_pool: PathBuf,

    /// Validated (vendored) manifest used by conformance tests.
    #[arg(long, default_value = "tests/ext_conformance/VALIDATED_MANIFEST.json")]
    validated_manifest: PathBuf,

    /// Onboarding queue used to prioritize unvendored candidates.
    #[arg(long, default_value = "docs/extension-onboarding-queue.json")]
    onboarding_queue: PathBuf,

    /// Output JSON report path.
    #[arg(
        long,
        default_value = "tests/ext_conformance/reports/pipeline/full_validation_report.json"
    )]
    out_json: PathBuf,

    /// Output Markdown report path.
    #[arg(
        long,
        default_value = "tests/ext_conformance/reports/pipeline/full_validation_report.md"
    )]
    out_md: PathBuf,

    /// Number of conformance shards to run.
    #[arg(long, default_value_t = 4)]
    shards: usize,

    /// Thread count per shard run.
    #[arg(long, default_value_t = 4)]
    shard_parallelism: usize,

    /// Skip running cargo commands; only aggregate existing artifacts.
    #[arg(long, default_value_t = false)]
    aggregate_only: bool,

    /// Run differential suite (official/community/third-party).
    #[arg(long, default_value_t = false)]
    run_diff: bool,

    /// Include npm differential test (ignored by default upstream).
    #[arg(long, default_value_t = false)]
    run_npm_diff: bool,

    /// Stop immediately on first failed stage.
    #[arg(long, default_value_t = false)]
    fail_fast: bool,

    /// Confidence threshold below which entries are queued for manual review.
    #[arg(long, default_value_t = 0.8)]
    review_threshold: f64,

    /// Maximum number of review queue entries to highlight in Markdown.
    #[arg(long, default_value_t = 100)]
    max_review: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum StageStatus {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StageResult {
    name: String,
    command: Option<String>,
    status: StageStatus,
    exit_code: Option<i32>,
    duration_ms: u64,
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ManifestDoc {
    extensions: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct ManifestEntry {
    id: String,
    conformance_tier: u32,
}

#[derive(Debug, Deserialize)]
struct OnboardingQueueDoc {
    #[serde(default)]
    all: Vec<OnboardingQueueEntry>,
    #[serde(default, alias = "top500")]
    top_500: Vec<OnboardingQueueEntry>,
    #[serde(default, alias = "top300")]
    top_300: Vec<OnboardingQueueEntry>,
    #[serde(default, alias = "top100")]
    top_100: Vec<OnboardingQueueEntry>,
}

impl OnboardingQueueDoc {
    fn into_entries(self) -> Vec<OnboardingQueueEntry> {
        if !self.all.is_empty() {
            self.all
        } else if !self.top_500.is_empty() {
            self.top_500
        } else if !self.top_300.is_empty() {
            self.top_300
        } else {
            self.top_100
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct OnboardingQueueEntry {
    id: String,
    #[serde(default)]
    rank: Option<usize>,
    #[serde(default, alias = "piRelevant")]
    pi_relevant: Option<bool>,
    #[serde(default, alias = "piRelevanceScore")]
    pi_relevance_score: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ShardReport {
    #[serde(alias = "manifestCount")]
    manifest_count: usize,
    results: Vec<ShardResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ShardResult {
    id: String,
    tier: u32,
    status: String,
    #[serde(alias = "failureReason")]
    failure_reason: Option<String>,
    #[serde(alias = "failureCategory")]
    failure_category: Option<String>,
    #[serde(alias = "durationMs")]
    duration_ms: u64,
}

#[derive(Debug, Deserialize)]
struct DossierIndex {
    dossiers: Vec<DossierRecord>,
}

#[derive(Debug, Clone, Deserialize)]
struct DossierRecord {
    #[serde(alias = "extensionId")]
    extension_id: String,
    category: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AutoRepairSummary {
    total: usize,
    loaded: usize,
    #[serde(alias = "cleanPass")]
    clean_pass: usize,
    #[serde(alias = "repairedPass")]
    repaired_pass: usize,
    failed: usize,
    skipped: usize,
    #[serde(alias = "repairsByPattern")]
    repairs_by_pattern: BTreeMap<String, usize>,
    #[serde(alias = "perExtension")]
    per_extension: Vec<AutoRepairExtension>,
}

#[derive(Debug, Clone, Deserialize)]
struct AutoRepairExtension {
    id: String,
    loaded: bool,
    error: Option<String>,
    #[serde(alias = "repairEvents")]
    repair_events: Vec<RepairEventRecord>,
}

#[derive(Debug, Clone, Deserialize)]
struct RepairEventRecord {
    pattern: String,
}

#[derive(Debug, Deserialize)]
struct ScenarioSummary {
    counts: ScenarioCounts,
    results: Vec<ScenarioResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ScenarioCounts {
    total: usize,
    pass: usize,
    fail: usize,
    skip: usize,
    error: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct ScenarioResult {
    #[serde(alias = "extensionId")]
    extension_id: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct ProviderCompatReport {
    #[serde(alias = "totalCells")]
    total_cells: usize,
    #[serde(alias = "passedCells")]
    passed_cells: usize,
    #[serde(alias = "failedCells")]
    failed_cells: usize,
    #[serde(alias = "skippedCells")]
    skipped_cells: usize,
    #[serde(alias = "providerFailures")]
    provider_failures: Vec<ProviderFailure>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderFailure {
    #[serde(alias = "extensionId")]
    extension_id: String,
    #[serde(alias = "providerMode")]
    provider_mode: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Pass,
    PassWithAutoRepair,
    ProjectBug,
    ProjectLimitation,
    ExtensionProblem,
    HarnessGap,
    NotTestedUnvendored,
    NotExtension,
    NeedsReview,
}

#[derive(Debug, Clone)]
struct Classification {
    verdict: Verdict,
    confidence: f64,
    reason: String,
    suggested_fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionAssessment {
    id: String,
    source_tier: String,
    candidate_status: String,
    in_manifest: bool,
    conformance_tier: Option<u32>,
    conformance_status: Option<String>,
    conformance_failure_category: Option<String>,
    conformance_failure_reason: Option<String>,
    conformance_duration_ms: Option<u64>,
    auto_repair_event_count: usize,
    auto_repair_patterns: Vec<String>,
    scenario_pass: usize,
    scenario_fail: usize,
    scenario_error: usize,
    provider_failures: usize,
    provider_failure_modes: Vec<String>,
    onboarding_rank: Option<usize>,
    onboarding_pi_relevant: Option<bool>,
    onboarding_pi_relevance_score: Option<u32>,
    verdict: Verdict,
    confidence: f64,
    needs_review: bool,
    classification_reason: String,
    suggested_fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConformanceAggregate {
    shards_expected: usize,
    shards_loaded: usize,
    manifest_count: usize,
    results_total: usize,
    pass: usize,
    fail: usize,
    skip: usize,
    pass_rate_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_field_names)]
struct ProviderAggregate {
    total_cells: usize,
    passed_cells: usize,
    failed_cells: usize,
    skipped_cells: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AutoRepairAggregate {
    total: usize,
    loaded: usize,
    clean_pass: usize,
    repaired_pass: usize,
    failed: usize,
    skipped: usize,
    repairs_by_pattern: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunOptions {
    aggregate_only: bool,
    run_diff: bool,
    run_npm_diff: bool,
    shards: usize,
    shard_parallelism: usize,
    fail_fast: bool,
    review_threshold: f64,
    max_review: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StageSummary {
    passed: usize,
    failed: usize,
    skipped: usize,
}

type ScenarioByExtension = HashMap<String, (usize, usize, usize)>;
type ProviderFailuresByExtension = HashMap<String, Vec<ProviderFailure>>;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CorpusSummary {
    total_candidates: usize,
    vendored: usize,
    unvendored: usize,
    manifest_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PipelineReport {
    schema: String,
    generated_at: String,
    run_options: RunOptions,
    stage_results: Vec<StageResult>,
    stage_summary: StageSummary,
    corpus: CorpusSummary,
    conformance: Option<ConformanceAggregate>,
    scenario: Option<ScenarioCounts>,
    provider_compat: Option<ProviderAggregate>,
    auto_repair: Option<AutoRepairAggregate>,
    verdict_counts: BTreeMap<String, usize>,
    needs_review_count: usize,
    review_queue: Vec<ExtensionAssessment>,
    onboarding_hotlist: Vec<ExtensionAssessment>,
    extensions: Vec<ExtensionAssessment>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    if args.shards == 0 {
        bail!("--shards must be > 0");
    }
    if args.shard_parallelism == 0 {
        bail!("--shard-parallelism must be > 0");
    }
    if !(0.0..=1.0).contains(&args.review_threshold) {
        bail!("--review-threshold must be in [0.0, 1.0]");
    }
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let mut stages = Vec::new();
    if args.aggregate_only {
        stages.push(StageResult {
            name: "command_pipeline".to_string(),
            command: None,
            status: StageStatus::Skipped,
            exit_code: None,
            duration_ms: 0,
            notes: Some("aggregate-only mode enabled".to_string()),
        });
    } else {
        run_commands(&project_root, &args, &mut stages)?;
    }

    let report = aggregate_report(&project_root, &args, stages)?;
    write_report_outputs(&report, &args.out_json, &args.out_md)?;

    eprintln!(
        "[ext_full_validation] wrote report JSON: {}",
        args.out_json.display()
    );
    eprintln!(
        "[ext_full_validation] wrote report MD:   {}",
        args.out_md.display()
    );
    Ok(())
}

fn run_commands(project_root: &Path, args: &Args, stages: &mut Vec<StageResult>) -> Result<()> {
    let onboarding_cmd = vec![
        "run".to_string(),
        "--bin".to_string(),
        "ext_onboarding_queue".to_string(),
        "--".to_string(),
        "--candidate-pool".to_string(),
        args.candidate_pool.display().to_string(),
        "--json-out".to_string(),
        args.onboarding_queue.display().to_string(),
        "--md-out".to_string(),
        project_root
            .join("docs/extension-onboarding-queue.md")
            .display()
            .to_string(),
    ];
    run_stage(
        project_root,
        stages,
        "refresh_onboarding_queue",
        "cargo",
        onboarding_cmd,
        &[],
        args.fail_fast,
    )?;

    for shard_index in 0..args.shards {
        let env = vec![
            ("PI_SHARD_INDEX".to_string(), shard_index.to_string()),
            ("PI_SHARD_TOTAL".to_string(), args.shards.to_string()),
            (
                "PI_SHARD_PARALLELISM".to_string(),
                args.shard_parallelism.to_string(),
            ),
        ];
        let cmd = vec![
            "test".to_string(),
            "--test".to_string(),
            "ext_conformance_generated".to_string(),
            "--features".to_string(),
            "ext-conformance".to_string(),
            "--".to_string(),
            "conformance_sharded_matrix".to_string(),
            "--nocapture".to_string(),
            "--exact".to_string(),
        ];
        run_stage(
            project_root,
            stages,
            &format!("conformance_shard_{shard_index}"),
            "cargo",
            cmd,
            &env,
            args.fail_fast,
        )?;
    }

    run_stage(
        project_root,
        stages,
        "conformance_failure_dossiers",
        "cargo",
        vec![
            "test".to_string(),
            "--test".to_string(),
            "ext_conformance_generated".to_string(),
            "--features".to_string(),
            "ext-conformance".to_string(),
            "--".to_string(),
            "conformance_failure_dossiers".to_string(),
            "--nocapture".to_string(),
            "--exact".to_string(),
        ],
        &[],
        args.fail_fast,
    )?;

    run_stage(
        project_root,
        stages,
        "provider_compat_matrix",
        "cargo",
        vec![
            "test".to_string(),
            "--test".to_string(),
            "ext_conformance_generated".to_string(),
            "--features".to_string(),
            "ext-conformance".to_string(),
            "--".to_string(),
            "conformance_provider_compat_matrix".to_string(),
            "--nocapture".to_string(),
            "--exact".to_string(),
        ],
        &[],
        args.fail_fast,
    )?;

    run_stage(
        project_root,
        stages,
        "scenario_conformance_suite",
        "cargo",
        vec![
            "test".to_string(),
            "--test".to_string(),
            "ext_conformance_scenarios".to_string(),
            "--features".to_string(),
            "ext-conformance".to_string(),
            "--".to_string(),
            "scenario_conformance_suite".to_string(),
            "--nocapture".to_string(),
            "--exact".to_string(),
        ],
        &[],
        args.fail_fast,
    )?;

    run_stage(
        project_root,
        stages,
        "auto_repair_full_corpus",
        "cargo",
        vec![
            "test".to_string(),
            "--test".to_string(),
            "e2e_auto_repair".to_string(),
            "full_corpus_with_auto_repair".to_string(),
            "--".to_string(),
            "--nocapture".to_string(),
            "--exact".to_string(),
            "--test-threads=1".to_string(),
        ],
        &[],
        args.fail_fast,
    )?;

    if args.run_diff {
        run_stage(
            project_root,
            stages,
            "diff_official_manifest",
            "cargo",
            vec![
                "test".to_string(),
                "--test".to_string(),
                "ext_conformance_diff".to_string(),
                "--features".to_string(),
                "ext-conformance".to_string(),
                "--".to_string(),
                "diff_official_manifest".to_string(),
                "--nocapture".to_string(),
                "--exact".to_string(),
            ],
            &[],
            args.fail_fast,
        )?;

        run_stage(
            project_root,
            stages,
            "diff_community_manifest",
            "cargo",
            vec![
                "test".to_string(),
                "--test".to_string(),
                "ext_conformance_diff".to_string(),
                "--features".to_string(),
                "ext-conformance".to_string(),
                "--".to_string(),
                "diff_community_manifest".to_string(),
                "--nocapture".to_string(),
                "--exact".to_string(),
            ],
            &[],
            args.fail_fast,
        )?;

        run_stage(
            project_root,
            stages,
            "diff_thirdparty_manifest",
            "cargo",
            vec![
                "test".to_string(),
                "--test".to_string(),
                "ext_conformance_diff".to_string(),
                "--features".to_string(),
                "ext-conformance".to_string(),
                "--".to_string(),
                "diff_thirdparty_manifest".to_string(),
                "--nocapture".to_string(),
                "--exact".to_string(),
            ],
            &[],
            args.fail_fast,
        )?;

        if args.run_npm_diff {
            run_stage(
                project_root,
                stages,
                "diff_npm_manifest",
                "cargo",
                vec![
                    "test".to_string(),
                    "--test".to_string(),
                    "ext_conformance_diff".to_string(),
                    "--features".to_string(),
                    "ext-conformance".to_string(),
                    "--".to_string(),
                    "diff_npm_manifest".to_string(),
                    "--ignored".to_string(),
                    "--nocapture".to_string(),
                    "--exact".to_string(),
                ],
                &[],
                args.fail_fast,
            )?;
        } else {
            stages.push(StageResult {
                name: "diff_npm_manifest".to_string(),
                command: None,
                status: StageStatus::Skipped,
                exit_code: None,
                duration_ms: 0,
                notes: Some("skipped; pass --run-npm-diff to include".to_string()),
            });
        }
    } else {
        stages.push(StageResult {
            name: "differential_suite".to_string(),
            command: None,
            status: StageStatus::Skipped,
            exit_code: None,
            duration_ms: 0,
            notes: Some("skipped; pass --run-diff to include".to_string()),
        });
    }

    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn run_stage(
    project_root: &Path,
    stages: &mut Vec<StageResult>,
    name: &str,
    program: &str,
    args: Vec<String>,
    env: &[(String, String)],
    fail_fast: bool,
) -> Result<()> {
    let cmdline = format!("{program} {}", args.join(" "));
    eprintln!("\n[ext_full_validation] stage: {name}");
    eprintln!("[ext_full_validation] command: {cmdline}");

    let started = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(&args)
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (key, value) in env {
        cmd.env(key, value);
    }

    let status_result = cmd.status();
    let elapsed = started.elapsed();
    let elapsed_ms = elapsed
        .as_millis()
        .min(u128::from(u64::MAX))
        .try_into()
        .unwrap_or(u64::MAX);

    let (status, exit_code, notes) = match status_result {
        Ok(status) => (
            if status.success() {
                StageStatus::Pass
            } else {
                StageStatus::Fail
            },
            status.code(),
            None,
        ),
        Err(err) => (
            StageStatus::Fail,
            None,
            Some(format!("failed to start command: {err}")),
        ),
    };
    let stage = StageResult {
        name: name.to_string(),
        command: Some(cmdline),
        status,
        exit_code,
        duration_ms: elapsed_ms,
        notes,
    };

    let failed = matches!(stage.status, StageStatus::Fail);
    stages.push(stage);
    if failed && fail_fast {
        bail!("stage failed and --fail-fast is enabled: {name}");
    }

    Ok(())
}

fn aggregate_report(
    project_root: &Path,
    args: &Args,
    stages: Vec<StageResult>,
) -> Result<PipelineReport> {
    let pool: CandidatePool = read_json(&project_root.join(&args.candidate_pool))
        .with_context(|| format!("read candidate pool: {}", args.candidate_pool.display()))?;

    let manifest: ManifestDoc = read_json(&project_root.join(&args.validated_manifest))
        .with_context(|| format!("read manifest: {}", args.validated_manifest.display()))?;

    let onboarding_stage_ok =
        args.aggregate_only || stage_passed(&stages, "refresh_onboarding_queue");
    let onboarding: Option<OnboardingQueueDoc> = if onboarding_stage_ok {
        read_json_optional(&project_root.join(&args.onboarding_queue)).with_context(|| {
            format!("read onboarding queue: {}", args.onboarding_queue.display())
        })?
    } else {
        None
    };

    let conformance_stage_ok = args.aggregate_only
        || (0..args.shards).all(|idx| stage_passed(&stages, &format!("conformance_shard_{idx}")));
    let dossier_stage_ok =
        args.aggregate_only || stage_passed(&stages, "conformance_failure_dossiers");
    let auto_repair_stage_ok =
        args.aggregate_only || stage_passed(&stages, "auto_repair_full_corpus");
    let scenario_stage_ok =
        args.aggregate_only || stage_passed(&stages, "scenario_conformance_suite");
    let provider_stage_ok = args.aggregate_only || stage_passed(&stages, "provider_compat_matrix");

    let (conformance, conformance_map) = if conformance_stage_ok {
        load_sharded_conformance(project_root, args.shards)?
    } else {
        (None, HashMap::new())
    };
    let dossier_map = if dossier_stage_ok {
        load_dossier_map(project_root)?
    } else {
        HashMap::new()
    };
    let (auto_repair, repair_map) = if auto_repair_stage_ok {
        load_auto_repair(project_root)?
    } else {
        (None, HashMap::new())
    };
    let (scenario, scenario_counts_by_ext) = if scenario_stage_ok {
        load_scenarios(project_root)?
    } else {
        (None, HashMap::new())
    };
    let (provider, provider_failures_by_ext) = if provider_stage_ok {
        load_provider_compat(project_root)?
    } else {
        (None, HashMap::new())
    };

    let manifest_map: HashMap<String, ManifestEntry> = manifest
        .extensions
        .into_iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect();

    let queue_map: HashMap<String, OnboardingQueueEntry> = onboarding
        .map(|doc| {
            doc.into_entries()
                .into_iter()
                .map(|entry| (entry.id.clone(), entry))
                .collect()
        })
        .unwrap_or_default();

    let classify_ctx = ClassificationContext {
        manifest_map: &manifest_map,
        conformance_map: &conformance_map,
        dossier_map: &dossier_map,
        repair_map: &repair_map,
        scenario_by_ext: &scenario_counts_by_ext,
        provider_failures_by_ext: &provider_failures_by_ext,
        queue_map: &queue_map,
        review_threshold: args.review_threshold,
    };

    let mut assessments = pool
        .items
        .iter()
        .map(|item| classify_item(item, &classify_ctx))
        .collect::<Vec<_>>();

    assessments.sort_by(|left, right| left.id.cmp(&right.id));

    let mut verdict_counts: BTreeMap<String, usize> = BTreeMap::new();
    for assessment in &assessments {
        let key = serde_json::to_value(assessment.verdict)
            .ok()
            .and_then(|v| v.as_str().map(ToString::to_string))
            .unwrap_or_else(|| format!("{:?}", assessment.verdict));
        *verdict_counts.entry(key).or_insert(0) += 1;
    }

    let mut review_queue = assessments
        .iter()
        .filter(|entry| entry.needs_review)
        .cloned()
        .collect::<Vec<_>>();
    review_queue.sort_by(review_priority_cmp);

    let mut onboarding_hotlist = assessments
        .iter()
        .filter(|entry| entry.candidate_status.eq_ignore_ascii_case("unvendored"))
        .filter(|entry| entry.onboarding_pi_relevant.unwrap_or(false))
        .cloned()
        .collect::<Vec<_>>();
    onboarding_hotlist.sort_by(|left, right| {
        left.onboarding_rank
            .unwrap_or(usize::MAX)
            .cmp(&right.onboarding_rank.unwrap_or(usize::MAX))
            .then_with(|| left.id.cmp(&right.id))
    });

    let stage_summary = StageSummary {
        passed: stages
            .iter()
            .filter(|s| matches!(s.status, StageStatus::Pass))
            .count(),
        failed: stages
            .iter()
            .filter(|s| matches!(s.status, StageStatus::Fail))
            .count(),
        skipped: stages
            .iter()
            .filter(|s| matches!(s.status, StageStatus::Skipped))
            .count(),
    };

    let corpus = CorpusSummary {
        total_candidates: pool.items.len(),
        vendored: pool
            .items
            .iter()
            .filter(|i| i.status.eq_ignore_ascii_case("vendored"))
            .count(),
        unvendored: pool
            .items
            .iter()
            .filter(|i| i.status.eq_ignore_ascii_case("unvendored"))
            .count(),
        manifest_count: manifest_map.len(),
    };

    Ok(PipelineReport {
        schema: "pi.ext.full_validation.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        run_options: RunOptions {
            aggregate_only: args.aggregate_only,
            run_diff: args.run_diff,
            run_npm_diff: args.run_npm_diff,
            shards: args.shards,
            shard_parallelism: args.shard_parallelism,
            fail_fast: args.fail_fast,
            review_threshold: args.review_threshold,
            max_review: args.max_review,
        },
        stage_results: stages,
        stage_summary,
        corpus,
        conformance,
        scenario,
        provider_compat: provider,
        auto_repair,
        verdict_counts,
        needs_review_count: review_queue.len(),
        review_queue: review_queue.into_iter().take(args.max_review).collect(),
        onboarding_hotlist: onboarding_hotlist.into_iter().take(100).collect(),
        extensions: assessments,
    })
}

fn review_priority_cmp(
    left: &ExtensionAssessment,
    right: &ExtensionAssessment,
) -> std::cmp::Ordering {
    verdict_rank(left.verdict)
        .cmp(&verdict_rank(right.verdict))
        .then_with(|| {
            left.confidence
                .partial_cmp(&right.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            left.onboarding_rank
                .unwrap_or(usize::MAX)
                .cmp(&right.onboarding_rank.unwrap_or(usize::MAX))
        })
        .then_with(|| left.id.cmp(&right.id))
}

const fn verdict_rank(verdict: Verdict) -> u8 {
    match verdict {
        Verdict::ProjectBug => 0,
        Verdict::NeedsReview => 1,
        Verdict::ProjectLimitation => 2,
        Verdict::HarnessGap => 3,
        Verdict::ExtensionProblem => 4,
        Verdict::NotTestedUnvendored => 5,
        Verdict::NotExtension => 6,
        Verdict::PassWithAutoRepair => 7,
        Verdict::Pass => 8,
    }
}

struct ClassificationContext<'a> {
    manifest_map: &'a HashMap<String, ManifestEntry>,
    conformance_map: &'a HashMap<String, ShardResult>,
    dossier_map: &'a HashMap<String, DossierRecord>,
    repair_map: &'a HashMap<String, AutoRepairExtension>,
    scenario_by_ext: &'a ScenarioByExtension,
    provider_failures_by_ext: &'a ProviderFailuresByExtension,
    queue_map: &'a HashMap<String, OnboardingQueueEntry>,
    review_threshold: f64,
}

fn classify_item(
    item: &pi::extension_popularity::CandidateItem,
    ctx: &ClassificationContext<'_>,
) -> ExtensionAssessment {
    let manifest_entry = lookup_by_id_or_alias(ctx.manifest_map, item);
    let conformance = lookup_by_id_or_alias(ctx.conformance_map, item);
    let dossier = lookup_by_id_or_alias(ctx.dossier_map, item);
    let repair = lookup_by_id_or_alias(ctx.repair_map, item);
    let (scenario_pass, scenario_fail, scenario_error) =
        lookup_by_id_or_alias(ctx.scenario_by_ext, item)
            .copied()
            .unwrap_or((0, 0, 0));
    let provider_failures = ctx
        .provider_failures_by_ext
        .get(&item.id)
        .or_else(|| {
            item.aliases
                .iter()
                .find_map(|alias| ctx.provider_failures_by_ext.get(alias))
        })
        .cloned()
        .unwrap_or_default();
    let provider_failure_modes = dedup_sorted(
        provider_failures
            .iter()
            .map(|failure| failure.provider_mode.clone())
            .collect(),
    );
    let provider_failure_count = provider_failure_modes.len();
    let queue = lookup_by_id_or_alias(ctx.queue_map, item);

    let repair_patterns = repair.map_or_else(Vec::new, |entry| {
        let mut patterns = entry
            .repair_events
            .iter()
            .map(|event| event.pattern.clone())
            .collect::<Vec<_>>();
        patterns.sort();
        patterns.dedup();
        patterns
    });

    let failure_category = conformance
        .and_then(|result| result.failure_category.clone())
        .or_else(|| dossier.and_then(|record| record.category.clone()));

    let failure_reason = conformance
        .and_then(|result| result.failure_reason.clone())
        .or_else(|| dossier.and_then(|record| record.reason.clone()))
        .or_else(|| repair.and_then(|entry| entry.error.clone()));

    let classification = if item.status.eq_ignore_ascii_case("unvendored") {
        classify_unvendored(item, queue)
    } else {
        classify_vendored(
            manifest_entry,
            conformance,
            repair,
            scenario_fail,
            scenario_error,
            provider_failure_count,
            failure_category.as_deref(),
            failure_reason.as_deref(),
            item.notes.as_deref(),
        )
    };

    let needs_review = classification.confidence < ctx.review_threshold
        || matches!(
            classification.verdict,
            Verdict::NeedsReview | Verdict::ProjectBug
        );

    ExtensionAssessment {
        id: item.id.clone(),
        source_tier: item.source_tier.clone(),
        candidate_status: item.status.clone(),
        in_manifest: manifest_entry.is_some(),
        conformance_tier: manifest_entry.map(|entry| entry.conformance_tier),
        conformance_status: conformance.map(|result| result.status.clone()),
        conformance_failure_category: failure_category,
        conformance_failure_reason: failure_reason,
        conformance_duration_ms: conformance.map(|result| result.duration_ms),
        auto_repair_event_count: repair.map_or(0, |entry| entry.repair_events.len()),
        auto_repair_patterns: repair_patterns,
        scenario_pass,
        scenario_fail,
        scenario_error,
        provider_failures: provider_failure_count,
        provider_failure_modes,
        onboarding_rank: queue.and_then(|entry| entry.rank),
        onboarding_pi_relevant: queue.and_then(onboarding_entry_relevant),
        onboarding_pi_relevance_score: queue.and_then(|entry| entry.pi_relevance_score),
        verdict: classification.verdict,
        confidence: classification.confidence,
        needs_review,
        classification_reason: classification.reason,
        suggested_fix: classification.suggested_fix,
    }
}

fn classify_unvendored(
    item: &pi::extension_popularity::CandidateItem,
    queue: Option<&OnboardingQueueEntry>,
) -> Classification {
    let notes_lower = item
        .notes
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();

    if notes_lower.contains("no extensionapi default-export entrypoint")
        || notes_lower.contains("no extension entrypoint")
        || notes_lower.contains("not a pi extension")
        || notes_lower.contains("not an extension")
    {
        return Classification {
            verdict: Verdict::NotExtension,
            confidence: 0.99,
            reason: "Candidate appears to be non-extension package or lacks extension entrypoint."
                .to_string(),
            suggested_fix: None,
        };
    }

    let queue_note = queue.map_or_else(
        || "No onboarding rank available.".to_string(),
        |entry| {
            format!(
                "Onboarding rank {} (pi_relevant={}).",
                entry
                    .rank
                    .map_or_else(|| "n/a".to_string(), |rank| rank.to_string()),
                onboarding_entry_relevant(entry)
                    .map_or_else(|| "n/a".to_string(), |flag| flag.to_string())
            )
        },
    );

    Classification {
        verdict: Verdict::NotTestedUnvendored,
        confidence: 0.97,
        reason: format!(
            "Candidate is unvendored and has not gone through manifest+runtime validation. {queue_note}"
        ),
        suggested_fix: Some(
            "Vendor source artifact, add manifest entry, then run conformance shard suite."
                .to_string(),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn classify_vendored(
    manifest_entry: Option<&ManifestEntry>,
    conformance: Option<&ShardResult>,
    repair: Option<&AutoRepairExtension>,
    scenario_fail: usize,
    scenario_error: usize,
    provider_failures: usize,
    failure_category: Option<&str>,
    failure_reason: Option<&str>,
    item_notes: Option<&str>,
) -> Classification {
    if manifest_entry.is_none() {
        let notes = item_notes.unwrap_or_default().to_ascii_lowercase();
        if notes_indicate_non_extension(&notes) {
            return Classification {
                verdict: Verdict::NotExtension,
                confidence: 0.99,
                reason:
                    "Vendored candidate is intentionally excluded from direct ExtensionAPI conformance scope."
                        .to_string(),
                suggested_fix: None,
            };
        }

        return Classification {
            verdict: Verdict::HarnessGap,
            confidence: 0.84,
            reason:
                "Vendored candidate is not represented in VALIDATED_MANIFEST.json coverage."
                    .to_string(),
            suggested_fix: Some(
                "If this package is a direct ExtensionAPI entrypoint, add/repair manifest mapping and rerun conformance shards."
                    .to_string(),
            ),
        };
    }

    let Some(conformance) = conformance else {
        return Classification {
            verdict: Verdict::HarnessGap,
            confidence: 0.65,
            reason: "No conformance result found for manifest extension.".to_string(),
            suggested_fix: Some(
                "Run sharded conformance stage and ensure report artifacts exist.".to_string(),
            ),
        };
    };

    let status = conformance.status.trim().to_ascii_lowercase();
    if status == "pass" || status == "passed" {
        if let Some(repair_entry) = repair {
            let repair_error = repair_entry
                .error
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase();
            if !repair_entry.loaded
                && !repair_error.starts_with("artifact not found")
                && !repair_error.is_empty()
            {
                return Classification {
                    verdict: Verdict::NeedsReview,
                    confidence: 0.55,
                    reason: "Conformance passed but auto-repair run reported a load failure."
                        .to_string(),
                    suggested_fix: Some(
                        "Inspect auto-repair report and compare RepairMode::Off vs AutoStrict."
                            .to_string(),
                    ),
                };
            }
        }

        if scenario_fail > 0 {
            return Classification {
                verdict: Verdict::ProjectBug,
                confidence: 0.82,
                reason: format!(
                    "Registration conformance passed but {scenario_fail} scenario(s) failed at runtime."
                ),
                suggested_fix: Some(
                    "Investigate scenario diffs and runtime hostcall behavior.".to_string(),
                ),
            };
        }

        if scenario_error > 0 {
            return Classification {
                verdict: Verdict::NeedsReview,
                confidence: 0.7,
                reason: format!(
                    "Scenario conformance reported {scenario_error} runtime error(s) for this extension."
                ),
                suggested_fix: Some(
                    "Inspect scenario_conformance.json for error details and classify limitation vs bug."
                        .to_string(),
                ),
            };
        }

        if provider_failures > 0 {
            return Classification {
                verdict: Verdict::ProjectLimitation,
                confidence: 0.8,
                reason: format!(
                    "Extension passes by default but fails in {provider_failures} provider mode cell(s)."
                ),
                suggested_fix: Some(
                    "Add provider-mode compatibility shims or normalize API behavior.".to_string(),
                ),
            };
        }

        if let Some(repair_entry) = repair
            && repair_entry.loaded
            && !repair_entry.repair_events.is_empty()
        {
            return Classification {
                verdict: Verdict::PassWithAutoRepair,
                confidence: 0.98,
                reason: "Extension loads with auto-repair transformations.".to_string(),
                suggested_fix: Some(
                    "Promote proven safe repair patterns to auto-safe policy where appropriate."
                        .to_string(),
                ),
            };
        }

        return Classification {
            verdict: Verdict::Pass,
            confidence: 0.99,
            reason: "Extension passed conformance without requiring repair.".to_string(),
            suggested_fix: None,
        };
    }

    if status == "skip" || status == "skipped" {
        return Classification {
            verdict: Verdict::HarnessGap,
            confidence: 0.9,
            reason: "Conformance run skipped this extension (artifact or harness availability)."
                .to_string(),
            suggested_fix: Some(
                "Ensure artifact path is present and rerun conformance.".to_string(),
            ),
        };
    }

    let category = failure_category.unwrap_or("unknown").to_ascii_lowercase();
    let reason = failure_reason
        .unwrap_or("unknown failure")
        .to_ascii_lowercase();

    if category.contains("artifact_missing") {
        return Classification {
            verdict: Verdict::HarnessGap,
            confidence: 0.96,
            reason: "Conformance artifact missing for vendored extension.".to_string(),
            suggested_fix: Some(
                "Repair artifact checkout/path mapping in test harness.".to_string(),
            ),
        };
    }

    if category.contains("load_spec_error") {
        return Classification {
            verdict: Verdict::ExtensionProblem,
            confidence: 0.9,
            reason: "Extension entrypoint cannot be converted into load spec.".to_string(),
            suggested_fix: Some("Fix extension packaging/entrypoint metadata.".to_string()),
        };
    }

    if category.contains("runtime_start_error") {
        return Classification {
            verdict: Verdict::ProjectBug,
            confidence: 0.9,
            reason: "QuickJS runtime failed to initialize for this extension run.".to_string(),
            suggested_fix: Some(
                "Debug runtime init path and deterministic runtime config.".to_string(),
            ),
        };
    }

    if category.contains("manifest_missing") || category.contains("registration_mismatch") {
        return Classification {
            verdict: Verdict::HarnessGap,
            confidence: 0.86,
            reason: "Observed registration output diverges from manifest expectations.".to_string(),
            suggested_fix: Some(
                "Refresh expected snapshot from TS oracle and re-validate.".to_string(),
            ),
        };
    }

    if category.contains("extension_load_error") {
        if reason.contains("module not found")
            || reason.contains("cannot find module")
            || reason.contains("cannot resolve")
            || reason.contains("missing package")
            || reason.contains("import")
        {
            return Classification {
                verdict: Verdict::ProjectLimitation,
                confidence: 0.9,
                reason:
                    "Extension depends on modules/packages not yet provided by runtime shim layer."
                        .to_string(),
                suggested_fix: Some(
                    "Add virtual module stubs or dependency bridge for missing package imports."
                        .to_string(),
                ),
            };
        }

        if reason.contains("enoent")
            || reason.contains("no such file")
            || reason.contains("readfilesync")
        {
            return Classification {
                verdict: Verdict::ExtensionProblem,
                confidence: 0.78,
                reason: "Extension expects local assets/files unavailable at runtime.".to_string(),
                suggested_fix: Some(
                    "Bundle required assets or extend missing_asset auto-repair policy."
                        .to_string(),
                ),
            };
        }

        return Classification {
            verdict: Verdict::NeedsReview,
            confidence: 0.45,
            reason:
                "Extension load failure could not be cleanly mapped to limitation vs extension bug."
                    .to_string(),
            suggested_fix: Some("Inspect failure dossier and reproduce command.".to_string()),
        };
    }

    Classification {
        verdict: Verdict::NeedsReview,
        confidence: 0.4,
        reason: "Unknown failure category; manual triage required.".to_string(),
        suggested_fix: Some("Use generated dossier to classify root cause.".to_string()),
    }
}

fn load_sharded_conformance(
    project_root: &Path,
    shards_expected: usize,
) -> Result<(Option<ConformanceAggregate>, HashMap<String, ShardResult>)> {
    let report_dir = project_root.join("tests/ext_conformance/reports/sharded");
    let mut shard_reports = Vec::new();

    for shard_index in 0..shards_expected {
        let path = report_dir.join(format!("shard_{shard_index}_report.json"));
        let report: Option<ShardReport> = read_json_optional(&path)?;
        if let Some(report) = report {
            shard_reports.push(report);
        }
    }

    if shard_reports.is_empty() {
        return Ok((None, HashMap::new()));
    }

    let mut manifest_count = 0usize;
    let mut result_map = HashMap::new();
    for shard in &shard_reports {
        manifest_count = manifest_count.max(shard.manifest_count);
        for result in &shard.results {
            result_map
                .entry(result.id.clone())
                .and_modify(|existing| {
                    *existing = prefer_shard_result(existing, result);
                })
                .or_insert_with(|| result.clone());
        }
    }

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut skip = 0usize;
    for result in result_map.values() {
        let status = result.status.trim().to_ascii_lowercase();
        match status.as_str() {
            "pass" | "passed" => pass += 1,
            "fail" | "failed" => fail += 1,
            _ => skip += 1,
        }
    }

    let tested = pass + fail;
    let pass_rate = if tested == 0 {
        0.0
    } else {
        (pass as f64 / tested as f64) * 100.0
    };

    Ok((
        Some(ConformanceAggregate {
            shards_expected,
            shards_loaded: shard_reports.len(),
            manifest_count,
            results_total: result_map.len(),
            pass,
            fail,
            skip,
            pass_rate_pct: pass_rate,
        }),
        result_map,
    ))
}

fn load_dossier_map(project_root: &Path) -> Result<HashMap<String, DossierRecord>> {
    let path = project_root.join("tests/ext_conformance/reports/dossiers/dossier_index.json");
    let index: Option<DossierIndex> = read_json_optional(&path)?;
    Ok(index
        .map(|doc| {
            doc.dossiers
                .into_iter()
                .map(|record| (record.extension_id.clone(), record))
                .collect()
        })
        .unwrap_or_default())
}

fn load_auto_repair(
    project_root: &Path,
) -> Result<(
    Option<AutoRepairAggregate>,
    HashMap<String, AutoRepairExtension>,
)> {
    let path = project_root.join("tests/ext_conformance/reports/auto_repair_summary.json");
    let summary: Option<AutoRepairSummary> = read_json_optional(&path)?;
    let Some(AutoRepairSummary {
        total,
        loaded,
        clean_pass,
        repaired_pass,
        failed,
        skipped,
        repairs_by_pattern,
        per_extension,
    }) = summary
    else {
        return Ok((None, HashMap::new()));
    };

    let map = per_extension
        .iter()
        .cloned()
        .map(|entry| (entry.id.clone(), entry))
        .collect::<HashMap<_, _>>();

    Ok((
        Some(AutoRepairAggregate {
            total,
            loaded,
            clean_pass,
            repaired_pass,
            failed,
            skipped,
            repairs_by_pattern,
        }),
        map,
    ))
}

fn load_scenarios(project_root: &Path) -> Result<(Option<ScenarioCounts>, ScenarioByExtension)> {
    let path = project_root.join("tests/ext_conformance/reports/scenario_conformance.json");
    let summary: Option<ScenarioSummary> = read_json_optional(&path)?;
    let Some(summary) = summary else {
        return Ok((None, HashMap::new()));
    };

    let mut by_ext: ScenarioByExtension = HashMap::new();
    for result in &summary.results {
        let entry = by_ext
            .entry(result.extension_id.clone())
            .or_insert((0, 0, 0));
        let status = result.status.trim();
        if status.eq_ignore_ascii_case("pass") || status.eq_ignore_ascii_case("passed") {
            entry.0 += 1;
        } else if status.eq_ignore_ascii_case("fail") || status.eq_ignore_ascii_case("failed") {
            entry.1 += 1;
        } else if status.eq_ignore_ascii_case("error") || status.eq_ignore_ascii_case("errored") {
            entry.2 += 1;
        }
    }

    Ok((Some(summary.counts), by_ext))
}

fn load_provider_compat(
    project_root: &Path,
) -> Result<(Option<ProviderAggregate>, ProviderFailuresByExtension)> {
    let path = project_root
        .join("tests/ext_conformance/reports/provider_compat/provider_compat_report.json");
    let report: Option<ProviderCompatReport> = read_json_optional(&path)?;

    let mut by_ext: ProviderFailuresByExtension = HashMap::new();
    if let Some(report) = &report {
        for failure in &report.provider_failures {
            by_ext
                .entry(failure.extension_id.clone())
                .or_default()
                .push(ProviderFailure {
                    extension_id: failure.extension_id.clone(),
                    provider_mode: failure.provider_mode.to_ascii_lowercase(),
                });
        }
    }

    // Augment provider-specific failures from events. We only treat non-default
    // failures as provider-specific when default mode passes for that extension.
    let events_path = project_root
        .join("tests/ext_conformance/reports/provider_compat/provider_compat_events.jsonl");
    if events_path.exists() {
        let file = fs::File::open(&events_path)
            .with_context(|| format!("open {}", events_path.display()))?;
        let reader = BufReader::new(file);
        let mut event_state: HashMap<String, (bool, BTreeSet<String>)> = HashMap::new();
        for line in reader.lines() {
            let line = line.with_context(|| format!("read {}", events_path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<ProviderCompatEvent>(&line) else {
                continue;
            };
            let status = event.status.trim().to_ascii_lowercase();
            let mode = event.provider_mode.trim().to_ascii_lowercase();
            let entry = event_state
                .entry(event.extension_id)
                .or_insert_with(|| (false, BTreeSet::new()));
            if mode == "default" && (status == "pass" || status == "passed") {
                entry.0 = true;
            } else if mode != "default" && (status == "fail" || status == "failed") {
                entry.1.insert(mode);
            }
        }

        for (extension_id, (default_pass, non_default_fail_modes)) in event_state {
            if !default_pass || non_default_fail_modes.is_empty() {
                continue;
            }
            let failures = by_ext.entry(extension_id.clone()).or_default();
            for mode in non_default_fail_modes {
                failures.push(ProviderFailure {
                    extension_id: extension_id.clone(),
                    provider_mode: mode,
                });
            }
        }
    }

    let aggregate = report.map(|report| ProviderAggregate {
        total_cells: report.total_cells,
        passed_cells: report.passed_cells,
        failed_cells: report.failed_cells,
        skipped_cells: report.skipped_cells,
    });

    for failures in by_ext.values_mut() {
        failures.sort_by(|left, right| left.provider_mode.cmp(&right.provider_mode));
        failures.dedup_by(|left, right| left.provider_mode == right.provider_mode);
    }

    Ok((aggregate, by_ext))
}

#[derive(Debug, Deserialize)]
struct ProviderCompatEvent {
    #[serde(alias = "extensionId")]
    extension_id: String,
    #[serde(alias = "providerMode")]
    provider_mode: String,
    status: String,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn read_json_optional<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    read_json(path).map(Some)
}

fn write_report_outputs(report: &PipelineReport, out_json: &Path, out_md: &Path) -> Result<()> {
    if let Some(parent) = out_json.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if let Some(parent) = out_md.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(report).context("serialize pipeline report")?;
    fs::write(out_json, format!("{json}\n"))
        .with_context(|| format!("write {}", out_json.display()))?;

    let md = render_markdown(report, out_json, out_md);
    fs::write(out_md, md).with_context(|| format!("write {}", out_md.display()))?;
    Ok(())
}

fn render_markdown(report: &PipelineReport, out_json: &Path, out_md: &Path) -> String {
    let mut md = String::new();
    md.push_str("# Full Extension Validation Report\n\n");
    let _ = writeln!(md, "- Generated: `{}`", report.generated_at);
    let _ = writeln!(md, "- Schema: `{}`", report.schema);
    let _ = writeln!(md, "- JSON output: `{}`", out_json.display());
    let _ = writeln!(md, "- Markdown output: `{}`", out_md.display());
    md.push('\n');

    md.push_str("## Stage Summary\n\n");
    md.push_str("| Metric | Value |\n|---|---|\n");
    let _ = writeln!(md, "| Passed stages | {} |", report.stage_summary.passed);
    let _ = writeln!(md, "| Failed stages | {} |", report.stage_summary.failed);
    let _ = writeln!(md, "| Skipped stages | {} |", report.stage_summary.skipped);
    md.push('\n');

    md.push_str("| Stage | Status | Exit | Duration (ms) | Notes |\n|---|---|---:|---:|---|\n");
    for stage in &report.stage_results {
        let status = match stage.status {
            StageStatus::Pass => "pass",
            StageStatus::Fail => "fail",
            StageStatus::Skipped => "skipped",
        };
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} |",
            markdown_table_cell(stage.name.as_str()),
            status,
            stage
                .exit_code
                .map_or_else(|| "-".to_string(), |code| code.to_string()),
            stage.duration_ms,
            markdown_table_cell(stage.notes.as_deref().unwrap_or("-"))
        );
    }
    md.push('\n');

    md.push_str("## Corpus\n\n");
    md.push_str("| Metric | Value |\n|---|---|\n");
    let _ = writeln!(
        md,
        "| Total candidates | {} |",
        report.corpus.total_candidates
    );
    let _ = writeln!(md, "| Vendored | {} |", report.corpus.vendored);
    let _ = writeln!(md, "| Unvendored | {} |", report.corpus.unvendored);
    let _ = writeln!(md, "| Manifest count | {} |", report.corpus.manifest_count);
    md.push('\n');

    if let Some(conformance) = &report.conformance {
        md.push_str("## Conformance Aggregate\n\n");
        md.push_str("| Metric | Value |\n|---|---|\n");
        let _ = writeln!(md, "| Shards expected | {} |", conformance.shards_expected);
        let _ = writeln!(md, "| Shards loaded | {} |", conformance.shards_loaded);
        let _ = writeln!(md, "| Results total | {} |", conformance.results_total);
        let _ = writeln!(md, "| Pass | {} |", conformance.pass);
        let _ = writeln!(md, "| Fail | {} |", conformance.fail);
        let _ = writeln!(md, "| Skip | {} |", conformance.skip);
        let _ = writeln!(md, "| Pass rate | {:.1}% |", conformance.pass_rate_pct);
        md.push('\n');
    }

    md.push_str("## Verdict Breakdown\n\n");
    md.push_str("| Verdict | Count |\n|---|---:|\n");
    for (verdict, count) in &report.verdict_counts {
        let _ = writeln!(md, "| {verdict} | {count} |");
    }
    let _ = writeln!(
        md,
        "\n- Needs review count: `{}`",
        report.needs_review_count
    );
    md.push('\n');

    md.push_str("## Review Queue\n\n");
    if report.review_queue.is_empty() {
        md.push_str("_No review items._\n\n");
    } else {
        md.push_str("| Extension | Verdict | Confidence | Reason |\n|---|---|---:|---|\n");
        for item in &report.review_queue {
            let _ = writeln!(
                md,
                "| {} | {} | {:.2} | {} |",
                markdown_table_cell(item.id.as_str()),
                verdict_label(item.verdict),
                item.confidence,
                markdown_table_cell(item.classification_reason.as_str())
            );
        }
        md.push('\n');
    }

    md.push_str("## Onboarding Hotlist\n\n");
    if report.onboarding_hotlist.is_empty() {
        md.push_str("_No pi-relevant unvendored entries found._\n");
    } else {
        md.push_str("| Extension | Rank | Relevance | Status |\n|---|---:|---:|---|\n");
        for item in &report.onboarding_hotlist {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} |",
                markdown_table_cell(item.id.as_str()),
                item.onboarding_rank
                    .map_or_else(|| "-".to_string(), |v| v.to_string()),
                item.onboarding_pi_relevance_score
                    .map_or_else(|| "-".to_string(), |v| v.to_string()),
                markdown_table_cell(item.candidate_status.as_str())
            );
        }
    }

    md
}

fn stage_passed(stages: &[StageResult], name: &str) -> bool {
    stages
        .iter()
        .find(|stage| stage.name == name)
        .is_some_and(|stage| matches!(stage.status, StageStatus::Pass))
}

fn lookup_by_id_or_alias<'a, T>(
    map: &'a HashMap<String, T>,
    item: &pi::extension_popularity::CandidateItem,
) -> Option<&'a T> {
    if let Some(value) = map.get(&item.id) {
        return Some(value);
    }

    if let Some(value) = item
        .aliases
        .iter()
        .find_map(|alias| map.get(alias.as_str()))
    {
        return Some(value);
    }

    derive_candidate_id_variants(item)
        .into_iter()
        .find_map(|candidate| map.get(&candidate))
}

fn derive_candidate_id_variants(item: &pi::extension_popularity::CandidateItem) -> Vec<String> {
    let mut out = Vec::new();
    push_candidate_id_variants(&item.id, &mut out);
    for alias in &item.aliases {
        push_candidate_id_variants(alias, &mut out);
    }
    dedup_sorted(out)
}

fn push_candidate_id_variants(raw: &str, out: &mut Vec<String>) {
    if let Some(rest) = raw.strip_prefix("npm/@") {
        // Normalize scoped npm IDs to the flattened artifact/manifest convention:
        // npm/@scope/name -> npm/scope-name
        out.push(format!("npm/{}", rest.replace('/', "-")));
    }

    if let Some(rest) = raw.strip_prefix("npm:@") {
        out.push(format!("npm/{}", rest.replace('/', "-")));
    }
}

fn notes_indicate_non_extension(notes_lower: &str) -> bool {
    [
        "no extensionapi default-export entrypoint",
        "no extension entrypoint",
        "not a pi extension",
        "not an extension",
        "customtoolfactory",
        "toolapi tool factory",
        "tool factories",
        "not tracked as a direct extensionapi",
        "excluded from direct extensionapi",
        "kept out of master catalog",
        "typed git utility library surface",
    ]
    .iter()
    .any(|marker| notes_lower.contains(marker))
}

fn onboarding_entry_relevant(entry: &OnboardingQueueEntry) -> Option<bool> {
    entry
        .pi_relevant
        .or_else(|| entry.pi_relevance_score.map(|score| score > 0))
}

fn dedup_sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn markdown_table_cell(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('\r', "")
        .replace('\n', "<br/>")
}

const fn shard_status_rank(status: &str) -> u8 {
    if status.eq_ignore_ascii_case("fail") || status.eq_ignore_ascii_case("failed") {
        0
    } else if status.eq_ignore_ascii_case("pass") || status.eq_ignore_ascii_case("passed") {
        1
    } else {
        2
    }
}

fn prefer_shard_result(existing: &ShardResult, candidate: &ShardResult) -> ShardResult {
    let existing_rank = shard_status_rank(existing.status.as_str());
    let candidate_rank = shard_status_rank(candidate.status.as_str());
    if candidate_rank < existing_rank {
        return candidate.clone();
    }
    if candidate_rank > existing_rank {
        return existing.clone();
    }

    if candidate.duration_ms >= existing.duration_ms {
        candidate.clone()
    } else {
        existing.clone()
    }
}

fn verdict_label(verdict: Verdict) -> String {
    serde_json::to_value(verdict)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| format!("{verdict:?}"))
}
