#![forbid(unsafe_code)]
#![allow(clippy::too_many_lines)]

use asupersync::runtime::RuntimeBuilder;
use asupersync::sync::Mutex;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use pi::agent::{Agent, AgentConfig, AgentSession, SemanticContextBundleInjection};
use pi::compaction::ResolvedCompactionSettings;
use pi::model::{AssistantMessage, ContentBlock, Message, StopReason, TextContent, Usage};
use pi::provider::{Context, Provider, StreamEvent, StreamOptions};
use pi::semantic_workspace_graph::{
    BeadActionabilityStatus, ContextArtifactCacheScope, ContextArtifactCacheStatus,
    ContextBundleBudget, ContextBundleCacheProbe, ContextBundleRequest, EvidenceFreshnessStatus,
    GraphInputStatus, RedactionStatus, SemanticContextBundlePlanner, SemanticEdgeType,
    SemanticNodeType, SemanticWorkspaceGraph, SemanticWorkspaceGraphBuildOptions,
    SemanticWorkspaceGraphBuilder, classify_evidence_freshness, normalize_context_artifact_path,
};
use pi::session::Session;
use pi::tools::ToolRegistry;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn reference_time() -> TestResult<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339("2026-05-13T00:00:00Z")?.with_timezone(&Utc))
}

fn write_fixture(root: &Path, relative_path: &str, content: &str) -> TestResult {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn run_fixture_git(root: &Path, args: &[&str]) -> TestResult {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: stdout={} stderr={}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

fn fixture_git_output(root: &Path, args: &[&str]) -> TestResult<String> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: stdout={} stderr={}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn initialize_fixture_git_workspace(root: &Path) -> TestResult {
    run_fixture_git(root, &["init", "-b", "main"])?;
    run_fixture_git(
        root,
        &["config", "user.email", "pi-context-e2e@example.invalid"],
    )?;
    run_fixture_git(root, &["config", "user.name", "Pi Context E2E"])?;
    run_fixture_git(root, &["add", "."])?;
    run_fixture_git(root, &["commit", "-m", "fixture baseline"])?;
    Ok(())
}

fn canonical_dropin_contract_fixture() -> serde_json::Value {
    let hard_gates = (1..=12)
        .map(|number| {
            json!({
                "gate_id": format!("G{number:02}-fixture-gate"),
                "blocking": number != 6,
                "owner_issue_primary": format!("bd-gate-{number:02}"),
                "required_artifacts": [format!("docs/evidence/gate-{number:02}.json")]
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema": "pi.dropin.certification_contract.v1",
        "hard_gates": hard_gates,
        "release_process_enforcement": {
            "verdict_artifact_contract": {
                "path": "docs/evidence/dropin-certification-verdict.json",
                "schema": "pi.dropin.certification_verdict.v1",
                "required_fields": [
                    "git_commit",
                    "generated_at_utc",
                    "overall_verdict",
                    "hard_gate_results",
                    "blocking_reasons",
                    "evidence_index"
                ]
            }
        }
    })
}

fn canonical_certification_lane_fixture(generated_at: &str) -> TestResult<serde_json::Value> {
    let canonical_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/full_suite_gate/certification_verdict.json");
    let canonical: serde_json::Value = serde_json::from_slice(&fs::read(canonical_path)?)?;
    let mut gates = canonical["gates"]
        .as_array()
        .ok_or("canonical certification lane gates must be an array")?
        .clone();
    for gate in &mut gates {
        gate["status"] = json!("pass");
        gate.as_object_mut()
            .ok_or("canonical certification lane gate must be an object")?
            .remove("detail");
    }
    let blocking_total = gates
        .iter()
        .filter(|gate| gate["blocking"].as_bool() == Some(true))
        .count();
    let total_gates = gates.len();
    Ok(json!({
        "schema": "pi.ci.certification_lane.v1",
        "lane": "full",
        "generated_at": generated_at,
        "verdict": "pass",
        "policy": "Full certification: all blocking gates must pass for release. Waived gates are tracked but do not block. Expired waivers fail the waiver_lifecycle gate.",
        "gates": gates,
        "waiver_audit": {
            "schema": "pi.ci.waiver_audit.v1",
            "generated_at": generated_at,
            "total_waivers": 0,
            "active": 0,
            "expired": 0,
            "expiring_soon": 0,
            "invalid": 0,
            "waivers": [],
            "raw_waivers": []
        },
        "waivers_applied": [],
        "summary": {
            "total_gates": total_gates,
            "passed": total_gates,
            "failed": 0,
            "warned": 0,
            "skipped": 0,
            "waived": 0,
            "blocking_pass": blocking_total,
            "blocking_total": blocking_total,
            "all_blocking_pass": true
        },
        "promotion_rules": {
            "can_promote": true,
            "blocker_gates": [],
            "waiver_gates": [],
            "conditions": ["All blocking gates pass (including waivers)"]
        },
        "rerun_guidance": {
            "preflight_command": "cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact",
            "full_command": "cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact",
            "single_gate_template": "See reproduce_command field on each gate"
        }
    }))
}

fn commit_fixture_path(root: &Path, path: &str, message: &str) -> TestResult {
    run_fixture_git(root, &["add", path])?;
    run_fixture_git(root, &["commit", "-m", message])?;
    Ok(())
}

fn install_canonical_dropin_claim_fixture(root: &Path) -> TestResult {
    let lane = canonical_certification_lane_fixture("2026-05-13T00:00:00.000Z")?;
    install_canonical_dropin_claim_fixture_with_lane(root, &lane)
}

fn install_canonical_dropin_claim_fixture_with_lane(
    root: &Path,
    lane: &serde_json::Value,
) -> TestResult {
    let contract = canonical_dropin_contract_fixture();
    write_fixture(
        root,
        "docs/contracts/dropin-certification-contract.json",
        &serde_json::to_string_pretty(&contract)?,
    )?;
    for number in 1..=12 {
        write_fixture(
            root,
            &format!("docs/evidence/gate-{number:02}.json"),
            &serde_json::to_string_pretty(&json!({
                "schema": "fixture.dropin_gate.v1",
                "gate": number,
                "status": "pass"
            }))?,
        )?;
    }
    write_fixture(
        root,
        "tests/full_suite_gate/certification_verdict.json",
        &serde_json::to_string_pretty(lane)?,
    )?;
    initialize_fixture_git_workspace(root)?;
    let source_commit = fixture_git_output(root, &["rev-parse", "HEAD"])?;

    let hard_gate_results = contract["hard_gates"]
        .as_array()
        .ok_or("fixture contract hard_gates must be an array")?
        .iter()
        .map(|gate| {
            json!({
                "gate_id": gate["gate_id"],
                "status": "pass",
                "blocking": gate["blocking"],
                "detail": null,
                "artifact_paths": gate["required_artifacts"],
                "bead": gate["owner_issue_primary"]
            })
        })
        .collect::<Vec<_>>();
    let evidence_index = contract["hard_gates"]
        .as_array()
        .ok_or("fixture contract hard_gates must be an array")?
        .iter()
        .map(|gate| {
            json!({
                "path": gate["required_artifacts"][0],
                "exists": true
            })
        })
        .collect::<Vec<_>>();
    let verdict = json!({
        "schema": "pi.dropin.certification_verdict.v1",
        "git_commit": source_commit,
        "generated_at_utc": "2026-05-13T00:00:00Z",
        "overall_verdict": "CERTIFIED",
        "hard_gate_results": hard_gate_results,
        "blocking_reasons": [],
        "evidence_index": evidence_index,
        "source": {
            "certification_lane_artifact": "tests/full_suite_gate/certification_verdict.json",
            "lane_schema": "pi.ci.certification_lane.v1",
            "lane_verdict": "pass"
        },
        "claim_surface": "release_facing"
    });
    fs::write(
        root.join("docs/evidence/dropin-certification-verdict.json"),
        serde_json::to_vec_pretty(&verdict)?,
    )?;
    commit_fixture_path(
        root,
        "docs/evidence/dropin-certification-verdict.json",
        "bind canonical drop-in verdict",
    )?;
    Ok(())
}

fn bind_fixture_performance_summary_to_source(root: &Path) -> TestResult {
    initialize_fixture_git_workspace(root)?;
    let source_commit = fixture_git_output(root, &["rev-parse", "HEAD"])?;
    let summary_path = root.join("tests/perf/reports/budget_summary.json");
    let mut summary: serde_json::Value = serde_json::from_slice(&fs::read(&summary_path)?)?;
    summary["source_commit"] = json!(source_commit);
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;
    run_fixture_git(root, &["add", "tests/perf/reports/budget_summary.json"])?;
    run_fixture_git(root, &["commit", "-m", "bind performance evidence"])?;
    Ok(())
}

#[cfg(unix)]
fn shell_single_quote(path: &Path) -> TestResult<String> {
    let path = path
        .to_str()
        .ok_or("fixture path must be valid UTF-8 for a POSIX shell command")?;
    Ok(format!("'{}'", path.replace('\'', "'\\''")))
}

#[cfg(unix)]
fn bind_fixture_performance_summary_with_hiding_filter(root: &Path) -> TestResult {
    run_fixture_git(root, &["init", "-b", "main"])?;
    run_fixture_git(
        root,
        &["config", "user.email", "pi-context-e2e@example.invalid"],
    )?;
    run_fixture_git(root, &["config", "user.name", "Pi Context E2E"])?;

    let summary_path = root.join("tests/perf/reports/budget_summary.json");
    let canonical_summary = root.join(".git/canonical-performance-summary.json");
    fs::copy(&summary_path, &canonical_summary)?;
    let filter_command = format!("cat {}", shell_single_quote(&canonical_summary)?);
    run_fixture_git(
        root,
        &["config", "filter.canonical-summary.clean", &filter_command],
    )?;
    run_fixture_git(
        root,
        &["config", "filter.canonical-summary.required", "true"],
    )?;
    run_fixture_git(root, &["add", "."])?;
    run_fixture_git(root, &["commit", "-m", "fixture baseline"])?;

    let source_commit = fixture_git_output(root, &["rev-parse", "HEAD"])?;
    let mut summary: serde_json::Value = serde_json::from_slice(&fs::read(&summary_path)?)?;
    summary["source_commit"] = json!(source_commit);
    let summary_bytes = serde_json::to_vec_pretty(&summary)?;
    fs::write(&summary_path, &summary_bytes)?;
    fs::write(&canonical_summary, &summary_bytes)?;
    run_fixture_git(root, &["add", "tests/perf/reports/budget_summary.json"])?;
    run_fixture_git(root, &["commit", "-m", "bind performance evidence"])?;
    Ok(())
}

#[cfg(unix)]
fn bind_fixture_performance_summary_with_source_hiding_filter(root: &Path) -> TestResult {
    run_fixture_git(root, &["init", "-b", "main"])?;
    run_fixture_git(
        root,
        &["config", "user.email", "pi-context-e2e@example.invalid"],
    )?;
    run_fixture_git(root, &["config", "user.name", "Pi Context E2E"])?;

    let source_path = root.join("src/lib.rs");
    let canonical_source = root.join(".git/canonical-source.rs");
    fs::copy(&source_path, &canonical_source)?;
    let filter_command = format!("cat {}", shell_single_quote(&canonical_source)?);
    run_fixture_git(
        root,
        &["config", "filter.canonical-source.clean", &filter_command],
    )?;
    run_fixture_git(
        root,
        &["config", "filter.canonical-source.required", "true"],
    )?;
    run_fixture_git(root, &["add", "."])?;
    run_fixture_git(root, &["commit", "-m", "fixture baseline"])?;

    let source_commit = fixture_git_output(root, &["rev-parse", "HEAD"])?;
    let summary_path = root.join("tests/perf/reports/budget_summary.json");
    let mut summary: serde_json::Value = serde_json::from_slice(&fs::read(&summary_path)?)?;
    summary["source_commit"] = json!(source_commit);
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;
    run_fixture_git(root, &["add", "tests/perf/reports/budget_summary.json"])?;
    run_fixture_git(root, &["commit", "-m", "bind performance evidence"])?;
    Ok(())
}

fn fixture_workspace() -> TestResult<TempDir> {
    let temp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .and_then(|tmpdir| {
            fs::create_dir_all(&tmpdir)
                .ok()
                .and_then(|()| tempfile::Builder::new().tempdir_in(&tmpdir).ok())
        })
        .map_or_else(
            || tempfile::Builder::new().tempdir_in(std::env::temp_dir()),
            Ok,
        )?;
    let root = temp.path();

    write_fixture(
        root,
        "Cargo.toml",
        r#"[package]
name = "semantic-workspace-fixture"
version = "0.0.0"
edition = "2024"
include = [
    "/docs/evidence/tool-output-context-cache.jsonl",
    "/src/**",
]
"#,
    )?;
    write_fixture(
        root,
        "src/lib.rs",
        r"
pub mod providers;

pub struct Widget;

pub fn build_widget() -> Widget {
    Widget
}
",
    )?;
    write_fixture(
        root,
        "src/providers/openai.rs",
        r"
pub struct OpenAiProvider;

pub fn stream_response() {}
",
    )?;
    write_fixture(
        root,
        "src/session.rs",
        r"
pub struct SessionStore;

pub fn save_session() {}
",
    )?;
    write_fixture(
        root,
        "src/extensions.rs",
        r"
pub struct ExtensionHost;

pub fn load_extension() {}
",
    )?;
    write_fixture(
        root,
        "tests/widget_flow.rs",
        r"
#[test]
fn builds_widget() {
    assert_eq!(2 + 2, 4);
}
",
    )?;
    write_fixture(
        root,
        "tests/provider_streaming.rs",
        r"
#[test]
fn streams_openai_provider() {
    assert_eq!(2 + 2, 4);
}
",
    )?;
    write_fixture(
        root,
        "tests/session_flow.rs",
        r"
#[test]
fn saves_session() {
    assert_eq!(2 + 2, 4);
}
",
    )?;
    write_fixture(
        root,
        "tests/extension_flow.rs",
        r"
#[test]
fn loads_extension() {
    assert_eq!(2 + 2, 4);
}
",
    )?;
    write_fixture(
        root,
        "README.md",
        r"
# Pi Fixture

## Evidence

Strict drop-in certification cites docs/evidence/dropin-certification-verdict.json.
Release-facing claims must suppress docs/evidence/uncertified.json.
Missing evidence must suppress docs/evidence/missing.json.
Perf budget claims cite tests/perf/reports/budget_summary.json.
Extension closeout claims cite docs/evidence/extension-health-delta-failure-disposition.json.
Parity ledger claims cite docs/evidence/dropin-parity-gap-ledger.json.
",
    )?;
    write_fixture(
        root,
        "docs/evidence/dropin-certification-verdict.json",
        r#"{
  "schema": "pi.dropin.certification_verdict.v1",
  "generated_at": "2026-01-01T00:00:00Z",
  "overall_verdict": "CERTIFIED",
  "claim_surface": "release_facing"
}"#,
    )?;
    write_fixture(
        root,
        "tests/perf/reports/budget_summary.json",
        &serde_json::to_string_pretty(&semantic_perf_budget_fixture())?,
    )?;
    write_fixture(
        root,
        "docs/evidence/extension-health-delta-failure-disposition.json",
        r#"{
  "schema": "pi.ext.health_delta_failure_disposition.v1",
  "generated_at": "2026-05-13T00:00:00Z",
  "source_report_generated_at": "2026-05-13T00:00:00Z",
  "claim_surface": "release_facing"
}"#,
    )?;
    write_fixture(
        root,
        "docs/evidence/dropin-parity-gap-ledger.json",
        r#"{
  "schema": "pi.dropin.parity_gap_ledger.v1",
  "generated_at_utc": "2026-05-13T00:00:00Z",
  "claim_surface": "release_facing",
  "gaps": []
}"#,
    )?;
    write_fixture(
        root,
        "docs/evidence/uncertified.json",
        r#"{
  "schema": "pi.dropin_certification.verdict.v1",
  "generated_at": "2026-05-13T00:00:00Z",
  "overall_verdict": "NOT_CERTIFIED",
  "claim_surface": "release_facing"
}"#,
    )?;
    write_fixture(root, "docs/evidence/malformed.json", "{ not valid json")?;
    let issues = [
        json!({
            "id": "bd-open",
            "title": "Open work",
            "status": "open",
            "priority": 1,
            "issue_type": "feature",
            "updated_at": "2026-05-13T00:00:00Z",
            "external_ref": "docs/evidence/dropin-parity-gap-ledger.json"
        })
        .to_string(),
        json!({
            "id": "bd-blocked",
            "title": "Blocked work",
            "status": "open",
            "priority": 1,
            "issue_type": "feature",
            "updated_at": "2026-05-13T00:00:00Z",
            "dependencies": [
                {
                    "issue_id": "bd-blocked",
                    "depends_on_id": "bd-open",
                    "type": "blocks"
                }
            ]
        })
        .to_string(),
        json!({
            "id": "bd-claimed",
            "title": "Claimed work",
            "status": "in_progress",
            "priority": 1,
            "issue_type": "task",
            "updated_at": "2026-05-13T00:00:00Z"
        })
        .to_string(),
        json!({
            "id": "bd-closed",
            "title": "Closed work",
            "status": "closed",
            "priority": 2,
            "issue_type": "task",
            "closed_at": "2026-05-01T00:00:00Z"
        })
        .to_string(),
        json!({
            "id": "bd-tombstone",
            "title": "Deleted work",
            "status": "tombstone",
            "deleted": true
        })
        .to_string(),
        "{ not valid bead json".to_string(),
    ]
    .join("\n");
    write_fixture(root, ".beads/issues.jsonl", &issues)?;

    Ok(temp)
}

