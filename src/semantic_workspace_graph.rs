//! Deterministic semantic workspace graph builder.
//!
//! The graph is advisory context only. It indexes workspace facts with
//! freshness and actionability metadata, but it never replaces Beads, Agent
//! Mail, README evidence gates, or validation commands as sources of truth.

#![allow(clippy::missing_const_for_fn, clippy::too_many_lines)]

use chrono::{DateTime, Duration, Utc};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt::{self, Write as _};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

pub const SEMANTIC_WORKSPACE_GRAPH_SCHEMA: &str = "pi.semantic_workspace_graph.v1";
pub const GRAPH_BUILDER_SCHEMA: &str = "pi.semantic_workspace_graph.builder_trace.v1";
pub const SEMANTIC_CONTEXT_BUNDLE_SCHEMA: &str = "pi.semantic_context_bundle.v1";

const DEFAULT_STALE_AFTER_DAYS: i64 = 1;
const RELEASE_FACING_EVIDENCE_STALE_AFTER_DAYS: i64 = 14;
const DEFAULT_CACHE_TTL_SECONDS: u64 = 6 * 60 * 60;
const DEFAULT_CONTEXT_CACHE_TTL_SECONDS: u64 = 15 * 60;
const CONTEXT_PRIVACY_POLICY_VERSION: &str = "pi.context_privacy.v1";

struct DuplicateRejectingJsonValue(Value);

impl<'de> Deserialize<'de> for DuplicateRejectingJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateRejectingJsonVisitor)
    }
}

struct DuplicateRejectingJsonVisitor;

impl<'de> Visitor<'de> for DuplicateRejectingJsonVisitor {
    type Value = DuplicateRejectingJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(DuplicateRejectingJsonValue)
            .ok_or_else(|| E::custom("JSON number must be finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(Value::String(
            value.to_string(),
        )))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<DuplicateRejectingJsonValue>()? {
            values.push(value.0);
        }
        Ok(DuplicateRejectingJsonValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = serde_json::Map::new();
        while let Some(key) = entries.next_key::<String>()? {
            let value = entries.next_value::<DuplicateRejectingJsonValue>()?;
            if object.insert(key, value.0).is_some() {
                return Err(<A::Error as serde::de::Error>::custom(
                    "duplicate JSON object key",
                ));
            }
        }
        Ok(DuplicateRejectingJsonValue(Value::Object(object)))
    }
}

