//! Deterministic semantic workspace graph builder.
//!
//! The graph is advisory context only. It indexes workspace facts with
//! freshness and actionability metadata, but it never replaces Beads, Agent
//! Mail, README evidence gates, or validation commands as sources of truth.

#![allow(clippy::missing_const_for_fn, clippy::too_many_lines)]

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

pub const SEMANTIC_WORKSPACE_GRAPH_SCHEMA: &str = "pi.semantic_workspace_graph.v1";
pub const GRAPH_BUILDER_SCHEMA: &str = "pi.semantic_workspace_graph.builder_trace.v1";
pub const SEMANTIC_CONTEXT_BUNDLE_SCHEMA: &str = "pi.semantic_context_bundle.v1";

const DEFAULT_STALE_AFTER_DAYS: i64 = 90;
const DEFAULT_CACHE_TTL_SECONDS: u64 = 6 * 60 * 60;
const DEFAULT_CONTEXT_CACHE_TTL_SECONDS: u64 = 15 * 60;
const CONTEXT_PRIVACY_POLICY_VERSION: &str = "pi.context_privacy.v1";

#[derive(Debug, Clone)]
pub struct SemanticWorkspaceGraphBuilder {
    root: PathBuf,
    options: SemanticWorkspaceGraphBuildOptions,
}

#[derive(Debug, Clone)]
pub struct SemanticWorkspaceGraphBuildOptions {
    pub root_inputs: Vec<PathBuf>,
    pub reference_time_utc: Option<DateTime<Utc>>,
    pub stale_after_days: i64,
    pub cache_scope: ContextArtifactCacheScope,
    pub cache_ttl_seconds: u64,
}

impl Default for SemanticWorkspaceGraphBuildOptions {
    fn default() -> Self {
        Self {
            root_inputs: vec![
                PathBuf::from("src"),
                PathBuf::from("tests"),
                PathBuf::from("README.md"),
                PathBuf::from("docs"),
                PathBuf::from(".beads/issues.jsonl"),
            ],
            reference_time_utc: None,
            stale_after_days: DEFAULT_STALE_AFTER_DAYS,
            cache_scope: ContextArtifactCacheScope::default(),
            cache_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
        }
    }
}