fn permuted_large_context_indices(count: usize) -> Vec<usize> {
    let mut indices = (0..count).collect::<Vec<_>>();
    indices.sort_by_key(|idx| (idx * 37 + 11) % count);
    indices
}

fn write_large_context_fixtures(root: &Path, order: &[usize]) -> TestResult {
    for idx in order {
        let module = format!("context_unit_{idx:03}");
        write_fixture(
            root,
            &format!("src/context/{module}.rs"),
            &format!(
                r"
pub struct ContextUnit{idx};

pub fn {module}_value() -> usize {{
    {idx}
}}
"
            ),
        )?;
        write_fixture(
            root,
            &format!("tests/context/{module}_flow.rs"),
            &format!(
                r"
#[test]
fn validates_{module}() {{
    assert_eq!({idx} + 1, {next});
}}
",
                next = idx + 1
            ),
        )?;
        write_fixture(
            root,
            &format!("docs/context/{module}.md"),
            &format!(
                r"
# Context Unit {idx}

This fixture gives the semantic context planner a large deterministic workspace.
"
            ),
        )?;
        write_fixture(
            root,
            &format!("docs/evidence/context_budget_{idx:03}.json"),
            &format!(
                r#"{{
  "schema": "pi.context.fixture_evidence.v1",
  "generated_at": "2026-05-13T00:00:00Z",
  "module": "{module}",
  "claim_surface": "internal_perf_budget"
}}"#
            ),
        )?;
    }
    Ok(())
}

fn large_context_fixture_workspace(order: &[usize]) -> TestResult<TempDir> {
    let temp = fixture_workspace()?;
    write_large_context_fixtures(temp.path(), order)?;
    Ok(temp)
}

fn add_incremental_context_fixture(root: &Path) -> TestResult {
    write_fixture(
        root,
        "src/context/incremental_refresh.rs",
        r#"
pub struct IncrementalRefresh;

pub fn refresh_context_bundle() -> &'static str {
    "incremental"
}
"#,
    )?;
    write_fixture(
        root,
        "tests/context/incremental_refresh_flow.rs",
        r#"
#[test]
fn validates_incremental_refresh() {
    assert_eq!("incremental".len(), 11);
}
"#,
    )?;
    write_fixture(
        root,
        "docs/context/incremental_refresh.md",
        r"
# Incremental Refresh

The context planner must keep deterministic output after a single workspace change.
",
    )
}

fn resolved_cargo_target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || root.join("target"),
        |raw| {
            let target_dir = PathBuf::from(raw);
            if target_dir.is_absolute() {
                target_dir
            } else {
                root.join(target_dir)
            }
        },
    )
}

fn resolved_tmpdir() -> PathBuf {
    std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

fn elapsed_ms(start: Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1000.0).max(0.001)
}

fn add_sensitive_context_fixtures(root: &Path) -> TestResult {
    write_fixture(
        root,
        "tests/fixtures/vcr/oauth_refresh_sensitive.json",
        r#"{
  "schema": "pi.vcr.fixture.v1",
  "generated_at": "2026-05-13T00:00:00Z",
  "authorization": "Bearer sk-secret",
  "request": {"body": {"prompt": "hidden prompt"}},
  "response": {"body": {"access_token": "hidden token"}}
}"#,
    )?;
    write_fixture(
        root,
        "tests/fixtures/context_artifacts/provider-auth.log",
        "request body contains API_KEY=sk-secret and prompt text",
    )
}

fn e2e_assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        api: "openai-responses".to_string(),
        provider: "context-e2e-provider".to_string(),
        model: "context-e2e-model".to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        stop_details: None,
        error_message: None,
        timestamp: 0,
    }
}

#[derive(Debug, Clone)]
struct CapturedContextE2eCall {
    system_prompt: Option<String>,
    messages: Vec<Message>,
}

#[derive(Debug, Clone)]
struct ContextE2eProvider {
    calls: Arc<StdMutex<Vec<CapturedContextE2eCall>>>,
}

impl ContextE2eProvider {
    fn new() -> Self {
        Self {
            calls: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Arc<StdMutex<Vec<CapturedContextE2eCall>>> {
        Arc::clone(&self.calls)
    }
}

#[async_trait]
impl Provider for ContextE2eProvider {
    fn name(&self) -> &'static str {
        "context-e2e-provider"
    }

    fn api(&self) -> &'static str {
        "openai-responses"
    }

    fn model_id(&self) -> &'static str {
        "context-e2e-model"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<Pin<Box<dyn Stream<Item = pi::error::Result<StreamEvent>> + Send>>> {
        match self.calls.lock() {
            Ok(calls) => calls,
            Err(poisoned) => poisoned.into_inner(),
        }
        .push(CapturedContextE2eCall {
            system_prompt: context.system_prompt.as_ref().map(ToString::to_string),
            messages: context.messages.iter().cloned().collect(),
        });
        Ok(Box::pin(futures::stream::iter(vec![Ok(
            StreamEvent::Done {
                reason: StopReason::Stop,
                message: e2e_assistant_message("deterministic context response"),
            },
        )])))
    }
}

fn write_context_e2e_jsonl_log(root: &Path, records: &[serde_json::Value]) -> TestResult<String> {
    let path = root.join("context-intelligence-e2e.jsonl");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    for record in records {
        writeln!(file, "{}", serde_json::to_string(record)?)?;
    }
    let log = fs::read_to_string(path)?;
    for line in log.lines() {
        let _: serde_json::Value = serde_json::from_str(line)?;
    }
    Ok(log)
}

fn context_message_content(messages: &[Message]) -> TestResult<&str> {
    messages
        .iter()
        .find_map(|message| match message {
            Message::Custom(custom) if custom.custom_type == "semantic_context_bundle" => {
                Some(custom.content.as_str())
            }
            _ => None,
        })
        .ok_or_else(|| "missing semantic context custom message".into())
}

fn build_fixture_graph(root: &Path) -> TestResult<SemanticWorkspaceGraph> {
    Ok(SemanticWorkspaceGraphBuilder::new(root)
        .with_reference_time(reference_time()?)
        .add_expected_path("docs/evidence/missing.json")
        .build()?)
}

fn node_with_source<'a>(
    graph: &'a SemanticWorkspaceGraph,
    node_type: SemanticNodeType,
    source_path: &str,
) -> TestResult<&'a pi::semantic_workspace_graph::SemanticGraphNode> {
    graph
        .nodes
        .iter()
        .find(|node| node.node_type == node_type && node.source_path == source_path)
        .ok_or_else(|| format!("missing {node_type:?} node for {source_path}").into())
}