pub(crate) fn parse_json_rejecting_duplicate_keys(content: &str) -> serde_json::Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_str(content);
    let value = DuplicateRejectingJsonValue::deserialize(&mut deserializer)?.0;
    deserializer.end()?;
    Ok(value)
}

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

        match input.surface {
            SourceSurface::RustCodeModules | SourceSurface::IntegrationAndContractTests => {
                let content = String::from_utf8_lossy(&bytes);
                Self::ingest_rust_file(input, &content, &content_sha256, size_bytes, state);
            }
            SourceSurface::ReadmeAndDocs => {
                let content = String::from_utf8_lossy(&bytes);
                Self::ingest_markdown_file(input, &content, &content_sha256, size_bytes, state);
            }
            SourceSurface::EvidenceArtifacts => match std::str::from_utf8(&bytes) {
                Ok(content) => {
                    self.ingest_evidence_file(input, content, &content_sha256, size_bytes, state);
                }
                Err(_) => Self::ingest_invalid_utf8_evidence_file(
                    input,
                    &bytes,
                    &content_sha256,
                    size_bytes,
                    state,
                ),
            },
            SourceSurface::BeadsIssueGraph => {
                let content = String::from_utf8_lossy(&bytes);
                self.ingest_beads_jsonl(input, &content, &content_sha256, size_bytes, state);
            }
            SourceSurface::RuntimeArtifacts => {
                let content = String::from_utf8_lossy(&bytes);
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

        match parse_json_rejecting_duplicate_keys(content) {
            Ok(value) => {
                let redaction = assess_redaction(&input.source_path, content, Some(&value));
                let mut evidence_node = evidence_artifact_node(
                    &input.source_path,
                    &value,
                    content.as_bytes(),
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

    fn ingest_invalid_utf8_evidence_file(
        input: &DiscoveredInput,
        bytes: &[u8],
        content_sha256: &str,
        size_bytes: u64,
        state: &mut GraphBuildState,
    ) {
        let line_count = if bytes.is_empty() {
            0
        } else {
            bytes.split(|byte| *byte == b'\n').count()
        };
        let mut file_node = file_region_node(
            &input.source_path,
            content_sha256,
            size_bytes,
            1,
            line_count,
            input.surface.as_str(),
        );
        file_node.redaction_status = file_node
            .redaction_status
            .max(RedactionStatus::SensitiveOmitted);
        let file_node_id = file_node.id.clone();
        state.push_node(file_node);

        let mut node = missing_or_unreadable_evidence_node(
            &input.source_path,
            EvidenceFreshnessStatus::Malformed,
            "invalid_utf8",
        );
        node.content_sha256 = Some(content_sha256.to_string());
        node.redaction_status = node.redaction_status.max(RedactionStatus::SensitiveOmitted);
        state.push_edge(edge(
            SemanticEdgeType::Tracks,
            &file_node_id,
            &node.id,
            "invalid_utf8_evidence",
        ));
        state.register_evidence_node(&input.source_path, &node.id);
        state.push_node(node);
        state.push_trace(GraphBuildTraceEvent::new(
            input.surface.as_str(),
            input.source_path.clone(),
            GraphInputStatus::Malformed,
            "invalid_utf8",
            1,
            1,
        ));
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

const PERF_BUDGET_SUMMARY_SCHEMA: &str = "pi.perf.budget_summary.v2";
const PERF_BUDGET_SUMMARY_PATH: &str = "tests/perf/reports/budget_summary.json";
const DROPIN_CERTIFICATION_CONTRACT_PATH: &str =
    "docs/contracts/dropin-certification-contract.json";
const DROPIN_CERTIFICATION_CONTRACT_SCHEMA: &str = "pi.dropin.certification_contract.v1";
const DROPIN_CERTIFICATION_VERDICT_PATH: &str = "docs/evidence/dropin-certification-verdict.json";
const DROPIN_CERTIFICATION_VERDICT_SCHEMA: &str = "pi.dropin.certification_verdict.v1";
const DROPIN_CERTIFICATION_LANE_PATH: &str = "tests/full_suite_gate/certification_verdict.json";
const DROPIN_CERTIFICATION_LANE_SCHEMA: &str = "pi.ci.certification_lane.v1";
const DROPIN_MAX_EVIDENCE_AGE_HOURS: i64 = 168;
const DROPIN_CERTIFICATION_LANE_POLICY: &str = "Full certification: all blocking gates must pass for release. Waived gates are tracked but do not block. Expired waivers fail the waiver_lifecycle gate.";
const DROPIN_LANE_TOP_LEVEL_FIELDS: &[&str] = &[
    "schema",
    "lane",
    "generated_at",
    "verdict",
    "policy",
    "gates",
    "waiver_audit",
    "waivers_applied",
    "summary",
    "promotion_rules",
    "rerun_guidance",
];
#[derive(Clone, Copy)]
struct DropinLaneGateIdentity {
    id: &'static str,
    name: &'static str,
    bead: &'static str,
    blocking: bool,
    artifact_path: &'static str,
    reproduce_command: Option<&'static str>,
}

const DROPIN_FULL_LANE_GATES: &[DropinLaneGateIdentity] = &[
    DropinLaneGateIdentity {
        id: "non_mock_unit",
        name: "Non-mock unit compliance",
        bead: "bd-1f42.2.6",
        blocking: true,
        artifact_path: "docs/non-mock-rubric.json",
        reproduce_command: Some("cargo test --test non_mock_compliance_gate -- --nocapture"),
    },
    DropinLaneGateIdentity {
        id: "e2e_log_contract",
        name: "E2E log contract and transcripts",
        bead: "bd-1f42.3.6",
        blocking: false,
        artifact_path: "tests/e2e_results",
        reproduce_command: None,
    },
    DropinLaneGateIdentity {
        id: "ext_must_pass",
        name: "Extension must-pass gate",
        bead: "bd-1f42.4.4",
        blocking: true,
        artifact_path: "tests/ext_conformance/reports/gate/must_pass_gate_verdict.json",
        reproduce_command: Some(
            "cargo test --test ext_conformance_generated --features ext-conformance -- conformance_must_pass_gate --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "ext_provider_compat",
        name: "Extension provider compatibility matrix",
        bead: "bd-1f42.4.6",
        blocking: false,
        artifact_path: "tests/ext_conformance/reports/provider_compat/provider_compat_report.json",
        reproduce_command: Some(
            "cargo test --test ext_conformance_generated --features ext-conformance -- conformance_provider_compat_matrix --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "evidence_bundle",
        name: "Unified evidence bundle",
        bead: "bd-1f42.6.8",
        blocking: false,
        artifact_path: "tests/evidence_bundle/index.json",
        reproduce_command: Some(
            "cargo test --test ci_evidence_bundle -- build_evidence_bundle --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "cross_platform",
        name: "Cross-platform matrix validation",
        bead: "bd-1f42.6.7",
        blocking: true,
        artifact_path: "tests/cross_platform_reports/linux/platform_report.json",
        reproduce_command: Some(
            "cargo test --test ci_cross_platform_matrix -- cross_platform_matrix --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "conformance_regression",
        name: "Conformance regression gate",
        bead: "bd-1f42.4",
        blocking: true,
        artifact_path: "tests/ext_conformance/reports/regression_verdict.json",
        reproduce_command: Some("cargo test --test conformance_regression_gate -- --nocapture"),
    },
    DropinLaneGateIdentity {
        id: "conformance_pass_rate",
        name: "Conformance pass rate >= 80%",
        bead: "bd-1f42.4",
        blocking: true,
        artifact_path: "tests/ext_conformance/reports/conformance_summary.json",
        reproduce_command: Some("cargo test --test conformance_report -- --nocapture"),
    },
    DropinLaneGateIdentity {
        id: "suite_classification",
        name: "Suite classification guard",
        bead: "bd-1f42.6.1",
        blocking: true,
        artifact_path: "tests/suite_classification.toml",
        reproduce_command: None,
    },
    DropinLaneGateIdentity {
        id: "traceability_matrix",
        name: "Requirement traceability matrix",
        bead: "bd-1f42.6.4",
        blocking: false,
        artifact_path: "docs/traceability_matrix.json",
        reproduce_command: None,
    },
    DropinLaneGateIdentity {
        id: "e2e_scenario_matrix",
        name: "Canonical E2E scenario matrix",
        bead: "bd-1f42.8.5.1",
        blocking: false,
        artifact_path: "docs/e2e_scenario_matrix.json",
        reproduce_command: Some("python3 scripts/check_traceability_matrix.py"),
    },
    DropinLaneGateIdentity {
        id: "provider_gap_matrix",
        name: "Provider gap test matrix coverage",
        bead: "bd-3uqg.11.11.5",
        blocking: false,
        artifact_path: "docs/provider-gaps-test-matrix.json",
        reproduce_command: Some(
            "cargo test --test provider_native_contract --test e2e_provider_scenarios -- --nocapture",
        ),
    },
    DropinLaneGateIdentity {
        id: "sec_conformance",
        name: "SEC-6.4 security compatibility conformance",
        bead: "bd-1a2cu",
        blocking: true,
        artifact_path: "tests/full_suite_gate/sec_conformance_verdict.json",
        reproduce_command: Some("cargo test --test sec_compatibility_conformance -- --nocapture"),
    },
    DropinLaneGateIdentity {
        id: "perf3x_bead_coverage",
        name: "PERF-3X bead-to-artifact coverage audit",
        bead: "bd-3ar8v.6.11",
        blocking: true,
        artifact_path: "tests/full_suite_gate/perf3x_bead_coverage_audit.json",
        reproduce_command: Some(
            "cargo test --test ci_full_suite_gate -- perf3x_bead_coverage_contract_is_well_formed --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "practical_finish_checkpoint",
        name: "Practical-finish checkpoint (docs-only residual filter)",
        bead: "bd-3ar8v.6.9",
        blocking: true,
        artifact_path: "tests/full_suite_gate/practical_finish_checkpoint.json",
        reproduce_command: Some(
            "cargo test --test ci_full_suite_gate -- practical_finish_report_fails_when_technical_open_issues_remain --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "extension_remediation_backlog",
        name: "Extension remediation backlog artifact integrity",
        bead: "bd-3ar8v.6.8",
        blocking: true,
        artifact_path: "tests/full_suite_gate/extension_remediation_backlog.json",
        reproduce_command: Some(
            "cargo test --test qa_certification_dossier -- certification_dossier --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "opportunity_matrix_integrity",
        name: "Opportunity matrix artifact integrity",
        bead: "bd-3ar8v.6.1",
        blocking: true,
        artifact_path: "tests/perf/reports/opportunity_matrix.json",
        reproduce_command: Some(
            "cargo test --test release_evidence_gate -- phase1_weighted_attribution_contract_links_phase5_consumers --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "parameter_sweeps_integrity",
        name: "Parameter sweeps artifact integrity",
        bead: "bd-3ar8v.6.2",
        blocking: true,
        artifact_path: "tests/perf/reports/parameter_sweeps.json",
        reproduce_command: Some(
            "cargo test --test release_evidence_gate -- parameter_sweeps_contract_links_phase1_matrix_and_readiness --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "conformance_stress_lineage",
        name: "Conformance+stress lineage coherence",
        bead: "bd-3ar8v.6.3",
        blocking: true,
        artifact_path: "tests/ext_conformance/reports/conformance_summary.json",
        reproduce_command: Some(
            "cargo test --test ci_full_suite_gate -- conformance_stress_lineage_passes_with_valid_artifacts --nocapture --exact",
        ),
    },
    DropinLaneGateIdentity {
        id: "waiver_lifecycle",
        name: "Waiver lifecycle compliance",
        bead: "bd-1f42.8.8.1",
        blocking: true,
        artifact_path: "tests/full_suite_gate/waiver_audit.json",
        reproduce_command: Some(
            "cargo test --test ci_full_suite_gate -- waiver_lifecycle_audit --nocapture --exact",
        ),
    },
];
const DROPIN_VERDICT_REQUIRED_FIELDS: &[&str] = &[
    "git_commit",
    "generated_at_utc",
    "overall_verdict",
    "hard_gate_results",
    "blocking_reasons",
    "evidence_index",
];
const PERF_CANONICAL_BUDGET_INVENTORY_SHA256: &str =
    "96e3147ef23e1c634d56265581975a2b619ac9a701f4839ef6f3f4b3987226ad";
const PERF_TOP_LEVEL_FIELDS: &[&str] = &[
    "schema",
    "generated_at",
    "source_commit",
    "run_id",
    "correlation_id",
    "strict_mode",
    "total_budgets",
    "ci_enforced",
    "ci_with_data",
    "ci_fail",
    "ci_no_data",
    "pass",
    "fail",
    "no_data",
    "data_contract_failures_count",
    "failing_data_contracts",
    "budgets",
    "budget_results",
    "claim_readiness",
];
const PERF_BUDGET_FIELDS: &[&str] = &[
    "name",
    "category",
    "metric",
    "unit",
    "threshold",
    "comparison",
    "methodology",
    "ci_enforced",
];
const PERF_RESULT_REQUIRED_FIELDS: &[&str] = &[
    "budget_name",
    "category",
    "threshold",
    "comparison",
    "unit",
    "actual",
    "status",
    "source",
    "ci_enforced",
];
const PERF_FAILURE_REQUIRED_FIELDS: &[&str] = &["contract_id", "detail", "remediation"];
const PERF_CLAIM_READINESS_FIELDS: &[&str] = &[
    "status",
    "performance_claims_authorized",
    "blocking_reason_codes",
];

#[derive(Debug)]
struct ValidatedPerformanceBudgetClaim {
    source_commit: Option<String>,
    claim_ready: bool,
}

#[derive(Debug)]
struct PerformanceBudgetDefinition {
    category: String,
    unit: String,
    threshold: f64,
    comparison: String,
    ci_enforced: bool,
}

fn performance_exact_object<'a>(
    value: &'a Value,
    required: &[&str],
    optional: &[&str],
    label: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    let missing = required
        .iter()
        .filter(|field| !object.contains_key(**field))
        .copied()
        .collect::<Vec<_>>();
    let unexpected = object
        .keys()
        .filter(|field| !required.contains(&field.as_str()) && !optional.contains(&field.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() && unexpected.is_empty() {
        Ok(object)
    } else {
        Err(format!(
            "{label} fields are not exact (missing={missing:?}, unexpected={unexpected:?})"
        ))
    }
}

fn performance_nonempty_string<'a>(value: &'a Value, label: &str) -> Result<&'a str, String> {
    let raw = value
        .as_str()
        .ok_or_else(|| format!("{label} must be a string"))?;
    if raw.is_empty() || raw.trim() != raw {
        Err(format!(
            "{label} must be non-empty and free of surrounding whitespace"
        ))
    } else {
        Ok(raw)
    }
}

fn performance_uint(value: &Value, label: &str) -> Result<u64, String> {
    value
        .as_u64()
        .filter(|number| *number <= i64::MAX.unsigned_abs())
        .ok_or_else(|| format!("{label} must be a non-negative signed 64-bit integer"))
}

fn performance_finite_number(value: &Value, label: &str, positive: bool) -> Result<f64, String> {
    let number = value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| format!("{label} must be a finite number"))?;
    if positive && number <= 0.0 {
        Err(format!("{label} must be a positive finite number"))
    } else {
        Ok(number)
    }
}

fn performance_nullable_lineage(value: &Value, label: &str) -> Result<Option<String>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let raw = performance_nonempty_string(value, label)?;
    let mut chars = raw.chars();
    let valid_start = chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric());
    let valid_rest =
        chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '/' | '-'));
    if valid_start && valid_rest && raw.len() <= 256 {
        Ok(Some(raw.to_string()))
    } else {
        Err(format!("{label} must be a canonical lineage identifier"))
    }
}

fn performance_source_commit(value: &Value) -> Result<Option<String>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let raw = performance_nonempty_string(value, "source_commit")?;
    if matches!(raw.len(), 40 | 64)
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(Some(raw.to_string()))
    } else {
        Err("source_commit must be null or a canonical full lowercase Git object ID".to_string())
    }
}

fn performance_generated_at(value: &Value) -> Result<DateTime<Utc>, String> {
    let raw = performance_nonempty_string(value, "generated_at")?;
    let bytes = raw.as_bytes();
    let millisecond_utc_shape = bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if !millisecond_utc_shape {
        return Err(
            "generated_at must use canonical millisecond-precision UTC RFC3339".to_string(),
        );
    }
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map_err(|error| format!("generated_at is not valid RFC3339: {error}"))?;
    if parsed.offset().local_minus_utc() != 0
        || parsed.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) != raw
    {
        return Err(
            "generated_at must use canonical millisecond-precision UTC RFC3339".to_string(),
        );
    }
    Ok(parsed.with_timezone(&Utc))
}

fn performance_budget_inventory_sha256(budgets: &[Value]) -> Result<String, String> {
    let mut canonical = String::from("[");
    for (index, budget) in budgets.iter().enumerate() {
        let label = format!("budgets[{index}]");
        let object = budget
            .as_object()
            .ok_or_else(|| format!("{label} must be an object"))?;
        if index != 0 {
            canonical.push(',');
        }
        let name = serde_json::to_string(performance_nonempty_string(
            &object["name"],
            &format!("{label}.name"),
        )?)
        .map_err(|error| format!("failed to serialize {label}.name: {error}"))?;
        let category = serde_json::to_string(performance_nonempty_string(
            &object["category"],
            &format!("{label}.category"),
        )?)
        .map_err(|error| format!("failed to serialize {label}.category: {error}"))?;
        let metric = serde_json::to_string(performance_nonempty_string(
            &object["metric"],
            &format!("{label}.metric"),
        )?)
        .map_err(|error| format!("failed to serialize {label}.metric: {error}"))?;
        let unit = serde_json::to_string(performance_nonempty_string(
            &object["unit"],
            &format!("{label}.unit"),
        )?)
        .map_err(|error| format!("failed to serialize {label}.unit: {error}"))?;
        let threshold =
            performance_finite_number(&object["threshold"], &format!("{label}.threshold"), true)?;
        let rounded_threshold = (threshold * 1_000_000.0).round() / 1_000_000.0;
        if threshold.total_cmp(&rounded_threshold).is_ne() {
            return Err(format!(
                "{label}.threshold exceeds canonical six-decimal precision"
            ));
        }
        let comparison = serde_json::to_string(performance_nonempty_string(
            &object["comparison"],
            &format!("{label}.comparison"),
        )?)
        .map_err(|error| format!("failed to serialize {label}.comparison: {error}"))?;
        let ci_enforced = object["ci_enforced"]
            .as_bool()
            .ok_or_else(|| format!("{label}.ci_enforced must be a boolean"))?;
        let methodology = serde_json::to_string(performance_nonempty_string(
            &object["methodology"],
            &format!("{label}.methodology"),
        )?)
        .map_err(|error| format!("failed to serialize {label}.methodology: {error}"))?;
        write!(
            canonical,
            "{{\"name\":{name},\"category\":{category},\"metric\":{metric},\"unit\":{unit},\"threshold\":{threshold:.6},\"comparison\":{comparison},\"ci_enforced\":{ci_enforced},\"methodology\":{methodology}}}"
        )
        .map_err(|error| format!("failed to serialize canonical budget inventory: {error}"))?;
    }
    canonical.push(']');
    Ok(format!("{:x}", Sha256::digest(canonical.as_bytes())))
}

fn validate_performance_budget_contract(
    value: &Value,
) -> Result<ValidatedPerformanceBudgetClaim, String> {
    let top = performance_exact_object(value, PERF_TOP_LEVEL_FIELDS, &[], "performance summary")?;
    if top.get("schema").and_then(Value::as_str) != Some(PERF_BUDGET_SUMMARY_SCHEMA) {
        return Err(format!(
            "schema must be {PERF_BUDGET_SUMMARY_SCHEMA}, found {:?}",
            top.get("schema")
        ));
    }
    performance_generated_at(&top["generated_at"])?;
    let source_commit = performance_source_commit(&top["source_commit"])?;
    let run_id = performance_nullable_lineage(&top["run_id"], "run_id")?;
    let correlation_id = performance_nullable_lineage(&top["correlation_id"], "correlation_id")?;
    if run_id != correlation_id {
        return Err("run_id and correlation_id must both be null or match".to_string());
    }
    let strict_mode = top["strict_mode"]
        .as_bool()
        .ok_or_else(|| "strict_mode must be a boolean".to_string())?;

    let count_names = [
        "total_budgets",
        "ci_enforced",
        "ci_with_data",
        "ci_fail",
        "ci_no_data",
        "pass",
        "fail",
        "no_data",
        "data_contract_failures_count",
    ];
    let mut counts = BTreeMap::new();
    for name in count_names {
        counts.insert(name, performance_uint(&top[name], name)?);
    }

    let budgets = top["budgets"]
        .as_array()
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| "budgets must be a non-empty array".to_string())?;
    let results = top["budget_results"]
        .as_array()
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| "budget_results must be a non-empty array".to_string())?;
    let failures = top["failing_data_contracts"]
        .as_array()
        .ok_or_else(|| "failing_data_contracts must be an array".to_string())?;

    let mut definitions = BTreeMap::new();
    let mut definition_order = Vec::with_capacity(budgets.len());
    for (index, budget) in budgets.iter().enumerate() {
        let label = format!("budgets[{index}]");
        let object = performance_exact_object(budget, PERF_BUDGET_FIELDS, &[], &label)?;
        let name = performance_nonempty_string(&object["name"], &format!("{label}.name"))?;
        for field in ["category", "metric", "unit", "methodology"] {
            performance_nonempty_string(&object[field], &format!("{label}.{field}"))?;
        }
        let comparison = match performance_nonempty_string(
            &object["comparison"],
            &format!("{label}.comparison"),
        )? {
            comparison @ ("maximum" | "minimum") => comparison,
            comparison => {
                return Err(format!(
                    "{label}.comparison has unsupported value {comparison:?}"
                ));
            }
        };
        let definition = PerformanceBudgetDefinition {
            category: performance_nonempty_string(
                &object["category"],
                &format!("{label}.category"),
            )?
            .to_string(),
            unit: performance_nonempty_string(&object["unit"], &format!("{label}.unit"))?
                .to_string(),
            threshold: performance_finite_number(
                &object["threshold"],
                &format!("{label}.threshold"),
                true,
            )?,
            comparison: comparison.to_string(),
            ci_enforced: object["ci_enforced"]
                .as_bool()
                .ok_or_else(|| format!("{label}.ci_enforced must be a boolean"))?,
        };
        if definitions.insert(name.to_string(), definition).is_some() {
            return Err(format!("duplicate budget name: {name}"));
        }
        definition_order.push(name.to_string());
    }
    let inventory_sha256 = performance_budget_inventory_sha256(budgets)?;
    if inventory_sha256 != PERF_CANONICAL_BUDGET_INVENTORY_SHA256 {
        return Err(format!(
            "budget inventory does not match the canonical producer contract (observed_sha256={inventory_sha256}, expected_sha256={PERF_CANONICAL_BUDGET_INVENTORY_SHA256})"
        ));
    }

    let mut result_names = BTreeSet::new();
    let mut result_order = Vec::with_capacity(results.len());
    let mut pass_count = 0usize;
    let mut fail_count = 0usize;
    let mut no_data_count = 0usize;
    let mut ci_with_data = 0usize;
    let mut ci_fail = 0usize;
    let mut ci_no_data = 0usize;
    for (index, result) in results.iter().enumerate() {
        let label = format!("budget_results[{index}]");
        let object = performance_exact_object(
            result,
            PERF_RESULT_REQUIRED_FIELDS,
            &["failure_reason"],
            &label,
        )?;
        let name =
            performance_nonempty_string(&object["budget_name"], &format!("{label}.budget_name"))?;
        if !result_names.insert(name.to_string()) {
            return Err(format!("duplicate budget result: {name}"));
        }
        result_order.push(name.to_string());
        let definition = definitions
            .get(name)
            .ok_or_else(|| format!("budget result has no matching definition: {name}"))?;
        let category =
            performance_nonempty_string(&object["category"], &format!("{label}.category"))?;
        let unit = performance_nonempty_string(&object["unit"], &format!("{label}.unit"))?;
        let comparison =
            performance_nonempty_string(&object["comparison"], &format!("{label}.comparison"))?;
        let threshold =
            performance_finite_number(&object["threshold"], &format!("{label}.threshold"), true)?;
        let ci_enforced = object["ci_enforced"]
            .as_bool()
            .ok_or_else(|| format!("{label}.ci_enforced must be a boolean"))?;
        if category != definition.category
            || unit != definition.unit
            || comparison != definition.comparison
            || threshold.total_cmp(&definition.threshold).is_ne()
            || ci_enforced != definition.ci_enforced
        {
            return Err(format!(
                "budget result {name} does not match its category/unit/threshold/CI definition"
            ));
        }
        performance_nonempty_string(&object["source"], &format!("{label}.source"))?;

        let status = object["status"]
            .as_str()
            .ok_or_else(|| format!("{label}.status must be a string"))?;
        if !matches!(status, "PASS" | "FAIL" | "NO_DATA") {
            return Err(format!(
                "budget result {name} has unsupported status: {status}"
            ));
        }
        let failure_reason = object.get("failure_reason");
        if let Some(reason) = failure_reason {
            performance_nonempty_string(reason, &format!("{label}.failure_reason"))?;
        }
        if object["actual"].is_null() {
            if strict_mode && definition.ci_enforced {
                if status != "FAIL"
                    || failure_reason.and_then(Value::as_str) != Some("missing_measurement_data")
                {
                    return Err(format!(
                        "strict CI budget {name} without data must be FAIL with failure_reason=missing_measurement_data"
                    ));
                }
            } else if status != "NO_DATA" || failure_reason.is_some() {
                return Err(format!(
                    "budget {name} without data must be NO_DATA without a failure reason"
                ));
            }
        } else {
            let actual =
                performance_finite_number(&object["actual"], &format!("{label}.actual"), false)?;
            if actual < 0.0 {
                return Err(format!("{label}.actual must be non-negative"));
            }
            let passes = if definition.comparison == "minimum" {
                actual >= threshold
            } else {
                actual <= threshold
            };
            let expected_status = if passes { "PASS" } else { "FAIL" };
            if status != expected_status || failure_reason.is_some() {
                return Err(format!(
                    "budget result {name} is inconsistent with actual={actual}, threshold={threshold}, and expected status={expected_status}"
                ));
            }
        }

        match status {
            "PASS" => pass_count += 1,
            "FAIL" => fail_count += 1,
            "NO_DATA" => no_data_count += 1,
            _ => unreachable!("performance status validated above"),
        }
        if definition.ci_enforced {
            ci_with_data += usize::from(!object["actual"].is_null());
            ci_fail += usize::from(status == "FAIL");
            ci_no_data += usize::from(status == "NO_DATA");
        }
    }
    let definition_names = definitions.keys().cloned().collect::<BTreeSet<_>>();
    if result_names != definition_names || result_order != definition_order {
        return Err(
            "budget_results must match canonical budget declaration order and membership"
                .to_string(),
        );
    }

    let mut failure_fingerprints = BTreeSet::new();
    for (index, failure) in failures.iter().enumerate() {
        let label = format!("failing_data_contracts[{index}]");
        let object = performance_exact_object(
            failure,
            PERF_FAILURE_REQUIRED_FIELDS,
            &["budget_name"],
            &label,
        )?;
        let contract_id =
            performance_nonempty_string(&object["contract_id"], &format!("{label}.contract_id"))?;
        let detail = performance_nonempty_string(&object["detail"], &format!("{label}.detail"))?;
        let remediation =
            performance_nonempty_string(&object["remediation"], &format!("{label}.remediation"))?;
        let budget_name = match object.get("budget_name") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let name = performance_nonempty_string(value, &format!("{label}.budget_name"))?;
                if !definitions.contains_key(name) {
                    return Err(format!(
                        "data-contract failure references unknown budget: {name}"
                    ));
                }
                Some(name.to_string())
            }
        };
        if !failure_fingerprints.insert((
            contract_id.to_string(),
            detail.to_string(),
            remediation.to_string(),
            budget_name,
        )) {
            return Err(format!("duplicate data-contract failure at index {index}"));
        }
    }

    let ci_enforced_count = definitions
        .values()
        .filter(|definition| definition.ci_enforced)
        .count();
    let derived_counts = [
        ("total_budgets", budgets.len()),
        ("ci_enforced", ci_enforced_count),
        ("ci_with_data", ci_with_data),
        ("ci_fail", ci_fail),
        ("ci_no_data", ci_no_data),
        ("pass", pass_count),
        ("fail", fail_count),
        ("no_data", no_data_count),
        ("data_contract_failures_count", failures.len()),
    ];
    for (name, expected) in derived_counts {
        let expected = u64::try_from(expected)
            .map_err(|_| format!("derived {name} exceeds the supported count range"))?;
        if counts[name] != expected {
            return Err(format!(
                "{name}={} is inconsistent with derived value {expected}",
                counts[name]
            ));
        }
    }
    if counts["pass"]
        .checked_add(counts["fail"])
        .and_then(|count| count.checked_add(counts["no_data"]))
        != Some(counts["total_budgets"])
    {
        return Err("pass + fail + no_data must equal total_budgets".to_string());
    }

    let claim = performance_exact_object(
        &top["claim_readiness"],
        PERF_CLAIM_READINESS_FIELDS,
        &[],
        "claim_readiness",
    )?;
    let reasons = claim["blocking_reason_codes"]
        .as_array()
        .ok_or_else(|| "claim_readiness.blocking_reason_codes must be an array".to_string())?;
    let reported_reasons = reasons
        .iter()
        .enumerate()
        .map(|(index, reason)| {
            performance_nonempty_string(
                reason,
                &format!("claim_readiness.blocking_reason_codes[{index}]"),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !reported_reasons.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(
            "claim_readiness.blocking_reason_codes must be sorted and duplicate-free".to_string(),
        );
    }

    let mut expected_reasons = BTreeSet::new();
    if counts["no_data"] != 0 {
        expected_reasons.insert("budget_data_missing");
    }
    if counts["fail"] != 0 {
        expected_reasons.insert("budget_failed");
    }
    if counts["ci_with_data"] != counts["ci_enforced"] || counts["ci_no_data"] != 0 {
        expected_reasons.insert("ci_budget_data_missing");
    }
    if counts["ci_fail"] != 0 {
        expected_reasons.insert("ci_budget_failed");
    }
    if correlation_id.is_none() {
        expected_reasons.insert("correlation_id_missing");
    }
    if counts["data_contract_failures_count"] != 0 {
        expected_reasons.insert("data_contract_failure");
    }
    if run_id.is_none() {
        expected_reasons.insert("run_id_missing");
    }
    if source_commit.is_none() {
        expected_reasons.insert("source_commit_unbound");
    }
    if !strict_mode {
        expected_reasons.insert("strict_mode_disabled");
    }
    let expected_reasons = expected_reasons.into_iter().collect::<Vec<_>>();
    if reported_reasons != expected_reasons {
        return Err(format!(
            "claim_readiness blockers disagree with derived blockers (reported={reported_reasons:?}, expected={expected_reasons:?})"
        ));
    }
    let claim_ready = expected_reasons.is_empty();
    let expected_status = if claim_ready {
        "claim_ready"
    } else {
        "blocked"
    };
    if claim["status"].as_str() != Some(expected_status)
        || claim["performance_claims_authorized"].as_bool() != Some(claim_ready)
    {
        return Err(
            "claim_readiness status or authorization contradicts derived blockers".to_string(),
        );
    }

    Ok(ValidatedPerformanceBudgetClaim {
        source_commit,
        claim_ready,
    })
}

fn classify_performance_budget_claim(
    value: &Value,
    options: &SemanticWorkspaceGraphBuildOptions,
    repository_root: Option<&Path>,
    artifact_source_path: Option<&str>,
    captured_artifact_bytes: Option<&[u8]>,
    canonical_path: bool,
) -> Option<(EvidenceFreshnessStatus, bool, String)> {
    let schema = value.get("schema").and_then(Value::as_str);
    if schema == Some("pi.perf.budget_summary.v1") {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_schema_not_current".to_string(),
        ));
    }
    if schema != Some(PERF_BUDGET_SUMMARY_SCHEMA) {
        return canonical_path.then(|| {
            (
                EvidenceFreshnessStatus::Malformed,
                false,
                "performance_budget_schema_not_current".to_string(),
            )
        });
    }

    let Some(Ok(generated_at)) = value.get("generated_at").map(performance_generated_at) else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    if options.reference_time_utc.is_some_and(|reference| {
        generated_at.signed_duration_since(reference) > Duration::minutes(5)
    }) {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_generated_at_in_future".to_string(),
        ));
    }

    let Ok(validated) = validate_performance_budget_contract(value) else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    if options.reference_time_utc.is_some_and(|reference_time| {
        evidence_age_exceeds_policy(
            value,
            options,
            artifact_source_path,
            generated_at,
            reference_time,
        )
    }) {
        return Some((
            EvidenceFreshnessStatus::Stale,
            false,
            "generated_at_older_than_policy".to_string(),
        ));
    }
    if !validated.claim_ready {
        return Some((
            EvidenceFreshnessStatus::Uncertified,
            false,
            "performance_claims_not_authorized".to_string(),
        ));
    }
    let Some(source_commit) = validated.source_commit.as_deref() else {
        return Some((
            EvidenceFreshnessStatus::Malformed,
            false,
            "performance_budget_claim_readiness_malformed".to_string(),
        ));
    };
    match performance_source_binding_failure(
        repository_root,
        artifact_source_path,
        source_commit,
        captured_artifact_bytes,
    ) {
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
}

fn evidence_age_exceeds_policy(
    value: &Value,
    options: &SemanticWorkspaceGraphBuildOptions,
    artifact_source_path: Option<&str>,
    generated_at: DateTime<Utc>,
    reference_time: DateTime<Utc>,
) -> bool {
    let evidence_age = reference_time.signed_duration_since(generated_at);
    if matches!(
        artifact_source_path,
        Some(DROPIN_CERTIFICATION_VERDICT_PATH | PERF_BUDGET_SUMMARY_PATH)
    ) {
        evidence_age > Duration::hours(DROPIN_MAX_EVIDENCE_AGE_HOURS)
    } else if value.get("claim_surface").and_then(Value::as_str) == Some("release_facing") {
        evidence_age > Duration::days(RELEASE_FACING_EVIDENCE_STALE_AFTER_DAYS)
    } else {
        Duration::try_days(options.stale_after_days)
            .is_none_or(|stale_after| evidence_age > stale_after)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PerformanceSourceBindingFailure {
    Unavailable,
    Invalid(&'static str),
}

#[derive(Debug)]
struct RepositoryGitContext {
    worktree: PathBuf,
    git_dir: PathBuf,
    git_executable: PathBuf,
}

#[cfg(unix)]
fn trusted_git_executable() -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let mut candidates = vec![
        PathBuf::from("/usr/bin/git"),
        PathBuf::from("/usr/local/bin/git"),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&path)
                .filter(|directory| directory.is_absolute())
                .map(|directory| directory.join("git")),
        );
    }

    let mut seen = BTreeSet::new();
    for candidate in candidates {
        let Ok(canonical) = fs::canonicalize(candidate) else {
            continue;
        };
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&canonical) else {
            continue;
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.permissions().mode() & 0o111 == 0
            || metadata.permissions().mode() & 0o022 != 0
        {
            continue;
        }
        let trusted_ancestors = canonical.ancestors().skip(1).all(|ancestor| {
            fs::symlink_metadata(ancestor).is_ok_and(|ancestor_metadata| {
                ancestor_metadata.is_dir()
                    && !ancestor_metadata.file_type().is_symlink()
                    && ancestor_metadata.uid() == 0
                    && ancestor_metadata.permissions().mode() & 0o022 == 0
            })
        });
        if trusted_ancestors {
            return Some(canonical);
        }
    }
    None
}

#[cfg(windows)]
fn trusted_git_executable() -> Option<PathBuf> {
    [
        r"C:\Program Files\Git\cmd\git.exe",
        r"C:\Program Files\Git\bin\git.exe",
    ]
    .into_iter()
    .find_map(|candidate| {
        let canonical = fs::canonicalize(candidate).ok()?;
        let metadata = fs::symlink_metadata(&canonical).ok()?;
        (metadata.is_file() && !metadata.file_type().is_symlink()).then_some(canonical)
    })
}

#[cfg(not(any(unix, windows)))]
fn trusted_git_executable() -> Option<PathBuf> {
    None
}

fn canonical_real_directory(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut lexical = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => lexical.push(prefix.as_os_str()),
            Component::RootDir => lexical.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // An absolute filesystem root is its own parent.
                lexical.pop();
            }
            Component::Normal(segment) => {
                lexical.push(segment);
                let metadata = fs::symlink_metadata(&lexical).ok()?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return None;
                }
            }
        }
    }
    fs::canonicalize(lexical).ok()
}