impl SemanticWorkspaceGraphBuilder {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            options: SemanticWorkspaceGraphBuildOptions::default(),
        }
    }

    pub fn with_options(
        root: impl Into<PathBuf>,
        options: SemanticWorkspaceGraphBuildOptions,
    ) -> Self {
        Self {
            root: root.into(),
            options,
        }
    }

    #[must_use]
    pub fn add_expected_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.options.root_inputs.push(path.into());
        self
    }

    #[must_use]
    pub fn with_reference_time(mut self, reference_time_utc: DateTime<Utc>) -> Self {
        self.options.reference_time_utc = Some(reference_time_utc);
        self
    }

    #[must_use]
    pub fn with_cache_scope(mut self, cache_scope: ContextArtifactCacheScope) -> Self {
        self.options.cache_scope = cache_scope;
        self
    }

    #[must_use]
    pub fn with_cache_ttl_seconds(mut self, cache_ttl_seconds: u64) -> Self {
        self.options.cache_ttl_seconds = cache_ttl_seconds;
        self
    }

    pub fn build(&self) -> Result<SemanticWorkspaceGraph, SemanticGraphBuildError> {
        let metadata =
            fs::metadata(&self.root).map_err(|source| SemanticGraphBuildError::RootUnreadable {
                root: self.root.display().to_string(),
                source,
            })?;
        if !metadata.is_dir() {
            return Err(SemanticGraphBuildError::RootNotDirectory {
                root: self.root.display().to_string(),
            });
        }

        let mut state = GraphBuildState::default();
        for input in self.discover_inputs(&mut state) {
            self.ingest_file(&input, &mut state);
        }
        state.resolve_pending_links();
        state.sort();

        Ok(SemanticWorkspaceGraph {
            schema: SEMANTIC_WORKSPACE_GRAPH_SCHEMA.to_string(),
            builder_schema: GRAPH_BUILDER_SCHEMA.to_string(),
            root: normalize_path(&self.root),
            cache_scope: self.options.cache_scope.clone(),
            cache_ttl_seconds: self.options.cache_ttl_seconds,
            nodes: state.nodes,
            edges: state.edges,
            input_fingerprints: state.input_fingerprints,
            trace: state.trace,
        })
    }

    fn discover_inputs(&self, state: &mut GraphBuildState) -> Vec<DiscoveredInput> {
        let mut seen = BTreeSet::new();
        let mut inputs = Vec::new();
        for configured in &self.options.root_inputs {
            let absolute = self.root.join(configured);
            if !absolute.exists() {
                let source_path = normalize_relative_path(&self.root, &absolute);
                Self::record_missing_input(state, &source_path);
                continue;
            }
            self.collect_path(&absolute, &mut seen, &mut inputs, state);
        }
        inputs.sort_by(|left, right| left.source_path.cmp(&right.source_path));
        inputs
    }

    fn collect_path(
        &self,
        absolute: &Path,
        seen: &mut BTreeSet<String>,
        inputs: &mut Vec<DiscoveredInput>,
        state: &mut GraphBuildState,
    ) {
        let source_path = normalize_relative_path(&self.root, absolute);
        if absolute.is_dir() {
            let Ok(entries) = fs::read_dir(absolute) else {
                state.push_trace(GraphBuildTraceEvent::new(
                    SourceSurface::Unknown.as_str(),
                    source_path,
                    GraphInputStatus::Unreadable,
                    "directory_read_failed",
                    0,
                    0,
                ));
                return;
            };

            let mut child_paths = Vec::new();
            for entry in entries.flatten() {
                child_paths.push(entry.path());
            }
            child_paths.sort_by_key(|left| normalize_path(left));
            for child in child_paths {
                if should_skip_dir(&child) {
                    continue;
                }
                self.collect_path(&child, seen, inputs, state);
            }
            return;
        }

        let Some(surface) = surface_for_path(&source_path) else {
            return;
        };
        if seen.insert(source_path.clone()) {
            inputs.push(DiscoveredInput {
                absolute_path: absolute.to_path_buf(),
                source_path,
                surface,
            });
        }
    }

    fn ingest_file(&self, input: &DiscoveredInput, state: &mut GraphBuildState) {
        let start_nodes = state.nodes.len();
        let start_edges = state.edges.len();
        let Ok(bytes) = fs::read(&input.absolute_path) else {
            state.push_trace(GraphBuildTraceEvent::new(
                input.surface.as_str(),
                input.source_path.clone(),
                GraphInputStatus::Unreadable,
                "file_read_failed",
                0,
                0,
            ));
            if input.surface == SourceSurface::EvidenceArtifacts {
                let node = missing_or_unreadable_evidence_node(
                    &input.source_path,
                    EvidenceFreshnessStatus::Missing,
                    "file_read_failed",
                );
                state.register_evidence_node(&input.source_path, &node.id);
                state.push_node(node);
            }
            return;
        };

        let content_sha256 = sha256_hex(&bytes);
        let size_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let mtime_unix_ns = file_mtime_unix_ns(&input.absolute_path).unwrap_or(None);
        let normalized = normalize_context_artifact_path(&input.source_path);
        let normalized_source_path = normalized
            .normalized_path
            .clone()
            .unwrap_or_else(|| normalize_relative_path(&self.root, &input.absolute_path));
        let cache_valid_until_unix_ns = self.cache_valid_until_unix_ns();
        let cache_status = if normalized.accepted {
            ContextArtifactCacheStatus::Valid
        } else {
            ContextArtifactCacheStatus::UnsafePath
        };
        state.input_fingerprints.push(InputFingerprint {
            source_path: input.source_path.clone(),
            normalized_source_path: normalized_source_path.clone(),
            surface_id: input.surface.as_str().to_string(),
            sha256: content_sha256.clone(),
            cache_key_sha256: cache_key_sha256(
                &self.options.cache_scope,
                &normalized_source_path,
                &content_sha256,
            ),
            size_bytes,
            mtime_unix_ns,
            cache_scope: self.options.cache_scope.clone(),
            cache_valid_until_unix_ns,
            cache_status,
        });

        let content = String::from_utf8_lossy(&bytes);
        match input.surface {
            SourceSurface::RustCodeModules | SourceSurface::IntegrationAndContractTests => {
                Self::ingest_rust_file(input, &content, &content_sha256, size_bytes, state);
            }
            SourceSurface::ReadmeAndDocs => {
                Self::ingest_markdown_file(input, &content, &content_sha256, size_bytes, state);
            }
            SourceSurface::EvidenceArtifacts => {
                self.ingest_evidence_file(input, &content, &content_sha256, size_bytes, state);
            }
            SourceSurface::BeadsIssueGraph => {
                self.ingest_beads_jsonl(input, &content, &content_sha256, size_bytes, state);
            }
            SourceSurface::RuntimeArtifacts => {
                Self::ingest_runtime_artifact(input, &content, &content_sha256, size_bytes, state);
            }
            SourceSurface::Unknown => {}
        }

        state.push_trace(GraphBuildTraceEvent::new(
            input.surface.as_str(),
            input.source_path.clone(),
            GraphInputStatus::Indexed,
            "indexed",
            state.nodes.len().saturating_sub(start_nodes),
            state.edges.len().saturating_sub(start_edges),
        ));
    }

    fn cache_valid_until_unix_ns(&self) -> Option<u64> {
        let reference_time = self.options.reference_time_utc?;
        datetime_unix_ns(reference_time)?
            .checked_add(self.options.cache_ttl_seconds.checked_mul(1_000_000_000)?)
    }

    fn ingest_rust_file(
        input: &DiscoveredInput,
        content: &str,
        content_sha256: &str,
        size_bytes: u64,
        state: &mut GraphBuildState,
    ) {
        let line_count = count_lines(content);
        let redaction = assess_redaction(&input.source_path, content, None);
        let mut file_node = file_region_node(
            &input.source_path,
            content_sha256,
            size_bytes,
            1,
            line_count,
            input.surface.as_str(),
        );
        apply_redaction_metadata(&mut file_node, &redaction);
        let file_node_id = file_node.id.clone();
        state.push_node(file_node);

        if is_provider_surface(&input.source_path) {
            let provider_node = provider_surface_node(&input.source_path, content_sha256);
            state.push_edge(edge(
                SemanticEdgeType::Contains,
                &file_node_id,
                &provider_node.id,
                "provider_module_surface",
            ));
            state.push_node(provider_node);
        }

        let mut pending_test_attribute = false;
        for (idx, line) in content.lines().enumerate() {
            let line_number = idx.saturating_add(1);
            let trimmed = line.trim_start();
            if is_test_attribute(trimmed) {
                pending_test_attribute = true;
                continue;
            }

            if let Some(symbol) = parse_rust_symbol(trimmed) {
                if input.surface == SourceSurface::IntegrationAndContractTests
                    && pending_test_attribute
                    && symbol.kind == "fn"
                {
                    let test_node = test_case_node(
                        &input.source_path,
                        &symbol.name,
                        line_number,
                        content_sha256,
                    );
                    let command_node = validation_command_node(&input.source_path, &symbol.name);
                    state.push_edge(edge(
                        SemanticEdgeType::Exercises,
                        &file_node_id,
                        &test_node.id,
                        "rust_test_case",
                    ));
                    state.push_edge(edge(
                        SemanticEdgeType::SuggestsValidation,
                        &test_node.id,
                        &command_node.id,
                        "focused_test_command",
                    ));
                    state.push_node(test_node);
                    state.push_node(command_node);
                }

                let symbol_node = code_symbol_node(
                    &input.source_path,
                    &symbol.kind,
                    &symbol.name,
                    line_number,
                    content_sha256,
                );
                state.push_edge(edge(
                    SemanticEdgeType::Defines,
                    &file_node_id,
                    &symbol_node.id,
                    "rust_symbol",
                ));
                state.push_node(symbol_node);
                pending_test_attribute = false;
            } else if !trimmed.starts_with("#[") && !trimmed.is_empty() {
                pending_test_attribute = false;
            }
        }
    }

    fn ingest_markdown_file(
        input: &DiscoveredInput,
        content: &str,
        content_sha256: &str,
        size_bytes: u64,
        state: &mut GraphBuildState,
    ) {
        let line_count = count_lines(content);
        let redaction = assess_redaction(&input.source_path, content, None);
        let mut file_node = file_region_node(
            &input.source_path,
            content_sha256,
            size_bytes,
            1,
            line_count,
            input.surface.as_str(),
        );
        apply_redaction_metadata(&mut file_node, &redaction);
        let file_node_id = file_node.id.clone();
        state.push_node(file_node);

        for (idx, line) in content.lines().enumerate() {
            let line_number = idx.saturating_add(1);
            if let Some((level, title)) = parse_markdown_heading(line) {
                let section_node = doc_section_node(
                    &input.source_path,
                    level,
                    &title,
                    line_number,
                    content_sha256,
                );
                state.push_edge(edge(
                    SemanticEdgeType::Contains,
                    &file_node_id,
                    &section_node.id,
                    "markdown_heading",
                ));
                state.push_node(section_node);
            }

            for target_path in extract_evidence_citations(line) {
                let claim_surface = claim_surface_for_markdown_line(line);
                let citation_node = doc_citation_node(
                    &input.source_path,
                    &target_path,
                    line_number,
                    content_sha256,
                    claim_surface,
                );
                state.push_edge(edge(
                    SemanticEdgeType::Contains,
                    &file_node_id,
                    &citation_node.id,
                    "markdown_evidence_citation",
                ));
                state.push_pending_citation(PendingEvidenceCitation {
                    source_node_id: citation_node.id.clone(),
                    source_path: input.source_path.clone(),
                    target_path,
                    line_number,
                    claim_surface,
                });
                state.push_node(citation_node);
            }
        }
    }

    fn ingest_evidence_file(
        &self,
        input: &DiscoveredInput,
        content: &str,
        content_sha256: &str,
        size_bytes: u64,
        state: &mut GraphBuildState,
    ) {
        let line_count = count_lines(content);
        let raw_redaction = assess_redaction(&input.source_path, content, None);
        let mut file_node = file_region_node(
            &input.source_path,
            content_sha256,
            size_bytes,
            1,
            line_count,
            input.surface.as_str(),
        );
        apply_redaction_metadata(&mut file_node, &raw_redaction);
        let file_node_id = file_node.id.clone();
        state.push_node(file_node);

        match serde_json::from_str::<Value>(content) {
            Ok(value) => {
                let redaction = assess_redaction(&input.source_path, content, Some(&value));
                let mut evidence_node = evidence_artifact_node(
                    &input.source_path,
                    &value,
                    content_sha256,
                    &self.options,
                    &self.root,
                );
                apply_redaction_metadata(&mut evidence_node, &redaction);
                state.push_edge(edge(
                    SemanticEdgeType::Tracks,
                    &file_node_id,
                    &evidence_node.id,
                    "json_evidence_artifact",
                ));
                state.register_evidence_node(&input.source_path, &evidence_node.id);
                state.push_node(evidence_node);
            }
            Err(error) => {
                let mut node = missing_or_unreadable_evidence_node(
                    &input.source_path,
                    EvidenceFreshnessStatus::Malformed,
                    "json_parse_failed",
                );
                let privacy = classify_text_privacy(&input.source_path, content);
                node.redaction_status = node.redaction_status.max(privacy.status);
                apply_privacy_metadata(&mut node.metadata, &privacy);
                node.content_sha256 = Some(content_sha256.to_string());
                node.metadata.insert(
                    "parse_error".to_string(),
                    json!(redact_error_message(&error.to_string())),
                );
                apply_redaction_metadata(&mut node, &raw_redaction);
                state.push_edge(edge(
                    SemanticEdgeType::Tracks,
                    &file_node_id,
                    &node.id,
                    "malformed_json_evidence",
                ));
                state.register_evidence_node(&input.source_path, &node.id);
                state.push_node(node);
                state.push_trace(GraphBuildTraceEvent::new(
                    input.surface.as_str(),
                    input.source_path.clone(),
                    GraphInputStatus::Malformed,
                    "json_parse_failed",
                    1,
                    1,
                ));
            }
        }
    }

    fn ingest_beads_jsonl(
        &self,
        input: &DiscoveredInput,
        content: &str,
        content_sha256: &str,
        size_bytes: u64,
        state: &mut GraphBuildState,
    ) {
        let line_count = count_lines(content);
        let redaction = assess_redaction(&input.source_path, content, None);
        let mut file_node = file_region_node(
            &input.source_path,
            content_sha256,
            size_bytes,
            1,
            line_count,
            input.surface.as_str(),
        );
        apply_redaction_metadata(&mut file_node, &redaction);
        let file_node_id = file_node.id.clone();
        state.push_node(file_node);

        for (idx, line) in content.lines().enumerate() {
            let line_number = idx.saturating_add(1);
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<Value>(line) {
                Ok(value) => {
                    let classified =
                        classify_bead_actionability(&value, self.options.reference_time_utc);
                    let bead_id = value
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("missing-bead-id");
                    let node = bead_node(
                        &input.source_path,
                        line_number,
                        bead_id,
                        &value,
                        &classified,
                    );
                    state.push_edge(edge(
                        SemanticEdgeType::Tracks,
                        &file_node_id,
                        &node.id,
                        "beads_jsonl_record",
                    ));
                    add_bead_dependency_edges(&node.id, &value, state);
                    if let Some(external_ref) = bead_external_ref(&value) {
                        state.push_pending_external_ref(PendingBeadExternalRef {
                            source_node_id: node.id.clone(),
                            bead_id: bead_id.to_string(),
                            external_ref: external_ref.to_string(),
                        });
                    }
                    state.push_node(node);
                }
                Err(error) => {
                    let classified = ClassifiedBeadActionability {
                        status: BeadActionabilityStatus::UnknownFailClosed,
                        planner_may_claim: false,
                        reason: "malformed_jsonl".to_string(),
                    };
                    let mut node = bead_node(
                        &input.source_path,
                        line_number,
                        &format!("malformed-line-{line_number}"),
                        &json!({ "id": format!("malformed-line-{line_number}") }),
                        &classified,
                    );
                    node.metadata.insert(
                        "parse_error".to_string(),
                        json!(redact_error_message(&error.to_string())),
                    );
                    state.push_edge(edge(
                        SemanticEdgeType::Tracks,
                        &file_node_id,
                        &node.id,
                        "malformed_beads_jsonl_record",
                    ));
                    state.push_node(node);
                    state.push_trace(GraphBuildTraceEvent::new(
                        input.surface.as_str(),
                        input.source_path.clone(),
                        GraphInputStatus::Malformed,
                        "beads_jsonl_parse_failed",
                        1,
                        1,
                    ));
                }
            }
        }
    }

    fn ingest_runtime_artifact(
        input: &DiscoveredInput,
        content: &str,
        content_sha256: &str,
        size_bytes: u64,
        state: &mut GraphBuildState,
    ) {
        let line_count = count_lines(content);
        let redaction = assess_redaction(&input.source_path, content, None);
        let mut file_node = file_region_node(
            &input.source_path,
            content_sha256,
            size_bytes,
            1,
            line_count,
            input.surface.as_str(),
        );
        apply_redaction_metadata(&mut file_node, &redaction);
        state.push_node(file_node);
    }

    fn record_missing_input(state: &mut GraphBuildState, source_path: &str) {
        let surface = surface_for_path(source_path).unwrap_or(SourceSurface::Unknown);
        state.push_trace(GraphBuildTraceEvent::new(
            surface.as_str(),
            source_path.to_string(),
            GraphInputStatus::Missing,
            "expected_input_missing",
            0,
            0,
        ));
        if surface == SourceSurface::EvidenceArtifacts {
            let node = missing_or_unreadable_evidence_node(
                source_path,
                EvidenceFreshnessStatus::Missing,
                "expected_input_missing",
            );
            state.register_evidence_node(source_path, &node.id);
            state.push_node(node);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticWorkspaceGraph {
    pub schema: String,
    pub builder_schema: String,
    pub root: String,
    pub cache_scope: ContextArtifactCacheScope,
    pub cache_ttl_seconds: u64,
    pub nodes: Vec<SemanticGraphNode>,
    pub edges: Vec<SemanticGraphEdge>,
    pub input_fingerprints: Vec<InputFingerprint>,
    pub trace: Vec<GraphBuildTraceEvent>,
}

impl SemanticWorkspaceGraph {
    pub fn nodes_by_type(&self, node_type: SemanticNodeType) -> Vec<&SemanticGraphNode> {
        self.nodes
            .iter()
            .filter(|node| node.node_type == node_type)
            .collect()
    }

    pub fn evidence_node_for_path(&self, source_path: &str) -> Option<&SemanticGraphNode> {
        self.nodes.iter().find(|node| {
            node.node_type == SemanticNodeType::EvidenceArtifact && node.source_path == source_path
        })
    }

    pub fn evidence_status_for_path(&self, source_path: &str) -> Option<EvidenceFreshnessStatus> {
        self.evidence_node_for_path(source_path)
            .and_then(|node| node.freshness_status)
    }

    pub fn release_claim_allowed_for_path(&self, source_path: &str) -> Option<bool> {
        self.evidence_node_for_path(source_path).and_then(|node| {
            node.metadata
                .get("release_claim_allowed")
                .and_then(Value::as_bool)
        })
    }

    pub fn suppressible_claim_evidence(&self) -> Vec<&SemanticGraphNode> {
        self.nodes
            .iter()
            .filter(|node| {
                node.node_type == SemanticNodeType::EvidenceArtifact
                    && node
                        .metadata
                        .get("suppresses_release_claim_context")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            })
            .collect()
    }

    pub fn cache_validation_for_path(
        &self,
        source_path: &str,
        requested_scope: &ContextArtifactCacheScope,
        now_unix_ns: u64,
    ) -> ContextArtifactCacheStatus {
        let normalized = normalize_context_artifact_path(source_path);
        if !normalized.accepted {
            return ContextArtifactCacheStatus::UnsafePath;
        }
        let Some(normalized_source_path) = normalized.normalized_path else {
            return ContextArtifactCacheStatus::UnsafePath;
        };
        let Some(fingerprint) = self
            .input_fingerprints
            .iter()
            .find(|fingerprint| fingerprint.normalized_source_path == normalized_source_path)
        else {
            return ContextArtifactCacheStatus::MissingFingerprint;
        };
        fingerprint.cache_validation(requested_scope, now_unix_ns)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextArtifactCacheScope {
    pub workspace_identity: String,
    pub branch_identity: String,
    pub session_scope: String,
}

impl Default for ContextArtifactCacheScope {
    fn default() -> Self {
        Self {
            workspace_identity: "workspace-unspecified".to_string(),
            branch_identity: "branch-unspecified".to_string(),
            session_scope: "session-unspecified".to_string(),
        }
    }
}

impl ContextArtifactCacheScope {
    #[must_use]
    pub fn new(
        workspace_identity: impl Into<String>,
        branch_identity: impl Into<String>,
        session_scope: impl Into<String>,
    ) -> Self {
        Self {
            workspace_identity: workspace_identity.into(),
            branch_identity: branch_identity.into(),
            session_scope: session_scope.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundleBudget {
    pub max_items: usize,
    pub max_bytes: u64,
}

impl Default for ContextBundleBudget {
    fn default() -> Self {
        Self {
            max_items: 24,
            max_bytes: 32 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundleRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
    pub changed_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at_utc: Option<String>,
    #[serde(default = "default_context_cache_ttl_seconds")]
    pub cache_ttl_seconds: u64,
    pub budget: ContextBundleBudget,
}

impl Default for ContextBundleRequest {
    fn default() -> Self {
        Self {
            query: None,
            bead_id: None,
            changed_paths: Vec::new(),
            failing_command: None,
            workspace_id: None,
            branch: None,
            session_id: None,
            generated_at_utc: None,
            cache_ttl_seconds: DEFAULT_CONTEXT_CACHE_TTL_SECONDS,
            budget: ContextBundleBudget::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticContextBundle {
    pub schema: String,
    pub budget: ContextBundleBudget,
    pub selected_items: Vec<ContextBundleItem>,
    pub excluded_items: Vec<ContextBundleExclusion>,
    pub stale_evidence_suppressions: Vec<ContextBundleExclusion>,
    pub redaction_summary: ContextRedactionSummary,
    pub invalidation_policy: ContextBundleInvalidationPolicy,
    pub path_normalization: Vec<ContextPathNormalization>,
    pub suggested_validation_commands: Vec<String>,
    pub estimated_bytes: u64,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundleItem {
    pub node_id: String,
    pub node_type: SemanticNodeType,
    pub source_path: String,
    pub title: String,
    pub reason: String,
    pub score: i64,
    pub estimated_bytes: u64,
    pub estimated_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_status: Option<EvidenceFreshnessStatus>,
    pub redaction_status: RedactionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundleExclusion {
    pub node_id: String,
    pub node_type: SemanticNodeType,
    pub source_path: String,
    pub title: String,
    pub reason: String,
    pub score: i64,
    pub estimated_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_status: Option<EvidenceFreshnessStatus>,
    pub redaction_status: RedactionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRedactionSummary {
    pub policy_version: String,
    pub overall_status: RedactionStatus,
    pub selected_redacted_nodes: usize,
    pub selected_sensitive_omissions: usize,
    pub suppressed_unsafe_nodes: usize,
    pub redacted_metadata_keys: BTreeSet<String>,
    pub sensitive_path_kinds: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundleInvalidationPolicy {
    pub policy_version: String,
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub input_fingerprint_sha256: String,
    pub cache_ttl_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at_utc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_utc: Option<String>,
    pub invalidates_on: Vec<String>,
    pub cacheable: bool,
}

impl ContextBundleInvalidationPolicy {
    #[must_use]
    pub fn validate_probe(&self, probe: &ContextBundleCacheProbe) -> ContextBundleCacheValidation {
        let mut invalidation_reasons = Vec::new();
        if !self.cacheable {
            invalidation_reasons.push("cache_not_cacheable".to_string());
        }
        if self.workspace_id != probe.workspace_id {
            invalidation_reasons.push("workspace_id_changed".to_string());
        }
        if self.branch != probe.branch {
            invalidation_reasons.push("branch_changed".to_string());
        }
        if optional_cache_scope_value_changed(self.session_id.as_ref(), probe.session_id.as_ref()) {
            invalidation_reasons.push("session_id_changed".to_string());
        }
        if cache_text_values_changed(
            &self.input_fingerprint_sha256,
            &probe.input_fingerprint_sha256,
        ) {
            invalidation_reasons.push("input_fingerprint_changed".to_string());
        }
        match (&self.expires_at_utc, &probe.now_utc) {
            (Some(expires_at), Some(now)) => {
                match (
                    DateTime::parse_from_rfc3339(expires_at),
                    DateTime::parse_from_rfc3339(now),
                ) {
                    (Ok(expires_at), Ok(now)) if now > expires_at => {
                        invalidation_reasons.push("cache_ttl_expired".to_string());
                    }
                    (Ok(_), Ok(_)) => {}
                    _ => invalidation_reasons.push("invalid_cache_timestamp".to_string()),
                }
            }
            _ => invalidation_reasons.push("missing_cache_timestamp".to_string()),
        }

        ContextBundleCacheValidation {
            valid: invalidation_reasons.is_empty(),
            invalidation_reasons,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundleCacheProbe {
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub input_fingerprint_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_utc: Option<String>,
}

fn optional_cache_scope_value_changed(left: Option<&String>, right: Option<&String>) -> bool {
    match (left.map(String::as_str), right.map(String::as_str)) {
        (Some(left), Some(right)) => cache_text_values_changed(left, right),
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
    }
}

fn cache_text_values_changed(left: &str, right: &str) -> bool {
    !left.as_bytes().iter().eq(right.as_bytes().iter())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundleCacheValidation {
    pub valid: bool,
    pub invalidation_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPathNormalization {
    pub raw_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_path: Option<String>,
    pub accepted: bool,
    pub reason: String,
}

pub struct SemanticContextBundlePlanner<'a> {
    graph: &'a SemanticWorkspaceGraph,
}

impl<'a> SemanticContextBundlePlanner<'a> {
    #[must_use]
    pub fn new(graph: &'a SemanticWorkspaceGraph) -> Self {
        Self { graph }
    }

    #[must_use]
    pub fn plan(&self, request: &ContextBundleRequest) -> SemanticContextBundle {
        let query_terms = tokenize_context_query(request.query.as_deref());
        let path_normalization = normalize_context_paths(&request.changed_paths);
        let related_ids = self.related_node_ids(request, &path_normalization);
        let mut candidates = self.scored_candidates(request, &query_terms, &related_ids);
        candidates.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.node.source_path.cmp(&right.node.source_path))
                .then_with(|| left.node.id.cmp(&right.node.id))
        });

        let mut selected_items = Vec::new();
        let mut excluded_items = Vec::new();
        let mut stale_evidence_suppressions = Vec::new();
        let mut stale_evidence_suppression_keys = BTreeSet::new();
        let mut suggested_validation_commands = BTreeSet::new();
        let mut estimated_bytes = 0_u64;

        for candidate in candidates {
            if let Some(suppression_reason) = candidate.suppression_reason {
                let exclusion_reason = if suppression_reason == "unsafe_to_emit_by_redaction_policy"
                {
                    candidate.reason.as_str()
                } else {
                    suppression_reason
                };
                let exclusion = candidate.to_exclusion(exclusion_reason);
                if suppression_reason == "suppressed_stale_or_unsafe_evidence"
                    && stale_evidence_suppression_keys
                        .insert((exclusion.source_path.clone(), exclusion.reason.clone()))
                {
                    stale_evidence_suppressions.push(exclusion.clone());
                }
                excluded_items.push(exclusion);
                continue;
            }

            if selected_items.len() >= request.budget.max_items
                || estimated_bytes.saturating_add(candidate.estimated_bytes)
                    > request.budget.max_bytes
            {
                excluded_items.push(candidate.to_exclusion("budget_exceeded"));
                continue;
            }

            estimated_bytes = estimated_bytes.saturating_add(candidate.estimated_bytes);
            if candidate.node.node_type == SemanticNodeType::ValidationCommand
                && let Some(command) = candidate
                    .node
                    .metadata
                    .get("command")
                    .and_then(Value::as_str)
            {
                suggested_validation_commands.insert(command.to_string());
            }
            selected_items.push(candidate.to_item());
        }

        for changed_path in path_normalization
            .iter()
            .filter_map(|path| path.normalized_path.as_deref())
        {
            for node in self.graph.nodes.iter().filter(|node| {
                node.source_path == changed_path
                    && node.redaction_status == RedactionStatus::UnsafeToEmit
            }) {
                let already_accounted_for =
                    selected_items.iter().any(|item| item.node_id == node.id)
                        || excluded_items.iter().any(|item| item.node_id == node.id);
                if !already_accounted_for {
                    excluded_items.push(unsafe_changed_path_exclusion(node));
                }
            }
        }

        SemanticContextBundle {
            schema: SEMANTIC_CONTEXT_BUNDLE_SCHEMA.to_string(),
            budget: request.budget.clone(),
            redaction_summary: build_redaction_summary(&selected_items, &excluded_items),
            invalidation_policy: self.invalidation_policy(request),
            path_normalization,
            selected_items,
            excluded_items,
            stale_evidence_suppressions,
            suggested_validation_commands: suggested_validation_commands.into_iter().collect(),
            estimated_bytes,
            estimated_tokens: estimate_tokens(estimated_bytes),
        }
    }

    fn related_node_ids(
        &self,
        request: &ContextBundleRequest,
        path_normalization: &[ContextPathNormalization],
    ) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        if let Some(bead_id) = request.bead_id.as_deref()
            && let Some(bead_node) = self.graph.nodes.iter().find(|node| {
                node.node_type == SemanticNodeType::Bead
                    && node.metadata.get("bead_id").and_then(Value::as_str) == Some(bead_id)
            })
        {
            ids.insert(bead_node.id.clone());
            for edge in &self.graph.edges {
                if edge.source == bead_node.id {
                    ids.insert(edge.target.clone());
                }
                if edge.target == bead_node.id {
                    ids.insert(edge.source.clone());
                }
            }
        }

        for changed_path in path_normalization
            .iter()
            .filter_map(|path| path.normalized_path.as_deref())
        {
            for node in &self.graph.nodes {
                if paths_are_related(&node.source_path, changed_path) {
                    ids.insert(node.id.clone());
                }
            }
        }

        self.expand_related_edges(ids)
    }

    fn expand_related_edges(&self, mut ids: BTreeSet<String>) -> BTreeSet<String> {
        let mut changed = true;
        while changed {
            changed = false;
            for edge in &self.graph.edges {
                if ids.contains(&edge.source) && ids.insert(edge.target.clone()) {
                    changed = true;
                }
                if ids.contains(&edge.target) && ids.insert(edge.source.clone()) {
                    changed = true;
                }
            }
        }
        ids
    }

    fn scored_candidates(
        &self,
        request: &ContextBundleRequest,
        query_terms: &[String],
        related_ids: &BTreeSet<String>,
    ) -> Vec<ScoredContextNode<'a>> {
        let failing_command = request
            .failing_command
            .as_deref()
            .map(str::to_ascii_lowercase);
        let suppressible_evidence_paths: BTreeSet<&str> = self
            .graph
            .suppressible_claim_evidence()
            .into_iter()
            .map(|node| node.source_path.as_str())
            .collect();
        self.graph
            .nodes
            .iter()
            .filter_map(|node| {
                if node.node_type == SemanticNodeType::CodeSymbol
                    && node.source_path.starts_with("tests/")
                {
                    return None;
                }

                let mut score = 0_i64;
                let mut reasons = Vec::new();

                if related_ids.contains(&node.id) {
                    score += 180;
                    reasons.push("related_to_bead_or_changed_path");
                }

                if !query_terms.is_empty() {
                    let matched_terms = matched_query_terms(node, query_terms);
                    if !matched_terms.is_empty() {
                        score += i64::try_from(matched_terms.len()).unwrap_or(i64::MAX) * 45;
                        reasons.push("query_match");
                    }
                }

                if let Some(failing_command) = failing_command.as_deref()
                    && validation_command_matches(node, failing_command)
                {
                    score += 220;
                    reasons.push("failing_command_match");
                }

                if score > 0 {
                    score += base_node_score(node);
                }

                if score > 0
                    && node.node_type == SemanticNodeType::EvidenceArtifact
                    && node
                        .metadata
                        .get("release_claim_allowed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    score += 30;
                    reasons.push("current_release_claim_evidence");
                }

                let must_suppress = node
                    .metadata
                    .get("suppresses_release_claim_context")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || suppressible_evidence_paths.contains(node.source_path.as_str());
                if must_suppress && score > 0 {
                    reasons.push("suppressed_by_claim_gate");
                }

                if score <= 0 {
                    None
                } else {
                    for reason in privacy_reason_fragments(node) {
                        reasons.push(reason);
                    }
                    let suppression_reason =
                        if node.redaction_status == RedactionStatus::UnsafeToEmit {
                            reasons.push("unsafe_to_emit_by_redaction_policy");
                            Some("unsafe_to_emit_by_redaction_policy")
                        } else if must_suppress {
                            Some("suppressed_stale_or_unsafe_evidence")
                        } else {
                            None
                        };

                    Some(ScoredContextNode {
                        node,
                        score,
                        estimated_bytes: estimate_node_bytes(node),
                        reason: reasons.join(","),
                        suppression_reason,
                    })
                }
            })
            .collect()
    }

    fn invalidation_policy(
        &self,
        request: &ContextBundleRequest,
    ) -> ContextBundleInvalidationPolicy {
        let workspace_id = request
            .workspace_id
            .clone()
            .unwrap_or_else(|| stable_id("workspace", &[&self.graph.root]));
        let input_fingerprint_sha256 = graph_input_fingerprint_digest(self.graph);
        let ttl_seconds = request
            .cache_ttl_seconds
            .max(1)
            .min(i64::MAX.try_into().unwrap_or(u64::MAX));
        let generated_at_utc = request.generated_at_utc.clone();
        let expires_at_utc = generated_at_utc
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .and_then(|generated_at| {
                let ttl = Duration::seconds(i64::try_from(ttl_seconds).ok()?);
                Some(
                    generated_at
                        .with_timezone(&Utc)
                        .checked_add_signed(ttl)?
                        .to_rfc3339(),
                )
            });
        let cacheable = request.generated_at_utc.is_some()
            && request.branch.is_some()
            && request.session_id.is_some();

        ContextBundleInvalidationPolicy {
            policy_version: CONTEXT_PRIVACY_POLICY_VERSION.to_string(),
            workspace_id,
            branch: request.branch.clone(),
            session_id: request.session_id.clone(),
            input_fingerprint_sha256,
            cache_ttl_seconds: ttl_seconds,
            generated_at_utc,
            expires_at_utc,
            invalidates_on: vec![
                "workspace_id_change".to_string(),
                "branch_change".to_string(),
                "session_id_change".to_string(),
                "input_fingerprint_change".to_string(),
                "cache_ttl_expiry".to_string(),
                "redaction_policy_version_change".to_string(),
            ],
            cacheable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFingerprint {
    pub source_path: String,
    pub normalized_source_path: String,
    pub surface_id: String,
    pub sha256: String,
    pub cache_key_sha256: String,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_unix_ns: Option<u64>,
    pub cache_scope: ContextArtifactCacheScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_valid_until_unix_ns: Option<u64>,
    pub cache_status: ContextArtifactCacheStatus,
}

impl InputFingerprint {
    #[must_use]
    pub fn cache_validation(
        &self,
        requested_scope: &ContextArtifactCacheScope,
        now_unix_ns: u64,
    ) -> ContextArtifactCacheStatus {
        if self.cache_status != ContextArtifactCacheStatus::Valid {
            return self.cache_status;
        }
        if self.cache_scope.workspace_identity != requested_scope.workspace_identity {
            return ContextArtifactCacheStatus::WorkspaceMismatch;
        }
        if self.cache_scope.branch_identity != requested_scope.branch_identity {
            return ContextArtifactCacheStatus::BranchMismatch;
        }
        if cache_text_values_changed(
            &self.cache_scope.session_scope,
            &requested_scope.session_scope,
        ) {
            return ContextArtifactCacheStatus::SessionMismatch;
        }
        let Some(cache_valid_until_unix_ns) = self.cache_valid_until_unix_ns else {
            return ContextArtifactCacheStatus::Expired;
        };
        if now_unix_ns > cache_valid_until_unix_ns {
            ContextArtifactCacheStatus::Expired
        } else {
            ContextArtifactCacheStatus::Valid
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticGraphNode {
    pub id: String,
    pub node_type: SemanticNodeType,
    pub source_path: String,
    pub title: String,
    pub stable_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_end: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_status: Option<EvidenceFreshnessStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bead_actionability_status: Option<BeadActionabilityStatus>,
    pub redaction_status: RedactionStatus,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticGraphEdge {
    pub id: String,
    pub edge_type: SemanticEdgeType,
    pub source: String,
    pub target: String,
    pub reason: String,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphBuildTraceEvent {
    pub schema: String,
    pub surface_id: String,
    pub source_path: String,
    pub status: GraphInputStatus,
    pub reason: String,
    pub node_count: usize,
    pub edge_count: usize,
}

impl GraphBuildTraceEvent {
    fn new(
        surface_id: &str,
        source_path: String,
        status: GraphInputStatus,
        reason: &str,
        node_count: usize,
        edge_count: usize,
    ) -> Self {
        Self {
            schema: GRAPH_BUILDER_SCHEMA.to_string(),
            surface_id: surface_id.to_string(),
            source_path,
            status,
            reason: reason.to_string(),
            node_count,
            edge_count,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticNodeType {
    CodeSymbol,
    FileRegion,
    TestCase,
    DocSection,
    EvidenceArtifact,
    Bead,
    ProviderSurface,
    ValidationCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticEdgeType {
    Contains,
    Defines,
    Exercises,
    Validates,
    CitesEvidence,
    Tracks,
    Blocks,
    DependsOn,
    SuggestsValidation,
    Supersedes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceFreshnessStatus {
    Current,
    HistoricalSnapshot,
    Stale,
    Missing,
    Malformed,
    Uncertified,
    FreshnessUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadActionabilityStatus {
    ActionableOpen,
    ClaimedInProgress,
    StalledReopenCandidate,
    Blocked,
    ClosedReferenceOnly,
    TombstoneReferenceOnly,
    UnknownFailClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphInputStatus {
    Indexed,
    Missing,
    Unreadable,
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionStatus {
    None,
    Redacted,
    SensitiveOmitted,
    UnsafeToEmit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextArtifactCacheStatus {
    Valid,
    Expired,
    WorkspaceMismatch,
    BranchMismatch,
    SessionMismatch,
    MissingFingerprint,
    UnsafePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedBeadActionability {
    pub status: BeadActionabilityStatus,
    pub planner_may_claim: bool,
    pub reason: String,
}

#[derive(Debug)]
pub enum SemanticGraphBuildError {
    RootUnreadable { root: String, source: io::Error },
    RootNotDirectory { root: String },
}

impl fmt::Display for SemanticGraphBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootUnreadable { root, source } => {
                write!(f, "semantic graph root is unreadable: {root}: {source}")
            }
            Self::RootNotDirectory { root } => {
                write!(f, "semantic graph root is not a directory: {root}")
            }
        }
    }
}

impl StdError for SemanticGraphBuildError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::RootUnreadable { source, .. } => Some(source),
            Self::RootNotDirectory { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredInput {
    absolute_path: PathBuf,
    source_path: String,
    surface: SourceSurface,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SourceSurface {
    RustCodeModules,
    IntegrationAndContractTests,
    ReadmeAndDocs,
    EvidenceArtifacts,
    BeadsIssueGraph,
    RuntimeArtifacts,
    Unknown,
}

impl SourceSurface {
    fn as_str(self) -> &'static str {
        match self {
            Self::RustCodeModules => "rust_code_modules",
            Self::IntegrationAndContractTests => "integration_and_contract_tests",
            Self::ReadmeAndDocs => "readme_and_docs",
            Self::EvidenceArtifacts => "dropin_and_parity_evidence",
            Self::BeadsIssueGraph => "beads_issue_graph",
            Self::RuntimeArtifacts => "runtime_artifacts",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Default)]
struct GraphBuildState {
    nodes: Vec<SemanticGraphNode>,
    edges: Vec<SemanticGraphEdge>,
    input_fingerprints: Vec<InputFingerprint>,
    trace: Vec<GraphBuildTraceEvent>,
    evidence_node_ids: BTreeMap<String, String>,
    pending_citations: Vec<PendingEvidenceCitation>,
    pending_external_refs: Vec<PendingBeadExternalRef>,
}

impl GraphBuildState {
    fn push_node(&mut self, node: SemanticGraphNode) {
        self.nodes.push(node);
    }

    fn push_edge(&mut self, edge: SemanticGraphEdge) {
        self.edges.push(edge);
    }

    fn push_trace(&mut self, event: GraphBuildTraceEvent) {
        self.trace.push(event);
    }

    fn register_evidence_node(&mut self, source_path: &str, node_id: &str) {
        self.evidence_node_ids
            .insert(source_path.to_string(), node_id.to_string());
    }

    fn push_pending_citation(&mut self, citation: PendingEvidenceCitation) {
        self.pending_citations.push(citation);
    }

    fn push_pending_external_ref(&mut self, external_ref: PendingBeadExternalRef) {
        self.pending_external_refs.push(external_ref);
    }

    fn resolve_pending_links(&mut self) {
        let citations = std::mem::take(&mut self.pending_citations);
        for citation in citations {
            let target = self.ensure_evidence_target(&citation.target_path);
            let mut metadata = BTreeMap::new();
            metadata.insert(
                "citation_source_path".to_string(),
                json!(citation.source_path),
            );
            metadata.insert("citation_path".to_string(), json!(citation.target_path));
            metadata.insert("line_number".to_string(), json!(citation.line_number));
            metadata.insert("claim_surface".to_string(), json!(citation.claim_surface));
            self.push_edge(edge_with_metadata(
                SemanticEdgeType::CitesEvidence,
                &citation.source_node_id,
                &target,
                "markdown_evidence_citation",
                metadata,
            ));
        }

        let external_refs = std::mem::take(&mut self.pending_external_refs);
        for external_ref in external_refs {
            let Some(target_path) = evidence_path_from_external_ref(&external_ref.external_ref)
            else {
                continue;
            };
            let target = self.ensure_evidence_target(target_path);
            let mut metadata = BTreeMap::new();
            metadata.insert("bead_id".to_string(), json!(external_ref.bead_id));
            metadata.insert("external_ref".to_string(), json!(external_ref.external_ref));
            self.push_edge(edge_with_metadata(
                SemanticEdgeType::Tracks,
                &external_ref.source_node_id,
                &target,
                "bead_external_ref",
                metadata,
            ));
        }
    }

    fn ensure_evidence_target(&mut self, source_path: &str) -> String {
        if let Some(node_id) = self.evidence_node_ids.get(source_path) {
            return node_id.clone();
        }

        let mut node = missing_or_unreadable_evidence_node(
            source_path,
            EvidenceFreshnessStatus::Missing,
            "linked_evidence_target_missing",
        );
        node.metadata
            .insert("linked_target_missing".to_string(), json!(true));
        let node_id = node.id.clone();
        self.register_evidence_node(source_path, &node_id);
        self.push_node(node);
        self.push_trace(GraphBuildTraceEvent::new(
            SourceSurface::EvidenceArtifacts.as_str(),
            source_path.to_string(),
            GraphInputStatus::Missing,
            "linked_evidence_target_missing",
            1,
            0,
        ));
        node_id
    }

    fn sort(&mut self) {
        self.nodes.sort_by(|left, right| left.id.cmp(&right.id));
        self.nodes.dedup_by(|left, right| left.id == right.id);
        self.edges.sort_by(|left, right| left.id.cmp(&right.id));
        self.edges.dedup_by(|left, right| left.id == right.id);
        self.input_fingerprints
            .sort_by(|left, right| left.source_path.cmp(&right.source_path));
        self.input_fingerprints
            .dedup_by(|left, right| left.source_path == right.source_path);
        self.trace.sort_by(|left, right| {
            left.source_path
                .cmp(&right.source_path)
                .then_with(|| left.surface_id.cmp(&right.surface_id))
                .then_with(|| left.reason.cmp(&right.reason))
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEvidenceCitation {
    source_node_id: String,
    source_path: String,
    target_path: String,
    line_number: usize,
    claim_surface: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingBeadExternalRef {
    source_node_id: String,
    bead_id: String,
    external_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScoredContextNode<'a> {
    node: &'a SemanticGraphNode,
    score: i64,
    estimated_bytes: u64,
    reason: String,
    suppression_reason: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodePrivacyClassification {
    status: RedactionStatus,
    redacted_metadata_keys: BTreeSet<String>,
    sensitive_path_kind: Option<&'static str>,
}

impl ScoredContextNode<'_> {
    fn to_item(&self) -> ContextBundleItem {
        ContextBundleItem {
            node_id: self.node.id.clone(),
            node_type: self.node.node_type,
            source_path: self.node.source_path.clone(),
            title: self.node.title.clone(),
            reason: self.reason.clone(),
            score: self.score,
            estimated_bytes: self.estimated_bytes,
            estimated_tokens: estimate_tokens(self.estimated_bytes),
            freshness_status: self.node.freshness_status,
            redaction_status: self.node.redaction_status,
        }
    }

    fn to_exclusion(&self, reason: &str) -> ContextBundleExclusion {
        let reason = if reason == "unsafe_to_emit_by_redaction_policy" {
            let mut parts = vec![reason.to_string()];
            parts.extend(
                privacy_reason_fragments(self.node)
                    .into_iter()
                    .map(ToString::to_string),
            );
            parts.join(",")
        } else {
            reason.to_string()
        };

        ContextBundleExclusion {
            node_id: self.node.id.clone(),
            node_type: self.node.node_type,
            source_path: self.node.source_path.clone(),
            title: self.node.title.clone(),
            reason,
            score: self.score,
            estimated_bytes: self.estimated_bytes,
            freshness_status: self.node.freshness_status,
            redaction_status: self.node.redaction_status,
        }
    }
}

fn unsafe_changed_path_exclusion(node: &SemanticGraphNode) -> ContextBundleExclusion {
    let mut reason_parts = vec!["unsafe_to_emit_by_redaction_policy".to_string()];
    reason_parts.extend(
        privacy_reason_fragments(node)
            .into_iter()
            .map(ToString::to_string),
    );
    let estimated_bytes = estimate_node_bytes(node);
    ContextBundleExclusion {
        node_id: node.id.clone(),
        node_type: node.node_type,
        source_path: node.source_path.clone(),
        title: node.title.clone(),
        reason: reason_parts.join(","),
        score: 0,
        estimated_bytes,
        freshness_status: node.freshness_status,
        redaction_status: node.redaction_status,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRustSymbol {
    kind: String,
    name: String,
}

pub fn classify_bead_actionability(
    value: &Value,
    reference_time_utc: Option<DateTime<Utc>>,
) -> ClassifiedBeadActionability {
    if value
        .get("deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return ClassifiedBeadActionability {
            status: BeadActionabilityStatus::TombstoneReferenceOnly,
            planner_may_claim: false,
            reason: "tombstone_is_never_actionable".to_string(),
        };
    }

    let Some(status) = value.get("status").and_then(Value::as_str) else {
        return ClassifiedBeadActionability {
            status: BeadActionabilityStatus::UnknownFailClosed,
            planner_may_claim: false,
            reason: "missing_status".to_string(),
        };
    };

    match status {
        "open" => {
            if has_blocking_dependency(value) {
                ClassifiedBeadActionability {
                    status: BeadActionabilityStatus::Blocked,
                    planner_may_claim: false,
                    reason: "open_with_blocking_dependency".to_string(),
                }
            } else {
                ClassifiedBeadActionability {
                    status: BeadActionabilityStatus::ActionableOpen,
                    planner_may_claim: true,
                    reason: "open_without_blockers".to_string(),
                }
            }
        }
        "in_progress" => classify_in_progress_bead(value, reference_time_utc),
        "closed" => ClassifiedBeadActionability {
            status: BeadActionabilityStatus::ClosedReferenceOnly,
            planner_may_claim: false,
            reason: "closed_work_is_context_only".to_string(),
        },
        "tombstone" => ClassifiedBeadActionability {
            status: BeadActionabilityStatus::TombstoneReferenceOnly,
            planner_may_claim: false,
            reason: "tombstone_is_never_actionable".to_string(),
        },
        _ => ClassifiedBeadActionability {
            status: BeadActionabilityStatus::UnknownFailClosed,
            planner_may_claim: false,
            reason: "unknown_status".to_string(),
        },
    }
}

fn classify_in_progress_bead(
    value: &Value,
    reference_time_utc: Option<DateTime<Utc>>,
) -> ClassifiedBeadActionability {
    let Some(reference_time_utc) = reference_time_utc else {
        return ClassifiedBeadActionability {
            status: BeadActionabilityStatus::ClaimedInProgress,
            planner_may_claim: false,
            reason: "claimed_by_an_agent".to_string(),
        };
    };

    let Some(updated_at) = value.get("updated_at").and_then(Value::as_str) else {
        return ClassifiedBeadActionability {
            status: BeadActionabilityStatus::ClaimedInProgress,
            planner_may_claim: false,
            reason: "claimed_without_updated_at".to_string(),
        };
    };

    let Ok(updated_at) = DateTime::parse_from_rfc3339(updated_at) else {
        return ClassifiedBeadActionability {
            status: BeadActionabilityStatus::UnknownFailClosed,
            planner_may_claim: false,
            reason: "invalid_updated_at".to_string(),
        };
    };

    if reference_time_utc
        .signed_duration_since(updated_at.with_timezone(&Utc))
        .num_hours()
        >= 24
    {
        ClassifiedBeadActionability {
            status: BeadActionabilityStatus::StalledReopenCandidate,
            planner_may_claim: false,
            reason: "in_progress_updated_at_is_stale".to_string(),
        }
    } else {
        ClassifiedBeadActionability {
            status: BeadActionabilityStatus::ClaimedInProgress,
            planner_may_claim: false,
            reason: "claimed_by_an_agent".to_string(),
        }
    }
}

fn classify_performance_budget_claim(
    value: &Value,
    options: &SemanticWorkspaceGraphBuildOptions,
    repository_root: Option<&Path>,
) -> Option<(EvidenceFreshnessStatus, bool, String)> {
    let schema = value.get("schema").and_then(Value::as_str)?;
    if schema == "pi.perf.budget_summary.v1" {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_schema_not_current".to_string(),
        ));
    }
    if schema != "pi.perf.budget_summary.v2" {
        return None;
    }

    let canonical_generated_at =
        value
            .get("generated_at")
            .and_then(Value::as_str)
            .and_then(|raw| {
                DateTime::parse_from_rfc3339(raw)
                    .ok()
                    .filter(|parsed| {
                        parsed.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) == raw
                    })
                    .map(|parsed| parsed.with_timezone(&Utc))
            });
    let Some(generated_at) = canonical_generated_at else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    if options
        .reference_time_utc
        .is_some_and(|reference| generated_at > reference + Duration::minutes(5))
    {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_generated_at_in_future".to_string(),
        ));
    }

    let Some(strict_mode) = value.get("strict_mode").and_then(Value::as_bool) else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    let source_commit = match value.get("source_commit") {
        Some(Value::Null) => None,
        Some(Value::String(commit))
            if commit.len() == 40
                && commit
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                && !commit.bytes().all(|byte| byte == b'0') =>
        {
            Some(commit.as_str())
        }
        _ => {
            return Some((
                EvidenceFreshnessStatus::Malformed,
                false,
                "performance_budget_claim_readiness_malformed".to_string(),
            ));
        }
    };
    let optional_lineage = |field: &str| match value.get(field) {
        Some(Value::Null) => Some(None),
        Some(Value::String(lineage))
            if !lineage.is_empty()
                && lineage.trim() == lineage
                && !lineage.chars().any(char::is_control) =>
        {
            Some(Some(lineage.as_str()))
        }
        _ => None,
    };
    let (Some(run_id), Some(correlation_id)) = (
        optional_lineage("run_id"),
        optional_lineage("correlation_id"),
    ) else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    let counts = (
        value.get("total_budgets").and_then(Value::as_u64),
        value.get("pass").and_then(Value::as_u64),
        value.get("fail").and_then(Value::as_u64),
        value.get("no_data").and_then(Value::as_u64),
        value.get("ci_enforced").and_then(Value::as_u64),
        value.get("ci_with_data").and_then(Value::as_u64),
        value.get("ci_fail").and_then(Value::as_u64),
        value.get("ci_no_data").and_then(Value::as_u64),
        value
            .get("data_contract_failures_count")
            .and_then(Value::as_u64),
    );
    let (
        Some(total_budgets),
        Some(pass),
        Some(fail),
        Some(no_data),
        Some(ci_enforced),
        Some(ci_with_data),
        Some(ci_fail),
        Some(ci_no_data),
        Some(contract_failures),
    ) = counts
    else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    if total_budgets == 0
        || ci_enforced == 0
        || pass
            .checked_add(fail)
            .and_then(|count| count.checked_add(no_data))
            != Some(total_budgets)
        || ci_enforced > total_budgets
        || ci_with_data
            .checked_add(ci_no_data)
            .is_none_or(|count| count != ci_enforced)
        || ci_fail > ci_enforced
        || ci_fail > fail
    {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    }

    let mut expected_blockers = BTreeSet::new();
    if !strict_mode {
        expected_blockers.insert("strict_mode_disabled");
    }
    if source_commit.is_none() {
        expected_blockers.insert("source_commit_unbound");
    }
    if run_id.is_none() {
        expected_blockers.insert("run_id_missing");
    }
    if correlation_id.is_none() || run_id != correlation_id {
        expected_blockers.insert("correlation_id_missing");
    }
    if ci_with_data != ci_enforced || ci_no_data != 0 {
        expected_blockers.insert("ci_budget_data_missing");
    }
    if ci_fail != 0 {
        expected_blockers.insert("ci_budget_failed");
    }
    if no_data != 0 {
        expected_blockers.insert("budget_data_missing");
    }
    if fail != 0 {
        expected_blockers.insert("budget_failed");
    }
    if contract_failures != 0 {
        expected_blockers.insert("data_contract_failure");
    }

    let Some(claim) = value.get("claim_readiness").and_then(Value::as_object) else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    let status = claim.get("status").and_then(Value::as_str);
    let authorized = claim
        .get("performance_claims_authorized")
        .and_then(Value::as_bool);
    let Some(blockers) = claim.get("blocking_reason_codes").and_then(Value::as_array) else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    let blockers = blockers
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>();
    let Some(blockers) = blockers else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    let expected = expected_blockers.into_iter().collect::<Vec<_>>();
    let claims_ready = expected.is_empty();
    if blockers != expected
        || status
            != Some(if claims_ready {
                "claim_ready"
            } else {
                "blocked"
            })
        || authorized != Some(claims_ready)
    {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    }
    if claims_ready {
        let Some(source_commit) = source_commit else {
            return Some((
                EvidenceFreshnessStatus::Malformed,
                false,
                "performance_budget_claim_readiness_malformed".to_string(),
            ));
        };
        match performance_source_binding_failure(repository_root, source_commit) {
            None => None,
            Some(PerformanceSourceBindingFailure::Unavailable) => Some((
                EvidenceFreshnessStatus::Uncertified,
                false,
                "performance_budget_source_binding_unavailable".to_string(),
            )),
            Some(PerformanceSourceBindingFailure::Invalid(reason)) => Some((
                EvidenceFreshnessStatus::Malformed,
                false,
                reason.to_string(),
            )),
        }
    } else {
        Some((
            EvidenceFreshnessStatus::Uncertified,
            false,
            "performance_claims_not_authorized".to_string(),
        ))
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PerformanceSourceBindingFailure {
    Unavailable,
    Invalid(&'static str),
}

fn performance_source_binding_failure(
    repository_root: Option<&Path>,
    source_commit: &str,
) -> Option<PerformanceSourceBindingFailure> {
    let Some(repository_root) = repository_root else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(head) = git_stdout(repository_root, &["rev-parse", "--verify", "HEAD^{commit}"])
    else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(resolved_source) = git_stdout(
        repository_root,
        &[
            "rev-parse",
            "--verify",
            &format!("{source_commit}^{{commit}}"),
        ],
    ) else {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_source_commit_unresolvable",
        ));
    };
    if resolved_source != source_commit {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_source_commit_not_exact",
        ));
    }

    let ancestor = Command::new("git")
        .arg("-C")
        .arg(repository_root)
        .args(["merge-base", "--is-ancestor", source_commit, &head])
        .status()
        .is_ok_and(|status| status.success());
    if !ancestor {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_source_commit_not_ancestor",
        ));
    }
    if source_commit == head {
        return None;
    }

    let Some(changed_paths) = git_stdout(
        repository_root,
        &["diff", "--name-only", "--no-renames", source_commit, &head],
    ) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if changed_paths.is_empty() {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_source_commit_not_release_bound",
        ));
    }
    let evidence_only = changed_paths.lines().all(|path| {
        [
            "tests/perf/reports/",
            "tests/e2e_results/",
            "tests/ext_conformance/reports/",
            "tests/certification/",
            "docs/evidence/",
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix))
    });
    if !evidence_only {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_source_commit_not_release_bound",
        ));
    }
    None
}

fn git_stdout(repository_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|stdout| stdout.trim().to_string())
}

pub fn classify_evidence_freshness(
    value: &Value,
    options: &SemanticWorkspaceGraphBuildOptions,
) -> (EvidenceFreshnessStatus, bool, String) {
    classify_evidence_freshness_in_repository(value, options, None)
}

fn classify_evidence_freshness_in_repository(
    value: &Value,
    options: &SemanticWorkspaceGraphBuildOptions,
    repository_root: Option<&Path>,
) -> (EvidenceFreshnessStatus, bool, String) {
    if value
        .get("claim_surface")
        .and_then(Value::as_str)
        .is_some_and(|surface| surface == "historical_snapshot")
    {
        return (
            EvidenceFreshnessStatus::HistoricalSnapshot,
            false,
            "claim_surface_is_historical_snapshot".to_string(),
        );
    }

    if let Some(classification) = classify_performance_budget_claim(value, options, repository_root)
    {
        return classification;
    }

    if value
        .get("overall_verdict")
        .and_then(Value::as_str)
        .is_some_and(|verdict| verdict != "CERTIFIED")
    {
        return (
            EvidenceFreshnessStatus::Uncertified,
            false,
            "overall_verdict_not_certified".to_string(),
        );
    }

    let Some(generated_at) = evidence_generated_at(value) else {
        return (
            EvidenceFreshnessStatus::FreshnessUnknown,
            false,
            "missing_generated_at".to_string(),
        );
    };

    let Ok(generated_at) = DateTime::parse_from_rfc3339(generated_at) else {
        return (
            EvidenceFreshnessStatus::Malformed,
            false,
            "invalid_generated_at".to_string(),
        );
    };

    let Some(reference_time_utc) = options.reference_time_utc else {
        return (
            EvidenceFreshnessStatus::FreshnessUnknown,
            false,
            "reference_time_not_provided".to_string(),
        );
    };

    if reference_time_utc
        .signed_duration_since(generated_at.with_timezone(&Utc))
        .num_days()
        > options.stale_after_days
    {
        (
            EvidenceFreshnessStatus::Stale,
            false,
            "generated_at_older_than_policy".to_string(),
        )
    } else {
        (
            EvidenceFreshnessStatus::Current,
            true,
            "generated_at_within_policy".to_string(),
        )
    }
}

fn file_region_node(
    source_path: &str,
    content_sha256: &str,
    size_bytes: u64,
    line_start: usize,
    line_end: usize,
    surface_id: &str,
) -> SemanticGraphNode {
    let stable_key = source_path.to_string();
    let mut metadata = BTreeMap::new();
    metadata.insert("surface_id".to_string(), json!(surface_id));
    let privacy = classify_node_privacy(source_path, None);
    apply_privacy_metadata(&mut metadata, &privacy);
    SemanticGraphNode {
        id: stable_id("file_region", &[&stable_key]),
        node_type: SemanticNodeType::FileRegion,
        source_path: source_path.to_string(),
        title: source_path.to_string(),
        stable_key,
        content_sha256: Some(content_sha256.to_string()),
        size_bytes: Some(size_bytes),
        line_start: Some(line_start),
        line_end: Some(line_end),
        freshness_status: None,
        bead_actionability_status: None,
        redaction_status: privacy.status,
        metadata,
    }
}

fn code_symbol_node(
    source_path: &str,
    kind: &str,
    name: &str,
    line: usize,
    content_sha256: &str,
) -> SemanticGraphNode {
    let stable_key = format!("{source_path}:{kind}:{name}:{line}");
    let mut metadata = BTreeMap::new();
    metadata.insert("symbol_kind".to_string(), json!(kind));
    SemanticGraphNode {
        id: stable_id("code_symbol", &[&stable_key]),
        node_type: SemanticNodeType::CodeSymbol,
        source_path: source_path.to_string(),
        title: name.to_string(),
        stable_key,
        content_sha256: Some(content_sha256.to_string()),
        size_bytes: None,
        line_start: Some(line),
        line_end: Some(line),
        freshness_status: None,
        bead_actionability_status: None,
        redaction_status: RedactionStatus::None,
        metadata,
    }
}

fn test_case_node(
    source_path: &str,
    name: &str,
    line: usize,
    content_sha256: &str,
) -> SemanticGraphNode {
    let stable_key = format!("{source_path}:test:{name}:{line}");
    SemanticGraphNode {
        id: stable_id("test_case", &[&stable_key]),
        node_type: SemanticNodeType::TestCase,
        source_path: source_path.to_string(),
        title: name.to_string(),
        stable_key,
        content_sha256: Some(content_sha256.to_string()),
        size_bytes: None,
        line_start: Some(line),
        line_end: Some(line),
        freshness_status: None,
        bead_actionability_status: None,
        redaction_status: RedactionStatus::None,
        metadata: BTreeMap::new(),
    }
}

fn doc_section_node(
    source_path: &str,
    level: usize,
    title: &str,
    line: usize,
    content_sha256: &str,
) -> SemanticGraphNode {
    let stable_key = format!("{source_path}:heading:{level}:{line}:{title}");
    let mut metadata = BTreeMap::new();
    metadata.insert("heading_level".to_string(), json!(level));
    let privacy = classify_node_privacy(source_path, None);
    apply_privacy_metadata(&mut metadata, &privacy);
    SemanticGraphNode {
        id: stable_id("doc_section", &[&stable_key]),
        node_type: SemanticNodeType::DocSection,
        source_path: source_path.to_string(),
        title: redact_sensitive_text(title),
        stable_key,
        content_sha256: Some(content_sha256.to_string()),
        size_bytes: None,
        line_start: Some(line),
        line_end: Some(line),
        freshness_status: None,
        bead_actionability_status: None,
        redaction_status: privacy.status,
        metadata,
    }
}

fn doc_citation_node(
    source_path: &str,
    target_path: &str,
    line: usize,
    content_sha256: &str,
    claim_surface: &str,
) -> SemanticGraphNode {
    let stable_key = format!("{source_path}:citation:{line}:{target_path}");
    let mut metadata = BTreeMap::new();
    metadata.insert("citation_path".to_string(), json!(target_path));
    metadata.insert("claim_surface".to_string(), json!(claim_surface));
    metadata.insert(
        "release_claim_candidate".to_string(),
        json!(claim_surface == "release_facing"),
    );
    let privacy = classify_node_privacy(source_path, None);
    apply_privacy_metadata(&mut metadata, &privacy);
    SemanticGraphNode {
        id: stable_id("doc_section", &[&stable_key]),
        node_type: SemanticNodeType::DocSection,
        source_path: source_path.to_string(),
        title: format!("citation:{target_path}"),
        stable_key,
        content_sha256: Some(content_sha256.to_string()),
        size_bytes: None,
        line_start: Some(line),
        line_end: Some(line),
        freshness_status: None,
        bead_actionability_status: None,
        redaction_status: privacy.status,
        metadata,
    }
}

fn evidence_artifact_node(
    source_path: &str,
    value: &Value,
    content_sha256: &str,
    options: &SemanticWorkspaceGraphBuildOptions,
    repository_root: &Path,
) -> SemanticGraphNode {
    let artifact_schema = value
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("schema_missing");
    let stable_key = format!("{source_path}:{artifact_schema}");
    let (freshness_status, release_claim_allowed, reason) =
        classify_evidence_freshness_in_repository(value, options, Some(repository_root));
    let mut metadata = BTreeMap::new();
    let privacy = classify_node_privacy(source_path, Some(value));
    metadata.insert("artifact_schema".to_string(), json!(artifact_schema));
    if let Some(generated_at) = evidence_generated_at(value) {
        metadata.insert("generated_at".to_string(), json!(generated_at));
    }
    if let Some(claim_surface) = value.get("claim_surface").and_then(Value::as_str) {
        metadata.insert("claim_surface".to_string(), json!(claim_surface));
    }
    if let Some(overall_verdict) = value.get("overall_verdict").and_then(Value::as_str) {
        metadata.insert("overall_verdict".to_string(), json!(overall_verdict));
    }
    if let Some(claim_readiness) = value.get("claim_readiness").and_then(Value::as_object) {
        if let Some(status) = claim_readiness.get("status").and_then(Value::as_str) {
            metadata.insert("claim_readiness_status".to_string(), json!(status));
        }
        if let Some(authorized) = claim_readiness
            .get("performance_claims_authorized")
            .and_then(Value::as_bool)
        {
            metadata.insert(
                "performance_claims_authorized".to_string(),
                json!(authorized),
            );
        }
        if let Some(blockers) = claim_readiness
            .get("blocking_reason_codes")
            .and_then(Value::as_array)
        {
            metadata.insert("blocking_reason_codes".to_string(), json!(blockers));
        }
    }
    if let Some(source_generated_at) = value
        .get("source_report_generated_at")
        .and_then(Value::as_str)
    {
        metadata.insert(
            "source_report_generated_at".to_string(),
            json!(source_generated_at),
        );
    }
    metadata.insert(
        "release_claim_allowed".to_string(),
        json!(release_claim_allowed),
    );
    metadata.insert("freshness_reason".to_string(), json!(reason));
    metadata.insert(
        "claim_gate_status".to_string(),
        json!(claim_gate_status(freshness_status, release_claim_allowed)),
    );
    metadata.insert(
        "suppresses_release_claim_context".to_string(),
        json!(!release_claim_allowed),
    );
    if source_path.ends_with("dropin-certification-verdict.json") {
        metadata.insert(
            "strict_replacement_claim_allowed".to_string(),
            json!(release_claim_allowed),
        );
    }
    apply_privacy_metadata(&mut metadata, &privacy);

    SemanticGraphNode {
        id: stable_id("evidence_artifact", &[&stable_key]),
        node_type: SemanticNodeType::EvidenceArtifact,
        source_path: source_path.to_string(),
        title: artifact_schema.to_string(),
        stable_key,
        content_sha256: Some(content_sha256.to_string()),
        size_bytes: None,
        line_start: None,
        line_end: None,
        freshness_status: Some(freshness_status),
        bead_actionability_status: None,
        redaction_status: privacy.status,
        metadata,
    }
}

fn missing_or_unreadable_evidence_node(
    source_path: &str,
    freshness_status: EvidenceFreshnessStatus,
    reason: &str,
) -> SemanticGraphNode {
    let stable_key = format!("{source_path}:missing_or_unreadable");
    let mut metadata = BTreeMap::new();
    let privacy = classify_node_privacy(source_path, None);
    metadata.insert("freshness_reason".to_string(), json!(reason));
    metadata.insert("release_claim_allowed".to_string(), json!(false));
    metadata.insert(
        "claim_gate_status".to_string(),
        json!(claim_gate_status(freshness_status, false)),
    );
    metadata.insert("suppresses_release_claim_context".to_string(), json!(true));
    apply_privacy_metadata(&mut metadata, &privacy);
    SemanticGraphNode {
        id: stable_id("evidence_artifact", &[&stable_key]),
        node_type: SemanticNodeType::EvidenceArtifact,
        source_path: source_path.to_string(),
        title: source_path.to_string(),
        stable_key,
        content_sha256: None,
        size_bytes: None,
        line_start: None,
        line_end: None,
        freshness_status: Some(freshness_status),
        bead_actionability_status: None,
        redaction_status: privacy.status,
        metadata,
    }
}

fn bead_node(
    source_path: &str,
    line: usize,
    bead_id: &str,
    value: &Value,
    classified: &ClassifiedBeadActionability,
) -> SemanticGraphNode {
    let stable_key = bead_id.to_string();
    let mut metadata = BTreeMap::new();
    metadata.insert("bead_id".to_string(), json!(bead_id));
    metadata.insert(
        "planner_may_claim".to_string(),
        json!(classified.planner_may_claim),
    );
    metadata.insert(
        "actionability_reason".to_string(),
        json!(classified.reason.clone()),
    );
    if let Some(status) = value.get("status").and_then(Value::as_str) {
        metadata.insert("status".to_string(), json!(status));
    }
    if let Some(title) = value.get("title").and_then(Value::as_str) {
        metadata.insert("title".to_string(), json!(redact_sensitive_text(title)));
    }
    if let Some(priority) = value.get("priority").and_then(Value::as_i64) {
        metadata.insert("priority".to_string(), json!(priority));
    }
    if let Some(issue_type) = value.get("issue_type").and_then(Value::as_str) {
        metadata.insert("issue_type".to_string(), json!(issue_type));
    }
    if let Some(external_ref) = bead_external_ref(value) {
        metadata.insert(
            "external_ref".to_string(),
            json!(redact_sensitive_text(external_ref)),
        );
    }
    let privacy = classify_node_privacy(source_path, Some(value));
    apply_privacy_metadata(&mut metadata, &privacy);

    SemanticGraphNode {
        id: stable_id("bead", &[bead_id]),
        node_type: SemanticNodeType::Bead,
        source_path: source_path.to_string(),
        title: bead_id.to_string(),
        stable_key,
        content_sha256: None,
        size_bytes: None,
        line_start: Some(line),
        line_end: Some(line),
        freshness_status: None,
        bead_actionability_status: Some(classified.status),
        redaction_status: privacy.status,
        metadata,
    }
}

fn provider_surface_node(source_path: &str, content_sha256: &str) -> SemanticGraphNode {
    let provider = Path::new(source_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown_provider");
    let stable_key = format!("provider:{provider}:{source_path}");
    let mut metadata = BTreeMap::new();
    metadata.insert("provider_id".to_string(), json!(provider));
    SemanticGraphNode {
        id: stable_id("provider_surface", &[&stable_key]),
        node_type: SemanticNodeType::ProviderSurface,
        source_path: source_path.to_string(),
        title: provider.to_string(),
        stable_key,
        content_sha256: Some(content_sha256.to_string()),
        size_bytes: None,
        line_start: None,
        line_end: None,
        freshness_status: None,
        bead_actionability_status: None,
        redaction_status: RedactionStatus::None,
        metadata,
    }
}

fn validation_command_node(source_path: &str, test_name: &str) -> SemanticGraphNode {
    let test_target = Path::new(source_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown_test");
    let command = format!("cargo test --test {test_target} {test_name}");
    let stable_key = command.clone();
    let mut metadata = BTreeMap::new();
    metadata.insert("command".to_string(), json!(command));
    metadata.insert("test_target".to_string(), json!(test_target));
    SemanticGraphNode {
        id: stable_id("validation_command", &[&stable_key]),
        node_type: SemanticNodeType::ValidationCommand,
        source_path: source_path.to_string(),
        title: stable_key.clone(),
        stable_key,
        content_sha256: None,
        size_bytes: None,
        line_start: None,
        line_end: None,
        freshness_status: None,
        bead_actionability_status: None,
        redaction_status: RedactionStatus::None,
        metadata,
    }
}

fn edge(
    edge_type: SemanticEdgeType,
    source: &str,
    target: &str,
    reason: &str,
) -> SemanticGraphEdge {
    edge_with_metadata(edge_type, source, target, reason, BTreeMap::new())
}

fn edge_with_metadata(
    edge_type: SemanticEdgeType,
    source: &str,
    target: &str,
    reason: &str,
    metadata: BTreeMap<String, Value>,
) -> SemanticGraphEdge {
    let edge_type_key = format!("{edge_type:?}");
    let stable_key = [edge_type_key.as_str(), source, target, reason];
    SemanticGraphEdge {
        id: stable_id("edge", &stable_key),
        edge_type,
        source: source.to_string(),
        target: target.to_string(),
        reason: reason.to_string(),
        metadata,
    }
}

fn add_bead_dependency_edges(current_node_id: &str, value: &Value, state: &mut GraphBuildState) {
    let Some(dependencies) = value.get("dependencies").and_then(Value::as_array) else {
        return;
    };
    for dependency in dependencies {
        let Some(depends_on_id) = dependency.get("depends_on_id").and_then(Value::as_str) else {
            continue;
        };
        let relation = dependency
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("depends_on");
        let edge_type = if relation == "blocks" {
            SemanticEdgeType::Blocks
        } else {
            SemanticEdgeType::DependsOn
        };
        let target = stable_id("bead", &[depends_on_id]);
        state.push_edge(edge(
            edge_type,
            current_node_id,
            &target,
            "beads_jsonl_dependency",
        ));
    }
}

fn has_blocking_dependency(value: &Value) -> bool {
    value
        .get("dependencies")
        .and_then(Value::as_array)
        .is_some_and(|dependencies| {
            dependencies.iter().any(|dependency| {
                dependency
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|relation| relation == "blocks")
            })
        })
}

fn tokenize_context_query(query: Option<&str>) -> Vec<String> {
    query
        .unwrap_or_default()
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .map(str::trim)
        .filter(|term| term.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn base_node_score(node: &SemanticGraphNode) -> i64 {
    match node.node_type {
        SemanticNodeType::Bead => 35,
        SemanticNodeType::ValidationCommand => 30,
        SemanticNodeType::TestCase => 25,
        SemanticNodeType::EvidenceArtifact | SemanticNodeType::ProviderSurface => 20,
        SemanticNodeType::CodeSymbol => 15,
        SemanticNodeType::DocSection => 12,
        SemanticNodeType::FileRegion => 10,
    }
}

fn matched_query_terms(node: &SemanticGraphNode, query_terms: &[String]) -> Vec<String> {
    let haystack = format!(
        "{} {} {}",
        node.source_path,
        node.title,
        searchable_metadata(&node.metadata)
    )
    .to_ascii_lowercase();
    query_terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .cloned()
        .collect()
}

fn searchable_metadata(metadata: &BTreeMap<String, Value>) -> String {
    let mut values = Vec::new();
    for key in [
        "bead_id",
        "title",
        "issue_type",
        "artifact_schema",
        "provider_id",
        "command",
        "test_target",
        "citation_path",
        "external_ref",
    ] {
        if let Some(value) = metadata.get(key).and_then(Value::as_str) {
            values.push(value);
        }
    }
    values.join(" ")
}

fn validation_command_matches(node: &SemanticGraphNode, failing_command: &str) -> bool {
    node.node_type == SemanticNodeType::ValidationCommand
        && node
            .metadata
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| {
                let command = command.to_ascii_lowercase();
                command.contains(failing_command) || failing_command.contains(&command)
            })
}

fn privacy_reason_fragments(node: &SemanticGraphNode) -> Vec<&str> {
    let mut fragments = Vec::new();
    if let Some(keys) = node
        .metadata
        .get("redacted_metadata_keys")
        .and_then(Value::as_array)
    {
        for key in keys.iter().filter_map(Value::as_str) {
            fragments.push(match key {
                "credential_like" => "redacted_key:credential_like",
                "prompt_or_payload" => "redacted_key:prompt_or_payload",
                _ => "redacted_key:other",
            });
        }
    }
    if let Some(kind) = node
        .metadata
        .get("sensitive_path_kind")
        .and_then(Value::as_str)
    {
        fragments.push(match kind {
            "vcr_fixture" => "sensitive_path:vcr_fixture",
            "log_artifact" => "sensitive_path:log_artifact",
            "credential_path" => "sensitive_path:credential_path",
            _ => "sensitive_path:other",
        });
    }
    fragments
}

fn paths_are_related(left: &str, right: &str) -> bool {
    left == right || path_starts_with(left, right) || path_starts_with(right, left)
}

fn path_starts_with(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn estimate_node_bytes(node: &SemanticGraphNode) -> u64 {
    if let Some(size_bytes) = node.size_bytes {
        return size_bytes.clamp(128, 16 * 1024);
    }
    let line_count = match (node.line_start, node.line_end) {
        (Some(start), Some(end)) if end >= start => end.saturating_sub(start).saturating_add(1),
        _ => 1,
    };
    u64::try_from(line_count)
        .unwrap_or(u64::MAX)
        .saturating_mul(160)
        .clamp(128, 8 * 1024)
}

fn estimate_tokens(bytes: u64) -> u64 {
    bytes.saturating_add(3) / 4
}

fn default_context_cache_ttl_seconds() -> u64 {
    DEFAULT_CONTEXT_CACHE_TTL_SECONDS
}

fn normalize_context_paths(raw_paths: &[String]) -> Vec<ContextPathNormalization> {
    raw_paths
        .iter()
        .map(|raw_path| normalize_context_artifact_path(raw_path))
        .collect()
}

#[must_use]
pub fn normalize_context_artifact_path(raw_path: &str) -> ContextPathNormalization {
    if raw_path.trim().is_empty() {
        return ContextPathNormalization {
            raw_path: raw_path.to_string(),
            normalized_path: None,
            accepted: false,
            reason: "empty_path".to_string(),
        };
    }
    if raw_path.contains('\0') {
        return ContextPathNormalization {
            raw_path: raw_path.to_string(),
            normalized_path: None,
            accepted: false,
            reason: "nul_byte_rejected".to_string(),
        };
    }
    if raw_path.contains('\\') {
        return ContextPathNormalization {
            raw_path: raw_path.to_string(),
            normalized_path: None,
            accepted: false,
            reason: "backslash_separator_rejected".to_string(),
        };
    }

    let path = Path::new(raw_path);
    if path.is_absolute() {
        return ContextPathNormalization {
            raw_path: raw_path.to_string(),
            normalized_path: None,
            accepted: false,
            reason: "absolute_path_rejected".to_string(),
        };
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return ContextPathNormalization {
                        raw_path: raw_path.to_string(),
                        normalized_path: None,
                        accepted: false,
                        reason: "parent_escape_rejected".to_string(),
                    };
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::Prefix(_) | Component::RootDir => {
                return ContextPathNormalization {
                    raw_path: raw_path.to_string(),
                    normalized_path: None,
                    accepted: false,
                    reason: "root_or_prefix_rejected".to_string(),
                };
            }
        }
    }

    if parts.is_empty() {
        return ContextPathNormalization {
            raw_path: raw_path.to_string(),
            normalized_path: None,
            accepted: false,
            reason: "empty_normalized_path".to_string(),
        };
    }

    ContextPathNormalization {
        raw_path: raw_path.to_string(),
        normalized_path: Some(parts.join("/")),
        accepted: true,
        reason: "normalized".to_string(),
    }
}

fn graph_input_fingerprint_digest(graph: &SemanticWorkspaceGraph) -> String {
    let mut hasher = Sha256::new();
    hasher.update(graph.root.as_bytes());
    for fingerprint in &graph.input_fingerprints {
        hasher.update(b"\0");
        hasher.update(fingerprint.source_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(fingerprint.surface_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(fingerprint.sha256.as_bytes());
        hasher.update(b"\0");
        hasher.update(fingerprint.size_bytes.to_string().as_bytes());
        if let Some(mtime_unix_ns) = fingerprint.mtime_unix_ns {
            hasher.update(b"\0");
            hasher.update(mtime_unix_ns.to_string().as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
}

fn build_redaction_summary(
    selected_items: &[ContextBundleItem],
    excluded_items: &[ContextBundleExclusion],
) -> ContextRedactionSummary {
    let mut redacted_metadata_keys = BTreeSet::new();
    let mut sensitive_path_kinds = BTreeSet::new();
    let mut overall_status = RedactionStatus::None;
    let mut selected_redacted_nodes = 0;
    let mut selected_sensitive_omissions = 0;
    let mut suppressed_unsafe_nodes = 0;

    for item in selected_items {
        overall_status = overall_status.max(item.redaction_status);
        match item.redaction_status {
            RedactionStatus::Redacted => selected_redacted_nodes += 1,
            RedactionStatus::SensitiveOmitted => selected_sensitive_omissions += 1,
            RedactionStatus::UnsafeToEmit => suppressed_unsafe_nodes += 1,
            RedactionStatus::None => {}
        }
        collect_privacy_hints_from_reason(
            &item.reason,
            &mut redacted_metadata_keys,
            &mut sensitive_path_kinds,
        );
    }

    for item in excluded_items {
        overall_status = overall_status.max(item.redaction_status);
        if item.redaction_status == RedactionStatus::UnsafeToEmit {
            suppressed_unsafe_nodes += 1;
        }
        collect_privacy_hints_from_reason(
            &item.reason,
            &mut redacted_metadata_keys,
            &mut sensitive_path_kinds,
        );
    }

    ContextRedactionSummary {
        policy_version: CONTEXT_PRIVACY_POLICY_VERSION.to_string(),
        overall_status,
        selected_redacted_nodes,
        selected_sensitive_omissions,
        suppressed_unsafe_nodes,
        redacted_metadata_keys,
        sensitive_path_kinds,
    }
}

fn collect_privacy_hints_from_reason(
    reason: &str,
    redacted_metadata_keys: &mut BTreeSet<String>,
    sensitive_path_kinds: &mut BTreeSet<String>,
) {
    for part in reason.split(',') {
        if let Some(key) = part.strip_prefix("redacted_key:") {
            redacted_metadata_keys.insert(key.to_string());
        }
        if let Some(kind) = part.strip_prefix("sensitive_path:") {
            sensitive_path_kinds.insert(kind.to_string());
        }
    }
}

fn parse_rust_symbol(line: &str) -> Option<ParsedRustSymbol> {
    if line.starts_with("//") {
        return None;
    }

    let tokens: Vec<&str> = line
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|token| !token.is_empty())
        .collect();
    for window in tokens.windows(2) {
        let kind = window[0];
        if matches!(kind, "fn" | "struct" | "enum" | "trait" | "mod") {
            return Some(ParsedRustSymbol {
                kind: kind.to_string(),
                name: window[1].to_string(),
            });
        }
    }
    None
}

fn parse_markdown_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let title = trimmed[level..].trim();
    if title.is_empty() {
        return None;
    }
    Some((level, title.to_string()))
}

fn extract_evidence_citations(line: &str) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for token in line.split(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                '`' | '(' | ')' | '[' | ']' | ',' | ';' | '<' | '>' | '"' | '\''
            )
    }) {
        if let Some(path) = normalize_citation_path(token) {
            paths.insert(path);
        }
    }
    paths.into_iter().collect()
}

fn normalize_citation_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim_matches(|ch: char| {
        matches!(
            ch,
            '`' | '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\'' | ',' | ';' | ':' | '.'
        )
    });
    let without_anchor = trimmed.split('#').next().unwrap_or(trimmed);
    if is_claim_evidence_path(without_anchor) {
        Some(without_anchor.to_string())
    } else {
        None
    }
}

fn is_claim_evidence_path(path: &str) -> bool {
    path == "docs/parity-certification.json"
        || path.starts_with("docs/evidence/") && has_extension(path, "json")
        || path.starts_with("docs/contracts/") && has_extension(path, "json")
        || path.starts_with("tests/perf/reports/") && has_extension(path, "json")
        || path.starts_with("tests/golden_corpus/swarm_claim_readiness/")
            && has_extension(path, "json")
        || path.starts_with("tests/fixtures/vcr/") && has_extension(path, "json")
        || path.starts_with("tests/fixtures/context_artifacts/")
            && (has_extension(path, "json") || has_extension(path, "log"))
}

fn claim_surface_for_markdown_line(line: &str) -> &'static str {
    let lower = line.to_ascii_lowercase();
    if lower.contains("historical") || lower.contains("operator evidence only") {
        "historical_snapshot"
    } else if [
        "drop-in",
        "strict replacement",
        "release-facing",
        "release claim",
        "certified",
        "certification",
        "performance claim",
        "perf claim",
        "budget",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        "release_facing"
    } else {
        "documentation"
    }
}

fn evidence_generated_at(value: &Value) -> Option<&str> {
    value
        .get("generated_at")
        .or_else(|| value.get("generated_at_utc"))
        .and_then(Value::as_str)
}

fn claim_gate_status(
    freshness_status: EvidenceFreshnessStatus,
    release_claim_allowed: bool,
) -> &'static str {
    match (freshness_status, release_claim_allowed) {
        (EvidenceFreshnessStatus::Current, true) => "allowed",
        (EvidenceFreshnessStatus::HistoricalSnapshot, _) => "blocked_historical_snapshot",
        (EvidenceFreshnessStatus::Stale, _) => "blocked_stale",
        (EvidenceFreshnessStatus::Missing, _) => "blocked_missing",
        (EvidenceFreshnessStatus::Malformed, _) => "blocked_malformed",
        (EvidenceFreshnessStatus::Uncertified, _) => "blocked_uncertified",
        (EvidenceFreshnessStatus::FreshnessUnknown, _) => "blocked_freshness_unknown",
        (EvidenceFreshnessStatus::Current, false) => "blocked_current_policy",
    }
}

fn classify_node_privacy(source_path: &str, value: Option<&Value>) -> NodePrivacyClassification {
    let sensitive_path_kind = sensitive_context_path_kind(source_path);
    let mut redacted_metadata_keys = BTreeSet::new();
    if let Some(value) = value {
        collect_sensitive_json_keys(value, &mut redacted_metadata_keys);
    }

    let has_payload = value.is_some_and(contains_prompt_or_payload_key);
    let status =
        if sensitive_path_kind.is_some() && (!redacted_metadata_keys.is_empty() || has_payload) {
            RedactionStatus::UnsafeToEmit
        } else if !redacted_metadata_keys.is_empty() {
            RedactionStatus::Redacted
        } else if sensitive_path_kind.is_some() || has_payload {
            RedactionStatus::SensitiveOmitted
        } else {
            RedactionStatus::None
        };

    NodePrivacyClassification {
        status,
        redacted_metadata_keys,
        sensitive_path_kind,
    }
}

fn classify_text_privacy(source_path: &str, content: &str) -> NodePrivacyClassification {
    let sensitive_path_kind = sensitive_context_path_kind(source_path);
    let mut redacted_metadata_keys = BTreeSet::new();
    for token in content.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_')) {
        if let Some(category) = sensitive_metadata_key_category(token) {
            redacted_metadata_keys.insert(category.to_string());
        }
    }
    let lower_content = content.to_ascii_lowercase();
    let has_payload = [
        "prompt", "messages", "request", "response", "body", "content",
    ]
    .iter()
    .any(|needle| lower_content.contains(needle));
    let status =
        if sensitive_path_kind.is_some() && (!redacted_metadata_keys.is_empty() || has_payload) {
            RedactionStatus::UnsafeToEmit
        } else if !redacted_metadata_keys.is_empty() {
            RedactionStatus::Redacted
        } else if sensitive_path_kind.is_some() {
            RedactionStatus::SensitiveOmitted
        } else {
            RedactionStatus::None
        };

    NodePrivacyClassification {
        status,
        redacted_metadata_keys,
        sensitive_path_kind,
    }
}

fn assess_redaction(
    source_path: &str,
    content: &str,
    value: Option<&Value>,
) -> NodePrivacyClassification {
    let mut privacy = classify_node_privacy(source_path, value);
    if value.is_none() && privacy.sensitive_path_kind.is_some() {
        let text_privacy = classify_text_privacy(source_path, content);
        privacy.status = privacy.status.max(text_privacy.status);
        privacy
            .redacted_metadata_keys
            .extend(text_privacy.redacted_metadata_keys);
    }
    privacy
}

fn apply_redaction_metadata(node: &mut SemanticGraphNode, privacy: &NodePrivacyClassification) {
    node.redaction_status = node.redaction_status.max(privacy.status);
    apply_privacy_metadata(&mut node.metadata, privacy);
}

fn apply_privacy_metadata(
    metadata: &mut BTreeMap<String, Value>,
    privacy: &NodePrivacyClassification,
) {
    metadata.insert(
        "redaction_policy_version".to_string(),
        json!(CONTEXT_PRIVACY_POLICY_VERSION),
    );
    if !privacy.redacted_metadata_keys.is_empty() {
        metadata.insert(
            "redacted_metadata_keys".to_string(),
            json!(privacy.redacted_metadata_keys),
        );
    }
    if let Some(kind) = privacy.sensitive_path_kind {
        metadata.insert("sensitive_path_kind".to_string(), json!(kind));
    }
}

fn sensitive_context_path_kind(source_path: &str) -> Option<&'static str> {
    let lower = source_path.to_ascii_lowercase();
    if lower.contains("/vcr/") || lower.starts_with("tests/fixtures/vcr/") {
        Some("vcr_fixture")
    } else if has_extension(source_path, "log")
        || lower.starts_with("logs/")
        || lower.contains("/logs/")
    {
        Some("log_artifact")
    } else if lower.contains("auth") || lower.contains("credential") || lower.contains("secret") {
        Some("credential_path")
    } else {
        None
    }
}

fn collect_sensitive_json_keys(value: &Value, keys: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if let Some(category) = sensitive_metadata_key_category(key) {
                    keys.insert(category.to_string());
                }
                collect_sensitive_json_keys(value, keys);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_sensitive_json_keys(item, keys);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn sensitive_metadata_key_category(key: &str) -> Option<&'static str> {
    if is_sensitive_metadata_key(key) {
        Some("credential_like")
    } else if is_prompt_or_payload_key(key) {
        Some("prompt_or_payload")
    } else {
        None
    }
}

fn contains_prompt_or_payload_key(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            is_prompt_or_payload_key(key) || contains_prompt_or_payload_key(value)
        }),
        Value::Array(items) => items.iter().any(contains_prompt_or_payload_key),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

fn is_sensitive_metadata_key(key: &str) -> bool {
    const EXACT_KEYS: &[&str] = &[
        "authorization",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "id_token",
        "session_token",
        "private_key",
        "client_secret",
    ];

    let key = key.to_ascii_lowercase();
    EXACT_KEYS.contains(&key.as_str())
        || key.ends_with("_api_key")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.contains("credential")
        || key.contains("password")
        || key.contains("bearer")
}

fn is_prompt_or_payload_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "prompt" | "messages" | "request" | "response" | "body" | "content" | "transcript"
    ) || key.ends_with("_body")
        || key.ends_with("_content")
}

fn redact_sensitive_text(value: &str) -> String {
    if value
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .any(|part| {
            let part = part.to_ascii_lowercase();
            is_sensitive_metadata_key(&part)
                || part.starts_with("sk-")
                || part.starts_with("xox")
                || part.starts_with("ghp_")
        })
    {
        "[redacted-sensitive-text]".to_string()
    } else {
        value.to_string()
    }
}

fn bead_external_ref(value: &Value) -> Option<&str> {
    value
        .get("external_ref")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("metadata")
                .and_then(|metadata| metadata.get("external_ref"))
                .and_then(Value::as_str)
        })
}

fn evidence_path_from_external_ref(external_ref: &str) -> Option<&str> {
    if is_claim_evidence_path(external_ref) {
        Some(external_ref)
    } else {
        None
    }
}

fn is_test_attribute(line: &str) -> bool {
    line == "#[test]" || line.starts_with("#[tokio::test") || line.starts_with("#[asupersync::test")
}

fn is_provider_surface(source_path: &str) -> bool {
    source_path.starts_with("src/providers/")
        && has_extension(source_path, "rs")
        && !file_name_eq(source_path, "mod.rs")
}

fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, ".git" | "target"))
}

fn surface_for_path(source_path: &str) -> Option<SourceSurface> {
    if source_path == ".beads/issues.jsonl" {
        return Some(SourceSurface::BeadsIssueGraph);
    }
    if source_path == "README.md"
        || source_path.starts_with("docs/") && has_extension(source_path, "md")
    {
        return Some(SourceSurface::ReadmeAndDocs);
    }
    if source_path.starts_with("docs/") && has_extension(source_path, "json")
        || source_path.starts_with("tests/perf/reports/") && has_extension(source_path, "json")
        || source_path.starts_with("tests/golden_corpus/swarm_claim_readiness/")
            && has_extension(source_path, "json")
        || source_path.starts_with("tests/fixtures/vcr/") && has_extension(source_path, "json")
        || source_path.starts_with("tests/fixtures/context_artifacts/")
            && (has_extension(source_path, "json") || has_extension(source_path, "log"))
    {
        return Some(SourceSurface::EvidenceArtifacts);
    }
    if source_path.starts_with("logs/") || has_extension(source_path, "log") {
        return Some(SourceSurface::RuntimeArtifacts);
    }
    if source_path.starts_with("src/") && has_extension(source_path, "rs") {
        return Some(SourceSurface::RustCodeModules);
    }
    if source_path.starts_with("tests/") && has_extension(source_path, "rs") {
        return Some(SourceSurface::IntegrationAndContractTests);
    }
    None
}

fn has_extension(source_path: &str, extension: &str) -> bool {
    Path::new(source_path)
        .extension()
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn file_name_eq(source_path: &str, file_name: &str) -> bool {
    Path::new(source_path)
        .file_name()
        .is_some_and(|value| value.eq_ignore_ascii_case(file_name))
}

fn count_lines(content: &str) -> usize {
    content.lines().count().max(1)
}

fn file_mtime_unix_ns(path: &Path) -> io::Result<Option<u64>> {
    let modified = fs::metadata(path)?.modified()?;
    let Ok(duration) = modified.duration_since(UNIX_EPOCH) else {
        return Ok(None);
    };
    let nanos = duration.as_nanos();
    Ok(u64::try_from(nanos).ok())
}

fn datetime_unix_ns(timestamp: DateTime<Utc>) -> Option<u64> {
    let seconds = timestamp.timestamp();
    if seconds < 0 {
        return None;
    }
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::from(timestamp.timestamp_subsec_nanos()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn cache_key_sha256(
    scope: &ContextArtifactCacheScope,
    normalized_source_path: &str,
    content_sha256: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(scope.workspace_identity.as_bytes());
    hasher.update(b"\0");
    hasher.update(scope.branch_identity.as_bytes());
    hasher.update(b"\0");
    hasher.update(scope.session_scope.as_bytes());
    hasher.update(b"\0");
    hasher.update(normalized_source_path.as_bytes());
    hasher.update(b"\0");
    hasher.update(content_sha256.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn stable_id(kind: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    for part in parts {
        hasher.update(b"\0");
        hasher.update(part.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("swg:{kind}:{}", &digest[..16])
}

fn normalize_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map_or_else(|_| normalize_path(path), normalize_path)
}

fn normalize_path(path: &Path) -> String {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                parts.push(prefix.as_os_str().to_string_lossy().into_owned());
            }
            Component::RootDir => {
                parts.push(String::new());
            }
            Component::CurDir => {}
            Component::ParentDir => parts.push("..".to_string()),
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
        }
    }
    if parts.len() > 1 && parts.first().is_some_and(String::is_empty) {
        format!("/{}", parts[1..].join("/"))
    } else {
        parts.join("/")
    }
}

fn redact_error_message(message: &str) -> String {
    message
        .replace("authorization", "[redacted-keyword]")
        .replace("token", "[redacted-keyword]")
        .replace("secret", "[redacted-keyword]")
}