fn assert_performance_fixture_reason(
    root: &Path,
    source_path: &str,
    expected_reason: &str,
) -> TestResult {
    let graph = build_fixture_graph(root)?;
    let perf_budget = node_with_source(&graph, SemanticNodeType::EvidenceArtifact, source_path)?;
    assert_eq!(
        perf_budget.freshness_status,
        Some(EvidenceFreshnessStatus::Malformed)
    );
    assert_eq!(
        perf_budget.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    assert_eq!(
        perf_budget.metadata.get("freshness_reason"),
        Some(&json!(expected_reason))
    );
    Ok(())
}

fn ignore_fixture_path(root: &Path, relative_path: &str) -> TestResult {
    let mut exclude = fs::OpenOptions::new()
        .append(true)
        .open(root.join(".git/info/exclude"))?;
    writeln!(exclude, "/{relative_path}")?;
    Ok(())
}

fn bead_status(
    graph: &SemanticWorkspaceGraph,
    bead_id: &str,
) -> TestResult<BeadActionabilityStatus> {
    let node = graph
        .nodes
        .iter()
        .find(|node| {
            node.node_type == SemanticNodeType::Bead
                && node.metadata.get("bead_id") == Some(&json!(bead_id))
        })
        .ok_or_else(|| format!("missing bead node for {bead_id}"))?;
    node.bead_actionability_status
        .ok_or_else(|| format!("missing bead actionability for {bead_id}").into())
}

fn bundle_golden_summary(
    bundle: &pi::semantic_workspace_graph::SemanticContextBundle,
) -> serde_json::Value {
    json!({
        "selected": bundle
            .selected_items
            .iter()
            .map(|item| json!({
                "path": &item.source_path,
                "title": &item.title,
                "reason": &item.reason,
            }))
            .collect::<Vec<_>>(),
        "stale_suppressions": bundle
            .stale_evidence_suppressions
            .iter()
            .map(|item| json!({
                "path": &item.source_path,
                "reason": &item.reason,
            }))
            .collect::<Vec<_>>(),
        "commands": &bundle.suggested_validation_commands,
        "budget_excluded": bundle
            .excluded_items
            .iter()
            .filter(|item| item.reason == "budget_exceeded")
            .count(),
    })
}

#[test]
fn context_path_normalization_rejects_escape_and_normalizes_safe_paths() {
    let normalized = normalize_context_artifact_path("./src/../src/session.rs");
    assert!(normalized.accepted);
    assert_eq!(
        normalized.normalized_path.as_deref(),
        Some("src/session.rs")
    );
    assert_eq!(normalized.reason, "normalized");

    let absolute = normalize_context_artifact_path("/etc/passwd");
    assert!(!absolute.accepted);
    assert_eq!(absolute.reason, "absolute_path_rejected");

    let parent_escape = normalize_context_artifact_path("../secrets/auth.json");
    assert!(!parent_escape.accepted);
    assert_eq!(parent_escape.reason, "parent_escape_rejected");

    let nul = normalize_context_artifact_path("docs/evidence/good.json\0bad");
    assert!(!nul.accepted);
    assert_eq!(nul.reason, "nul_byte_rejected");

    let windows_escape = normalize_context_artifact_path("docs\\..\\secrets\\auth.json");
    assert!(!windows_escape.accepted);
    assert_eq!(windows_escape.reason, "backslash_separator_rejected");
}

#[test]
fn graph_cache_validation_enforces_scope_ttl_and_path_policy() -> TestResult {
    let temp = fixture_workspace()?;
    let reference_time = reference_time()?;
    let cache_scope = ContextArtifactCacheScope::new("workspace-a", "main", "session-a");
    let graph = SemanticWorkspaceGraphBuilder::new(temp.path())
        .with_reference_time(reference_time)
        .with_cache_scope(cache_scope.clone())
        .with_cache_ttl_seconds(900)
        .build()?;
    let now_ns = u64::try_from(reference_time.timestamp())? * 1_000_000_000;

    assert_eq!(
        graph.cache_validation_for_path("./src/../src/session.rs", &cache_scope, now_ns),
        ContextArtifactCacheStatus::Valid
    );
    assert_eq!(
        graph.cache_validation_for_path("src/missing.rs", &cache_scope, now_ns),
        ContextArtifactCacheStatus::MissingFingerprint
    );
    assert_eq!(
        graph.cache_validation_for_path("/etc/passwd", &cache_scope, now_ns),
        ContextArtifactCacheStatus::UnsafePath
    );
    assert_eq!(
        graph.cache_validation_for_path(
            "src/session.rs",
            &ContextArtifactCacheScope::new("workspace-b", "main", "session-a"),
            now_ns
        ),
        ContextArtifactCacheStatus::WorkspaceMismatch
    );
    assert_eq!(
        graph.cache_validation_for_path(
            "src/session.rs",
            &ContextArtifactCacheScope::new("workspace-a", "feature", "session-a"),
            now_ns
        ),
        ContextArtifactCacheStatus::BranchMismatch
    );
    assert_eq!(
        graph.cache_validation_for_path(
            "src/session.rs",
            &ContextArtifactCacheScope::new("workspace-a", "main", "session-b"),
            now_ns
        ),
        ContextArtifactCacheStatus::SessionMismatch
    );
    assert_eq!(
        graph.cache_validation_for_path(
            "src/session.rs",
            &cache_scope,
            now_ns + 901 * 1_000_000_000
        ),
        ContextArtifactCacheStatus::Expired
    );

    Ok(())
}

fn semantic_perf_budget_fixture() -> serde_json::Value {
    let checked_in: serde_json::Value =
        serde_json::from_str(include_str!("perf/reports/budget_summary.json"))
            .expect("checked-in performance summary must be valid JSON");
    let budgets = checked_in["budgets"]
        .as_array()
        .cloned()
        .expect("checked-in performance summary must contain canonical budgets");
    let budget_results = budgets
        .iter()
        .map(|budget| {
            json!({
                "budget_name": budget["name"],
                "category": budget["category"],
                "threshold": budget["threshold"],
                "comparison": budget["comparison"],
                "unit": budget["unit"],
                "actual": budget["threshold"],
                "status": "PASS",
                "source": "semantic graph contract fixture",
                "ci_enforced": budget["ci_enforced"]
            })
        })
        .collect::<Vec<_>>();
    let ci_enforced = budgets
        .iter()
        .filter(|budget| budget["ci_enforced"].as_bool() == Some(true))
        .count();
    json!({
        "schema": "pi.perf.budget_summary.v2",
        "generated_at": "2026-05-13T00:00:00.000Z",
        "source_commit": "0123456789abcdef0123456789abcdef01234567",
        "run_id": "fixture-run",
        "correlation_id": "fixture-run",
        "strict_mode": true,
        "total_budgets": budgets.len(),
        "pass": budgets.len(),
        "fail": 0,
        "no_data": 0,
        "ci_enforced": ci_enforced,
        "ci_with_data": ci_enforced,
        "ci_fail": 0,
        "ci_no_data": 0,
        "data_contract_failures_count": 0,
        "failing_data_contracts": [],
        "budgets": budgets,
        "budget_results": budget_results,
        "claim_readiness": {
            "status": "claim_ready",
            "performance_claims_authorized": true,
            "blocking_reason_codes": []
        }
    })
}

#[test]
fn evidence_ingestion_rejects_duplicate_object_keys_recursively() -> TestResult {
    let temp = fixture_workspace()?;
    let cases = [
        (
            "docs/evidence/duplicate-top-level.json",
            r#"{"schema":"fixture.duplicate.v1","attacker_secret_top_7f2a":1,"attacker_secret_top_7f2a":2}"#,
            "attacker_secret_top_7f2a",
        ),
        (
            "docs/evidence/duplicate-nested.json",
            r#"{"schema":"fixture.duplicate.v1","claim":{"attacker_secret_nested_9c4b":"blocked","attacker_secret_nested_9c4b":"claim_ready"}}"#,
            "attacker_secret_nested_9c4b",
        ),
    ];
    for (path, content, _) in cases {
        write_fixture(temp.path(), path, content)?;
    }

    let graph = build_fixture_graph(temp.path())?;
    for (path, _, duplicate_key) in cases {
        let node = node_with_source(&graph, SemanticNodeType::EvidenceArtifact, path)?;
        assert_eq!(
            node.freshness_status,
            Some(EvidenceFreshnessStatus::Malformed)
        );
        assert_eq!(
            node.metadata.get("freshness_reason"),
            Some(&json!("json_parse_failed"))
        );
        let parse_error = node
            .metadata
            .get("parse_error")
            .and_then(serde_json::Value::as_str)
            .ok_or("duplicate-key evidence must retain a parse error")?;
        assert!(
            parse_error.contains("duplicate JSON object key")
                && !parse_error.contains(duplicate_key),
            "unexpected duplicate-key parse error: {parse_error}"
        );
        assert!(
            !serde_json::to_string(&graph)?.contains(duplicate_key),
            "attacker-controlled duplicate key leaked into the graph"
        );
    }
    Ok(())
}

#[test]
fn evidence_ingestion_rejects_invalid_utf8_without_lossy_replacement() -> TestResult {
    let temp = fixture_workspace()?;
    let path = "docs/evidence/invalid-utf8.json";
    let full_path = temp.path().join(path);
    fs::write(
        &full_path,
        b"{\"schema\":\"fixture.invalid_utf8.v1\",\"value\":\"\xff\"}",
    )?;
    let expected_sha256 = format!("{:x}", Sha256::digest(fs::read(&full_path)?));

    let graph = build_fixture_graph(temp.path())?;
    let node = node_with_source(&graph, SemanticNodeType::EvidenceArtifact, path)?;
    assert_eq!(
        node.freshness_status,
        Some(EvidenceFreshnessStatus::Malformed)
    );
    assert_eq!(
        node.metadata.get("freshness_reason"),
        Some(&json!("invalid_utf8"))
    );
    assert_eq!(
        node.content_sha256.as_deref(),
        Some(expected_sha256.as_str())
    );
    assert_eq!(node.redaction_status, RedactionStatus::SensitiveOmitted);
    Ok(())
}

#[test]
fn canonical_dropin_verdict_admits_only_complete_source_bound_contract_evidence() -> TestResult {
    let temp = fixture_workspace()?;
    install_canonical_dropin_claim_fixture(temp.path())?;

    let graph = build_fixture_graph(temp.path())?;
    let verdict = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        verdict.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );
    assert_eq!(
        verdict.metadata.get("release_claim_allowed"),
        Some(&json!(true))
    );
    assert_eq!(
        verdict.metadata.get("strict_replacement_claim_allowed"),
        Some(&json!(true))
    );

    let bundle = SemanticContextBundlePlanner::new(&graph).plan(&ContextBundleRequest {
        query: Some("dropin certification verdict".to_string()),
        budget: ContextBundleBudget {
            max_items: 16,
            max_bytes: 32 * 1024,
        },
        ..ContextBundleRequest::default()
    });
    assert!(bundle.selected_items.iter().any(|item| {
        item.source_path == "docs/evidence/dropin-certification-verdict.json"
            && item.reason.contains("current_release_claim_evidence")
    }));
    Ok(())
}

#[test]
fn noncanonical_dropin_verdict_schema_never_admits_or_gains_claim_score() -> TestResult {
    let temp = fixture_workspace()?;
    let shadow_path = "docs/evidence/shadow/dropin-certification-verdict.json";
    write_fixture(
        temp.path(),
        shadow_path,
        &serde_json::to_string_pretty(&json!({
            "schema": "pi.dropin.certification_verdict.v1",
            "generated_at_utc": "2026-05-13T00:00:00Z",
            "overall_verdict": "CERTIFIED",
            "claim_surface": "release_facing"
        }))?,
    )?;

    let graph = build_fixture_graph(temp.path())?;
    let shadow = node_with_source(&graph, SemanticNodeType::EvidenceArtifact, shadow_path)?;
    assert_eq!(
        shadow.freshness_status,
        Some(EvidenceFreshnessStatus::Malformed)
    );
    assert_eq!(
        shadow.metadata.get("freshness_reason"),
        Some(&json!("dropin_verdict_noncanonical_path"))
    );
    assert_eq!(
        shadow.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    assert_eq!(
        shadow.metadata.get("strict_replacement_claim_allowed"),
        None
    );

    let bundle = SemanticContextBundlePlanner::new(&graph).plan(&ContextBundleRequest {
        query: Some("dropin certification verdict".to_string()),
        budget: ContextBundleBudget {
            max_items: 32,
            max_bytes: 64 * 1024,
        },
        ..ContextBundleRequest::default()
    });
    assert!(!bundle.selected_items.iter().any(|item| {
        item.source_path == shadow_path && item.reason.contains("current_release_claim_evidence")
    }));
    Ok(())
}

#[test]
fn skeletal_or_forged_certified_verdicts_never_admit_or_gain_claim_score() -> TestResult {
    let skeletal = fixture_workspace()?;
    let graph = build_fixture_graph(skeletal.path())?;
    let verdict = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        verdict.freshness_status,
        Some(EvidenceFreshnessStatus::Malformed)
    );
    assert_eq!(
        verdict.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    let bundle = SemanticContextBundlePlanner::new(&graph).plan(&ContextBundleRequest {
        query: Some("dropin certification verdict".to_string()),
        budget: ContextBundleBudget {
            max_items: 16,
            max_bytes: 32 * 1024,
        },
        ..ContextBundleRequest::default()
    });
    assert!(!bundle.selected_items.iter().any(|item| {
        item.source_path == "docs/evidence/dropin-certification-verdict.json"
            && item.reason.contains("current_release_claim_evidence")
    }));

    for mutation in [
        "missing_gate",
        "non_pass_gate",
        "blocking_reason",
        "evidence_order",
        "source_lane",
        "source_commit",
    ] {
        let temp = fixture_workspace()?;
        install_canonical_dropin_claim_fixture(temp.path())?;
        let path = temp
            .path()
            .join("docs/evidence/dropin-certification-verdict.json");
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        match mutation {
            "missing_gate" => {
                value["hard_gate_results"]
                    .as_array_mut()
                    .ok_or("fixture hard_gate_results must be an array")?
                    .pop();
            }
            "non_pass_gate" => value["hard_gate_results"][0]["status"] = json!("fail"),
            "blocking_reason" => value["blocking_reasons"] = json!(["forged override"]),
            "evidence_order" => value["evidence_index"]
                .as_array_mut()
                .ok_or("fixture evidence_index must be an array")?
                .swap(0, 1),
            "source_lane" => value["source"]["lane_verdict"] = json!("fail"),
            "source_commit" => {
                value["git_commit"] = json!("0000000000000000000000000000000000000000");
            }
            unexpected => return Err(format!("unexpected mutation: {unexpected}").into()),
        }
        fs::write(&path, serde_json::to_vec_pretty(&value)?)?;
        commit_fixture_path(
            temp.path(),
            "docs/evidence/dropin-certification-verdict.json",
            &format!("forge verdict {mutation}"),
        )?;

        let graph = build_fixture_graph(temp.path())?;
        let verdict = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "docs/evidence/dropin-certification-verdict.json",
        )?;
        assert_eq!(
            verdict.metadata.get("release_claim_allowed"),
            Some(&json!(false)),
            "mutation {mutation} must fail closed"
        );
        assert_eq!(
            verdict.metadata.get("strict_replacement_claim_allowed"),
            Some(&json!(false)),
            "mutation {mutation} must suppress strict replacement claims"
        );
    }
    Ok(())
}

#[test]
fn canonical_dropin_verdict_requires_immutable_head_bound_input_bytes() -> TestResult {
    for path in [
        "docs/contracts/dropin-certification-contract.json",
        "docs/evidence/dropin-certification-verdict.json",
        "docs/evidence/gate-01.json",
    ] {
        let temp = fixture_workspace()?;
        install_canonical_dropin_claim_fixture(temp.path())?;
        let full_path = temp.path().join(path);
        let mut bytes = fs::read(&full_path)?;
        bytes.push(b'\n');
        fs::write(full_path, bytes)?;

        let graph = build_fixture_graph(temp.path())?;
        let verdict = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "docs/evidence/dropin-certification-verdict.json",
        )?;
        assert_eq!(
            verdict.metadata.get("release_claim_allowed"),
            Some(&json!(false)),
            "dirty canonical input {path} must fail closed"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn canonical_dropin_verdict_requires_head_bound_input_modes() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = fixture_workspace()?;
    install_canonical_dropin_claim_fixture(temp.path())?;
    let path = temp.path().join("docs/evidence/gate-01.json");
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)?;

    let graph = build_fixture_graph(temp.path())?;
    let verdict = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        verdict.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn canonical_dropin_verdict_rejects_committed_executable_decision_input() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = fixture_workspace()?;
    install_canonical_dropin_claim_fixture(temp.path())?;
    let path = temp
        .path()
        .join("docs/evidence/dropin-certification-verdict.json");
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(&path, permissions)?;
    commit_fixture_path(
        temp.path(),
        "docs/evidence/dropin-certification-verdict.json",
        "forge executable verdict mode",
    )?;

    let graph = build_fixture_graph(temp.path())?;
    let verdict = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        verdict.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    assert_eq!(
        verdict.metadata.get("freshness_reason"),
        Some(&json!("dropin_verdict_provenance_not_regular_at_head"))
    );
    Ok(())
}

#[test]
fn canonical_dropin_verdict_freshness_uses_only_generated_at_utc() -> TestResult {
    let temp = fixture_workspace()?;
    install_canonical_dropin_claim_fixture(temp.path())?;
    let path = temp
        .path()
        .join("docs/evidence/dropin-certification-verdict.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    value["generated_at_utc"] = json!("2025-01-01T00:00:00Z");
    value["generated_at"] = json!("2026-05-13T00:00:00Z");
    fs::write(&path, serde_json::to_vec_pretty(&value)?)?;
    commit_fixture_path(
        temp.path(),
        "docs/evidence/dropin-certification-verdict.json",
        "forge alternate freshness timestamp",
    )?;

    let graph = build_fixture_graph(temp.path())?;
    let verdict = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        verdict.freshness_status,
        Some(EvidenceFreshnessStatus::Stale)
    );
    assert_eq!(
        verdict.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    assert_eq!(
        verdict.metadata.get("generated_at"),
        Some(&json!("2025-01-01T00:00:00Z"))
    );
    Ok(())
}

#[test]
fn canonical_dropin_verdict_uses_release_gate_age_limit() -> TestResult {
    for (generated_at, lane_generated_at, expected_status, expected_allowed) in [
        (
            "2026-05-06T00:00:00Z",
            "2026-05-06T00:00:00.000Z",
            EvidenceFreshnessStatus::Current,
            true,
        ),
        (
            "2026-05-05T23:59:59Z",
            "2026-05-13T00:00:00.000Z",
            EvidenceFreshnessStatus::Stale,
            false,
        ),
    ] {
        let temp = fixture_workspace()?;
        let lane = canonical_certification_lane_fixture(lane_generated_at)?;
        install_canonical_dropin_claim_fixture_with_lane(temp.path(), &lane)?;
        let path = temp
            .path()
            .join("docs/evidence/dropin-certification-verdict.json");
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        value["generated_at_utc"] = json!(generated_at);
        fs::write(&path, serde_json::to_vec_pretty(&value)?)?;
        commit_fixture_path(
            temp.path(),
            "docs/evidence/dropin-certification-verdict.json",
            &format!("set verdict age {generated_at}"),
        )?;

        let graph = build_fixture_graph(temp.path())?;
        let verdict = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "docs/evidence/dropin-certification-verdict.json",
        )?;
        assert_eq!(verdict.freshness_status, Some(expected_status));
        assert_eq!(
            verdict.metadata.get("release_claim_allowed"),
            Some(&json!(expected_allowed))
        );
    }
    Ok(())
}