fn repository_git_context(repository_root: &Path) -> Option<RepositoryGitContext> {
    let worktree = canonical_real_directory(repository_root)?;
    let git_executable = trusted_git_executable()?;
    let git_marker = worktree.join(".git");
    let marker_metadata = fs::symlink_metadata(&git_marker).ok()?;
    if marker_metadata.file_type().is_symlink() {
        return None;
    }
    let git_dir = if marker_metadata.is_dir() {
        fs::canonicalize(&git_marker).ok()?
    } else if marker_metadata.is_file() {
        let marker = fs::read_to_string(&git_marker).ok()?;
        let target = marker
            .trim_end_matches(['\r', '\n'])
            .strip_prefix("gitdir: ")?;
        if target.is_empty() || target.contains('\0') || target.lines().count() != 1 {
            return None;
        }
        let target = Path::new(target);
        let candidate = if target.is_absolute() {
            target.to_path_buf()
        } else {
            worktree.join(target)
        };
        let candidate_metadata = fs::symlink_metadata(&candidate).ok()?;
        if candidate_metadata.file_type().is_symlink() || !candidate_metadata.is_dir() {
            return None;
        }
        fs::canonicalize(candidate).ok()?
    } else {
        return None;
    };
    let git_dir_metadata = fs::symlink_metadata(&git_dir).ok()?;
    let head_metadata = fs::symlink_metadata(git_dir.join("HEAD")).ok()?;
    (git_dir_metadata.is_dir()
        && !git_dir_metadata.file_type().is_symlink()
        && head_metadata.is_file()
        && !head_metadata.file_type().is_symlink())
    .then_some(RepositoryGitContext {
        worktree,
        git_dir,
        git_executable,
    })
}

