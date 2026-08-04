//! Provider streaming tests backed by VCR cassettes.
//!
//! Recording (run locally with real API keys):
//! ```bash
//! ANTHROPIC_API_KEY=sk-ant-... VCR_MODE=record \
//!   cargo test provider_streaming::anthropic_
//! ```
//!
//! Playback (default in CI):
//! ```bash
//! VCR_MODE=playback VCR_CASSETTE_DIR=tests/fixtures/vcr \
//!   cargo test provider_streaming::anthropic_
//! ```
mod common;

use common::TestHarness;
use futures::{Stream, StreamExt};
use pi::model::{
    AssistantMessage, ContentBlock, Cost, Message, StopReason, StreamEvent, ToolCall,
    ToolResultMessage, Usage, UserContent, UserMessage,
};
use pi::provider::ToolDef;
use pi::vcr::{Cassette, VcrMode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::env;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[path = "provider_streaming/anthropic.rs"]
mod anthropic;
#[path = "provider_streaming/azure.rs"]
mod azure;
#[path = "provider_streaming/cohere.rs"]
mod cohere;
#[path = "provider_streaming/gemini.rs"]
mod gemini;
#[path = "provider_streaming/openai.rs"]
mod openai;
#[path = "provider_streaming/openai_responses.rs"]
mod openai_responses;

pub(crate) fn cassette_root() -> PathBuf {
    env::var("VCR_CASSETTE_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr"),
        PathBuf::from,
    )
}

fn env_truthy(name: &str) -> bool {
    env::var(name)
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub(crate) fn vcr_mode() -> VcrMode {
    match env::var("VCR_MODE")
        .ok()
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("record") => VcrMode::Record,
        Some("auto") => VcrMode::Auto,
        _ => VcrMode::Playback,
    }
}

pub(crate) fn vcr_strict() -> bool {
    env_truthy("VCR_STRICT")
}

const PROVIDER_REPLAY_CACHE_SCHEMA: &str = "pi.test.provider_replay_cache.v1";
const PROVIDER_REPLAY_CACHE_CASSETTE_VERSION: &str = "1.0";

pub(crate) struct ProviderReplayCacheSpec<'a> {
    pub provider: &'a str,
    pub route: &'a str,
    pub model: &'a str,
    pub scenario: &'a str,
    pub cassette_path: &'a Path,
    pub request_schema_hash: &'a str,
    pub mode: VcrMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderReplayCacheEntry {
    pub provider: String,
    pub route: String,
    pub model: String,
    pub request_schema_hash: String,
    pub cassette_version: String,
    pub fixture_sha256: String,
    pub git_commit: String,
    pub mode: VcrMode,
    pub interaction_count: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderReplayCacheRefusal {
    pub class: String,
    pub reason: String,
    pub recovery_action: String,
    pub fail_closed: bool,
}

pub(crate) fn provider_request_schema_hash(
    messages: &[Message],
    tools: &[ToolDef],
    options: &Value,
) -> String {
    let tool_schemas: Vec<_> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": &tool.name,
                "description": &tool.description,
                "parameters": &tool.parameters,
            })
        })
        .collect();
    let payload = json!({
        "messages": messages,
        "tools": tool_schemas,
        "options": options,
    });
    let bytes = serde_json::to_vec(&payload).expect("provider request schema should serialize");
    sha256_hex(&bytes)
}

pub(crate) fn build_provider_replay_cache_entry(
    spec: &ProviderReplayCacheSpec<'_>,
) -> Result<ProviderReplayCacheEntry, ProviderReplayCacheRefusal> {
    if spec.request_schema_hash.trim().is_empty() {
        return Err(replay_cache_refusal(
            "ambiguous_request_schema",
            "request schema hash is empty",
            "refuse_cache_reuse",
            true,
        ));
    }

    let cassette_bytes = std::fs::read(spec.cassette_path).map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            replay_cache_refusal(
                "missing_cassette",
                format!(
                    "cassette does not exist at {}",
                    spec.cassette_path.display()
                ),
                "record_or_skip",
                false,
            )
        } else {
            replay_cache_refusal(
                "invalid_cassette",
                format!("failed to read cassette: {err}"),
                "refuse_cache_reuse",
                true,
            )
        }
    })?;
    let fixture_sha256 = sha256_hex(&cassette_bytes);
    let cassette = serde_json::from_slice::<Cassette>(&cassette_bytes).map_err(|err| {
        replay_cache_refusal(
            "invalid_cassette",
            format!("failed to parse cassette JSON: {err}"),
            "refuse_cache_reuse",
            true,
        )
    })?;

    if cassette.version != PROVIDER_REPLAY_CACHE_CASSETTE_VERSION {
        return Err(replay_cache_refusal(
            "stale_cassette_version",
            format!(
                "cassette version {} does not match expected {}",
                cassette.version, PROVIDER_REPLAY_CACHE_CASSETTE_VERSION
            ),
            "refuse_cache_reuse",
            true,
        ));
    }

    if cassette.interactions.is_empty() {
        return Err(replay_cache_refusal(
            "empty_cassette",
            "cassette has no recorded interactions",
            "refuse_cache_reuse",
            true,
        ));
    }

    let git_commit = current_git_commit().ok_or_else(|| {
        replay_cache_refusal(
            "ambiguous_git_commit",
            "could not resolve the current git commit",
            "refuse_cache_reuse",
            true,
        )
    })?;

    Ok(ProviderReplayCacheEntry {
        provider: spec.provider.to_string(),
        route: spec.route.to_string(),
        model: spec.model.to_string(),
        request_schema_hash: spec.request_schema_hash.to_string(),
        cassette_version: cassette.version,
        fixture_sha256,
        git_commit,
        mode: spec.mode,
        interaction_count: cassette.interactions.len(),
    })
}

pub(crate) fn provider_replay_cache_key(entry: &ProviderReplayCacheEntry) -> String {
    let payload = json!({
        "provider": &entry.provider,
        "route": &entry.route,
        "model": &entry.model,
        "requestSchemaHash": &entry.request_schema_hash,
        "cassetteVersion": &entry.cassette_version,
        "fixtureSha256": &entry.fixture_sha256,
        "gitCommit": &entry.git_commit,
        "mode": entry.mode,
    });
    let bytes = serde_json::to_vec(&payload).expect("provider replay cache key should serialize");
    sha256_hex(&bytes)
}

pub(crate) fn provider_replay_cache_report(
    expected: Option<&ProviderReplayCacheEntry>,
    spec: &ProviderReplayCacheSpec<'_>,
) -> Value {
    let current = build_provider_replay_cache_entry(spec);
    provider_replay_cache_report_with_current(spec, expected, current)
}