#[test]
fn canonical_dropin_verdict_requires_actual_passing_lane_bytes() -> TestResult {
    for mutation in [
        "minimal_pass",
        "wrong_schema",
        "non_full_lane",
        "partial_inventory",
        "gate_identity",
        "nonblocking_warn",
        "nonblocking_skip",
        "summary_contradiction",
        "gate_status_contradiction",
        "promotion_contradiction",
        "conditions_contradiction",
        "invented_waiver",
    ] {
        let temp = fixture_workspace()?;
        let mut lane = canonical_certification_lane_fixture("2026-05-13T00:00:00.000Z")?;
        match mutation {
            "minimal_pass" => {
                lane = json!({
                "schema": "pi.ci.certification_lane.v1",
                "lane": "full",
                "verdict": "pass"
                });
            }
            "wrong_schema" => lane["schema"] = json!("fixture.attacker_lane.v99"),
            "non_full_lane" => lane["lane"] = json!("preflight"),
            "partial_inventory" => {
                lane["gates"]
                    .as_array_mut()
                    .ok_or("fixture lane gates must be an array")?
                    .pop();
                lane["summary"]["total_gates"] = json!(19);
                lane["summary"]["passed"] = json!(19);
                lane["summary"]["blocking_pass"] = json!(13);
                lane["summary"]["blocking_total"] = json!(13);
            }
            "gate_identity" => lane["gates"][0]["id"] = json!("attacker_gate"),
            "nonblocking_warn" => {
                lane["gates"][1]["status"] = json!("warn");
                lane["summary"]["passed"] = json!(19);
                lane["summary"]["warned"] = json!(1);
            }
            "nonblocking_skip" => {
                lane["gates"][1]["status"] = json!("skip");
                lane["summary"]["passed"] = json!(19);
                lane["summary"]["skipped"] = json!(1);
            }
            "summary_contradiction" => lane["summary"]["passed"] = json!(19),
            "gate_status_contradiction" => lane["gates"][0]["status"] = json!("fail"),
            "promotion_contradiction" => {
                lane["promotion_rules"]["can_promote"] = json!(false);
            }
            "conditions_contradiction" => {
                lane["promotion_rules"]["conditions"] = json!(["trust me"]);
            }
            "invented_waiver" => {
                lane["gates"][0]["status"] = json!("fail");
                lane["gates"][0]["detail"] = json!("forged waiver");
                lane["waiver_audit"] = json!({
                    "schema": "pi.ci.waiver_audit.v1",
                    "generated_at": "2026-05-13T00:00:00.000Z",
                    "total_waivers": 1,
                    "active": 1,
                    "expired": 0,
                    "expiring_soon": 0,
                    "invalid": 0,
                    "waivers": [{
                        "gate_id": "non_mock_unit",
                        "status": "active",
                        "days_remaining": 10
                    }],
                    "raw_waivers": [{
                        "gate_id": "non_mock_unit",
                        "owner": "attacker",
                        "created": "2026-05-01",
                        "expires": "2026-05-23",
                        "bead": "bd-attacker",
                        "reason": "self-authorized",
                        "scope": "full",
                        "remove_when": "never"
                    }]
                });
                lane["waivers_applied"] = json!(["non_mock_unit"]);
                lane["summary"]["passed"] = json!(19);
                lane["summary"]["waived"] = json!(1);
                lane["promotion_rules"]["waiver_gates"] = json!(["non_mock_unit"]);
                lane["promotion_rules"]["conditions"] = json!([
                    "All blocking gates pass (including waivers)",
                    "Waivers active for: non_mock_unit (review before release)"
                ]);
            }
            unexpected => return Err(format!("unexpected mutation: {unexpected}").into()),
        }
        install_canonical_dropin_claim_fixture_with_lane(temp.path(), &lane)?;

        let graph = build_fixture_graph(temp.path())?;
        let verdict = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "docs/evidence/dropin-certification-verdict.json",
        )?;
        assert_eq!(
            verdict.metadata.get("release_claim_allowed"),
            Some(&json!(false)),
            "lane mutation {mutation} must fail closed"
        );
        assert_eq!(
            verdict.metadata.get("freshness_reason"),
            Some(&json!("dropin_verdict_source_lane_invalid"))
        );
    }

    let temp = fixture_workspace()?;
    install_canonical_dropin_claim_fixture(temp.path())?;
    fs::write(
        temp.path()
            .join("tests/full_suite_gate/certification_verdict.json"),
        b"{ not valid JSON",
    )?;
    commit_fixture_path(
        temp.path(),
        "tests/full_suite_gate/certification_verdict.json",
        "forge malformed lane",
    )?;
    let graph = build_fixture_graph(temp.path())?;
    let verdict = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        verdict.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    assert_eq!(
        verdict.metadata.get("freshness_reason"),
        Some(&json!("dropin_verdict_source_lane_invalid"))
    );
    Ok(())
}

#[test]
fn canonical_dropin_lane_uses_exact_168_hour_age_boundary() -> TestResult {
    for (generated_at, expected_status, expected_allowed) in [
        (
            "2026-05-06T00:00:00.000Z",
            EvidenceFreshnessStatus::Current,
            true,
        ),
        (
            "2026-05-05T23:59:59.999Z",
            EvidenceFreshnessStatus::Malformed,
            false,
        ),
    ] {
        let temp = fixture_workspace()?;
        let lane = canonical_certification_lane_fixture(generated_at)?;
        install_canonical_dropin_claim_fixture_with_lane(temp.path(), &lane)?;
        let verdict_path = temp
            .path()
            .join("docs/evidence/dropin-certification-verdict.json");
        let mut verdict: serde_json::Value = serde_json::from_slice(&fs::read(&verdict_path)?)?;
        verdict["generated_at_utc"] = json!(generated_at.replace(".000Z", "Z"));
        fs::write(&verdict_path, serde_json::to_vec_pretty(&verdict)?)?;
        commit_fixture_path(
            temp.path(),
            "docs/evidence/dropin-certification-verdict.json",
            &format!("align verdict timestamp to lane {generated_at}"),
        )?;
        let graph = build_fixture_graph(temp.path())?;
        let verdict = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "docs/evidence/dropin-certification-verdict.json",
        )?;
        assert_eq!(verdict.freshness_status, Some(expected_status));
        assert_eq!(
            verdict.metadata.get("release_claim_allowed"),
            Some(&json!(expected_allowed))
        );
    }
    Ok(())
}

#[test]
fn canonical_dropin_verdict_and_lane_timestamps_must_describe_the_same_run() -> TestResult {
    for (lane_generated_at, expected_status, expected_allowed) in [
        (
            "2026-05-12T23:55:00.000Z",
            EvidenceFreshnessStatus::Current,
            true,
        ),
        (
            "2026-05-12T23:54:59.999Z",
            EvidenceFreshnessStatus::Malformed,
            false,
        ),
    ] {
        let temp = fixture_workspace()?;
        let lane = canonical_certification_lane_fixture(lane_generated_at)?;
        install_canonical_dropin_claim_fixture_with_lane(temp.path(), &lane)?;

        let graph = build_fixture_graph(temp.path())?;
        let verdict = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "docs/evidence/dropin-certification-verdict.json",
        )?;
        assert_eq!(verdict.freshness_status, Some(expected_status));
        assert_eq!(
            verdict.metadata.get("release_claim_allowed"),
            Some(&json!(expected_allowed))
        );
        if !expected_allowed {
            assert_eq!(
                verdict.metadata.get("freshness_reason"),
                Some(&json!("dropin_verdict_source_lane_invalid"))
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn canonical_dropin_verdict_rejects_symlinked_repository_path_components() -> TestResult {
    use std::os::unix::fs::symlink;

    let repository = fixture_workspace()?;
    install_canonical_dropin_claim_fixture(repository.path())?;
    let alias_parent = tempfile::tempdir()?;
    let alias = alias_parent.path().join("repository-alias");
    symlink(repository.path(), &alias)?;
    let disguised_alias = alias.join(".");
    let repository_parent = repository
        .path()
        .parent()
        .ok_or("fixture repository must have a parent")?;
    let repository_name = repository
        .path()
        .file_name()
        .ok_or("fixture repository must have a final path component")?;
    let parent_alias = alias_parent.path().join("repository-parent-alias");
    symlink(repository_parent, &parent_alias)?;
    let intermediate_alias = parent_alias.join(repository_name);
    let parent_dir_disguised_alias = intermediate_alias.join("..").join(repository_name);

    for repository_root in [
        &alias,
        &disguised_alias,
        &intermediate_alias,
        &parent_dir_disguised_alias,
    ] {
        let graph = build_fixture_graph(repository_root)?;
        let verdict = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "docs/evidence/dropin-certification-verdict.json",
        )?;
        assert_eq!(
            verdict.freshness_status,
            Some(EvidenceFreshnessStatus::Uncertified)
        );
        assert_eq!(
            verdict.metadata.get("release_claim_allowed"),
            Some(&json!(false))
        );
        assert_eq!(
            verdict.metadata.get("freshness_reason"),
            Some(&json!("dropin_verdict_source_binding_unavailable"))
        );
    }

    let real_parent_dir_path = repository.path().join("..").join(repository_name);
    let graph = build_fixture_graph(&real_parent_dir_path)?;
    let verdict = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        verdict.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );
    assert_eq!(
        verdict.metadata.get("release_claim_allowed"),
        Some(&json!(true))
    );
    Ok(())
}

#[test]
fn canonical_dropin_verdict_rejects_non_evidence_source_descendants() -> TestResult {
    let temp = fixture_workspace()?;
    install_canonical_dropin_claim_fixture(temp.path())?;
    let source_path = temp.path().join("src/lib.rs");
    let mut source = fs::read_to_string(&source_path)?;
    source.push_str("\npub fn post_certification_change() {}\n");
    fs::write(source_path, source)?;
    commit_fixture_path(temp.path(), "src/lib.rs", "change release source")?;

    let graph = build_fixture_graph(temp.path())?;
    let verdict = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        verdict.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    assert_eq!(
        verdict.metadata.get("freshness_reason"),
        Some(&json!("dropin_verdict_source_commit_not_release_bound"))
    );
    Ok(())
}

#[test]
fn canonical_critical_evidence_validators_ignore_payload_schema_dispatch() -> TestResult {
    for schema in [None, Some("fixture.attacker_budget.v99")] {
        let temp = fixture_workspace()?;
        let path = temp.path().join("tests/perf/reports/budget_summary.json");
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        match schema {
            Some(schema) => value["schema"] = json!(schema),
            None => {
                value
                    .as_object_mut()
                    .ok_or("performance fixture must be an object")?
                    .remove("schema");
            }
        }
        fs::write(&path, serde_json::to_vec_pretty(&value)?)?;

        assert_performance_fixture_reason(
            temp.path(),
            "tests/perf/reports/budget_summary.json",
            "performance_budget_schema_not_current",
        )?;
    }

    for mutation in ["missing_verdict", "unknown_schema"] {
        let temp = fixture_workspace()?;
        let path = temp
            .path()
            .join("docs/evidence/dropin-certification-verdict.json");
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        let expected_reason = if mutation == "missing_verdict" {
            value
                .as_object_mut()
                .ok_or("drop-in fixture must be an object")?
                .remove("overall_verdict");
            "overall_verdict_missing_or_invalid"
        } else {
            value["schema"] = json!("fixture.attacker_dropin_verdict.v99");
            "dropin_verdict_schema_invalid"
        };
        fs::write(&path, serde_json::to_vec_pretty(&value)?)?;

        let graph = build_fixture_graph(temp.path())?;
        let node = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "docs/evidence/dropin-certification-verdict.json",
        )?;
        assert_eq!(
            node.freshness_status,
            Some(EvidenceFreshnessStatus::Malformed)
        );
        assert_eq!(
            node.metadata.get("release_claim_allowed"),
            Some(&json!(false))
        );
        assert_eq!(
            node.metadata.get("freshness_reason"),
            Some(&json!(expected_reason))
        );
    }
    Ok(())
}

#[test]
fn evidence_freshness_rejects_timestamps_beyond_clock_skew() -> TestResult {
    let options = SemanticWorkspaceGraphBuildOptions {
        reference_time_utc: Some(reference_time()?),
        ..SemanticWorkspaceGraphBuildOptions::default()
    };
    let generic = json!({
        "schema": "fixture.future.v1",
        "generated_at": "2026-05-13T00:05:01Z",
        "claim_surface": "release_facing"
    });
    assert_eq!(
        classify_evidence_freshness(&generic, &options),
        (
            EvidenceFreshnessStatus::Malformed,
            false,
            "generated_at_in_future".to_string()
        )
    );

    let temp = fixture_workspace()?;
    install_canonical_dropin_claim_fixture(temp.path())?;
    let path = temp
        .path()
        .join("docs/evidence/dropin-certification-verdict.json");
    let mut verdict: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    verdict["generated_at_utc"] = json!("2026-05-13T00:05:01Z");
    fs::write(&path, serde_json::to_vec_pretty(&verdict)?)?;
    commit_fixture_path(
        temp.path(),
        "docs/evidence/dropin-certification-verdict.json",
        "forge future verdict timestamp",
    )?;
    let graph = build_fixture_graph(temp.path())?;
    let node = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        node.metadata.get("freshness_reason"),
        Some(&json!("generated_at_in_future"))
    );
    assert_eq!(
        node.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    Ok(())
}

#[test]
fn generic_release_evidence_defaults_to_fourteen_day_freshness() -> TestResult {
    let options = SemanticWorkspaceGraphBuildOptions {
        reference_time_utc: Some(reference_time()?),
        ..SemanticWorkspaceGraphBuildOptions::default()
    };
    for (generated_at, expected_status, expected_allowed) in [
        (
            "2026-04-29T00:00:00Z",
            EvidenceFreshnessStatus::Current,
            true,
        ),
        (
            "2026-04-28T23:59:59Z",
            EvidenceFreshnessStatus::Stale,
            false,
        ),
    ] {
        let evidence = json!({
            "schema": "fixture.release_evidence.v1",
            "generated_at": generated_at,
            "claim_surface": "release_facing"
        });
        let classification = classify_evidence_freshness(&evidence, &options);
        assert_eq!(classification.0, expected_status);
        assert_eq!(classification.1, expected_allowed);
    }
    Ok(())
}

#[test]
fn ordinary_evidence_defaults_to_exact_twenty_four_hour_freshness() -> TestResult {
    let options = SemanticWorkspaceGraphBuildOptions {
        reference_time_utc: Some(reference_time()?),
        ..SemanticWorkspaceGraphBuildOptions::default()
    };
    for (generated_at, expected_status, expected_allowed) in [
        (
            "2026-05-12T00:00:00Z",
            EvidenceFreshnessStatus::Current,
            true,
        ),
        (
            "2026-05-11T23:59:59Z",
            EvidenceFreshnessStatus::Stale,
            false,
        ),
    ] {
        let evidence = json!({
            "schema": "fixture.ordinary_evidence.v1",
            "generated_at": generated_at
        });
        let classification = classify_evidence_freshness(&evidence, &options);
        assert_eq!(classification.0, expected_status);
        assert_eq!(classification.1, expected_allowed);
    }
    Ok(())
}