fn repository_git_command(context: &RepositoryGitContext) -> Command {
    let mut command = Command::new(&context.git_executable);
    command
        .arg("--git-dir")
        .arg(&context.git_dir)
        .arg("--work-tree")
        .arg(&context.worktree)
        .args(["-c", "core.bare=false", "-c", "core.fsmonitor=false"])
        .arg("-c")
        .arg(format!("core.worktree={}", context.worktree.display()));
    for (variable, _) in std::env::vars_os() {
        if variable
            .to_string_lossy()
            .as_bytes()
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"GIT_"))
        {
            command.env_remove(variable);
        }
    }
    command.env("GIT_LITERAL_PATHSPECS", "1");
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    command.env(
        "GIT_CONFIG_GLOBAL",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_OPTIONAL_LOCKS", "0");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn git_output(context: &RepositoryGitContext, args: &[&str]) -> Option<Vec<u8>> {
    let output = repository_git_command(context).args(args).output().ok()?;
    output.status.success().then_some(output.stdout)
}

fn git_stdout(context: &RepositoryGitContext, args: &[&str]) -> Option<String> {
    String::from_utf8(git_output(context, args)?)
        .ok()
        .map(|stdout| stdout.trim().to_string())
}

type GitRecordFields<'a> = (&'a [u8], &'a [u8], &'a [u8], &'a [u8]);

fn parse_canonical_git_record(record: &[u8]) -> Option<GitRecordFields<'_>> {
    let tab = record.iter().position(|byte| *byte == b'\t')?;
    let path = &record[tab + 1..];
    if path.is_empty() {
        return None;
    }
    let mut fields = record[..tab].split(|byte| *byte == b' ');
    let first = fields.next().filter(|field| !field.is_empty())?;
    let second = fields.next().filter(|field| !field.is_empty())?;
    let third = fields.next().filter(|field| !field.is_empty())?;
    fields
        .next()
        .is_none()
        .then_some((first, second, third, path))
}

fn canonical_nul_records(output: &[u8]) -> Option<Vec<&[u8]>> {
    if output.is_empty() {
        return Some(Vec::new());
    }
    let body = output.strip_suffix(&[0])?;
    if body.is_empty() {
        return None;
    }
    let records = body.split(|byte| *byte == 0).collect::<Vec<_>>();
    records
        .iter()
        .all(|record| !record.is_empty())
        .then_some(records)
}

#[cfg(unix)]
fn git_path_from_bytes(path: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    PathBuf::from(OsStr::from_bytes(path))
}

#[cfg(not(unix))]
fn git_path_from_bytes(path: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(path).ok().map(PathBuf::from)
}

#[cfg(unix)]
fn symlink_target_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt as _;

    fs::read_link(path)
        .ok()
        .map(|target| target.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn symlink_target_bytes(path: &Path) -> Option<Vec<u8>> {
    fs::read_link(path)
        .ok()?
        .to_str()
        .map(|target| target.as_bytes().to_vec())
}

fn git_blob_oid(object_format: &str, bytes: &[u8]) -> Option<String> {
    let header = format!("blob {}\0", bytes.len());
    match object_format {
        "sha1" => {
            let mut hasher = Sha1::new();
            hasher.update(header.as_bytes());
            hasher.update(bytes);
            Some(format!("{:x}", hasher.finalize()))
        }
        "sha256" => {
            let mut hasher = Sha256::new();
            hasher.update(header.as_bytes());
            hasher.update(bytes);
            Some(format!("{:x}", hasher.finalize()))
        }
        _ => None,
    }
}

fn tracked_worktree_blob(root: &Path, relative: &Path, expected_mode: &[u8]) -> Option<Vec<u8>> {
    if relative.is_absolute()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    let components = relative.components().collect::<Vec<_>>();
    let mut path = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return None;
        };
        path.push(name);
        let metadata = fs::symlink_metadata(&path).ok()?;
        if index + 1 != components.len() {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return None;
            }
            continue;
        }
        return match expected_mode {
            b"120000" if metadata.file_type().is_symlink() => symlink_target_bytes(&path),
            b"100644" | b"100755" if metadata.is_file() && !metadata.file_type().is_symlink() => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;

                    let executable = metadata.permissions().mode() & 0o111 != 0;
                    if executable != (expected_mode == b"100755") {
                        return None;
                    }
                }
                fs::read(&path).ok()
            }
            _ => None,
        };
    }
    None
}

fn performance_tracked_head_state_failure(
    context: &RepositoryGitContext,
    expected_head: &str,
) -> Option<PerformanceSourceBindingFailure> {
    let Some(object_format) = git_stdout(context, &["rev-parse", "--show-object-format"]) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if !matches!(object_format.as_str(), "sha1" | "sha256") {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    }
    let Some(index_output) = git_output(context, &["ls-files", "--stage", "-z"]) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(index_records) = canonical_nul_records(&index_output) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let mut index_entries = BTreeMap::new();
    for record in index_records {
        let Some((mode, oid, stage, path)) = parse_canonical_git_record(record) else {
            return Some(PerformanceSourceBindingFailure::Unavailable);
        };
        if stage != b"0"
            || index_entries
                .insert(path.to_vec(), (mode.to_vec(), oid.to_vec()))
                .is_some()
        {
            return Some(PerformanceSourceBindingFailure::Invalid(
                "performance_budget_repository_tracked_state_not_head",
            ));
        }
    }

    let Some(tree_output) = git_output(
        context,
        &["ls-tree", "-r", "-z", "--full-tree", expected_head],
    ) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(tree_records) = canonical_nul_records(&tree_output) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    for record in tree_records {
        let Some((mode, object_type, oid, path)) = parse_canonical_git_record(record) else {
            return Some(PerformanceSourceBindingFailure::Unavailable);
        };
        let Some((index_mode, index_oid)) = index_entries.remove(path) else {
            return Some(PerformanceSourceBindingFailure::Invalid(
                "performance_budget_repository_tracked_state_not_head",
            ));
        };
        if object_type != b"blob" || index_mode != mode || index_oid != oid {
            return Some(PerformanceSourceBindingFailure::Invalid(
                "performance_budget_repository_tracked_state_not_head",
            ));
        }
        #[cfg(unix)]
        let relative = git_path_from_bytes(path);
        #[cfg(not(unix))]
        let Some(relative) = git_path_from_bytes(path) else {
            return Some(PerformanceSourceBindingFailure::Unavailable);
        };
        let Some(bytes) = tracked_worktree_blob(&context.worktree, &relative, mode) else {
            return Some(PerformanceSourceBindingFailure::Invalid(
                "performance_budget_repository_tracked_state_not_head",
            ));
        };
        if git_blob_oid(&object_format, &bytes).as_deref() != std::str::from_utf8(oid).ok() {
            return Some(PerformanceSourceBindingFailure::Invalid(
                "performance_budget_repository_tracked_state_not_head",
            ));
        }
    }
    (!index_entries.is_empty()).then_some(PerformanceSourceBindingFailure::Invalid(
        "performance_budget_repository_tracked_state_not_head",
    ))
}

fn performance_repository_state_failure(
    context: &RepositoryGitContext,
    expected_head: &str,
) -> Option<PerformanceSourceBindingFailure> {
    let Some(top_level) = git_stdout(context, &["rev-parse", "--show-toplevel"]) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if fs::canonicalize(top_level).ok().as_deref() != Some(context.worktree.as_path()) {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    }
    let Some(index_entries) = git_output(context, &["ls-files", "-v", "-z"]) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(index_entries) = canonical_nul_records(&index_entries) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if index_entries.iter().any(|entry| {
        entry.len() < 3 || entry[1] != b' ' || entry[2..].is_empty() || entry[0] != b'H'
    }) {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_repository_index_flags_not_default",
        ));
    }
    let Some(status) = git_output(
        context,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--no-renames",
        ],
    ) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if !status.is_empty() {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_repository_not_clean",
        ));
    }
    performance_tracked_head_state_failure(context, expected_head)
}

fn performance_repository_end_state_failure(
    context: &RepositoryGitContext,
    expected_head: &str,
) -> Option<PerformanceSourceBindingFailure> {
    let Some(current_head) = git_stdout(context, &["rev-parse", "--verify", "HEAD^{commit}"])
    else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if current_head != expected_head {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_repository_head_changed",
        ));
    }
    performance_repository_state_failure(context, expected_head)
}

fn performance_artifact_end_state_failure(
    context: &RepositoryGitContext,
    expected_head: &str,
    artifact_path: &Path,
    captured_artifact_bytes: &[u8],
) -> Option<PerformanceSourceBindingFailure> {
    if let Some(failure) = performance_repository_end_state_failure(context, expected_head) {
        return Some(failure);
    }
    match fs::read(artifact_path) {
        Ok(bytes) if bytes == captured_artifact_bytes => None,
        Ok(_) => Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_changed_during_validation",
        )),
        Err(_) => Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_unreadable",
        )),
    }
}

fn performance_artifact_relative_path(source_path: &str) -> Option<&Path> {
    let path = Path::new(source_path);
    (!source_path.is_empty()
        && !source_path.contains('\\')
        && !source_path.contains('\0')
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_))))
    .then_some(path)
}

