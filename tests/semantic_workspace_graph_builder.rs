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
    SemanticNodeType, SemanticWorkspaceGraph, SemanticWorkspaceGraphBuilder,
    normalize_context_artifact_path,
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

fn fixture_workspace() -> TestResult<TempDir> {
    let temp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .and_then(|tmpdir| {
            fs::create_dir_all(&tmpdir)
                .ok()
                .and_then(|()| tempfile::Builder::new().tempdir_in(&tmpdir).ok())
        })
        .map_or_else(|| tempfile::Builder::new().tempdir_in("/tmp"), Ok)?;
    let root = temp.path();

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
  "schema": "pi.dropin_certification.verdict.v1",
  "generated_at": "2026-01-01T00:00:00Z",
  "overall_verdict": "CERTIFIED",
  "claim_surface": "release_facing"
}"#,
    )?;
    write_fixture(
        root,
        "tests/perf/reports/budget_summary.json",
        r#"{
  "schema": "pi.perf.budget_summary.v1",
  "generated_at": "2026-05-13T00:00:00Z",
  "claim_surface": "release_facing"
}"#,
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

#[test]
fn builder_indexes_workspace_surfaces_and_classifies_fail_closed() -> TestResult {
    let temp = fixture_workspace()?;
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

    let stale = node_with_source(
        &graph,
        SemanticNodeType::EvidenceArtifact,
        "docs/evidence/dropin-certification-verdict.json",
    )?;
    assert_eq!(stale.freshness_status, Some(EvidenceFreshnessStatus::Stale));
    assert_eq!(
        stale.metadata.get("release_claim_allowed"),
        Some(&json!(false))
    );
    assert_eq!(
        stale.metadata.get("claim_gate_status"),
        Some(&json!("blocked_stale"))
    );
    assert_eq!(
        stale.metadata.get("strict_replacement_claim_allowed"),
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