pub(crate) fn record_provider_replay_cache_artifact(
    harness: &TestHarness,
    spec: &ProviderReplayCacheSpec<'_>,
) -> Value {
    let current = build_provider_replay_cache_entry(spec);
    let report = match current {
        Ok(entry) => {
            provider_replay_cache_report_with_current(spec, Some(&entry), Ok(entry.clone()))
        }
        Err(refusal) => provider_replay_cache_report_with_current(spec, None, Err(refusal)),
    };
    let artifact_path = harness.temp_path(format!(
        "provider_replay_cache_{}_{}.json",
        sanitize_artifact_part(spec.provider),
        sanitize_artifact_part(spec.scenario)
    ));
    let bytes =
        serde_json::to_vec_pretty(&report).expect("provider replay cache report should serialize");
    std::fs::write(&artifact_path, bytes).expect("write provider replay cache artifact");
    harness.record_artifact(
        format!("provider-replay-cache/{}:{}", spec.provider, spec.scenario),
        &artifact_path,
    );
    report
}

fn provider_replay_cache_report_with_current(
    spec: &ProviderReplayCacheSpec<'_>,
    expected: Option<&ProviderReplayCacheEntry>,
    current: Result<ProviderReplayCacheEntry, ProviderReplayCacheRefusal>,
) -> Value {
    let cassette_path = spec.cassette_path.display().to_string();
    match current {
        Ok(entry) => {
            let cache_key = provider_replay_cache_key(&entry);
            match expected {
                None => json!({
                    "schema": PROVIDER_REPLAY_CACHE_SCHEMA,
                    "scenario": spec.scenario,
                    "provider": spec.provider,
                    "route": spec.route,
                    "model": spec.model,
                    "cassettePath": &cassette_path,
                    "verdict": "miss",
                    "cacheReusable": false,
                    "failClosed": false,
                    "cacheKey": cache_key,
                    "currentEntry": provider_replay_cache_entry_json(&entry),
                }),
                Some(expected_entry) if *expected_entry == entry => json!({
                    "schema": PROVIDER_REPLAY_CACHE_SCHEMA,
                    "scenario": spec.scenario,
                    "provider": spec.provider,
                    "route": spec.route,
                    "model": spec.model,
                    "cassettePath": &cassette_path,
                    "verdict": "hit",
                    "cacheReusable": true,
                    "failClosed": false,
                    "cacheKey": cache_key,
                    "currentEntry": provider_replay_cache_entry_json(&entry),
                    "expectedEntry": provider_replay_cache_entry_json(expected_entry),
                }),
                Some(expected_entry) => json!({
                    "schema": PROVIDER_REPLAY_CACHE_SCHEMA,
                    "scenario": spec.scenario,
                    "provider": spec.provider,
                    "route": spec.route,
                    "model": spec.model,
                    "cassettePath": &cassette_path,
                    "verdict": "stale",
                    "cacheReusable": false,
                    "failClosed": true,
                    "cacheKey": cache_key,
                    "mismatches": provider_replay_cache_mismatches(expected_entry, &entry),
                    "currentEntry": provider_replay_cache_entry_json(&entry),
                    "expectedEntry": provider_replay_cache_entry_json(expected_entry),
                }),
            }
        }
        Err(refusal) => {
            let verdict = if refusal.class == "missing_cassette" {
                "miss"
            } else {
                "stale"
            };
            json!({
                "schema": PROVIDER_REPLAY_CACHE_SCHEMA,
                "scenario": spec.scenario,
                "provider": spec.provider,
                "route": spec.route,
                "model": spec.model,
                "cassettePath": &cassette_path,
                "verdict": verdict,
                "cacheReusable": false,
                "failClosed": refusal.fail_closed,
                "refusal": {
                    "class": refusal.class,
                    "reason": refusal.reason,
                    "recoveryAction": refusal.recovery_action,
                },
            })
        }
    }
}

fn replay_cache_refusal(
    class: impl Into<String>,
    reason: impl Into<String>,
    recovery_action: impl Into<String>,
    fail_closed: bool,
) -> ProviderReplayCacheRefusal {
    ProviderReplayCacheRefusal {
        class: class.into(),
        reason: reason.into(),
        recovery_action: recovery_action.into(),
        fail_closed,
    }
}

fn provider_replay_cache_entry_json(entry: &ProviderReplayCacheEntry) -> Value {
    json!({
        "provider": &entry.provider,
        "route": &entry.route,
        "model": &entry.model,
        "requestSchemaHash": &entry.request_schema_hash,
        "cassetteVersion": &entry.cassette_version,
        "fixtureSha256": &entry.fixture_sha256,
        "gitCommit": &entry.git_commit,
        "mode": entry.mode,
        "interactionCount": entry.interaction_count,
    })
}

fn provider_replay_cache_mismatches(
    expected: &ProviderReplayCacheEntry,
    current: &ProviderReplayCacheEntry,
) -> Vec<&'static str> {
    let mut mismatches = Vec::new();
    if expected.provider != current.provider {
        mismatches.push("provider");
    }
    if expected.route != current.route {
        mismatches.push("route");
    }
    if expected.model != current.model {
        mismatches.push("model");
    }
    if expected.request_schema_hash != current.request_schema_hash {
        mismatches.push("request_schema_hash");
    }
    if expected.cassette_version != current.cassette_version {
        mismatches.push("cassette_version");
    }
    if expected.fixture_sha256 != current.fixture_sha256 {
        mismatches.push("fixture_sha256");
    }
    if expected.git_commit != current.git_commit {
        mismatches.push("git_commit");
    }
    if expected.mode != current.mode {
        mismatches.push("mode");
    }
    mismatches
}

fn sanitize_artifact_part(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn current_git_commit() -> Option<String> {
    let git_dir = resolve_git_dir(Path::new(env!("CARGO_MANIFEST_DIR")))?;
    let head_path = git_dir.join("HEAD");
    let head = std::fs::read_to_string(head_path).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        for candidate in git_ref_candidates(&git_dir, reference) {
            if let Ok(hash) = std::fs::read_to_string(candidate)
                && let Some(hash) = normalize_git_hash(&hash)
            {
                return Some(hash);
            }
        }
        return read_packed_ref(&git_dir, reference);
    }
    normalize_git_hash(head)
}

fn resolve_git_dir(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let git_file = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = git_file.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = PathBuf::from(git_dir);
    if git_dir.is_absolute() {
        Some(git_dir)
    } else {
        Some(repo_root.join(git_dir))
    }
}

fn git_ref_candidates(git_dir: &Path, reference: &str) -> Vec<PathBuf> {
    let mut candidates = vec![git_dir.join(reference)];
    if let Some(common_dir) = common_git_dir(git_dir) {
        candidates.push(common_dir.join(reference));
    }
    candidates
}

fn common_git_dir(git_dir: &Path) -> Option<PathBuf> {
    let common_dir = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common_dir = PathBuf::from(common_dir.trim());
    if common_dir.is_absolute() {
        Some(common_dir)
    } else {
        Some(git_dir.join(common_dir))
    }
}