fn toml_line_without_comment(line: &str) -> Option<&str> {
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in line.bytes().enumerate() {
        match quote {
            Some(b'"') if escaped => escaped = false,
            Some(b'"') if byte == b'\\' => escaped = true,
            Some(delimiter) if byte == delimiter => quote = None,
            None if matches!(byte, b'"' | b'\'') => quote = Some(byte),
            None if byte == b'#' => return Some(&line[..index]),
            Some(_) | None => {}
        }
    }
    quote.is_none().then_some(line)
}

fn toml_string_array_is_complete(raw: &str) -> Option<bool> {
    let mut quote = None;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut opened = false;
    for byte in raw.bytes() {
        match quote {
            Some(b'"') if escaped => escaped = false,
            Some(b'"') if byte == b'\\' => escaped = true,
            Some(delimiter) if byte == delimiter => quote = None,
            None if matches!(byte, b'"' | b'\'') => quote = Some(byte),
            None if byte == b'[' => {
                opened = true;
                depth = depth.checked_add(1)?;
            }
            None if byte == b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(opened);
                }
            }
            Some(_) | None => {}
        }
    }
    (quote.is_none() && opened).then_some(false)
}

fn parse_toml_string_array(raw: &str) -> Option<Vec<String>> {
    let bytes = raw.as_bytes();
    let mut index = 0usize;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'[') {
        return None;
    }
    index += 1;
    let mut values = Vec::new();
    loop {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes.get(index) == Some(&b']') {
            index += 1;
            return bytes[index..]
                .iter()
                .all(u8::is_ascii_whitespace)
                .then_some(values);
        }
        let delimiter = *bytes.get(index)?;
        if !matches!(delimiter, b'"' | b'\'') {
            return None;
        }
        let start = index;
        index += 1;
        let content_start = index;
        let mut escaped = false;
        loop {
            let byte = *bytes.get(index)?;
            if delimiter == b'"' && escaped {
                escaped = false;
            } else if delimiter == b'"' && byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                break;
            }
            index += 1;
        }
        let value = if delimiter == b'"' {
            serde_json::from_str::<String>(&raw[start..=index]).ok()?
        } else {
            raw[content_start..index].to_string()
        };
        values.push(value);
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        match bytes.get(index) {
            Some(b',') => index += 1,
            Some(b']') => {}
            _ => return None,
        }
    }
}

fn source_package_include_patterns(cargo_toml: &str) -> Option<Vec<String>> {
    let mut in_package = false;
    let mut include_value = None::<String>;
    for raw_line in cargo_toml.lines() {
        let line = toml_line_without_comment(raw_line)?;
        let trimmed = line.trim();
        if let Some(value) = include_value.as_mut() {
            value.push('\n');
            value.push_str(trimmed);
            if toml_string_array_is_complete(value)? {
                return parse_toml_string_array(value);
            }
            continue;
        }
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package || trimmed.is_empty() {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() != "include" {
            continue;
        }
        let value = value.trim();
        if toml_string_array_is_complete(value)? {
            return parse_toml_string_array(value);
        }
        include_value = Some(value.to_string());
    }
    // Cargo's default package policy is broad when `package.include` is
    // absent. Treat docs/evidence as packaged rather than authorizing a
    // post-measurement follow-up whose distribution status was never narrowed.
    Some(vec!["/docs/evidence/**".to_string()])
}

fn performance_path_is_packaged(path: &str, package_patterns: &[String]) -> Option<bool> {
    for raw_pattern in package_patterns {
        if raw_pattern.is_empty() {
            return None;
        }
        let normalized = raw_pattern.strip_prefix('/').unwrap_or(raw_pattern);
        let pattern = glob::Pattern::new(normalized).ok()?;
        if pattern.matches(path)
            || normalized.strip_suffix("/**").is_some_and(|prefix| {
                path.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
            })
        {
            return Some(true);
        }
    }
    Some(false)
}

fn performance_source_binding_failure(
    repository_root: Option<&Path>,
    artifact_source_path: Option<&str>,
    source_commit: &str,
    captured_artifact_bytes: Option<&[u8]>,
) -> Option<PerformanceSourceBindingFailure> {
    let Some(repository_root) = repository_root else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(artifact_source_path) = artifact_source_path else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(captured_artifact_bytes) = captured_artifact_bytes else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(relative_artifact) = performance_artifact_relative_path(artifact_source_path) else {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_path_invalid",
        ));
    };
    let Some(git_context) = repository_git_context(repository_root) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let canonical_root = &git_context.worktree;
    let Some(head) = git_stdout(&git_context, &["rev-parse", "--verify", "HEAD^{commit}"]) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let mut artifact_path = canonical_root.clone();
    for component in relative_artifact.components() {
        let Component::Normal(name) = component else {
            return Some(PerformanceSourceBindingFailure::Invalid(
                "performance_budget_artifact_path_invalid",
            ));
        };
        artifact_path.push(name);
        let Ok(metadata) = fs::symlink_metadata(&artifact_path) else {
            return Some(PerformanceSourceBindingFailure::Invalid(
                "performance_budget_artifact_unreadable",
            ));
        };
        if metadata.file_type().is_symlink() {
            return Some(PerformanceSourceBindingFailure::Invalid(
                "performance_budget_artifact_symlink",
            ));
        }
    }
    if !artifact_path.is_file()
        || fs::canonicalize(&artifact_path)
            .ok()
            .is_none_or(|canonical| !canonical.starts_with(canonical_root))
    {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_path_invalid",
        ));
    }

    let Some(tree_entry) = git_output(
        &git_context,
        &[
            "ls-tree",
            "-z",
            "--full-tree",
            &head,
            "--",
            artifact_source_path,
        ],
    ) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(entries) = canonical_nul_records(&tree_entry) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(entry) = entries
        .as_slice()
        .first()
        .copied()
        .filter(|_| entries.len() == 1)
    else {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_not_tracked_at_head",
        ));
    };
    let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if &entry[tab + 1..] != artifact_source_path.as_bytes() {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_not_tracked_at_head",
        ));
    }
    let Some((mode, object_type, oid, _)) = parse_canonical_git_record(entry) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if !matches!(mode, b"100644" | b"100755") || object_type != b"blob" {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_not_regular_at_head",
        ));
    }
    let Ok(blob_oid) = std::str::from_utf8(oid) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(head_bytes) = git_output(&git_context, &["cat-file", "blob", blob_oid]) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let repository_state_failure = performance_repository_state_failure(&git_context, &head);
    if matches!(
        repository_state_failure,
        Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_repository_tracked_state_not_head"
        ))
    ) {
        return repository_state_failure;
    }
    if captured_artifact_bytes != head_bytes {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_changed_since_ingestion",
        ));
    }
    if let Some(failure) = repository_state_failure {
        return Some(failure);
    }
    let Ok(worktree_bytes) = fs::read(&artifact_path) else {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_unreadable",
        ));
    };
    if worktree_bytes != head_bytes {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_artifact_bytes_do_not_match_head",
        ));
    }

    let source_expression = format!("{source_commit}^{{commit}}");
    let Some(resolved_source) =
        git_stdout(&git_context, &["rev-parse", "--verify", &source_expression])
    else {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_source_commit_unresolvable",
        ));
    };
    if resolved_source != source_commit {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_source_commit_not_exact",
        ));
    }
    let Ok(ancestor) = repository_git_command(&git_context)
        .args(["merge-base", "--is-ancestor", source_commit, &head])
        .output()
    else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if !ancestor.status.success() {
        return Some(if ancestor.status.code() == Some(1) {
            PerformanceSourceBindingFailure::Invalid(
                "performance_budget_source_commit_not_ancestor",
            )
        } else {
            PerformanceSourceBindingFailure::Unavailable
        });
    }
    if source_commit == head {
        return performance_artifact_end_state_failure(
            &git_context,
            &head,
            &artifact_path,
            captured_artifact_bytes,
        );
    }

    let Some(changed_paths) = git_output(
        &git_context,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            source_commit,
            &head,
        ],
    ) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let Some(paths) = canonical_nul_records(&changed_paths) else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    if paths.is_empty() {
        return Some(PerformanceSourceBindingFailure::Invalid(
            "performance_budget_source_commit_not_release_bound",
        ));
    }
    let Some(paths) = paths
        .iter()
        .map(|path| std::str::from_utf8(path).ok())
        .collect::<Option<Vec<_>>>()
    else {
        return Some(PerformanceSourceBindingFailure::Unavailable);
    };
    let package_patterns = if paths.iter().any(|path| path.starts_with("docs/evidence/")) {
        let cargo_expression = format!("{source_commit}:Cargo.toml");
        let Some(cargo_toml) = git_output(&git_context, &["show", &cargo_expression])
            .and_then(|bytes| String::from_utf8(bytes).ok())
        else {
            return Some(PerformanceSourceBindingFailure::Unavailable);
        };
        let Some(patterns) = source_package_include_patterns(&cargo_toml) else {
            return Some(PerformanceSourceBindingFailure::Unavailable);
        };
        patterns
    } else {
        Vec::new()
    };
    let evidence_only = paths.iter().all(|path| {
        let packaged_docs_evidence = path.starts_with("docs/evidence/")
            && performance_path_is_packaged(path, &package_patterns) != Some(false);
        performance_artifact_relative_path(path).is_some()
            && !packaged_docs_evidence
            && [
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
    performance_artifact_end_state_failure(
        &git_context,
        &head,
        &artifact_path,
        captured_artifact_bytes,
    )
}

#[derive(Debug)]
struct DropinGateSpec {
    gate_id: String,
    blocking: bool,
    owner_issue: String,
    required_artifacts: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum DropinClaimFailure {
    Unavailable(&'static str),
    Invalid(&'static str),
}

fn dropin_canonical_repo_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    let invalid_prefix = path.is_empty()
        || path.contains('\\')
        || path.contains('\0')
        || path.starts_with('/')
        || bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    !invalid_prefix
        && path
            .split('/')
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

fn dropin_full_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn dropin_generated_at_is_canonical(value: &Value) -> bool {
    let Some(raw) = value.as_str() else {
        return false;
    };
    let bytes = raw.as_bytes();
    let shape = bytes.len() >= 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes.last() == Some(&b'Z')
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16)
                || index + 1 == bytes.len()
                || (index == 19 && bytes.len() > 20 && *byte == b'.')
                || byte.is_ascii_digit()
        })
        && (bytes.len() == 20 || bytes.len() > 21);
    shape
        && DateTime::parse_from_rfc3339(raw)
            .is_ok_and(|parsed| parsed.offset().local_minus_utc() == 0)
}

fn dropin_gate_id_is_canonical(gate_id: &str, expected_number: usize) -> bool {
    let expected_prefix = format!("G{expected_number:02}-");
    let Some(suffix) = gate_id.strip_prefix(&expected_prefix) else {
        return false;
    };
    !suffix.is_empty()
        && suffix.split('-').all(|component| {
            !component.is_empty()
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
        })
}

fn dropin_contract_gate_specs(contract: &Value) -> Result<Vec<DropinGateSpec>, DropinClaimFailure> {
    let object = contract.as_object().ok_or(DropinClaimFailure::Invalid(
        "dropin_verdict_contract_invalid",
    ))?;
    if object.get("schema").and_then(Value::as_str) != Some(DROPIN_CERTIFICATION_CONTRACT_SCHEMA) {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_contract_invalid",
        ));
    }
    let verdict_contract = object
        .get("release_process_enforcement")
        .and_then(Value::as_object)
        .and_then(|enforcement| enforcement.get("verdict_artifact_contract"))
        .and_then(Value::as_object)
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_contract_invalid",
        ))?;
    if verdict_contract.get("path").and_then(Value::as_str)
        != Some(DROPIN_CERTIFICATION_VERDICT_PATH)
        || verdict_contract.get("schema").and_then(Value::as_str)
            != Some(DROPIN_CERTIFICATION_VERDICT_SCHEMA)
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_contract_invalid",
        ));
    }
    let required_fields = verdict_contract
        .get("required_fields")
        .and_then(Value::as_array)
        .and_then(|fields| fields.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_contract_invalid",
        ))?;
    let expected_fields = DROPIN_VERDICT_REQUIRED_FIELDS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if required_fields.len() != expected_fields.len()
        || required_fields.iter().copied().collect::<BTreeSet<_>>() != expected_fields
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_contract_invalid",
        ));
    }

    let hard_gates = object
        .get("hard_gates")
        .and_then(Value::as_array)
        .filter(|gates| gates.len() == 12)
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_contract_invalid",
        ))?;
    hard_gates
        .iter()
        .enumerate()
        .map(|(index, gate)| {
            let gate = gate.as_object().ok_or(DropinClaimFailure::Invalid(
                "dropin_verdict_contract_invalid",
            ))?;
            let gate_id = gate
                .get("gate_id")
                .and_then(Value::as_str)
                .filter(|gate_id| dropin_gate_id_is_canonical(gate_id, index + 1))
                .ok_or(DropinClaimFailure::Invalid(
                    "dropin_verdict_contract_invalid",
                ))?;
            let blocking = gate.get("blocking").and_then(Value::as_bool).ok_or(
                DropinClaimFailure::Invalid("dropin_verdict_contract_invalid"),
            )?;
            let owner_issue = gate
                .get("owner_issue_primary")
                .and_then(Value::as_str)
                .filter(|owner| !owner.is_empty())
                .ok_or(DropinClaimFailure::Invalid(
                    "dropin_verdict_contract_invalid",
                ))?;
            let required_artifacts = gate
                .get("required_artifacts")
                .and_then(Value::as_array)
                .filter(|artifacts| !artifacts.is_empty())
                .and_then(|artifacts| {
                    artifacts
                        .iter()
                        .map(Value::as_str)
                        .map(|artifact| artifact.filter(|path| dropin_canonical_repo_path(path)))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or(DropinClaimFailure::Invalid(
                    "dropin_verdict_contract_invalid",
                ))?;
            if required_artifacts
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != required_artifacts.len()
            {
                return Err(DropinClaimFailure::Invalid(
                    "dropin_verdict_contract_invalid",
                ));
            }
            Ok(DropinGateSpec {
                gate_id: gate_id.to_string(),
                blocking,
                owner_issue: owner_issue.to_string(),
                required_artifacts: required_artifacts.into_iter().map(str::to_string).collect(),
            })
        })
        .collect()
}

fn dropin_verdict_payload(
    verdict: &Value,
    gate_specs: &[DropinGateSpec],
) -> Result<(String, Vec<String>), DropinClaimFailure> {
    let object = verdict.as_object().ok_or(DropinClaimFailure::Invalid(
        "dropin_verdict_contract_invalid",
    ))?;
    if DROPIN_VERDICT_REQUIRED_FIELDS
        .iter()
        .any(|field| !object.contains_key(*field))
        || object.get("schema").and_then(Value::as_str) != Some(DROPIN_CERTIFICATION_VERDICT_SCHEMA)
        || object.get("overall_verdict").and_then(Value::as_str) != Some("CERTIFIED")
        || !object
            .get("generated_at_utc")
            .is_some_and(dropin_generated_at_is_canonical)
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_contract_invalid",
        ));
    }

    let source_commit = object
        .get("git_commit")
        .and_then(Value::as_str)
        .filter(|commit| dropin_full_git_oid(commit))
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_source_commit_invalid",
        ))?;
    let hard_gate_results = object
        .get("hard_gate_results")
        .and_then(Value::as_array)
        .filter(|gates| gates.len() == gate_specs.len())
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_hard_gates_invalid",
        ))?;
    for (gate, expected) in hard_gate_results.iter().zip(gate_specs) {
        let Some(gate) = gate.as_object() else {
            return Err(DropinClaimFailure::Invalid(
                "dropin_verdict_hard_gates_invalid",
            ));
        };
        let detail_valid = gate
            .get("detail")
            .is_none_or(|detail| detail.is_null() || detail.is_string());
        let artifacts_match = gate
            .get("artifact_paths")
            .and_then(Value::as_array)
            .and_then(|artifacts| {
                artifacts
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<Vec<_>>>()
            })
            .is_some_and(|artifacts| {
                artifacts
                    == expected
                        .required_artifacts
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
            });
        if gate.get("gate_id").and_then(Value::as_str) != Some(expected.gate_id.as_str())
            || gate.get("status").and_then(Value::as_str) != Some("pass")
            || gate.get("blocking").and_then(Value::as_bool) != Some(expected.blocking)
            || gate.get("bead").and_then(Value::as_str) != Some(expected.owner_issue.as_str())
            || !detail_valid
            || !artifacts_match
        {
            return Err(DropinClaimFailure::Invalid(
                "dropin_verdict_hard_gates_invalid",
            ));
        }
    }

    if object
        .get("blocking_reasons")
        .and_then(Value::as_array)
        .is_none_or(|reasons| !reasons.is_empty())
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_blocking_reasons_invalid",
        ));
    }
    let source =
        object
            .get("source")
            .and_then(Value::as_object)
            .ok_or(DropinClaimFailure::Invalid(
                "dropin_verdict_source_lane_invalid",
            ))?;
    if source
        .get("certification_lane_artifact")
        .and_then(Value::as_str)
        != Some(DROPIN_CERTIFICATION_LANE_PATH)
        || source.get("lane_schema").and_then(Value::as_str)
            != Some(DROPIN_CERTIFICATION_LANE_SCHEMA)
        || source.get("lane_verdict").and_then(Value::as_str) != Some("pass")
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }

    let expected_evidence_paths = gate_specs
        .iter()
        .flat_map(|gate| gate.required_artifacts.iter())
        .fold(Vec::<String>::new(), |mut paths, artifact| {
            if !paths.contains(artifact) {
                paths.push(artifact.clone());
            }
            paths
        });
    let evidence_index = object
        .get("evidence_index")
        .and_then(Value::as_array)
        .filter(|index| !index.is_empty())
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_evidence_index_invalid",
        ))?;
    let mut evidence_paths = Vec::with_capacity(evidence_index.len());
    for entry in evidence_index {
        let Some(entry) = entry.as_object() else {
            return Err(DropinClaimFailure::Invalid(
                "dropin_verdict_evidence_index_invalid",
            ));
        };
        let Some(path) = entry
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| dropin_canonical_repo_path(path))
        else {
            return Err(DropinClaimFailure::Invalid(
                "dropin_verdict_evidence_index_invalid",
            ));
        };
        if entry.len() != 2
            || !entry.contains_key("exists")
            || entry.get("exists").and_then(Value::as_bool) != Some(true)
        {
            return Err(DropinClaimFailure::Invalid(
                "dropin_verdict_evidence_index_invalid",
            ));
        }
        evidence_paths.push(path.to_string());
    }
    if evidence_paths != expected_evidence_paths
        || evidence_paths.iter().collect::<BTreeSet<_>>().len() != evidence_paths.len()
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_evidence_index_invalid",
        ));
    }
    Ok((source_commit.to_string(), evidence_paths))
}