#[test]
fn invalid_generic_freshness_window_fails_closed_without_panicking() -> TestResult {
    let options = SemanticWorkspaceGraphBuildOptions {
        reference_time_utc: Some(reference_time()?),
        stale_after_days: i64::MAX,
        ..SemanticWorkspaceGraphBuildOptions::default()
    };
    let evidence = json!({
        "schema": "fixture.ordinary_evidence.v1",
        "generated_at": "2026-05-13T00:00:00Z"
    });
    assert_eq!(
        classify_evidence_freshness(&evidence, &options),
        (
            EvidenceFreshnessStatus::Stale,
            false,
            "generated_at_older_than_policy".to_string()
        )
    );
    Ok(())
}

#[test]
fn extreme_reference_time_does_not_overflow_future_skew_check() {
    let options = SemanticWorkspaceGraphBuildOptions {
        reference_time_utc: Some(DateTime::<Utc>::MAX_UTC),
        ..SemanticWorkspaceGraphBuildOptions::default()
    };
    let evidence = json!({
        "schema": "fixture.ordinary_evidence.v1",
        "generated_at": "2026-05-13T00:00:00Z"
    });
    assert_eq!(
        classify_evidence_freshness(&evidence, &options),
        (
            EvidenceFreshnessStatus::Stale,
            false,
            "generated_at_older_than_policy".to_string()
        )
    );
}

#[test]
fn performance_budget_freshness_requires_current_global_claim_readiness() -> TestResult {
    let options = SemanticWorkspaceGraphBuildOptions {
        reference_time_utc: Some(reference_time()?),
        ..SemanticWorkspaceGraphBuildOptions::default()
    };

    let mut legacy = semantic_perf_budget_fixture();
    legacy["schema"] = json!("pi.perf.budget_summary.v1");
    let legacy_classification = classify_evidence_freshness(&legacy, &options);
    assert_eq!(legacy_classification.0, EvidenceFreshnessStatus::Malformed);
    assert!(!legacy_classification.1);

    let mut blocked = semantic_perf_budget_fixture();
    let total_budgets = blocked["total_budgets"]
        .as_u64()
        .ok_or("fixture total_budgets must be an integer")?;
    blocked["budget_results"][1]["actual"] = serde_json::Value::Null;
    blocked["budget_results"][1]["status"] = json!("NO_DATA");
    blocked["pass"] = json!(total_budgets - 1);
    blocked["no_data"] = json!(1);
    blocked["claim_readiness"] = json!({
        "status": "blocked",
        "performance_claims_authorized": false,
        "blocking_reason_codes": ["budget_data_missing"]
    });
    let blocked_classification = classify_evidence_freshness(&blocked, &options);
    assert_eq!(
        blocked_classification.0,
        EvidenceFreshnessStatus::Uncertified
    );
    assert!(!blocked_classification.1);

    let mut ci_exceeds_total = semantic_perf_budget_fixture();
    ci_exceeds_total["ci_enforced"] = json!(99);
    ci_exceeds_total["ci_with_data"] = json!(99);
    let mut incomplete_ci_partition = semantic_perf_budget_fixture();
    incomplete_ci_partition["ci_with_data"] = json!(1);
    let mut ci_fail_exceeds_global_fail = semantic_perf_budget_fixture();
    ci_fail_exceeds_global_fail["ci_fail"] = json!(1);
    for contradictory in [
        ci_exceeds_total,
        incomplete_ci_partition,
        ci_fail_exceeds_global_fail,
    ] {
        let classification = classify_evidence_freshness(&contradictory, &options);
        assert_eq!(classification.0, EvidenceFreshnessStatus::Malformed);
        assert!(!classification.1);
    }

    let ready_classification =
        classify_evidence_freshness(&semantic_perf_budget_fixture(), &options);
    assert_eq!(ready_classification.0, EvidenceFreshnessStatus::Uncertified);
    assert!(!ready_classification.1);
    assert_eq!(
        ready_classification.2,
        "performance_budget_source_binding_unavailable"
    );

    let historical = json!({
        "schema": "pi.perf.budget_summary.v1",
        "generated_at": "2026-05-13T00:00:00Z",
        "claim_surface": "historical_snapshot"
    });
    let historical_classification = classify_evidence_freshness(&historical, &options);
    assert_eq!(
        historical_classification.0,
        EvidenceFreshnessStatus::HistoricalSnapshot
    );
    assert!(!historical_classification.1);
    Ok(())
}

#[test]
fn canonical_performance_budget_uses_release_gate_age_limit() -> TestResult {
    for (generated_at, expected_status, expected_allowed) in [
        (
            "2026-05-06T00:00:00.000Z",
            EvidenceFreshnessStatus::Current,
            true,
        ),
        (
            "2026-05-05T23:59:59.000Z",
            EvidenceFreshnessStatus::Stale,
            false,
        ),
    ] {
        let temp = fixture_workspace()?;
        bind_fixture_performance_summary_to_source(temp.path())?;
        let path = temp.path().join("tests/perf/reports/budget_summary.json");
        let mut summary: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        summary["generated_at"] = json!(generated_at);
        fs::write(&path, serde_json::to_vec_pretty(&summary)?)?;
        commit_fixture_path(
            temp.path(),
            "tests/perf/reports/budget_summary.json",
            &format!("set performance evidence age {generated_at}"),
        )?;

        let graph = build_fixture_graph(temp.path())?;
        let performance = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "tests/perf/reports/budget_summary.json",
        )?;
        assert_eq!(performance.freshness_status, Some(expected_status));
        assert_eq!(
            performance.metadata.get("release_claim_allowed"),
            Some(&json!(expected_allowed))
        );
    }
    Ok(())
}

#[test]
fn canonical_blocked_performance_budget_still_enforces_age_limit() -> TestResult {
    for (generated_at, expected_status) in [
        (
            "2026-05-06T00:00:00.000Z",
            EvidenceFreshnessStatus::Uncertified,
        ),
        ("2026-05-05T23:59:59.999Z", EvidenceFreshnessStatus::Stale),
    ] {
        let temp = fixture_workspace()?;
        bind_fixture_performance_summary_to_source(temp.path())?;
        let path = temp.path().join("tests/perf/reports/budget_summary.json");
        let mut summary: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        let total_budgets = summary["total_budgets"]
            .as_u64()
            .ok_or("fixture total_budgets must be an integer")?;
        summary["generated_at"] = json!(generated_at);
        summary["budget_results"][1]["actual"] = serde_json::Value::Null;
        summary["budget_results"][1]["status"] = json!("NO_DATA");
        summary["pass"] = json!(total_budgets - 1);
        summary["no_data"] = json!(1);
        summary["claim_readiness"] = json!({
            "status": "blocked",
            "performance_claims_authorized": false,
            "blocking_reason_codes": ["budget_data_missing"]
        });
        fs::write(&path, serde_json::to_vec_pretty(&summary)?)?;
        commit_fixture_path(
            temp.path(),
            "tests/perf/reports/budget_summary.json",
            &format!("set blocked performance evidence age {generated_at}"),
        )?;

        let graph = build_fixture_graph(temp.path())?;
        let performance = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "tests/perf/reports/budget_summary.json",
        )?;
        assert_eq!(performance.freshness_status, Some(expected_status));
        assert_eq!(
            performance.metadata.get("release_claim_allowed"),
            Some(&json!(false))
        );
    }
    Ok(())
}

#[test]
fn performance_budget_freshness_rejects_missing_or_forged_detail_rows() -> TestResult {
    let options = SemanticWorkspaceGraphBuildOptions {
        reference_time_utc: Some(reference_time()?),
        ..SemanticWorkspaceGraphBuildOptions::default()
    };

    for field in ["budgets", "budget_results", "failing_data_contracts"] {
        let mut missing = semantic_perf_budget_fixture();
        missing
            .as_object_mut()
            .ok_or("performance fixture must be an object")?
            .remove(field);
        let classification = classify_evidence_freshness(&missing, &options);
        assert_eq!(classification.0, EvidenceFreshnessStatus::Malformed);
        assert!(!classification.1);
    }

    let mut forged_status = semantic_perf_budget_fixture();
    forged_status["budget_results"][0]["status"] = json!("FAIL");

    let mut forged_count = semantic_perf_budget_fixture();
    let pass = forged_count["pass"]
        .as_u64()
        .ok_or("fixture pass count must be an integer")?;
    forged_count["pass"] = json!(pass - 1);
    forged_count["fail"] = json!(1);

    let mut reordered_results = semantic_perf_budget_fixture();
    reordered_results["budget_results"]
        .as_array_mut()
        .ok_or("fixture budget_results must be an array")?
        .swap(0, 1);

    let mut forged_inventory = semantic_perf_budget_fixture();
    forged_inventory["budgets"][0]["methodology"] = json!("forged methodology");

    let mut forged_contract_failures = semantic_perf_budget_fixture();
    forged_contract_failures["failing_data_contracts"] = json!([{
        "contract_id": "forged-contract",
        "detail": "forged detail",
        "remediation": "forged remediation"
    }]);

    for forged in [
        forged_status,
        forged_count,
        reordered_results,
        forged_inventory,
        forged_contract_failures,
    ] {
        let classification = classify_evidence_freshness(&forged, &options);
        assert_eq!(classification.0, EvidenceFreshnessStatus::Malformed);
        assert!(!classification.1);
    }
    Ok(())
}

#[test]
fn performance_budget_freshness_rejects_unresolvable_source_commit() -> TestResult {
    let temp = fixture_workspace()?;
    initialize_fixture_git_workspace(temp.path())?;
    let graph = build_fixture_graph(temp.path())?;
    let perf_budget = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "tests/perf/reports/budget_summary.json",
    )?;

    assert_eq!(
        perf_budget.freshness_status,
        Some(EvidenceFreshnessStatus::Malformed)
    );
    assert_eq!(
        perf_budget.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    assert_eq!(
        perf_budget.metadata.get("freshness_reason"),
        Some(&json!("performance_budget_source_commit_unresolvable"))
    );
    Ok(())
}

#[test]
fn performance_budget_freshness_accepts_clean_head_bound_artifact() -> TestResult {
    let temp = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(temp.path())?;
    let graph = build_fixture_graph(temp.path())?;
    let perf_budget = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "tests/perf/reports/budget_summary.json",
    )?;

    assert_eq!(
        perf_budget.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );
    assert_eq!(
        perf_budget.metadata.get("release_claim_allowed"),
        Some(&json!(true))
    );
    assert_eq!(
        perf_budget.metadata.get("freshness_reason"),
        Some(&json!("generated_at_within_policy"))
    );
    Ok(())
}

#[test]
fn performance_budget_source_binding_rejects_dirty_staged_and_untracked_sources() -> TestResult {
    let dirty = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(dirty.path())?;
    fs::OpenOptions::new()
        .append(true)
        .open(dirty.path().join("src/lib.rs"))?
        .write_all(b"\n// unstaged source change\n")?;
    assert_performance_fixture_reason(
        dirty.path(),
        "tests/perf/reports/budget_summary.json",
        "performance_budget_repository_not_clean",
    )?;

    let staged = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(staged.path())?;
    fs::OpenOptions::new()
        .append(true)
        .open(staged.path().join("src/lib.rs"))?
        .write_all(b"\n// staged source change\n")?;
    run_fixture_git(staged.path(), &["add", "src/lib.rs"])?;
    assert_performance_fixture_reason(
        staged.path(),
        "tests/perf/reports/budget_summary.json",
        "performance_budget_repository_not_clean",
    )?;

    let untracked = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(untracked.path())?;
    write_fixture(
        untracked.path(),
        "src/untracked_release_source.rs",
        "pub fn untracked_release_source() {}\n",
    )?;
    assert_performance_fixture_reason(
        untracked.path(),
        "tests/perf/reports/budget_summary.json",
        "performance_budget_repository_not_clean",
    )?;

    #[cfg(unix)]
    {
        let filtered = fixture_workspace()?;
        write_fixture(
            filtered.path(),
            ".gitattributes",
            "src/lib.rs filter=canonical-source\n",
        )?;
        bind_fixture_performance_summary_with_source_hiding_filter(filtered.path())?;
        fs::OpenOptions::new()
            .append(true)
            .open(filtered.path().join("src/lib.rs"))?
            .write_all(b"\n// source dirt hidden by a clean filter\n")?;
        run_fixture_git(filtered.path(), &["add", "src/lib.rs"])?;
        run_fixture_git(filtered.path(), &["diff", "--cached", "--quiet"])?;
        assert!(
            fixture_git_output(
                filtered.path(),
                &[
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--no-renames",
                ],
            )?
            .is_empty(),
            "fixture must hide raw source drift from ordinary Git status"
        );
        assert_performance_fixture_reason(
            filtered.path(),
            "tests/perf/reports/budget_summary.json",
            "performance_budget_repository_tracked_state_not_head",
        )?;
    }
    Ok(())
}

#[test]
fn performance_budget_source_binding_rejects_hidden_index_flags() -> TestResult {
    let temp = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(temp.path())?;

    for (enable, disable) in [
        ("--assume-unchanged", "--no-assume-unchanged"),
        ("--skip-worktree", "--no-skip-worktree"),
    ] {
        run_fixture_git(temp.path(), &["update-index", enable, "src/lib.rs"])?;
        let graph_result = build_fixture_graph(temp.path());
        let restore_result = run_fixture_git(temp.path(), &["update-index", disable, "src/lib.rs"]);
        restore_result?;
        let graph = graph_result?;
        let perf_budget = node_with_source(
            &graph,
            SemanticNodeType::EvidenceArtifact,
            "tests/perf/reports/budget_summary.json",
        )?;
        assert_eq!(
            perf_budget.metadata.get("freshness_reason"),
            Some(&json!(
                "performance_budget_repository_index_flags_not_default"
            ))
        );
        assert_eq!(
            perf_budget.metadata.get("release_claim_allowed"),
            Some(&json!(false))
        );
    }
    Ok(())
}