fn read_packed_ref(git_dir: &Path, reference: &str) -> Option<String> {
    for packed_ref_path in git_ref_candidates(git_dir, "packed-refs") {
        let Ok(packed_refs) = std::fs::read_to_string(packed_ref_path) else {
            continue;
        };
        for line in packed_refs.lines() {
            let mut fields = line.split_whitespace();
            let Some(hash) = fields.next() else {
                continue;
            };
            let Some(packed_reference) = fields.next() else {
                continue;
            };
            if packed_reference == reference {
                return normalize_git_hash(hash);
            }
        }
    }
    None
}

fn normalize_git_hash(raw: &str) -> Option<String> {
    let hash = raw.trim();
    if hash.len() >= 7 && hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Some(hash.to_string())
    } else {
        None
    }
}

pub(crate) struct StreamOutcome {
    pub events: Vec<StreamEvent>,
    pub stream_error: Option<String>,
}

pub(crate) async fn collect_events<S>(mut stream: S) -> StreamOutcome
where
    S: Stream<Item = pi::PiResult<StreamEvent>> + Unpin,
{
    let mut events = Vec::new();
    let mut stream_error = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => events.push(event),
            Err(err) => {
                stream_error = Some(err.to_string());
                break;
            }
        }
    }
    StreamOutcome {
        events,
        stream_error,
    }
}

pub(crate) struct StreamSummary {
    pub timeline: Vec<String>,
    pub event_count: usize,
    pub has_start: bool,
    pub has_done: bool,
    pub has_error_event: bool,
    pub text: String,
    pub thinking: String,
    pub tool_calls: Vec<ToolCall>,
    pub text_deltas: usize,
    pub thinking_deltas: usize,
    pub tool_call_deltas: usize,
    pub stop_reason: Option<StopReason>,
    pub stream_error: Option<String>,
}

pub(crate) fn summarize_events(outcome: &StreamOutcome) -> StreamSummary {
    let mut summary = StreamSummary {
        timeline: Vec::new(),
        event_count: outcome.events.len(),
        has_start: false,
        has_done: false,
        has_error_event: false,
        text: String::new(),
        thinking: String::new(),
        tool_calls: Vec::new(),
        text_deltas: 0,
        thinking_deltas: 0,
        tool_call_deltas: 0,
        stop_reason: None,
        stream_error: outcome.stream_error.clone(),
    };

    for event in &outcome.events {
        match event {
            StreamEvent::Start { .. } => {
                summary.has_start = true;
                summary.timeline.push("start".to_string());
            }
            StreamEvent::TextStart { .. } => {
                summary.timeline.push("text_start".to_string());
            }
            StreamEvent::TextDelta { delta, .. } => {
                summary.text_deltas += 1;
                summary.text.push_str(delta);
                summary.timeline.push("text_delta".to_string());
            }
            StreamEvent::TextEnd { content, .. } => {
                summary.text.clone_from(content);
                summary.timeline.push("text_end".to_string());
            }
            StreamEvent::ThinkingStart { .. } => {
                summary.timeline.push("thinking_start".to_string());
            }
            StreamEvent::ThinkingDelta { delta, .. } => {
                summary.thinking_deltas += 1;
                summary.thinking.push_str(delta);
                summary.timeline.push("thinking_delta".to_string());
            }
            StreamEvent::ThinkingEnd { content, .. } => {
                summary.thinking.clone_from(content);
                summary.timeline.push("thinking_end".to_string());
            }
            StreamEvent::ToolCallStart { .. } => {
                summary.timeline.push("tool_call_start".to_string());
            }
            StreamEvent::ToolCallDelta { .. } => {
                summary.tool_call_deltas += 1;
                summary.timeline.push("tool_call_delta".to_string());
            }
            StreamEvent::ToolCallEnd { tool_call, .. } => {
                summary.tool_calls.push(tool_call.clone());
                summary.timeline.push("tool_call_end".to_string());
            }
            StreamEvent::Done { reason, .. } => {
                summary.has_done = true;
                summary.stop_reason = Some(*reason);
                summary.timeline.push("done".to_string());
            }
            StreamEvent::Error { reason, .. } => {
                summary.has_error_event = true;
                summary.stop_reason = Some(*reason);
                summary.timeline.push("error".to_string());
            }
        }
    }

    summary
}