#[derive(Debug)]
struct DropinLaneWaiverAudit {
    generated_at: DateTime<Utc>,
    expired: u64,
    invalid: u64,
    eligible_gate_ids: BTreeSet<String>,
}

fn dropin_lane_time_is_current(
    value: &Value,
    reference_time: DateTime<Utc>,
) -> Result<DateTime<Utc>, DropinClaimFailure> {
    let generated_at = performance_generated_at(value)
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    if generated_at.signed_duration_since(reference_time) > Duration::minutes(5)
        || reference_time.signed_duration_since(generated_at)
            > Duration::hours(DROPIN_MAX_EVIDENCE_AGE_HOURS)
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }
    Ok(generated_at)
}

fn dropin_lane_waiver_audit(
    value: &Value,
    reference_time: DateTime<Utc>,
) -> Result<DropinLaneWaiverAudit, DropinClaimFailure> {
    const FIELDS: &[&str] = &[
        "schema",
        "generated_at",
        "total_waivers",
        "active",
        "expired",
        "expiring_soon",
        "invalid",
        "waivers",
        "raw_waivers",
    ];
    let audit = performance_exact_object(value, FIELDS, &[], "certification lane waiver_audit")
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    if audit.get("schema").and_then(Value::as_str) != Some("pi.ci.waiver_audit.v1") {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }
    let generated_at = dropin_lane_time_is_current(&audit["generated_at"], reference_time)?;
    let total = performance_uint(&audit["total_waivers"], "waiver_audit.total_waivers")
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    let active = performance_uint(&audit["active"], "waiver_audit.active")
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    let expired = performance_uint(&audit["expired"], "waiver_audit.expired")
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    let expiring_soon = performance_uint(&audit["expiring_soon"], "waiver_audit.expiring_soon")
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    let invalid = performance_uint(&audit["invalid"], "waiver_audit.invalid")
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    let validations = audit["waivers"]
        .as_array()
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ))?;
    let raw_waivers = audit["raw_waivers"]
        .as_array()
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ))?;
    if total != u64::try_from(raw_waivers.len()).unwrap_or(u64::MAX)
        || u64::try_from(validations.len()).unwrap_or(u64::MAX)
            != active
                .checked_add(expired)
                .and_then(|count| count.checked_add(expiring_soon))
                .and_then(|count| count.checked_add(invalid))
                .unwrap_or(u64::MAX)
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }
    // Strict replacement claims never rely on waivers. This also prevents a
    // self-authored lane from promoting itself with invented lifecycle data.
    if total != 0
        || active != 0
        || expired != 0
        || expiring_soon != 0
        || invalid != 0
        || !validations.is_empty()
        || !raw_waivers.is_empty()
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }

    let mut validation_statuses = BTreeMap::new();
    for validation in validations {
        let validation = performance_exact_object(
            validation,
            &["gate_id", "status"],
            &["detail", "days_remaining"],
            "certification lane waiver validation",
        )
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
        let gate_id =
            performance_nonempty_string(&validation["gate_id"], "waiver validation gate_id")
                .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
        let status = validation["status"]
            .as_str()
            .filter(|status| matches!(*status, "active" | "expired" | "expiring_soon" | "invalid"))
            .ok_or(DropinClaimFailure::Invalid(
                "dropin_verdict_source_lane_invalid",
            ))?;
        if validation
            .get("detail")
            .is_some_and(|detail| !detail.is_string())
            || validation
                .get("days_remaining")
                .is_some_and(|days| days.as_i64().is_none())
            || validation_statuses
                .insert(gate_id.to_string(), status.to_string())
                .is_some()
        {
            return Err(DropinClaimFailure::Invalid(
                "dropin_verdict_source_lane_invalid",
            ));
        }
    }
    let observed_counts = ["active", "expired", "expiring_soon", "invalid"].map(|status| {
        u64::try_from(
            validation_statuses
                .values()
                .filter(|observed| observed.as_str() == status)
                .count(),
        )
        .unwrap_or(u64::MAX)
    });
    if observed_counts != [active, expired, expiring_soon, invalid] {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }

    let mut raw_scopes = BTreeMap::new();
    for waiver in raw_waivers {
        let waiver = performance_exact_object(
            waiver,
            &[
                "gate_id",
                "owner",
                "created",
                "expires",
                "bead",
                "reason",
                "scope",
                "remove_when",
            ],
            &[],
            "certification lane raw waiver",
        )
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
        for field in [
            "gate_id",
            "owner",
            "created",
            "expires",
            "bead",
            "reason",
            "scope",
            "remove_when",
        ] {
            performance_nonempty_string(&waiver[field], field)
                .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
        }
        let gate_id = waiver["gate_id"].as_str().unwrap_or_default();
        let scope = waiver["scope"]
            .as_str()
            .filter(|scope| matches!(*scope, "full" | "preflight" | "both"))
            .ok_or(DropinClaimFailure::Invalid(
                "dropin_verdict_source_lane_invalid",
            ))?;
        if raw_scopes
            .insert(gate_id.to_string(), scope.to_string())
            .is_some()
        {
            return Err(DropinClaimFailure::Invalid(
                "dropin_verdict_source_lane_invalid",
            ));
        }
    }
    if raw_scopes.keys().collect::<BTreeSet<_>>()
        != validation_statuses.keys().collect::<BTreeSet<_>>()
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }
    let eligible_gate_ids = validation_statuses
        .into_iter()
        .filter(|(gate_id, status)| {
            matches!(status.as_str(), "active" | "expiring_soon")
                && raw_scopes
                    .get(gate_id)
                    .is_some_and(|scope| matches!(scope.as_str(), "full" | "both"))
        })
        .map(|(gate_id, _)| gate_id)
        .collect();
    Ok(DropinLaneWaiverAudit {
        generated_at,
        expired,
        invalid,
        eligible_gate_ids,
    })
}