#[test]
fn performance_budget_source_binding_rejects_untracked_or_substituted_artifact() -> TestResult {
    let untracked = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(untracked.path())?;
    let tracked_summary = fs::read_to_string(
        untracked
            .path()
            .join("tests/perf/reports/budget_summary.json"),
    )?;
    let untracked_path = "tests/perf/reports/untracked_summary.json";
    write_fixture(untracked.path(), untracked_path, &tracked_summary)?;
    ignore_fixture_path(untracked.path(), untracked_path)?;
    assert_performance_fixture_reason(
        untracked.path(),
        untracked_path,
        "performance_budget_artifact_not_tracked_at_head",
    )?;

    #[cfg(unix)]
    {
        let substituted = fixture_workspace()?;
        let substituted_path = substituted
            .path()
            .join("tests/perf/reports/budget_summary.json");
        write_fixture(
            substituted.path(),
            ".gitattributes",
            "tests/perf/reports/budget_summary.json filter=canonical-summary\n",
        )?;
        bind_fixture_performance_summary_with_hiding_filter(substituted.path())?;
        let original = fs::read_to_string(&substituted_path)?;
        fs::write(&substituted_path, format!("{original} \n"))?;
        run_fixture_git(
            substituted.path(),
            &["add", "tests/perf/reports/budget_summary.json"],
        )?;
        run_fixture_git(substituted.path(), &["diff", "--cached", "--quiet"])?;
        let hidden_status = fixture_git_output(
            substituted.path(),
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--no-renames",
            ],
        )?;
        assert!(
            hidden_status.is_empty(),
            "fixture must hide the raw artifact substitution from Git status: {hidden_status:?}"
        );
        assert_performance_fixture_reason(
            substituted.path(),
            "tests/perf/reports/budget_summary.json",
            "performance_budget_repository_tracked_state_not_head",
        )?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn performance_budget_source_binding_rejects_symlink_artifact() -> TestResult {
    use std::os::unix::fs::symlink;

    let temp = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(temp.path())?;
    let symlink_path = "tests/perf/reports/symlink_summary.json";
    symlink("budget_summary.json", temp.path().join(symlink_path))?;
    ignore_fixture_path(temp.path(), symlink_path)?;
    assert_performance_fixture_reason(
        temp.path(),
        symlink_path,
        "performance_budget_artifact_symlink",
    )?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn performance_budget_source_binding_verifies_tracked_symlinks_and_file_modes() -> TestResult {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let symlink_fixture = fixture_workspace()?;
    symlink("lib.rs", symlink_fixture.path().join("src/tracked-link.rs"))?;
    bind_fixture_performance_summary_to_source(symlink_fixture.path())?;
    let graph = build_fixture_graph(symlink_fixture.path())?;
    let node = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "tests/perf/reports/budget_summary.json",
    )?;
    assert_eq!(
        node.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );

    let executable_fixture = fixture_workspace()?;
    let executable_path = executable_fixture.path().join("src/lib.rs");
    fs::set_permissions(&executable_path, fs::Permissions::from_mode(0o755))?;
    bind_fixture_performance_summary_to_source(executable_fixture.path())?;
    let graph = build_fixture_graph(executable_fixture.path())?;
    let node = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "tests/perf/reports/budget_summary.json",
    )?;
    assert_eq!(
        node.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );

    let hidden_mode_fixture = fixture_workspace()?;
    let hidden_mode_path = hidden_mode_fixture.path().join("src/lib.rs");
    fs::set_permissions(&hidden_mode_path, fs::Permissions::from_mode(0o644))?;
    bind_fixture_performance_summary_to_source(hidden_mode_fixture.path())?;
    run_fixture_git(
        hidden_mode_fixture.path(),
        &["config", "core.filemode", "false"],
    )?;
    fs::set_permissions(&hidden_mode_path, fs::Permissions::from_mode(0o755))?;
    assert!(
        fixture_git_output(
            hidden_mode_fixture.path(),
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--no-renames",
            ],
        )?
        .is_empty(),
        "fixture must hide chmod drift from ordinary Git status"
    );
    assert_performance_fixture_reason(
        hidden_mode_fixture.path(),
        "tests/perf/reports/budget_summary.json",
        "performance_budget_repository_tracked_state_not_head",
    )?;
    Ok(())
}

#[test]
fn performance_budget_source_binding_binds_the_ingested_artifact_bytes() -> TestResult {
    let temp = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(temp.path())?;

    let artifact_path = temp.path().join("tests/perf/reports/budget_summary.json");
    let mut stale_captured_bytes = fs::read(&artifact_path)?;
    stale_captured_bytes.extend_from_slice(b" \n");
    fs::write(&artifact_path, stale_captured_bytes)?;
    assert_performance_fixture_reason(
        temp.path(),
        "tests/perf/reports/budget_summary.json",
        "performance_budget_artifact_changed_since_ingestion",
    )?;
    Ok(())
}

#[test]
fn performance_budget_source_binding_rejects_committed_source_followup() -> TestResult {
    let temp = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(temp.path())?;
    fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join("src/lib.rs"))?
        .write_all(b"\n// committed post-measurement source change\n")?;
    run_fixture_git(temp.path(), &["add", "src/lib.rs"])?;
    run_fixture_git(temp.path(), &["commit", "-m", "source followup"])?;
    assert_performance_fixture_reason(
        temp.path(),
        "tests/perf/reports/budget_summary.json",
        "performance_budget_source_commit_not_release_bound",
    )?;
    Ok(())
}

#[test]
fn performance_budget_source_binding_rejects_packaged_evidence_followup() -> TestResult {
    let packaged = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(packaged.path())?;
    let packaged_path = "docs/evidence/tool-output-context-cache.jsonl";
    write_fixture(
        packaged.path(),
        packaged_path,
        "{\"schema\":\"fixture.packaged_evidence.v1\"}\n",
    )?;
    run_fixture_git(packaged.path(), &["add", packaged_path])?;
    run_fixture_git(
        packaged.path(),
        &["commit", "-m", "packaged evidence followup"],
    )?;
    assert_performance_fixture_reason(
        packaged.path(),
        "tests/perf/reports/budget_summary.json",
        "performance_budget_source_commit_not_release_bound",
    )?;

    let nonpackaged = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(nonpackaged.path())?;
    let nonpackaged_path = "docs/evidence/nonpackaged-release-receipt.json";
    write_fixture(
        nonpackaged.path(),
        nonpackaged_path,
        r#"{
  "schema": "fixture.nonpackaged_evidence.v1",
  "generated_at": "2026-05-13T00:00:00Z"
}"#,
    )?;
    run_fixture_git(nonpackaged.path(), &["add", nonpackaged_path])?;
    run_fixture_git(
        nonpackaged.path(),
        &["commit", "-m", "nonpackaged evidence followup"],
    )?;
    let graph = build_fixture_graph(nonpackaged.path())?;
    let perf_budget = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "tests/perf/reports/budget_summary.json",
    )?;
    assert_eq!(
        perf_budget.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );
    assert_eq!(
        perf_budget.metadata.get("release_claim_allowed"),
        Some(&json!(true))
    );
    Ok(())
}

#[test]
fn performance_budget_source_binding_rejects_unproved_default_package_policy() -> TestResult {
    let temp = fixture_workspace()?;
    write_fixture(
        temp.path(),
        "Cargo.toml",
        r#"[package]
name = "semantic-workspace-fixture"
version = "0.0.0"
edition = "2024"
"#,
    )?;
    bind_fixture_performance_summary_to_source(temp.path())?;

    let evidence_path = "docs/evidence/unproved-package-policy.json";
    write_fixture(
        temp.path(),
        evidence_path,
        "{\"schema\":\"fixture.unproved_package_policy.v1\"}\n",
    )?;
    run_fixture_git(temp.path(), &["add", evidence_path])?;
    run_fixture_git(
        temp.path(),
        &["commit", "-m", "evidence followup without include policy"],
    )?;
    assert_performance_fixture_reason(
        temp.path(),
        "tests/perf/reports/budget_summary.json",
        "performance_budget_source_commit_not_release_bound",
    )?;
    Ok(())
}

const HOSTILE_GIT_CHILD_ROOT: &str = "PI_SEMANTIC_HOSTILE_GIT_CHILD_ROOT";

#[test]
fn performance_budget_source_binding_hostile_git_environment_child() -> TestResult {
    let Some(root) = std::env::var_os(HOSTILE_GIT_CHILD_ROOT) else {
        return Ok(());
    };
    let graph = build_fixture_graph(Path::new(&root))?;
    let perf_budget = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "tests/perf/reports/budget_summary.json",
    )?;
    assert_eq!(
        perf_budget.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );
    assert_eq!(
        perf_budget.metadata.get("release_claim_allowed"),
        Some(&json!(true))
    );
    Ok(())
}

#[test]
fn performance_budget_source_binding_ignores_hostile_git_environment() -> TestResult {
    let temp = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(temp.path())?;
    let hostile_index = temp.path().join(".git/hostile-index");
    fs::write(&hostile_index, b"malformed alternate index")?;

    let mut child = Command::new(std::env::current_exe()?);
    child
        .arg("performance_budget_source_binding_hostile_git_environment_child")
        .args(["--exact", "--nocapture"])
        .env(HOSTILE_GIT_CHILD_ROOT, temp.path())
        .env("GIT_INDEX_FILE", &hostile_index)
        .env("GIT_DIR", temp.path().join(".git/hostile-dir"))
        .env("GIT_WORK_TREE", temp.path().join("hostile-worktree"))
        .env(
            "GIT_COMMON_DIR",
            temp.path().join(".git/hostile-common-dir"),
        )
        .env(
            "GIT_OBJECT_DIRECTORY",
            temp.path().join(".git/hostile-objects"),
        )
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            temp.path().join(".git/hostile-alternate-objects"),
        )
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.worktree")
        .env(
            "GIT_CONFIG_VALUE_0",
            temp.path().join("hostile-config-worktree"),
        );
    #[cfg(unix)]
    let fake_git_marker = {
        use std::os::unix::fs::PermissionsExt as _;

        let fake_git_dir = temp.path().join(".git/hostile-bin");
        let fake_git = fake_git_dir.join("git");
        let marker = temp.path().join(".git/fake-git-invoked");
        fs::create_dir_all(&fake_git_dir)?;
        fs::write(
            &fake_git,
            format!(
                "#!/bin/sh\nprintf invoked > {}\nexit 97\n",
                shell_single_quote(&marker)?
            ),
        )?;
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755))?;
        let original_path = std::env::var_os("PATH").unwrap_or_default();
        let hostile_path = std::env::join_paths(
            std::iter::once(fake_git_dir).chain(std::env::split_paths(&original_path)),
        )?;
        child.env("PATH", hostile_path);
        marker
    };
    let output = child.output()?;
    if !output.status.success() {
        return Err(format!(
            "hostile Git environment child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    #[cfg(unix)]
    assert!(
        !fake_git_marker.exists(),
        "source binding executed the attacker-controlled Git binary from PATH"
    );
    Ok(())
}

#[test]
fn performance_budget_source_binding_ignores_local_core_worktree_redirect() -> TestResult {
    let temp = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(temp.path())?;
    let mirror_parent = tempfile::Builder::new()
        .tempdir_in(std::env::var_os("TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from))?;
    let mirror = mirror_parent.path().join("clean-mirror");
    let mirror_text = mirror.to_string_lossy().into_owned();
    run_fixture_git(
        temp.path(),
        &["worktree", "add", "--detach", &mirror_text, "HEAD"],
    )?;
    run_fixture_git(temp.path(), &["config", "core.worktree", &mirror_text])?;

    fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join("src/lib.rs"))?
        .write_all(b"\n// dirt hidden by hostile local core.worktree\n")?;
    assert_performance_fixture_reason(
        temp.path(),
        "tests/perf/reports/budget_summary.json",
        "performance_budget_repository_not_clean",
    )?;
    Ok(())
}

#[test]
fn builder_indexes_workspace_surfaces_and_classifies_fail_closed() -> TestResult {
    let temp = fixture_workspace()?;
    bind_fixture_performance_summary_to_source(temp.path())?;
    let graph = build_fixture_graph(temp.path())?;
    let graph_again = build_fixture_graph(temp.path())?;

    assert_eq!(
        serde_json::to_value(&graph)?,
        serde_json::to_value(&graph_again)?
    );

    for node_type in [
        SemanticNodeType::CodeSymbol,
        SemanticNodeType::FileRegion,
        SemanticNodeType::TestCase,
        SemanticNodeType::DocSection,
        SemanticNodeType::EvidenceArtifact,
        SemanticNodeType::Bead,
        SemanticNodeType::ProviderSurface,
        SemanticNodeType::ValidationCommand,
    ] {
        assert!(
            !graph.nodes_by_type(node_type).is_empty(),
            "expected at least one {node_type:?} node"
        );
    }

    let dropin_verdict = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(
        dropin_verdict.freshness_status,
        Some(EvidenceFreshnessStatus::Malformed)
    );
    assert_eq!(
        dropin_verdict.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    assert_eq!(
        dropin_verdict.metadata.get("claim_gate_status"),
        Some(&json!("blocked_malformed"))
    );
    assert_eq!(
        dropin_verdict
            .metadata
            .get("strict_replacement_claim_allowed"),
        Some(&json!(false))
    );

    let uncertified = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/uncertified.json",
    )?;
    assert_eq!(
        uncertified.freshness_status,
        Some(EvidenceFreshnessStatus::Uncertified)
    );
    assert_eq!(
        uncertified.metadata.get("claim_gate_status"),
        Some(&json!("blocked_uncertified"))
    );

    let perf_budget = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "tests/perf/reports/budget_summary.json",
    )?;
    assert_eq!(
        perf_budget.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );
    assert_eq!(
        perf_budget.metadata.get("claim_gate_status"),
        Some(&json!("allowed"))
    );
    assert_eq!(
        perf_budget.metadata.get("claim_readiness_status"),
        Some(&json!("claim_ready"))
    );
    assert_eq!(
        perf_budget.metadata.get("performance_claims_authorized"),
        Some(&json!(true))
    );
    assert_eq!(
        perf_budget.metadata.get("blocking_reason_codes"),
        Some(&json!([]))
    );

    let extension_closeout = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/extension-health-delta-failure-disposition.json",
    )?;
    assert_eq!(
        extension_closeout.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );
    assert_eq!(
        extension_closeout
            .metadata
            .get("source_report_generated_at"),
        Some(&json!("2026-05-13T00:00:00Z"))
    );

    let parity_ledger = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-parity-gap-ledger.json",
    )?;
    assert_eq!(
        parity_ledger.freshness_status,
        Some(EvidenceFreshnessStatus::Current)
    );
    assert_eq!(
        parity_ledger.metadata.get("generated_at"),
        Some(&json!("2026-05-13T00:00:00Z"))
    );

    let malformed = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/malformed.json",
    )?;
    assert_eq!(
        malformed.freshness_status,
        Some(EvidenceFreshnessStatus::Malformed)
    );

    let missing = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/missing.json",
    )?;
    assert_eq!(
        missing.freshness_status,
        Some(EvidenceFreshnessStatus::Missing)
    );
    assert_eq!(
        graph.evidence_status_for_path("docs/evidence/missing.json"),
        Some(EvidenceFreshnessStatus::Missing)
    );
    assert_eq!(
        graph.release_claim_allowed_for_path("docs/evidence/missing.json"),
        Some(false)
    );
    assert!(
        graph
            .suppressible_claim_evidence()
            .iter()
            .any(|node| { node.source_path == "docs/evidence/missing.json" })
    );

    for cited_path in [
        "docs/evidence/dropin-certification-verdict.json",
        "docs/evidence/uncertified.json",
        "docs/evidence/missing.json",
        "tests/perf/reports/budget_summary.json",
        "docs/evidence/extension-health-delta-failure-disposition.json",
        "docs/evidence/dropin-parity-gap-ledger.json",
    ] {
        let target = node_with_source(&graph, SemanticNodeType::EvidenceArtifact, cited_path)?;
        assert!(
            graph.edges.iter().any(|edge| {
                edge.edge_type == SemanticEdgeType::CitesEvidence
                    && edge.target == target.id
                    && edge.metadata.get("citation_path") == Some(&json!(cited_path))
            }),
            "missing citation edge for {cited_path}"
        );
    }

    assert_eq!(
        bead_status(&graph, "bd-open")?,
        BeadActionabilityStatus::ActionableOpen
    );
    assert_eq!(
        bead_status(&graph, "bd-blocked")?,
        BeadActionabilityStatus::Blocked
    );
    assert_eq!(
        bead_status(&graph, "bd-claimed")?,
        BeadActionabilityStatus::ClaimedInProgress
    );
    assert_eq!(
        bead_status(&graph, "bd-closed")?,
        BeadActionabilityStatus::ClosedReferenceOnly
    );
    assert_eq!(
        bead_status(&graph, "bd-tombstone")?,
        BeadActionabilityStatus::TombstoneReferenceOnly
    );
    assert_eq!(
        bead_status(&graph, "malformed-line-6")?,
        BeadActionabilityStatus::UnknownFailClosed
    );

    let open_bead = graph
        .nodes
        .iter()
        .find(|node| {
            node.node_type == SemanticNodeType::Bead
                && node.metadata.get("bead_id") == Some(&json!("bd-open"))
        })
        .ok_or("missing bd-open bead node")?;
    assert_eq!(
        open_bead.metadata.get("external_ref"),
        Some(&json!("docs/evidence/dropin-parity-gap-ledger.json"))
    );
    assert!(graph.edges.iter().any(|edge| {
        edge.edge_type == SemanticEdgeType::Tracks
            && edge.reason == "bead_external_ref"
            && edge.source == open_bead.id
            && edge.target == parity_ledger.id
            && edge.metadata.get("external_ref")
                == Some(&json!("docs/evidence/dropin-parity-gap-ledger.json"))
    }));

    assert!(graph.trace.iter().any(|event| {
        event.status == GraphInputStatus::Missing
            && event.source_path == "docs/evidence/missing.json"
    }));
    assert!(graph.trace.iter().any(|event| {
        event.status == GraphInputStatus::Malformed
            && event.source_path == "docs/evidence/malformed.json"
    }));
    assert!(graph.trace.iter().any(|event| {
        event.status == GraphInputStatus::Malformed && event.source_path == ".beads/issues.jsonl"
    }));

    let command_nodes = graph.nodes_by_type(SemanticNodeType::ValidationCommand);
    assert!(command_nodes.iter().any(|node| {
        node.metadata.get("command") == Some(&json!("cargo test --test widget_flow builds_widget"))
    }));

    Ok(())
}