pub(crate) fn log_summary(harness: &TestHarness, scenario: &str, summary: &StreamSummary) {
    harness.log().info_ctx("stream", "Stream summary", |ctx| {
        ctx.push(("scenario".into(), scenario.to_string()));
        ctx.push(("events".into(), summary.event_count.to_string()));
        ctx.push(("text_deltas".into(), summary.text_deltas.to_string()));
        ctx.push((
            "thinking_deltas".into(),
            summary.thinking_deltas.to_string(),
        ));
        ctx.push(("tool_calls".into(), summary.tool_calls.len().to_string()));
        if let Some(reason) = summary.stop_reason {
            ctx.push(("stop_reason".into(), format!("{reason:?}")));
        }
        if let Some(error) = &summary.stream_error {
            ctx.push(("stream_error".into(), error.clone()));
        }
    });
    if !summary.timeline.is_empty() {
        harness.log().info(
            "timeline",
            format!("{scenario}: {}", summary.timeline.join(" -> ")),
        );
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct StreamExpectations {
    pub min_text_deltas: usize,
    pub min_thinking_deltas: usize,
    pub min_tool_calls: usize,
    pub allowed_stop_reasons: Option<Vec<StopReason>>,
    pub require_blank_line: bool,
    pub require_unicode: bool,
    pub min_tool_args_bytes: Option<usize>,
    pub allow_stream_error: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ErrorExpectation {
    pub status: u16,
    pub contains: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub(crate) enum ScenarioExpectation {
    Stream(StreamExpectations),
    Error(ErrorExpectation),
}

pub(crate) fn assert_stream_expectations(
    harness: &TestHarness,
    scenario: &str,
    summary: &StreamSummary,
    expectations: &StreamExpectations,
) {
    if !expectations.allow_stream_error {
        harness.assert_log("assert no stream error");
        assert!(
            summary.stream_error.is_none(),
            "{scenario}: unexpected stream error {:?}",
            summary.stream_error
        );
    }

    if summary.event_count > 0 {
        harness.assert_log("assert stream start");
        assert!(summary.has_start, "{scenario}: missing start event");
    }

    if expectations.min_text_deltas > 0 {
        harness.assert_log("assert text deltas");
        assert!(
            summary.text_deltas >= expectations.min_text_deltas,
            "{scenario}: expected >= {} text deltas, got {}",
            expectations.min_text_deltas,
            summary.text_deltas
        );
    }

    if expectations.min_thinking_deltas > 0 {
        harness.assert_log("assert thinking deltas");
        assert!(
            summary.thinking_deltas >= expectations.min_thinking_deltas,
            "{scenario}: expected >= {} thinking deltas, got {}",
            expectations.min_thinking_deltas,
            summary.thinking_deltas
        );
    }

    if expectations.min_tool_calls > 0 {
        harness.assert_log("assert tool calls");
        assert!(
            summary.tool_calls.len() >= expectations.min_tool_calls,
            "{scenario}: expected >= {} tool calls, got {}",
            expectations.min_tool_calls,
            summary.tool_calls.len()
        );
    }

    if let Some(min_bytes) = expectations.min_tool_args_bytes {
        harness.assert_log("assert tool args size");
        let max_args = summary
            .tool_calls
            .iter()
            .filter_map(|call| serde_json::to_vec(&call.arguments).ok().map(|v| v.len()))
            .max()
            .unwrap_or(0);
        assert!(
            max_args >= min_bytes,
            "{scenario}: expected tool args >= {min_bytes} bytes, got {max_args}"
        );
    }

    if expectations.require_blank_line {
        harness.assert_log("assert blank line");
        assert!(
            summary.text.contains("\n\n"),
            "{scenario}: expected blank line in text"
        );
    }

    if expectations.require_unicode {
        harness.assert_log("assert unicode");
        let has_unicode = !summary.text.is_ascii();
        assert!(has_unicode, "{scenario}: expected unicode in text");
    }

    if let Some(allowed) = &expectations.allowed_stop_reasons {
        harness.assert_log("assert stop reason");
        let Some(reason) = summary.stop_reason else {
            panic!("{scenario}: missing stop reason");
        };
        assert!(
            allowed.contains(&reason),
            "{scenario}: expected stop reason in {allowed:?}, got {reason:?}"
        );
    }
}

pub(crate) fn assert_tool_schema_fidelity(
    harness: &TestHarness,
    scenario: &str,
    tool_defs: &[ToolDef],
    tool_calls: &[ToolCall],
) {
    if tool_calls.is_empty() {
        return;
    }

    for tool_call in tool_calls {
        let Some(tool_def) = tool_defs.iter().find(|tool| tool.name == tool_call.name) else {
            panic!(
                "{scenario}: tool call '{}' has no matching schema definition",
                tool_call.name
            );
        };
        let validator = jsonschema::draft202012::options()
            .should_validate_formats(true)
            .build(&tool_def.parameters)
            .unwrap_or_else(|err| {
                panic!(
                    "{scenario}: invalid JSON schema for tool '{}': {err}",
                    tool_call.name
                )
            });
        if let Err(err) = validator.validate(&tool_call.arguments) {
            panic!(
                "{scenario}: tool '{}' arguments failed schema validation: {err}; arguments={}",
                tool_call.name, tool_call.arguments
            );
        }
    }

    harness
        .log()
        .info_ctx("contract", "Tool schema fidelity verified", |ctx| {
            ctx.push(("scenario".into(), scenario.to_string()));
            ctx.push(("tool_calls".into(), tool_calls.len().to_string()));
        });
}

pub(crate) fn record_stream_contract_artifact(
    harness: &TestHarness,
    provider: &str,
    scenario: &str,
    description: &str,
    summary: &StreamSummary,
) {
    let file_name = format!("{provider}_{scenario}.contract.json");
    let path = harness.temp_path(&file_name);
    let payload = json!({
        "schema": "pi.test.provider_contract.v1",
        "provider": provider,
        "scenario": scenario,
        "description": description,
        "event_count": summary.event_count,
        "has_start": summary.has_start,
        "has_done": summary.has_done,
        "has_error_event": summary.has_error_event,
        "timeline": &summary.timeline,
        "stop_reason": summary.stop_reason.as_ref().map(|reason| format!("{reason:?}")),
        "text_sha256": sha256_hex(summary.text.as_bytes()),
        "thinking_sha256": sha256_hex(summary.thinking.as_bytes()),
        "text_chars": summary.text.chars().count(),
        "thinking_chars": summary.thinking.chars().count(),
        "tool_call_count": summary.tool_calls.len(),
        "tool_call_ids": summary.tool_calls.iter().map(|call| call.id.clone()).collect::<Vec<_>>(),
        "tool_call_names": summary.tool_calls.iter().map(|call| call.name.clone()).collect::<Vec<_>>(),
        "stream_error": summary.stream_error.as_deref(),
    });
    let serialized = serde_json::to_string_pretty(&payload)
        .unwrap_or_else(|_| "{\"schema\":\"serialization_error\"}".to_string());
    std::fs::write(&path, serialized)
        .unwrap_or_else(|err| panic!("write stream contract artifact {}: {err}", path.display()));
    harness.record_artifact(format!("contract/{file_name}"), &path);
}

pub(crate) fn assert_error_translation(
    harness: &TestHarness,
    provider: &str,
    scenario: &str,
    description: &str,
    expectation: &ErrorExpectation,
    message: &str,
) {
    let needle = format!("HTTP {}", expectation.status);
    assert!(
        message.contains(&needle),
        "{scenario}: expected error to contain '{needle}', got '{message}'"
    );
    if let Some(fragment) = expectation.contains {
        assert!(
            message.contains(fragment),
            "{scenario}: expected error to contain '{fragment}', got '{message}'"
        );
    }
    harness.log().info("error", message);

    let file_name = format!("{provider}_{scenario}.error-contract.json");
    let path = harness.temp_path(&file_name);
    let payload = json!({
        "schema": "pi.test.provider_error_translation.v1",
        "provider": provider,
        "scenario": scenario,
        "description": description,
        "expected_status": expectation.status,
        "expected_fragment": expectation.contains,
        "message": message,
        "contains_http_status": message.contains(&needle),
    });
    let serialized = serde_json::to_string_pretty(&payload)
        .unwrap_or_else(|_| "{\"schema\":\"serialization_error\"}".to_string());
    std::fs::write(&path, serialized)
        .unwrap_or_else(|err| panic!("write error translation artifact {}: {err}", path.display()));
    harness.record_artifact(format!("contract/{file_name}"), &path);
}

pub(crate) fn user_text(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

pub(crate) fn assistant_tool_call_message(
    api: &str,
    provider: &str,
    model: &str,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> Message {
    Message::assistant(AssistantMessage {
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
            thought_signature: None,
        })],
        api: api.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        stop_details: None,
        error_message: None,
        timestamp: 0,
    })
}

pub(crate) fn tool_result_message(
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
    is_error: bool,
) -> Message {
    Message::ToolResult(std::sync::Arc::new(ToolResultMessage {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![ContentBlock::Text(pi::model::TextContent::new(
            content.to_string(),
        ))],
        details: None,
        is_error,
        timestamp: 0,
    }))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod backpressure_tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};

    const BACKPRESSURE_SCHEMA: &str = "pi.test.provider_stream_backpressure.v1";
    const PRESSURE_QUEUE_CAP: usize = 6;
    const SLOW_CONSUMER_DRAIN_EVERY: usize = 4;

    #[derive(Clone)]
    enum ProviderPressureChunk {
        Event(Box<StreamEvent>),
        RetryableError {
            class: &'static str,
            message: &'static str,
        },
        MalformedChunk {
            class: &'static str,
            message: &'static str,
        },
    }

    struct ProviderPressureFixture {
        provider: &'static str,
        fixture: &'static str,
        expected_text: String,
        expected_usage: Option<Usage>,
        expected_stop_reason: Option<StopReason>,
        expect_tool_call: bool,
        expected_error_class: Option<&'static str>,
        chunks: Vec<ProviderPressureChunk>,
    }

    #[derive(Default)]
    struct BackpressureReplay {
        events: Vec<StreamEvent>,
        chunk_count: usize,
        semantic_count: usize,
        coalesced_or_buffered_count: usize,
        max_queue_depth: usize,
        latency_steps: usize,
        retryable_error_count: usize,
        malformed_chunk_count: usize,
        error_classification: Option<&'static str>,
        error_message: Option<String>,
    }

    struct PressureEventSummary<'a> {
        event_count: usize,
        has_start: bool,
        has_done: bool,
        text: String,
        tool_call_deltas: usize,
        stop_reason: Option<StopReason>,
        stream_error: Option<&'a str>,
    }

    fn pressure_usage(seed: u64) -> Usage {
        Usage {
            input: 100 + seed,
            output: 20 + seed,
            cache_read: seed,
            cache_write: 0,
            total_tokens: 120 + (seed * 2),
            cost: Cost::default(),
        }
    }

    fn event(event: StreamEvent) -> ProviderPressureChunk {
        ProviderPressureChunk::Event(Box::new(event))
    }

    fn base_assistant(provider: &str, model: &str) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "stream".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        }
    }

    fn final_assistant(
        provider: &str,
        model: &str,
        text: &str,
        tool_call: ToolCall,
        usage: Usage,
    ) -> AssistantMessage {
        AssistantMessage {
            content: vec![
                ContentBlock::Text(pi::model::TextContent::new(text.to_string())),
                ContentBlock::ToolCall(tool_call),
            ],
            api: "stream".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            usage,
            stop_reason: StopReason::ToolUse,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        }
    }

    fn streaming_pressure_fixture(
        provider: &'static str,
        fixture: &'static str,
        model: &'static str,
        seed: u64,
    ) -> ProviderPressureFixture {
        let tool_call = ToolCall {
            id: format!("{provider}-tool-call"),
            name: "echo".to_string(),
            arguments: json!({ "query": format!("{provider}-pressure") }),
            thought_signature: None,
        };
        let text_deltas = [
            format!("{provider} "),
            "stream ".to_string(),
            "pressure ".to_string(),
            "keeps ".to_string(),
            "semantic ".to_string(),
            "chunks ".to_string(),
            "while ".to_string(),
            "coalescing ".to_string(),
            "low-value ".to_string(),
            "text ".to_string(),
            "deltas.".to_string(),
        ];
        let expected_text = text_deltas.concat();
        let usage = pressure_usage(seed);
        let mut chunks = vec![
            ProviderPressureChunk::RetryableError {
                class: "retryable_stream_error",
                message: "SSE poll returned WouldBlock and recovered",
            },
            event(StreamEvent::Start {
                partial: base_assistant(provider, model),
            }),
            event(StreamEvent::TextStart { content_index: 0 }),
        ];
        chunks.extend(text_deltas.iter().map(|delta| {
            event(StreamEvent::TextDelta {
                content_index: 0,
                delta: delta.clone(),
            })
        }));
        chunks.extend([
            event(StreamEvent::TextEnd {
                content_index: 0,
                content: expected_text.clone(),
            }),
            event(StreamEvent::ToolCallStart {
                content_index: 1,
                id: tool_call.id.clone(),
                name: tool_call.name.clone(),
            }),
            event(StreamEvent::ToolCallDelta {
                content_index: 1,
                delta: "{\"query\":\"".to_string(),
            }),
            event(StreamEvent::ToolCallDelta {
                content_index: 1,
                delta: format!("{provider}-pressure"),
            }),
            event(StreamEvent::ToolCallDelta {
                content_index: 1,
                delta: "\"}".to_string(),
            }),
            event(StreamEvent::ToolCallEnd {
                content_index: 1,
                tool_call: tool_call.clone(),
            }),
            event(StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: final_assistant(provider, model, &expected_text, tool_call, usage.clone()),
            }),
        ]);

        ProviderPressureFixture {
            provider,
            fixture,
            expected_text,
            expected_usage: Some(usage),
            expected_stop_reason: Some(StopReason::ToolUse),
            expect_tool_call: true,
            expected_error_class: None,
            chunks,
        }
    }

    fn malformed_pressure_fixture() -> ProviderPressureFixture {
        ProviderPressureFixture {
            provider: "openai-responses",
            fixture: "openai_responses_malformed_chunk_fail_closed",
            expected_text: String::new(),
            expected_usage: None,
            expected_stop_reason: None,
            expect_tool_call: false,
            expected_error_class: Some("malformed_chunk"),
            chunks: vec![
                ProviderPressureChunk::RetryableError {
                    class: "retryable_stream_error",
                    message: "SSE poll returned TimedOut and recovered",
                },
                ProviderPressureChunk::MalformedChunk {
                    class: "malformed_chunk",
                    message: "SSE error: JSON parse error: expected value at line 1 column 1",
                },
            ],
        }
    }

    fn native_pressure_fixtures() -> Vec<ProviderPressureFixture> {
        let mut fixtures = [
            (
                1,
                "anthropic",
                "anthropic_tool_use_backpressure",
                "claude-3-5-haiku-latest",
            ),
            (
                2,
                "azure-openai",
                "azure_openai_tool_use_backpressure",
                "gpt-4o-mini",
            ),
            (3, "cohere", "cohere_tool_use_backpressure", "command-r"),
            (
                4,
                "gemini",
                "gemini_tool_use_backpressure",
                "gemini-1.5-flash",
            ),
            (5, "openai", "openai_tool_use_backpressure", "gpt-4o-mini"),
            (
                6,
                "openai-responses",
                "openai_responses_tool_use_backpressure",
                "gpt-4o-mini",
            ),
        ]
        .into_iter()
        .map(|(seed, provider, fixture, model)| {
            streaming_pressure_fixture(provider, fixture, model, seed)
        })
        .collect::<Vec<_>>();
        fixtures.push(malformed_pressure_fixture());
        fixtures
    }

    fn is_coalescible_event(event: &StreamEvent) -> bool {
        matches!(
            event,
            StreamEvent::TextDelta { .. } | StreamEvent::ThinkingDelta { .. }
        )
    }

    fn is_semantic_event(event: &StreamEvent) -> bool {
        !matches!(
            event,
            StreamEvent::TextDelta { .. } | StreamEvent::ThinkingDelta { .. }
        )
    }

    fn push_queue(
        queue: &mut VecDeque<StreamEvent>,
        replay: &mut BackpressureReplay,
        event: StreamEvent,
    ) {
        queue.push_back(event);
        replay.max_queue_depth = replay.max_queue_depth.max(queue.len());
    }

    fn flush_coalesced(
        queue: &mut VecDeque<StreamEvent>,
        pending_text: &mut BTreeMap<usize, String>,
        pending_thinking: &mut BTreeMap<usize, String>,
        replay: &mut BackpressureReplay,
    ) {
        for (content_index, delta) in std::mem::take(pending_text) {
            push_queue(
                queue,
                replay,
                StreamEvent::TextDelta {
                    content_index,
                    delta,
                },
            );
        }
        for (content_index, delta) in std::mem::take(pending_thinking) {
            push_queue(
                queue,
                replay,
                StreamEvent::ThinkingDelta {
                    content_index,
                    delta,
                },
            );
        }
    }

    fn enqueue_or_coalesce(
        queue: &mut VecDeque<StreamEvent>,
        pending_text: &mut BTreeMap<usize, String>,
        pending_thinking: &mut BTreeMap<usize, String>,
        replay: &mut BackpressureReplay,
        event: StreamEvent,
    ) {
        match event {
            StreamEvent::TextDelta {
                content_index,
                delta,
            } if pending_text.contains_key(&content_index) || queue.len() >= PRESSURE_QUEUE_CAP => {
                pending_text
                    .entry(content_index)
                    .or_default()
                    .push_str(&delta);
                replay.coalesced_or_buffered_count += 1;
            }
            StreamEvent::ThinkingDelta {
                content_index,
                delta,
            } if pending_thinking.contains_key(&content_index)
                || queue.len() >= PRESSURE_QUEUE_CAP =>
            {
                pending_thinking
                    .entry(content_index)
                    .or_default()
                    .push_str(&delta);
                replay.coalesced_or_buffered_count += 1;
            }
            event => push_queue(queue, replay, event),
        }
    }

    fn drain_one(
        queue: &mut VecDeque<StreamEvent>,
        output: &mut Vec<StreamEvent>,
        replay: &mut BackpressureReplay,
    ) {
        if let Some(event) = queue.pop_front() {
            output.push(event);
            replay.latency_steps += 1;
        }
    }

    fn drain_all(
        queue: &mut VecDeque<StreamEvent>,
        output: &mut Vec<StreamEvent>,
        replay: &mut BackpressureReplay,
    ) {
        while !queue.is_empty() {
            drain_one(queue, output, replay);
        }
    }

    fn replay_under_pressure(fixture: &ProviderPressureFixture) -> BackpressureReplay {
        let mut replay = BackpressureReplay::default();
        let mut queue = VecDeque::new();
        let mut pending_text = BTreeMap::new();
        let mut pending_thinking = BTreeMap::new();
        let mut output = Vec::new();
        let chunks = fixture.chunks.clone();

        for chunk in chunks {
            replay.chunk_count += 1;
            replay.latency_steps += 1;
            match chunk {
                ProviderPressureChunk::Event(event) => {
                    let event = *event;
                    if !is_coalescible_event(&event) {
                        flush_coalesced(
                            &mut queue,
                            &mut pending_text,
                            &mut pending_thinking,
                            &mut replay,
                        );
                    }
                    if is_semantic_event(&event) {
                        replay.semantic_count += 1;
                        if !queue.is_empty() {
                            replay.coalesced_or_buffered_count += 1;
                        }
                    }
                    enqueue_or_coalesce(
                        &mut queue,
                        &mut pending_text,
                        &mut pending_thinking,
                        &mut replay,
                        event,
                    );
                }
                ProviderPressureChunk::RetryableError { class, message } => {
                    assert_eq!(class, "retryable_stream_error");
                    assert!(
                        message.contains("WouldBlock") || message.contains("TimedOut"),
                        "{}: retryable fixture must identify the transient condition",
                        fixture.fixture
                    );
                    replay.retryable_error_count += 1;
                }
                ProviderPressureChunk::MalformedChunk { class, message } => {
                    flush_coalesced(
                        &mut queue,
                        &mut pending_text,
                        &mut pending_thinking,
                        &mut replay,
                    );
                    drain_all(&mut queue, &mut output, &mut replay);
                    replay.malformed_chunk_count += 1;
                    replay.error_classification = Some(class);
                    replay.error_message = Some(message.to_string());
                    break;
                }
            }

            if replay.chunk_count % SLOW_CONSUMER_DRAIN_EVERY == 0 {
                drain_one(&mut queue, &mut output, &mut replay);
            }
        }

        flush_coalesced(
            &mut queue,
            &mut pending_text,
            &mut pending_thinking,
            &mut replay,
        );
        drain_all(&mut queue, &mut output, &mut replay);
        replay.events = output;
        replay
    }

    fn final_usage(events: &[StreamEvent]) -> Option<&Usage> {
        events.iter().rev().find_map(|event| match event {
            StreamEvent::Done { message, .. } => Some(&message.usage),
            StreamEvent::Error { error, .. } => Some(&error.usage),
            _ => None,
        })
    }

    fn assert_usage_matches(provider: &str, fixture: &str, actual: &Usage, expected: &Usage) {
        assert_eq!(
            actual.input, expected.input,
            "{provider}/{fixture}: input usage"
        );
        assert_eq!(
            actual.output, expected.output,
            "{provider}/{fixture}: output usage"
        );
        assert_eq!(
            actual.total_tokens, expected.total_tokens,
            "{provider}/{fixture}: total usage"
        );
        assert_eq!(
            actual.cache_read, expected.cache_read,
            "{provider}/{fixture}: cache read usage"
        );
    }

    fn assert_tool_call_boundary(fixture: &ProviderPressureFixture, events: &[StreamEvent]) {
        let start = events
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallStart { .. }));
        assert!(
            start.is_some(),
            "{}: missing tool-call start",
            fixture.fixture
        );
        let Some(start) = start else {
            return;
        };
        let first_delta = events
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallDelta { .. }));
        assert!(
            first_delta.is_some(),
            "{}: missing tool-call delta",
            fixture.fixture
        );
        let Some(first_delta) = first_delta else {
            return;
        };
        let last_delta = events
            .iter()
            .rposition(|event| matches!(event, StreamEvent::ToolCallDelta { .. }));
        assert!(
            last_delta.is_some(),
            "{}: missing tool-call delta",
            fixture.fixture
        );
        let Some(last_delta) = last_delta else {
            return;
        };
        let end = events
            .iter()
            .position(|event| matches!(event, StreamEvent::ToolCallEnd { .. }));
        assert!(end.is_some(), "{}: missing tool-call end", fixture.fixture);
        let Some(end) = end else {
            return;
        };
        let done = events
            .iter()
            .position(|event| matches!(event, StreamEvent::Done { .. }));
        assert!(done.is_some(), "{}: missing done event", fixture.fixture);
        let Some(done) = done else {
            return;
        };

        assert!(
            start < first_delta && last_delta < end && end < done,
            "{}: tool-call boundary ordering was lost",
            fixture.fixture
        );

        let tool_call = events.iter().find_map(|event| match event {
            StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
            _ => None,
        });
        assert!(
            tool_call.is_some(),
            "{}: tool-call end should carry a tool call",
            fixture.fixture
        );
        let Some(tool_call) = tool_call else {
            return;
        };
        assert_eq!(tool_call.name, "echo");
        assert_eq!(
            tool_call.arguments,
            json!({ "query": format!("{}-pressure", fixture.provider) })
        );
    }

    fn usage_json(usage: &Usage) -> Value {
        json!({
            "input": usage.input,
            "output": usage.output,
            "cacheRead": usage.cache_read,
            "cacheWrite": usage.cache_write,
            "totalTokens": usage.total_tokens,
        })
    }

    fn pressure_report_record(
        fixture: &ProviderPressureFixture,
        replay: &BackpressureReplay,
        summary: &PressureEventSummary<'_>,
    ) -> Value {
        let verdict = if fixture.expected_error_class.is_some() {
            "fail_closed"
        } else {
            "pass"
        };
        json!({
            "schema": BACKPRESSURE_SCHEMA,
            "provider": fixture.provider,
            "fixture": fixture.fixture,
            "chunk_count": replay.chunk_count,
            "semantic_count": replay.semantic_count,
            "coalesced_or_buffered_count": replay.coalesced_or_buffered_count,
            "max_queue_depth": replay.max_queue_depth,
            "latency_steps": replay.latency_steps,
            "retryable_error_count": replay.retryable_error_count,
            "malformed_chunk_count": replay.malformed_chunk_count,
            "event_count": summary.event_count,
            "tool_call_delta_count": summary.tool_call_deltas,
            "stop_reason": summary.stop_reason.as_ref().map(|reason| format!("{reason:?}")),
            "usage": final_usage(&replay.events).map(usage_json),
            "error_classification": replay.error_classification,
            "stream_error": replay.error_message.as_deref(),
            "verdict": verdict,
        })
    }

    fn summarize_pressure_events<'a>(
        events: &[StreamEvent],
        stream_error: Option<&'a str>,
    ) -> PressureEventSummary<'a> {
        let mut summary = PressureEventSummary {
            event_count: events.len(),
            has_start: false,
            has_done: false,
            text: String::new(),
            tool_call_deltas: 0,
            stop_reason: None,
            stream_error,
        };

        for event in events {
            match event {
                StreamEvent::Start { .. } => {
                    summary.has_start = true;
                }
                StreamEvent::TextDelta { delta, .. } => {
                    summary.text.push_str(delta);
                }
                StreamEvent::TextEnd { content, .. } => {
                    summary.text.clone_from(content);
                }
                StreamEvent::ToolCallDelta { .. } => {
                    summary.tool_call_deltas += 1;
                }
                StreamEvent::Done { reason, .. } => {
                    summary.has_done = true;
                    summary.stop_reason = Some(*reason);
                }
                StreamEvent::Error { reason, .. } => {
                    summary.stop_reason = Some(*reason);
                }
                StreamEvent::ThinkingStart { .. }
                | StreamEvent::ThinkingDelta { .. }
                | StreamEvent::ThinkingEnd { .. }
                | StreamEvent::TextStart { .. }
                | StreamEvent::ToolCallStart { .. }
                | StreamEvent::ToolCallEnd { .. } => {}
            }
        }

        summary
    }

    fn assert_pressure_fixture(fixture: &ProviderPressureFixture, replay: &BackpressureReplay) {
        let summary = summarize_pressure_events(&replay.events, replay.error_message.as_deref());

        if let Some(expected_error_class) = fixture.expected_error_class {
            assert_eq!(
                replay.error_classification,
                Some(expected_error_class),
                "{}: malformed stream error classification was lost",
                fixture.fixture
            );
            assert!(
                summary.stream_error.is_some(),
                "{}: malformed stream did not fail closed with a stream error",
                fixture.fixture
            );
            assert!(
                !summary.has_done,
                "{}: malformed stream must not synthesize Done",
                fixture.fixture
            );
            return;
        }

        assert!(
            summary.stream_error.is_none(),
            "{}: unexpected stream error {:?}",
            fixture.fixture,
            summary.stream_error
        );
        assert!(
            summary.has_start,
            "{}: missing start event",
            fixture.fixture
        );
        assert!(summary.has_done, "{}: missing done event", fixture.fixture);
        assert_eq!(
            summary.text, fixture.expected_text,
            "{}: replayed text changed under pressure",
            fixture.fixture
        );
        assert_eq!(
            summary.stop_reason, fixture.expected_stop_reason,
            "{}: final stop reason was lost",
            fixture.fixture
        );
        assert!(
            replay.retryable_error_count > 0,
            "{}: retryable stream errors were not exercised",
            fixture.fixture
        );
        assert!(
            replay.coalesced_or_buffered_count > 0,
            "{}: slow-consumer pressure did not buffer or coalesce",
            fixture.fixture
        );
        assert!(
            replay.max_queue_depth >= PRESSURE_QUEUE_CAP,
            "{}: replay never reached queue pressure",
            fixture.fixture
        );

        let expected_usage = fixture.expected_usage.as_ref();
        assert!(
            expected_usage.is_some(),
            "{}: streaming fixtures carry final usage",
            fixture.fixture
        );
        let Some(expected_usage) = expected_usage else {
            return;
        };
        let usage = final_usage(&replay.events);
        assert!(usage.is_some(), "{}: missing final usage", fixture.fixture);
        let Some(usage) = usage else {
            return;
        };
        assert_usage_matches(fixture.provider, fixture.fixture, usage, expected_usage);

        if fixture.expect_tool_call {
            assert_tool_call_boundary(fixture, &replay.events);
            assert!(
                summary.tool_call_deltas >= 3,
                "{}: expected fragmented tool-call deltas",
                fixture.fixture
            );
        }
    }

    fn record_backpressure_artifact(harness: &TestHarness, records: &[Value]) {
        let artifact_path = harness.temp_path("provider_stream_backpressure.jsonl");
        let mut jsonl = String::new();
        for record in records {
            let encoded = serde_json::to_string(record);
            assert!(
                encoded.is_ok(),
                "provider stream backpressure record should serialize"
            );
            let Ok(line) = encoded else {
                continue;
            };
            let _ = writeln!(jsonl, "{line}");
        }
        let write_result = std::fs::write(&artifact_path, jsonl);
        assert!(
            write_result.is_ok(),
            "write provider stream backpressure artifact {}: {:?}",
            artifact_path.display(),
            write_result.as_ref().err()
        );
        harness.record_artifact("provider-stream-backpressure.jsonl", &artifact_path);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn provider_backpressure_replay_preserves_semantics_across_native_fixtures() {
        let harness = TestHarness::new(
            "provider_backpressure_replay_preserves_semantics_across_native_fixtures",
        );
        let mut records = Vec::new();

        for fixture in native_pressure_fixtures() {
            let replay = replay_under_pressure(&fixture);
            let summary =
                summarize_pressure_events(&replay.events, replay.error_message.as_deref());
            assert_pressure_fixture(&fixture, &replay);
            records.push(pressure_report_record(&fixture, &replay, &summary));
        }

        assert!(
            records
                .iter()
                .any(|record| record.get("verdict") == Some(&json!("fail_closed"))),
            "expected a fail-closed malformed chunk replay record"
        );
        assert!(
            records.iter().all(|record| {
                record
                    .get("retryable_error_count")
                    .and_then(Value::as_u64)
                    .is_some_and(|count| count > 0)
            }),
            "every fixture should exercise recovered retryable stream errors"
        );

        record_backpressure_artifact(&harness, &records);
    }
}