fn validate_dropin_certification_lane(
    lane: &Value,
    reference_time: DateTime<Utc>,
    verdict_generated_at: DateTime<Utc>,
) -> Result<(), DropinClaimFailure> {
    let lane = performance_exact_object(
        lane,
        DROPIN_LANE_TOP_LEVEL_FIELDS,
        &[],
        "drop-in certification lane",
    )
    .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    if lane["schema"].as_str() != Some(DROPIN_CERTIFICATION_LANE_SCHEMA)
        || lane["lane"].as_str() != Some("full")
        || lane["policy"].as_str() != Some(DROPIN_CERTIFICATION_LANE_POLICY)
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }
    let lane_generated_at = dropin_lane_time_is_current(&lane["generated_at"], reference_time)?;
    let verdict_lane_delta = verdict_generated_at.signed_duration_since(lane_generated_at);
    let verdict_within_policy = verdict_generated_at.signed_duration_since(reference_time)
        <= Duration::minutes(5)
        && reference_time.signed_duration_since(verdict_generated_at)
            <= Duration::hours(DROPIN_MAX_EVIDENCE_AGE_HOURS);
    if verdict_within_policy
        && !(-Duration::minutes(5)..=Duration::minutes(5)).contains(&verdict_lane_delta)
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }
    let waiver_audit = dropin_lane_waiver_audit(&lane["waiver_audit"], reference_time)?;
    if waiver_audit.generated_at > lane_generated_at
        || lane_generated_at.signed_duration_since(waiver_audit.generated_at) > Duration::minutes(5)
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }
    let waivers_applied = lane["waivers_applied"]
        .as_array()
        .and_then(|waivers| {
            waivers
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()
        })
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ))?;
    let waived = waivers_applied.iter().copied().collect::<BTreeSet<_>>();
    if waived.len() != waivers_applied.len()
        || waived
            .iter()
            .any(|gate_id| !waiver_audit.eligible_gate_ids.contains(*gate_id))
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }

    let gates = lane["gates"]
        .as_array()
        .filter(|gates| gates.len() == DROPIN_FULL_LANE_GATES.len())
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ))?;
    let mut gate_ids = BTreeSet::new();
    let mut gate_rows = Vec::with_capacity(gates.len());
    for (gate, expected) in gates.iter().zip(DROPIN_FULL_LANE_GATES) {
        let gate = performance_exact_object(
            gate,
            &["id", "name", "bead", "status", "blocking"],
            &["artifact_path", "detail", "reproduce_command"],
            "certification lane gate",
        )
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
        for field in ["id", "name", "bead"] {
            performance_nonempty_string(&gate[field], field)
                .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
        }
        let id = gate["id"].as_str().unwrap_or_default();
        let status = gate["status"]
            .as_str()
            .filter(|status| matches!(*status, "pass" | "fail" | "warn" | "skip"))
            .ok_or(DropinClaimFailure::Invalid(
                "dropin_verdict_source_lane_invalid",
            ))?;
        let blocking = gate["blocking"]
            .as_bool()
            .ok_or(DropinClaimFailure::Invalid(
                "dropin_verdict_source_lane_invalid",
            ))?;
        if id != expected.id
            || gate["name"].as_str() != Some(expected.name)
            || gate["bead"].as_str() != Some(expected.bead)
            || blocking != expected.blocking
            || gate.get("artifact_path").and_then(Value::as_str) != Some(expected.artifact_path)
            || gate.get("reproduce_command").and_then(Value::as_str) != expected.reproduce_command
            || !gate_ids.insert(id)
            || waived.contains(id) && !matches!(status, "fail" | "warn")
            || gate.get("artifact_path").is_some_and(|path| {
                path.as_str()
                    .is_none_or(|path| !dropin_canonical_repo_path(path))
            })
            || gate
                .get("detail")
                .is_some_and(|detail| detail.as_str().is_none_or(str::is_empty))
            || gate
                .get("reproduce_command")
                .is_some_and(|command| command.as_str().is_none_or(str::is_empty))
        {
            return Err(DropinClaimFailure::Invalid(
                "dropin_verdict_source_lane_invalid",
            ));
        }
        gate_rows.push((id, status, blocking));
    }
    if waived.iter().any(|gate_id| !gate_ids.contains(gate_id)) {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }
    // The ordinary CI lane schema can represent warnings, skips, and
    // time-bounded waivers. A strict drop-in replacement claim is narrower:
    // its committed source lane must be an unwaived, all-pass snapshot.
    if !waived.is_empty() || gate_rows.iter().any(|(_, status, _)| *status != "pass") {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }

    let passed = gate_rows
        .iter()
        .filter(|(_, status, _)| *status == "pass")
        .count();
    let failed = gate_rows
        .iter()
        .filter(|(id, status, _)| *status == "fail" && !waived.contains(id))
        .count();
    let warned = gate_rows
        .iter()
        .filter(|(id, status, _)| *status == "warn" && !waived.contains(id))
        .count();
    let skipped = gate_rows
        .iter()
        .filter(|(_, status, _)| *status == "skip")
        .count();
    let blocking_total = gate_rows
        .iter()
        .filter(|(_, _, blocking)| *blocking)
        .count();
    let blocking_pass = gate_rows
        .iter()
        .filter(|(id, status, blocking)| *blocking && (*status == "pass" || waived.contains(id)))
        .count();
    let all_blocking_pass = blocking_pass == blocking_total;
    let expected_verdict = if all_blocking_pass && failed == 0 {
        "pass"
    } else if all_blocking_pass {
        "warn"
    } else {
        "fail"
    };
    if lane["verdict"].as_str() != Some(expected_verdict) || expected_verdict != "pass" {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }

    let summary = performance_exact_object(
        &lane["summary"],
        &[
            "total_gates",
            "passed",
            "failed",
            "warned",
            "skipped",
            "waived",
            "blocking_pass",
            "blocking_total",
            "all_blocking_pass",
        ],
        &[],
        "certification lane summary",
    )
    .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    for (field, expected) in [
        ("total_gates", gates.len()),
        ("passed", passed),
        ("failed", failed),
        ("warned", warned),
        ("skipped", skipped),
        ("waived", waived.len()),
        ("blocking_pass", blocking_pass),
        ("blocking_total", blocking_total),
    ] {
        if performance_uint(&summary[field], field).ok()
            != Some(u64::try_from(expected).unwrap_or(u64::MAX))
        {
            return Err(DropinClaimFailure::Invalid(
                "dropin_verdict_source_lane_invalid",
            ));
        }
    }
    if summary["all_blocking_pass"].as_bool() != Some(all_blocking_pass) {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }

    let promotion = performance_exact_object(
        &lane["promotion_rules"],
        &["can_promote", "blocker_gates", "waiver_gates", "conditions"],
        &[],
        "certification lane promotion_rules",
    )
    .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    let blocker_gates = gate_rows
        .iter()
        .filter(|(id, status, blocking)| {
            *blocking && *status != "pass" && *status != "skip" && !waived.contains(id)
        })
        .map(|(id, _, _)| *id)
        .collect::<Vec<_>>();
    let actual_blockers = promotion["blocker_gates"]
        .as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect::<Option<Vec<_>>>());
    let actual_waivers = promotion["waiver_gates"]
        .as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect::<Option<Vec<_>>>());
    let actual_conditions = promotion["conditions"].as_array().and_then(|conditions| {
        conditions
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
    });
    let mut expected_conditions = vec!["All blocking gates pass (including waivers)".to_string()];
    if !waivers_applied.is_empty() {
        expected_conditions.push(format!(
            "Waivers active for: {} (review before release)",
            waivers_applied.join(", ")
        ));
    }
    let can_promote = all_blocking_pass && waiver_audit.expired == 0 && waiver_audit.invalid == 0;
    if promotion["can_promote"].as_bool() != Some(can_promote)
        || !can_promote
        || actual_blockers.as_deref() != Some(blocker_gates.as_slice())
        || actual_waivers.as_deref() != Some(waivers_applied.as_slice())
        || actual_conditions.as_deref()
            != Some(
                expected_conditions
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice(),
            )
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }

    let rerun = performance_exact_object(
        &lane["rerun_guidance"],
        &["preflight_command", "full_command", "single_gate_template"],
        &[],
        "certification lane rerun_guidance",
    )
    .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    if rerun["preflight_command"].as_str()
        != Some("cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact")
        || rerun["full_command"].as_str()
            != Some(
                "cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact",
            )
        || rerun["single_gate_template"].as_str()
            != Some("See reproduce_command field on each gate")
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_lane_invalid",
        ));
    }
    Ok(())
}

fn dropin_head_regular_blob(
    context: &RepositoryGitContext,
    head: &str,
    source_path: &str,
    require_non_executable: bool,
) -> Result<Vec<u8>, DropinClaimFailure> {
    if !dropin_canonical_repo_path(source_path) {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_provenance_path_invalid",
        ));
    }
    let relative = Path::new(source_path);
    let tree_entry = git_output(
        context,
        &["ls-tree", "-z", "--full-tree", head, "--", source_path],
    )
    .ok_or(DropinClaimFailure::Unavailable(
        "dropin_verdict_source_binding_unavailable",
    ))?;
    let entries = canonical_nul_records(&tree_entry).ok_or(DropinClaimFailure::Unavailable(
        "dropin_verdict_source_binding_unavailable",
    ))?;
    let entry = entries
        .as_slice()
        .first()
        .copied()
        .filter(|_| entries.len() == 1)
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_provenance_not_tracked_at_head",
        ))?;
    let (mode, object_type, oid, recorded_path) = parse_canonical_git_record(entry).ok_or(
        DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable"),
    )?;
    let mode_is_valid = if require_non_executable {
        mode == b"100644"
    } else {
        matches!(mode, b"100644" | b"100755")
    };
    if recorded_path != source_path.as_bytes() || !mode_is_valid || object_type != b"blob" {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_provenance_not_regular_at_head",
        ));
    }
    let oid = std::str::from_utf8(oid).map_err(|_| {
        DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable")
    })?;
    let head_bytes = git_output(context, &["cat-file", "blob", oid]).ok_or(
        DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable"),
    )?;
    let worktree_bytes = tracked_worktree_blob(&context.worktree, relative, mode).ok_or(
        DropinClaimFailure::Invalid("dropin_verdict_provenance_not_regular_at_head"),
    )?;
    if worktree_bytes != head_bytes {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_provenance_bytes_do_not_match_head",
        ));
    }
    Ok(head_bytes)
}

fn dropin_repository_state(
    context: &RepositoryGitContext,
    head: &str,
) -> Result<(), DropinClaimFailure> {
    match performance_repository_state_failure(context, head) {
        None => Ok(()),
        Some(PerformanceSourceBindingFailure::Unavailable) => Err(DropinClaimFailure::Unavailable(
            "dropin_verdict_source_binding_unavailable",
        )),
        Some(PerformanceSourceBindingFailure::Invalid(_)) => Err(DropinClaimFailure::Invalid(
            "dropin_verdict_repository_not_clean",
        )),
    }
}

fn dropin_source_binding(
    context: &RepositoryGitContext,
    source_commit: &str,
    head: &str,
) -> Result<(), DropinClaimFailure> {
    let source_expression = format!("{source_commit}^{{commit}}");
    let resolved_source = git_stdout(context, &["rev-parse", "--verify", &source_expression])
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_source_commit_unresolvable",
        ))?;
    if resolved_source != source_commit {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_commit_not_exact",
        ));
    }
    if source_commit == head {
        return Ok(());
    }
    let ancestor = repository_git_command(context)
        .args(["merge-base", "--is-ancestor", source_commit, head])
        .output()
        .map_err(|_| {
            DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable")
        })?;
    if !ancestor.status.success() {
        return Err(if ancestor.status.code() == Some(1) {
            DropinClaimFailure::Invalid("dropin_verdict_source_commit_not_ancestor")
        } else {
            DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable")
        });
    }

    let changed_paths = git_output(
        context,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            source_commit,
            head,
        ],
    )
    .ok_or(DropinClaimFailure::Unavailable(
        "dropin_verdict_source_binding_unavailable",
    ))?;
    let changed_paths = canonical_nul_records(&changed_paths).ok_or(
        DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable"),
    )?;
    let changed_paths = changed_paths
        .iter()
        .map(|path| std::str::from_utf8(path).ok())
        .collect::<Option<Vec<_>>>()
        .ok_or(DropinClaimFailure::Unavailable(
            "dropin_verdict_source_binding_unavailable",
        ))?;
    if changed_paths
        .iter()
        .any(|path| !dropin_canonical_repo_path(path))
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_commit_not_release_bound",
        ));
    }
    let package_patterns = if changed_paths
        .iter()
        .any(|path| path.starts_with("docs/evidence/"))
    {
        let cargo_expression = format!("{source_commit}:Cargo.toml");
        let cargo_toml = git_output(context, &["show", &cargo_expression])
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or(DropinClaimFailure::Unavailable(
                "dropin_verdict_source_binding_unavailable",
            ))?;
        source_package_include_patterns(&cargo_toml).ok_or(DropinClaimFailure::Unavailable(
            "dropin_verdict_source_binding_unavailable",
        ))?
    } else {
        Vec::new()
    };
    let allowed_prefixes = [
        "docs/evidence/",
        "tests/ext_conformance/reports/",
        "tests/perf/reports/",
        "tests/cross_platform_reports/",
        "tests/franken_node_compat/reports/",
        "tests/evidence_bundle/",
        "tests/certification/",
    ];
    let evidence_only = changed_paths.iter().all(|path| {
        let packaged_docs_evidence = path.starts_with("docs/evidence/")
            && performance_path_is_packaged(path, &package_patterns) != Some(false);
        !packaged_docs_evidence
            && allowed_prefixes
                .iter()
                .any(|prefix| path.starts_with(prefix))
    });
    if !evidence_only {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_source_commit_not_release_bound",
        ));
    }
    Ok(())
}