#[test]
fn planner_emits_budgeted_golden_bundles_for_core_task_shapes() -> TestResult {
    let temp = fixture_workspace()?;
    let graph = build_fixture_graph(temp.path())?;
    let planner = SemanticContextBundlePlanner::new(&graph);

    let provider = planner.plan(&ContextBundleRequest {
        query: Some("openai provider streaming".to_string()),
        budget: ContextBundleBudget {
            max_items: 3,
            max_bytes: 4096,
        },
        ..ContextBundleRequest::default()
    });
    assert_eq!(
        bundle_golden_summary(&provider),
        json!({
            "selected": [
                {
                    "path": "tests/provider_streaming.rs",
                    "title": "cargo test --test provider_streaming streams_openai_provider",
                    "reason": "query_match"
                },
                {
                    "path": "tests/provider_streaming.rs",
                    "title": "streams_openai_provider",
                    "reason": "query_match"
                },
                {
                    "path": "src/providers/openai.rs",
                    "title": "openai",
                    "reason": "query_match"
                }
            ],
            "stale_suppressions": [],
            "commands": ["cargo test --test provider_streaming streams_openai_provider"],
            "budget_excluded": 5
        })
    );

    let session = planner.plan(&ContextBundleRequest {
        query: Some("session persistence save".to_string()),
        budget: ContextBundleBudget {
            max_items: 3,
            max_bytes: 4096,
        },
        ..ContextBundleRequest::default()
    });
    assert_eq!(
        bundle_golden_summary(&session),
        json!({
            "selected": [
                {
                    "path": "tests/session_flow.rs",
                    "title": "cargo test --test session_flow saves_session",
                    "reason": "query_match"
                },
                {
                    "path": "tests/session_flow.rs",
                    "title": "saves_session",
                    "reason": "query_match"
                },
                {
                    "path": "src/session.rs",
                    "title": "save_session",
                    "reason": "query_match"
                }
            ],
            "stale_suppressions": [],
            "commands": ["cargo test --test session_flow saves_session"],
            "budget_excluded": 3
        })
    );

    let extension = planner.plan(&ContextBundleRequest {
        query: Some("extension closeout health_delta".to_string()),
        budget: ContextBundleBudget {
            max_items: 3,
            max_bytes: 4096,
        },
        ..ContextBundleRequest::default()
    });
    assert_eq!(
        bundle_golden_summary(&extension),
        json!({
            "selected": [
                {
                    "path": "docs/evidence/extension-health-delta-failure-disposition.json",
                    "title": "pi.ext.health_delta_failure_disposition.v1",
                    "reason": "query_match,current_release_claim_evidence"
                },
                {
                    "path": "tests/extension_flow.rs",
                    "title": "cargo test --test extension_flow loads_extension",
                    "reason": "query_match"
                },
                {
                    "path": "tests/extension_flow.rs",
                    "title": "loads_extension",
                    "reason": "query_match"
                }
            ],
            "stale_suppressions": [],
            "commands": ["cargo test --test extension_flow loads_extension"],
            "budget_excluded": 6
        })
    );

    let swarm = planner.plan(&ContextBundleRequest {
        query: Some("drop-in swarm claim readiness".to_string()),
        bead_id: Some("bd-open".to_string()),
        changed_paths: vec!["README.md".to_string()],
        budget: ContextBundleBudget {
            max_items: 4,
            max_bytes: 2048,
        },
        ..ContextBundleRequest::default()
    });
    let swarm_summary = bundle_golden_summary(&swarm);
    assert_eq!(
        swarm_summary["stale_suppressions"],
        json!([
            {
                "path": "docs/evidence/dropin-certification-verdict.json",
                "reason": "suppressed_stale_or_unsafe_evidence"
            },
            {
                "path": "docs/evidence/uncertified.json",
                "reason": "suppressed_stale_or_unsafe_evidence"
            },
            {
                "path": "docs/evidence/missing.json",
                "reason": "suppressed_stale_or_unsafe_evidence"
            },
            {
                "path": "tests/perf/reports/budget_summary.json",
                "reason": "suppressed_stale_or_unsafe_evidence"
            }
        ])
    );
    assert!(swarm.selected_items.iter().any(|item| {
        item.source_path == "docs/evidence/dropin-parity-gap-ledger.json"
            && item.reason.contains("related_to_bead_or_changed_path")
    }));
    assert!(
        swarm
            .excluded_items
            .iter()
            .any(|item| { item.reason == "budget_exceeded" })
    );

    let failing_command = planner.plan(&ContextBundleRequest {
        failing_command: Some("cargo test --test session_flow saves_session".to_string()),
        budget: ContextBundleBudget {
            max_items: 1,
            max_bytes: 512,
        },
        ..ContextBundleRequest::default()
    });
    assert_eq!(
        failing_command.suggested_validation_commands,
        vec!["cargo test --test session_flow saves_session"]
    );

    Ok(())
}

#[test]
fn planner_redacts_sensitive_artifacts_and_fails_closed_cache_validation() -> TestResult {
    let temp = fixture_workspace()?;
    add_sensitive_context_fixtures(temp.path())?;
    let graph = build_fixture_graph(temp.path())?;

    let vcr_node = graph
        .evidence_node_for_path("tests/fixtures/vcr/oauth_refresh_sensitive.json")
        .ok_or("missing sensitive vcr node")?;
    assert_eq!(vcr_node.redaction_status, RedactionStatus::UnsafeToEmit);
    assert_eq!(
        vcr_node
            .metadata
            .get("sensitive_path_kind")
            .and_then(serde_json::Value::as_str),
        Some("vcr_fixture")
    );
    let redacted_keys = vcr_node
        .metadata
        .get("redacted_metadata_keys")
        .and_then(serde_json::Value::as_array)
        .ok_or("missing redacted metadata keys")?;
    assert!(
        redacted_keys
            .iter()
            .any(|key| matches!(key.as_str(), Some("credential_like")))
    );
    assert!(
        redacted_keys
            .iter()
            .any(|key| matches!(key.as_str(), Some("prompt_or_payload")))
    );
    assert!(
        !format!("{:?}", vcr_node.metadata).contains("sk-secret"),
        "graph metadata must not retain raw secret values"
    );

    let log_node = graph
        .evidence_node_for_path("tests/fixtures/context_artifacts/provider-auth.log")
        .ok_or("missing sensitive log node")?;
    assert_eq!(log_node.redaction_status, RedactionStatus::UnsafeToEmit);
    assert_eq!(
        log_node
            .metadata
            .get("sensitive_path_kind")
            .and_then(serde_json::Value::as_str),
        Some("log_artifact")
    );

    let planner = SemanticContextBundlePlanner::new(&graph);
    let bundle = planner.plan(&ContextBundleRequest {
        query: Some("oauth vcr authorization token".to_string()),
        changed_paths: vec![
            "tests/fixtures/vcr/oauth_refresh_sensitive.json".to_string(),
            "../outside/auth.json".to_string(),
        ],
        workspace_id: Some("workspace-a".to_string()),
        branch: Some("main".to_string()),
        session_id: Some("session-a".to_string()),
        generated_at_utc: Some("2026-05-13T00:00:00Z".to_string()),
        cache_ttl_seconds: 900,
        budget: ContextBundleBudget {
            max_items: 6,
            max_bytes: 4096,
        },
        ..ContextBundleRequest::default()
    });

    assert!(
        bundle
            .selected_items
            .iter()
            .all(|item| { item.redaction_status != RedactionStatus::UnsafeToEmit })
    );
    assert!(bundle.excluded_items.iter().any(|item| {
        item.source_path == "tests/fixtures/vcr/oauth_refresh_sensitive.json"
            && item.reason.contains("unsafe_to_emit_by_redaction_policy")
            && item.reason.contains("sensitive_path:vcr_fixture")
    }));
    assert_eq!(
        bundle.redaction_summary.overall_status,
        RedactionStatus::UnsafeToEmit
    );
    assert!(bundle.redaction_summary.suppressed_unsafe_nodes >= 1);
    assert!(
        bundle
            .redaction_summary
            .sensitive_path_kinds
            .contains("vcr_fixture")
    );
    assert!(
        bundle
            .path_normalization
            .iter()
            .any(|path| { !path.accepted && path.reason == "parent_escape_rejected" })
    );

    let valid_probe = ContextBundleCacheProbe {
        workspace_id: "workspace-a".to_string(),
        branch: Some("main".to_string()),
        session_id: Some("session-a".to_string()),
        input_fingerprint_sha256: bundle.invalidation_policy.input_fingerprint_sha256.clone(),
        now_utc: Some("2026-05-13T00:05:00Z".to_string()),
    };
    assert!(
        bundle
            .invalidation_policy
            .validate_probe(&valid_probe)
            .valid
    );

    let expired_probe = ContextBundleCacheProbe {
        workspace_id: "workspace-a".to_string(),
        branch: Some("feature".to_string()),
        session_id: Some("session-a".to_string()),
        input_fingerprint_sha256: "changed".to_string(),
        now_utc: Some("2026-05-13T00:20:00Z".to_string()),
    };
    let expired = bundle.invalidation_policy.validate_probe(&expired_probe);
    assert!(!expired.valid);
    assert!(
        expired
            .invalidation_reasons
            .contains(&"branch_changed".to_string())
    );
    assert!(
        expired
            .invalidation_reasons
            .contains(&"input_fingerprint_changed".to_string())
    );
    assert!(
        expired
            .invalidation_reasons
            .contains(&"cache_ttl_expired".to_string())
    );

    Ok(())
}