#[cfg(test)]
mod replay_cache_tests {
    use super::*;
    use pi::vcr::{Interaction, RecordedRequest, RecordedResponse};

    fn write_test_cassette(path: &Path, response: &str) {
        let cassette = Cassette {
            version: PROVIDER_REPLAY_CACHE_CASSETTE_VERSION.to_string(),
            test_name: "provider replay cache test".to_string(),
            recorded_at: "2026-05-12T00:00:00Z".to_string(),
            interactions: vec![Interaction {
                request: RecordedRequest {
                    method: "POST".to_string(),
                    url: "https://api.example.test/v1/chat/completions".to_string(),
                    headers: Vec::new(),
                    body: Some(json!({"model": "test-model"})),
                    body_text: None,
                },
                response: RecordedResponse {
                    status: 200,
                    headers: Vec::new(),
                    body_chunks: vec![response.to_string()],
                    body_chunks_base64: None,
                },
            }],
        };
        let bytes = serde_json::to_vec_pretty(&cassette).expect("serialize test cassette");
        std::fs::write(path, bytes).expect("write test cassette");
    }

    fn test_spec<'a>(
        cassette_path: &'a Path,
        request_schema_hash: &'a str,
    ) -> ProviderReplayCacheSpec<'a> {
        ProviderReplayCacheSpec {
            provider: "openai",
            route: "POST /v1/chat/completions",
            model: "test-model",
            scenario: "cache_lineage",
            cassette_path,
            request_schema_hash,
            mode: VcrMode::Playback,
        }
    }

    #[test]
    fn provider_replay_cache_accepts_matching_lineage() {
        let harness = TestHarness::new("provider_replay_cache_accepts_matching_lineage");
        let cassette_path = harness.temp_path("matching_cassette.json");
        write_test_cassette(&cassette_path, "first-response");
        let request_schema_hash =
            provider_request_schema_hash(&[user_text("hello")], &[], &json!({"maxTokens": 16}));
        let spec = test_spec(&cassette_path, &request_schema_hash);

        let expected_entry = build_provider_replay_cache_entry(&spec).expect("build cache entry");
        let report = provider_replay_cache_report(Some(&expected_entry), &spec);

        assert_eq!(
            report.get("schema"),
            Some(&json!(PROVIDER_REPLAY_CACHE_SCHEMA))
        );
        assert_eq!(report.get("verdict"), Some(&json!("hit")));
        assert_eq!(report.get("cacheReusable"), Some(&json!(true)));
        assert_eq!(report.get("failClosed"), Some(&json!(false)));
        assert_eq!(
            report
                .pointer("/currentEntry/fixtureSha256")
                .and_then(Value::as_str)
                .map(str::len),
            Some(64)
        );
        assert_eq!(
            report.get("cacheKey").and_then(Value::as_str).map(str::len),
            Some(64)
        );
    }

    #[test]
    fn provider_replay_cache_reports_missing_cassette_as_miss() {
        let harness = TestHarness::new("provider_replay_cache_reports_missing_cassette_as_miss");
        let cassette_path = harness.temp_path("missing_cassette.json");
        let request_schema_hash =
            provider_request_schema_hash(&[user_text("hello")], &[], &json!({"maxTokens": 16}));
        let spec = test_spec(&cassette_path, &request_schema_hash);

        let report = record_provider_replay_cache_artifact(&harness, &spec);

        assert_eq!(report.get("verdict"), Some(&json!("miss")));
        assert_eq!(report.get("cacheReusable"), Some(&json!(false)));
        assert_eq!(report.get("failClosed"), Some(&json!(false)));
        assert_eq!(
            report.pointer("/refusal/class"),
            Some(&json!("missing_cassette"))
        );
    }

    #[test]
    fn provider_replay_cache_rejects_stale_fixture_hash_fail_closed() {
        let harness =
            TestHarness::new("provider_replay_cache_rejects_stale_fixture_hash_fail_closed");
        let cassette_path = harness.temp_path("stale_cassette.json");
        write_test_cassette(&cassette_path, "first-response");
        let request_schema_hash =
            provider_request_schema_hash(&[user_text("hello")], &[], &json!({"maxTokens": 16}));
        let spec = test_spec(&cassette_path, &request_schema_hash);
        let expected_entry = build_provider_replay_cache_entry(&spec).expect("build cache entry");

        write_test_cassette(&cassette_path, "changed-response");
        let report = provider_replay_cache_report(Some(&expected_entry), &spec);
        assert_eq!(report.get("verdict"), Some(&json!("stale")));
        assert_eq!(report.get("cacheReusable"), Some(&json!(false)));
        assert_eq!(report.get("failClosed"), Some(&json!(true)));
        assert!(
            report
                .get("mismatches")
                .and_then(Value::as_array)
                .is_some_and(|mismatches| {
                    mismatches.iter().any(|value| value == "fixture_sha256")
                })
        );
    }

    #[test]
    fn provider_replay_cache_rejects_ambiguous_lineage_fail_closed() {
        let harness =
            TestHarness::new("provider_replay_cache_rejects_ambiguous_lineage_fail_closed");
        let cassette_path = harness.temp_path("ambiguous_cassette.json");
        write_test_cassette(&cassette_path, "first-response");
        let spec = test_spec(&cassette_path, "");

        let report = provider_replay_cache_report(None, &spec);

        assert_eq!(report.get("verdict"), Some(&json!("stale")));
        assert_eq!(report.get("cacheReusable"), Some(&json!(false)));
        assert_eq!(report.get("failClosed"), Some(&json!(true)));
        assert_eq!(
            report.pointer("/refusal/class"),
            Some(&json!("ambiguous_request_schema"))
        );
    }
}