fn validate_dropin_verdict_claim(
    value: &Value,
    repository_root: Option<&Path>,
    captured_artifact_bytes: Option<&[u8]>,
    reference_time_utc: Option<DateTime<Utc>>,
) -> Result<(), DropinClaimFailure> {
    let verdict_object = value.as_object().ok_or(DropinClaimFailure::Invalid(
        "dropin_verdict_contract_invalid",
    ))?;
    if DROPIN_VERDICT_REQUIRED_FIELDS
        .iter()
        .any(|field| !verdict_object.contains_key(*field))
        || !verdict_object
            .get("generated_at_utc")
            .is_some_and(dropin_generated_at_is_canonical)
    {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_contract_invalid",
        ));
    }
    let repository_root = repository_root.ok_or(DropinClaimFailure::Unavailable(
        "dropin_verdict_source_binding_unavailable",
    ))?;
    let captured_artifact_bytes = captured_artifact_bytes.ok_or(
        DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable"),
    )?;
    let reference_time = reference_time_utc.ok_or(DropinClaimFailure::Unavailable(
        "dropin_verdict_source_lane_freshness_unavailable",
    ))?;
    let verdict_generated_at = verdict_object["generated_at_utc"]
        .as_str()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|parsed| parsed.with_timezone(&Utc))
        .ok_or(DropinClaimFailure::Invalid(
            "dropin_verdict_contract_invalid",
        ))?;
    let context = repository_git_context(repository_root).ok_or(
        DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable"),
    )?;
    let head = git_stdout(&context, &["rev-parse", "--verify", "HEAD^{commit}"]).ok_or(
        DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable"),
    )?;
    dropin_repository_state(&context, &head)?;

    let verdict_bytes =
        dropin_head_regular_blob(&context, &head, DROPIN_CERTIFICATION_VERDICT_PATH, true)?;
    if verdict_bytes != captured_artifact_bytes {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_artifact_changed_since_ingestion",
        ));
    }
    let contract_bytes =
        dropin_head_regular_blob(&context, &head, DROPIN_CERTIFICATION_CONTRACT_PATH, true)?;
    let contract_text = std::str::from_utf8(&contract_bytes)
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_contract_invalid"))?;
    let contract = parse_json_rejecting_duplicate_keys(contract_text)
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_contract_invalid"))?;
    let gate_specs = dropin_contract_gate_specs(&contract)?;
    let (source_commit, evidence_paths) = dropin_verdict_payload(value, &gate_specs)?;
    let lane_bytes =
        dropin_head_regular_blob(&context, &head, DROPIN_CERTIFICATION_LANE_PATH, true)?;
    let lane_text = std::str::from_utf8(&lane_bytes)
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    let lane = parse_json_rejecting_duplicate_keys(lane_text)
        .map_err(|_| DropinClaimFailure::Invalid("dropin_verdict_source_lane_invalid"))?;
    validate_dropin_certification_lane(&lane, reference_time, verdict_generated_at)?;
    dropin_source_binding(&context, &source_commit, &head)?;

    let mut provenance_paths = vec![
        DROPIN_CERTIFICATION_CONTRACT_PATH.to_string(),
        DROPIN_CERTIFICATION_VERDICT_PATH.to_string(),
        DROPIN_CERTIFICATION_LANE_PATH.to_string(),
    ];
    provenance_paths.extend(evidence_paths);
    let mut seen = BTreeSet::new();
    for path in provenance_paths {
        if seen.insert(path.clone()) {
            let require_non_executable = matches!(
                path.as_str(),
                DROPIN_CERTIFICATION_CONTRACT_PATH
                    | DROPIN_CERTIFICATION_VERDICT_PATH
                    | DROPIN_CERTIFICATION_LANE_PATH
            );
            dropin_head_regular_blob(&context, &head, &path, require_non_executable)?;
        }
    }

    let current_head = git_stdout(&context, &["rev-parse", "--verify", "HEAD^{commit}"]).ok_or(
        DropinClaimFailure::Unavailable("dropin_verdict_source_binding_unavailable"),
    )?;
    if current_head != head {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_repository_head_changed",
        ));
    }
    dropin_repository_state(&context, &head)?;
    let final_verdict_bytes =
        dropin_head_regular_blob(&context, &head, DROPIN_CERTIFICATION_VERDICT_PATH, true)?;
    if final_verdict_bytes != captured_artifact_bytes {
        return Err(DropinClaimFailure::Invalid(
            "dropin_verdict_artifact_changed_during_validation",
        ));
    }
    Ok(())
}

pub fn classify_evidence_freshness(
    value: &Value,
    options: &SemanticWorkspaceGraphBuildOptions,
) -> (EvidenceFreshnessStatus, bool, String) {
    classify_evidence_freshness_in_repository(value, options, None, None, None)
}

fn classify_evidence_freshness_in_repository(
    value: &Value,
    options: &SemanticWorkspaceGraphBuildOptions,
    repository_root: Option<&Path>,
    artifact_source_path: Option<&str>,
    captured_artifact_bytes: Option<&[u8]>,
) -> (EvidenceFreshnessStatus, bool, String) {
    let canonical_performance_budget = artifact_source_path == Some(PERF_BUDGET_SUMMARY_PATH);
    if value.get("schema").and_then(Value::as_str) == Some(DROPIN_CERTIFICATION_VERDICT_SCHEMA)
        && artifact_source_path != Some(DROPIN_CERTIFICATION_VERDICT_PATH)
    {
        return (
            EvidenceFreshnessStatus::Malformed,
            false,
            "dropin_verdict_noncanonical_path".to_string(),
        );
    }
    if canonical_performance_budget
        && let Some(classification) = classify_performance_budget_claim(
            value,
            options,
            repository_root,
            artifact_source_path,
            captured_artifact_bytes,
            true,
        )
    {
        return classification;
    }

    if artifact_source_path == Some(DROPIN_CERTIFICATION_VERDICT_PATH) {
        if value.get("schema").and_then(Value::as_str) != Some(DROPIN_CERTIFICATION_VERDICT_SCHEMA)
        {
            return (
                EvidenceFreshnessStatus::Malformed,
                false,
                "dropin_verdict_schema_invalid".to_string(),
            );
        }
        let Some(overall_verdict) = value.get("overall_verdict").and_then(Value::as_str) else {
            return (
                EvidenceFreshnessStatus::Malformed,
                false,
                "overall_verdict_missing_or_invalid".to_string(),
            );
        };
        if overall_verdict != "CERTIFIED" {
            return (
                EvidenceFreshnessStatus::Uncertified,
                false,
                "overall_verdict_not_certified".to_string(),
            );
        }
        if let Err(failure) = validate_dropin_verdict_claim(
            value,
            repository_root,
            captured_artifact_bytes,
            options.reference_time_utc,
        ) {
            return match failure {
                DropinClaimFailure::Unavailable(reason) => (
                    EvidenceFreshnessStatus::Uncertified,
                    false,
                    reason.to_string(),
                ),
                DropinClaimFailure::Invalid(reason) => (
                    EvidenceFreshnessStatus::Malformed,
                    false,
                    reason.to_string(),
                ),
            };
        }
    }

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

    if !canonical_performance_budget
        && let Some(classification) = classify_performance_budget_claim(
            value,
            options,
            repository_root,
            artifact_source_path,
            captured_artifact_bytes,
            false,
        )
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

    let generated_at = if artifact_source_path == Some(DROPIN_CERTIFICATION_VERDICT_PATH) {
        value.get("generated_at_utc").and_then(Value::as_str)
    } else {
        evidence_generated_at(value)
    };
    let Some(generated_at) = generated_at else {
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

    let generated_at_utc = generated_at.with_timezone(&Utc);
    if generated_at_utc.signed_duration_since(reference_time_utc) > Duration::minutes(5) {
        return (
            EvidenceFreshnessStatus::Malformed,
            false,
            "generated_at_in_future".to_string(),
        );
    }

    let stale = evidence_age_exceeds_policy(
        value,
        options,
        artifact_source_path,
        generated_at_utc,
        reference_time_utc,
    );
    if stale {
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
    captured_artifact_bytes: &[u8],
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
        classify_evidence_freshness_in_repository(
            value,
            options,
            Some(repository_root),
            Some(source_path),
            Some(captured_artifact_bytes),
        );
    let mut metadata = BTreeMap::new();
    let privacy = classify_node_privacy(source_path, Some(value));
    metadata.insert("artifact_schema".to_string(), json!(artifact_schema));
    let generated_at = if source_path == DROPIN_CERTIFICATION_VERDICT_PATH {
        value.get("generated_at_utc").and_then(Value::as_str)
    } else {
        evidence_generated_at(value)
    };
    if let Some(generated_at) = generated_at {
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
    if source_path == DROPIN_CERTIFICATION_VERDICT_PATH {
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

#[cfg(test)]
mod git_record_parser_tests {
    use super::{
        canonical_nul_records, parse_canonical_git_record, repository_git_command,
        repository_git_context, trusted_git_executable,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::process::Command;

    #[test]
    fn accepts_only_canonical_git_record_headers() {
        assert_eq!(
            parse_canonical_git_record(b"100644 blob abcdef\tsrc/lib.rs"),
            Some((
                b"100644".as_slice(),
                b"blob".as_slice(),
                b"abcdef".as_slice(),
                b"src/lib.rs".as_slice(),
            ))
        );

        for malformed in [
            b" 100644 blob abcdef\tsrc/lib.rs".as_slice(),
            b"100644  blob abcdef\tsrc/lib.rs".as_slice(),
            b"100644 blob  abcdef\tsrc/lib.rs".as_slice(),
            b"100644 blob abcdef \tsrc/lib.rs".as_slice(),
            b"100644 blob abcdef src/lib.rs".as_slice(),
            b"100644 blob abcdef\t".as_slice(),
            b"100644 blob\tsrc/lib.rs".as_slice(),
            b"100644 blob abcdef extra\tsrc/lib.rs".as_slice(),
        ] {
            assert_eq!(parse_canonical_git_record(malformed), None);
        }
    }

    #[test]
    fn accepts_only_canonically_terminated_nul_records() {
        assert_eq!(canonical_nul_records(b""), Some(Vec::new()));
        assert_eq!(
            canonical_nul_records(b"first\0second\0"),
            Some(vec![b"first".as_slice(), b"second".as_slice()])
        );

        for malformed in [
            b"first".as_slice(),
            b"\0".as_slice(),
            b"first\0\0".as_slice(),
            b"first\0\0second\0".as_slice(),
        ] {
            assert_eq!(canonical_nul_records(malformed), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn repository_git_commands_ignore_hostile_global_configuration() {
        let temp = tempfile::tempdir().expect("create Git command fixture");
        let repository = temp.path().join("repository");
        let git = trusted_git_executable().expect("trusted Git executable");
        let init = Command::new(git)
            .args(["init", "-b", "main"])
            .arg(&repository)
            .output()
            .expect("initialize Git command fixture");
        assert!(init.status.success());
        let context = repository_git_context(&repository).expect("trusted Git context");

        let command = repository_git_command(&context);
        let env = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(OsString::from)))
            .collect::<BTreeMap<_, _>>();
        for (key, expected) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_LITERAL_PATHSPECS", "1"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_TERMINAL_PROMPT", "0"),
        ] {
            assert_eq!(
                env.get(&OsString::from(key)).and_then(Option::as_deref),
                Some(std::ffi::OsStr::new(expected)),
                "missing sanitized Git environment control {key}"
            );
        }

        let hostile_home = temp.path().join("hostile-home");
        fs::create_dir(&hostile_home).expect("create hostile Git home");
        fs::write(
            hostile_home.join(".gitconfig"),
            "[user]\nname = Hostile Global Identity\n",
        )
        .expect("write hostile global Git configuration");
        let global_lookup = repository_git_command(&context)
            .env("HOME", &hostile_home)
            .args(["config", "--global", "--get", "user.name"])
            .output()
            .expect("probe sanitized global Git configuration");
        assert!(!global_lookup.status.success());
        assert!(global_lookup.stdout.is_empty());
    }
}