#[test]
fn large_workspace_context_planner_budget_artifact_is_deterministic_under_randomized_order()
-> TestResult {
    let canonical_order = (0..48).collect::<Vec<_>>();
    let randomized_order = permuted_large_context_indices(48);
    let primary = large_context_fixture_workspace(&canonical_order)?;

    let cold_start = Instant::now();
    let _cold_graph = build_fixture_graph(primary.path())?;
    let cold_ms = elapsed_ms(cold_start);

    let warm_start = Instant::now();
    let _warm_graph = build_fixture_graph(primary.path())?;
    let warm_ms = elapsed_ms(warm_start);

    add_incremental_context_fixture(primary.path())?;
    let incremental_start = Instant::now();
    let incremental_graph = build_fixture_graph(primary.path())?;
    let incremental_ms = elapsed_ms(incremental_start);

    let request = ContextBundleRequest {
        query: Some("context planner budget incremental refresh".to_string()),
        changed_paths: vec![
            "src/context/incremental_refresh.rs".to_string(),
            "tests/context/incremental_refresh_flow.rs".to_string(),
        ],
        workspace_id: Some("context-budget-workspace".to_string()),
        branch: Some("main".to_string()),
        session_id: Some("context-budget-session".to_string()),
        generated_at_utc: Some("2026-05-13T00:00:00Z".to_string()),
        cache_ttl_seconds: 900,
        budget: ContextBundleBudget {
            max_items: 12,
            max_bytes: 16 * 1024,
        },
        ..ContextBundleRequest::default()
    };

    let planning_start = Instant::now();
    let bundle = SemanticContextBundlePlanner::new(&incremental_graph).plan(&request);
    let planning_ms = elapsed_ms(planning_start);

    let serialization_start = Instant::now();
    let bundle_json = serde_json::to_vec(&bundle)?;
    let serialization_ms = elapsed_ms(serialization_start);

    let replay = large_context_fixture_workspace(&randomized_order)?;
    add_incremental_context_fixture(replay.path())?;
    let replay_graph = build_fixture_graph(replay.path())?;
    let replay_bundle = SemanticContextBundlePlanner::new(&replay_graph).plan(&request);

    let bundle_summary = bundle_golden_summary(&bundle);
    let replay_summary = bundle_golden_summary(&replay_bundle);
    assert_eq!(
        bundle_summary, replay_summary,
        "large workspace planner output must not depend on filesystem creation order"
    );
    assert!(bundle.selected_items.iter().any(|item| {
        item.source_path == "src/context/incremental_refresh.rs"
            && item.reason.contains("related_to_bead_or_changed_path")
    }));
    assert!(bundle.estimated_bytes <= request.budget.max_bytes);

    let target_dir = resolved_cargo_target_dir(primary.path());
    let tmpdir = resolved_tmpdir();
    let artifact_dir = target_dir.join("perf");
    fs::create_dir_all(&artifact_dir)?;
    let summary_bytes = serde_json::to_vec(&bundle_summary)?;
    let summary_sha256 = format!("{:x}", Sha256::digest(&summary_bytes));
    let artifact = json!({
        "schema": "pi.semantic_context.performance_budget.v1",
        "generated_at": "2026-05-13T00:00:00Z",
        "run_id": "semantic-context-large-workspace-regression",
        "correlation_id": "semantic-context-large-workspace-regression",
        "environment": {
            "cargo_target_dir": target_dir.display().to_string(),
            "tmpdir": tmpdir.display().to_string()
        },
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH
        },
        "workspace": {
            "fixture": "synthetic_large_workspace",
            "file_order_cases": ["canonical", "permuted"],
            "graph_nodes": incremental_graph.nodes.len(),
            "graph_edges": incremental_graph.edges.len(),
            "trace_events": incremental_graph.trace.len()
        },
        "cache_hit_miss": {
            "cold_graph_build": "miss:no_prior_graph",
            "warm_graph_build": "hit:stable_input_fingerprints",
            "incremental_update": "miss:input_fingerprint_changed"
        },
        "determinism": {
            "randomized_file_order_checked": true,
            "matched": true,
            "first_summary_sha256": summary_sha256,
            "second_summary_sha256": summary_sha256
        },
        "metrics": {
            "context_graph_build_cold_ms": {"p95_ms": cold_ms},
            "context_graph_build_warm_ms": {"p95_ms": warm_ms},
            "context_incremental_update_ms": {"p95_ms": incremental_ms},
            "context_planning_ms": {"p95_ms": planning_ms},
            "context_bundle_serialization_ms": {"p95_ms": serialization_ms},
            "context_bundle_estimated_bytes": {"bytes": bundle.estimated_bytes}
        }
    });
    let artifact_path = artifact_dir.join("context_intelligence_planner_budget.json");
    fs::write(&artifact_path, serde_json::to_string_pretty(&artifact)?)?;
    let persisted: serde_json::Value = serde_json::from_slice(&fs::read(&artifact_path)?)?;
    assert_eq!(
        persisted["schema"],
        json!("pi.semantic_context.performance_budget.v1")
    );
    assert_eq!(
        persisted["environment"]["cargo_target_dir"],
        json!(target_dir.display().to_string())
    );
    assert_eq!(
        persisted["environment"]["tmpdir"],
        json!(tmpdir.display().to_string())
    );
    assert_eq!(persisted["determinism"]["matched"], json!(true));
    assert!(
        persisted["metrics"]["context_graph_build_cold_ms"]["p95_ms"]
            .as_f64()
            .is_some_and(|value| value.is_finite() && value > 0.0)
    );
    assert!(
        persisted["metrics"]["context_bundle_estimated_bytes"]["bytes"]
            .as_u64()
            .is_some_and(|value| value > 0)
    );
    assert!(!String::from_utf8(bundle_json)?.contains("sk-secret"));

    Ok(())
}

#[test]
fn no_mock_context_intelligence_e2e_logs_and_replays_real_workspace() -> TestResult {
    let runtime = RuntimeBuilder::current_thread().build()?;

    runtime.block_on(async {
        let temp = fixture_workspace()?;
        add_sensitive_context_fixtures(temp.path())?;
        initialize_fixture_git_workspace(temp.path())?;

        let graph = build_fixture_graph(temp.path())?;
        let planner = SemanticContextBundlePlanner::new(&graph);
        let request = ContextBundleRequest {
            query: Some("openai provider streaming oauth drop-in parity ledger".to_string()),
            bead_id: Some("bd-open".to_string()),
            changed_paths: vec![
                "src/providers/openai.rs".to_string(),
                "tests/fixtures/vcr/oauth_refresh_sensitive.json".to_string(),
                "README.md".to_string(),
                "../outside/auth.json".to_string(),
            ],
            failing_command: Some(
                "cargo test --test provider_streaming streams_openai_provider".to_string(),
            ),
            workspace_id: Some("workspace-context-e2e".to_string()),
            branch: Some("main".to_string()),
            session_id: Some("context-e2e-session".to_string()),
            generated_at_utc: Some("2026-05-13T00:00:00Z".to_string()),
            cache_ttl_seconds: 900,
            budget: ContextBundleBudget {
                max_items: 8,
                max_bytes: 8192,
            },
        };
        let bundle = planner.plan(&request);

        assert!(temp.path().join(".git").is_dir());
        assert!(bundle.budget.max_items >= bundle.selected_items.len());
        assert!(bundle.estimated_bytes <= bundle.budget.max_bytes);
        assert!(bundle.selected_items.iter().any(|item| {
            item.source_path == "src/providers/openai.rs" && item.reason.contains("query_match")
        }));
        assert!(bundle.selected_items.iter().any(|item| {
            item.source_path == "tests/provider_streaming.rs"
                && item.title.contains("provider_streaming")
        }));
        assert!(bundle.selected_items.iter().any(|item| {
            item.source_path == "docs/evidence/dropin-parity-gap-ledger.json"
                && item.reason.contains("related_to_bead_or_changed_path")
        }));
        for stale_path in [
            "docs/evidence/dropin-certification-verdict.json",
            "docs/evidence/uncertified.json",
            "docs/evidence/missing.json",
        ] {
            assert!(
                bundle
                    .stale_evidence_suppressions
                    .iter()
                    .any(|item| item.source_path == stale_path
                        && item.reason == "suppressed_stale_or_unsafe_evidence"),
                "missing stale suppression for {stale_path}"
            );
        }
        assert!(bundle.excluded_items.iter().any(|item| {
            item.source_path == "tests/fixtures/vcr/oauth_refresh_sensitive.json"
                && item.reason.contains("unsafe_to_emit_by_redaction_policy")
        }));
        assert!(bundle.redaction_summary.suppressed_unsafe_nodes >= 1);
        assert!(
            bundle
                .redaction_summary
                .sensitive_path_kinds
                .contains("vcr_fixture")
        );
        assert!(
            bundle
                .path_normalization
                .iter()
                .any(|path| !path.accepted && path.reason == "parent_escape_rejected")
        );
        assert_eq!(
            bundle.suggested_validation_commands,
            vec!["cargo test --test provider_streaming streams_openai_provider"]
        );
        assert!(bundle.invalidation_policy.cacheable);
        let valid_probe = ContextBundleCacheProbe {
            workspace_id: "workspace-context-e2e".to_string(),
            branch: Some("main".to_string()),
            session_id: Some("context-e2e-session".to_string()),
            input_fingerprint_sha256: bundle.invalidation_policy.input_fingerprint_sha256.clone(),
            now_utc: Some("2026-05-13T00:05:00Z".to_string()),
        };
        assert!(
            bundle
                .invalidation_policy
                .validate_probe(&valid_probe)
                .valid
        );

        let replay =
            SemanticContextBundlePlanner::new(&build_fixture_graph(temp.path())?).plan(&request);
        assert_eq!(
            serde_json::to_value(&bundle)?,
            serde_json::to_value(&replay)?
        );

        let provider = ContextE2eProvider::new();
        let calls = provider.calls();
        let agent = Agent::new(
            Arc::new(provider),
            ToolRegistry::from_tools(Vec::new()),
            AgentConfig::default(),
        );
        let sessions_root = temp.path().join(".pi-sessions");
        let mut session_state = Session::create_with_dir(Some(sessions_root.clone()));
        session_state.header.cwd = temp.path().display().to_string();
        session_state.header.id = "context-e2e-session".to_string();
        let session = Arc::new(Mutex::new(session_state));
        let mut agent_session = AgentSession::new(
            agent,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );
        agent_session.set_semantic_context_bundle(Some(
            SemanticContextBundleInjection::enabled(bundle.clone()).with_prompt_budget(8, 8192),
        ));

        agent_session
            .run_text("use no-mock context intelligence".to_string(), |_| {})
            .await?;

        let (call_count, captured) = {
            let calls = match calls.lock() {
                Ok(calls) => calls,
                Err(poisoned) => poisoned.into_inner(),
            };
            let call_count = calls.len();
            let captured = calls.first().cloned();
            drop(calls);
            (call_count, captured)
        };
        let Some(captured) = captured.filter(|_| call_count == 1) else {
            return Err(format!("expected one provider call, got {call_count}").into());
        };
        assert!(captured.system_prompt.is_none());
        let context_content = context_message_content(&captured.messages)?;
        assert!(context_content.contains("Semantic Context Bundle"));
        assert!(context_content.contains("src/providers/openai.rs"));
        assert!(context_content.contains("tests/provider_streaming.rs"));
        assert!(!context_content.contains("sk-secret"));
        assert!(!context_content.contains("hidden token"));

        let session_path = {
            let cx = pi::agent_cx::AgentCx::for_request();
            let session = session
                .lock(cx.cx())
                .await
                .map_err(|error| format!("session lock failed: {error}"))?;
            session
                .path
                .clone()
                .ok_or("session path missing after persisted agent run")?
        };
        let session_jsonl = fs::read_to_string(&session_path)?;
        assert!(session_jsonl.contains("semantic_context_bundle"));
        assert!(session_jsonl.contains("context-e2e-session"));
        assert!(!session_jsonl.contains("sk-secret"));
        assert!(!session_jsonl.contains("hidden token"));

        let log = write_context_e2e_jsonl_log(
            temp.path(),
            &[
                json!({
                    "event": "graph_built",
                    "git_workspace": temp.path().join(".git").is_dir(),
                    "nodes": graph.nodes.len(),
                    "edges": graph.edges.len(),
                    "trace_events": graph.trace.len()
                }),
                json!({
                    "event": "planner_decision",
                    "selected": bundle
                        .selected_items
                        .iter()
                        .map(|item| &item.source_path)
                        .collect::<Vec<_>>(),
                    "excluded": bundle
                        .excluded_items
                        .iter()
                        .map(|item| json!({
                            "path": &item.source_path,
                            "reason": &item.reason
                        }))
                        .collect::<Vec<_>>(),
                    "stale_suppressions": bundle
                        .stale_evidence_suppressions
                        .iter()
                        .map(|item| &item.source_path)
                        .collect::<Vec<_>>(),
                    "redaction": &bundle.redaction_summary,
                    "validation": &bundle.suggested_validation_commands,
                    "budget": {
                        "max_items": bundle.budget.max_items,
                        "max_bytes": bundle.budget.max_bytes,
                        "estimated_bytes": bundle.estimated_bytes
                    }
                }),
                json!({
                    "event": "prompt_assembled",
                    "provider_calls": 1,
                    "custom_context": true,
                    "session_path": session_path.strip_prefix(temp.path())
                        .unwrap_or(session_path.as_path())
                        .display()
                        .to_string()
                }),
                json!({
                    "event": "deterministic_replay",
                    "matched": true,
                    "cacheable": bundle.invalidation_policy.cacheable
                }),
            ],
        )?;
        assert_eq!(
            log.lines()
                .map(|line| {
                    let value: serde_json::Value =
                        serde_json::from_str(line).expect("valid JSONL record");
                    value["event"].as_str().expect("event string").to_string()
                })
                .collect::<Vec<_>>(),
            vec![
                "graph_built".to_string(),
                "planner_decision".to_string(),
                "prompt_assembled".to_string(),
                "deterministic_replay".to_string()
            ]
        );
        assert!(!log.contains("sk-secret"));
        assert!(!log.contains("hidden token"));

        Ok::<(), Box<dyn Error>>(())
    })?;

    Ok(())
}

#[test]
fn content_hashes_invalidate_without_changing_path_stable_ids() -> TestResult {
    let temp = fixture_workspace()?;
    let before = build_fixture_graph(temp.path())?;
    let before_fingerprint = before
        .input_fingerprints
        .iter()
        .find(|fingerprint| fingerprint.source_path == "src/lib.rs")
        .ok_or("missing src/lib.rs fingerprint before edit")?;
    let before_file_node = node_with_source(&before, SemanticNodeType::FileRegion, "src/lib.rs")?;

    write_fixture(
        temp.path(),
        "src/lib.rs",
        r"
pub mod providers;

pub struct Widget;

pub fn build_widget() -> Widget {
    Widget
}

pub fn build_second_widget() -> Widget {
    Widget
}
",
    )?;

    let after = build_fixture_graph(temp.path())?;
    let after_fingerprint = after
        .input_fingerprints
        .iter()
        .find(|fingerprint| fingerprint.source_path == "src/lib.rs")
        .ok_or("missing src/lib.rs fingerprint after edit")?;
    let after_file_node = node_with_source(&after, SemanticNodeType::FileRegion, "src/lib.rs")?;

    assert_ne!(before_fingerprint.sha256, after_fingerprint.sha256);
    assert_eq!(before_file_node.id, after_file_node.id);
    assert!(after.nodes.iter().any(|node| {
        node.node_type == SemanticNodeType::CodeSymbol && node.title == "build_second_widget"
    }));

    Ok(())
}

#[test]
fn malformed_fixture_classifications_do_not_emit_raw_secret_words() -> TestResult {
    let temp = tempfile::tempdir()?;
    write_fixture(
        temp.path(),
        "docs/evidence/bad.json",
        "{ token: secret authorization",
    )?;

    let graph = SemanticWorkspaceGraphBuilder::new(temp.path()).build()?;
    let encoded = serde_json::to_value(&graph)?;
    let text = serde_json::to_string(&encoded)?;

    assert!(!text.contains("authorization"));
    assert!(!text.contains("token"));
    assert!(!text.contains("secret"));

    let bad = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/bad.json",
    )?;
    assert_eq!(
        bad.freshness_status,
        Some(EvidenceFreshnessStatus::Malformed)
    );

    Ok(())
}
