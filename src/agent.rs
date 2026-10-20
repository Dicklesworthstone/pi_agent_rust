//! Agent runtime - the core orchestration loop.
//!
//! The agent coordinates between:
//! - Provider: Makes LLM API calls
//! - Tools: Executes tool calls from the assistant
//! - Session: Persists conversation history
//!
//! The main loop:
//! 1. Receive user input
//! 2. Build context (system prompt + history + tools)
//! 3. Stream completion from provider
//! 4. If tool calls: execute tools, append results, goto 3
//! 5. If done: return final message

use crate::auth::AuthStorage;
use crate::compaction::{self, ResolvedCompactionSettings};
use crate::compaction_worker::{
    CompactionAdmissionReason, CompactionAdmissionSignals, CompactionOrigin, CompactionQuota,
    CompactionWorkerState,
};
use crate::error::{Error, Result};
use crate::extension_events::{
    BeforeAgentStartOutcome, InputEventOutcome, SessionBeforeCompactOutcome,
    apply_before_agent_start_response, apply_input_event_response,
    apply_session_before_compact_response,
};
use crate::extension_tools::collect_extension_tool_wrappers;
use crate::extensions::{
    EXTENSION_EVENT_TIMEOUT_MS, ExtensionAiCompletionRequest, ExtensionCompactionOptions,
    ExtensionDeliverAs, ExtensionEventName, ExtensionHostActions, ExtensionLoadSpec,
    ExtensionManager, ExtensionPolicy, ExtensionRegion, ExtensionRuntimeHandle,
    ExtensionSendMessage, ExtensionSendUserMessage, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
    NativeRustExtensionLoadSpec, NativeRustExtensionRuntimeHandle, RepairPolicyMode,
    SessionActionOrigin, SessionActionOriginSource, resolve_extension_load_spec,
};
#[cfg(feature = "wasm-host")]
use crate::extensions::{WasmExtensionHost, WasmExtensionLoadSpec};
use crate::extensions_js::{PiJsRuntimeConfig, RepairMode};
use crate::model::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, CustomMessage, ImageContent, Message,
    StopReason, StreamEvent, TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage,
    UserContent, UserMessage,
};
use crate::models::{
    ModelEntry, ModelRegistry, model_requires_configured_credential, normalize_api_key_opt,
};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::semantic_workspace_graph::{ContextBundleItem, SemanticContextBundle};
use crate::session::{AutosaveFlushTrigger, Session, SessionHandle};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolRegistry, ToolUpdate};
use asupersync::runtime::{Runtime, RuntimeBuilder, RuntimeHandle};
use asupersync::sync::{Mutex, Notify, OwnedMutexGuard};
use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::warn;

const MIN_COMPATIBLE_TOOL_PARALLELISM: usize = 8;
const MAX_AUTO_COMPATIBLE_TOOL_PARALLELISM: usize = 64;
const MAX_CONFIGURED_COMPATIBLE_TOOL_PARALLELISM: usize = 256;
/// Maximum messages in steering queue to prevent unbounded growth
const MAX_STEERING_QUEUE_SIZE: usize = 100;
/// Maximum messages in follow-up queue to prevent unbounded growth
const MAX_FOLLOW_UP_QUEUE_SIZE: usize = 100;
/// Maximum messages in agent history to prevent unbounded growth
const MAX_AGENT_MESSAGES: usize = 10_000;
/// Schema identifier for per-turn latency budget breakdowns.
pub const TURN_LATENCY_BREAKDOWN_SCHEMA_V1: &str = "pi.agent.turn_latency_breakdown.v1";
/// Schema identifier for deterministic tool-effect batch plan evidence.
pub const TOOL_EFFECT_BATCH_PLAN_SCHEMA_V1: &str = "pi.agent.tool_effect_batch_plan.v1";
const TOOL_CANCELLATION_SCHEMA_V1: &str = "pi.tool.cancellation.v1";
const TOOL_APPROVAL_DENIED_SCHEMA_V1: &str = "pi.tool.approval_denied.v1";
const TOOL_APPROVAL_STATUS_SCHEMA_V1: &str = "pi.tool.approval_status.v1";
const SEMANTIC_CONTEXT_PROMPT_SCHEMA_V1: &str = "pi.semantic_context_prompt.v1";
const SEMANTIC_CONTEXT_PROVENANCE_SCHEMA_V1: &str = "pi.semantic_context_provenance.v1";
const SEMANTIC_CONTEXT_CUSTOM_TYPE: &str = "semantic_context_bundle";

/// Custom messages pi records for provenance only. They are persisted hidden
/// (`display: false`) so resumes and audits keep them, but their payload
/// reaches the model by another route (the semantic bundle rides the system
/// prompt), so replaying them into the provider context would only spend
/// budget. Every other custom message, hidden or not, is sent to the model:
/// `display` is a rendering flag, not a context-visibility flag.
fn context_excluded_custom_message(message: &CustomMessage) -> bool {
    !message.display && message.custom_type == SEMANTIC_CONTEXT_CUSTOM_TYPE
}
const DEFAULT_SEMANTIC_CONTEXT_PROMPT_MAX_BYTES: u64 = 16 * 1024;
const DEFAULT_SEMANTIC_CONTEXT_PROMPT_MAX_ITEMS: usize = 16;

/// Append dialect-repair audit entries using one stable shape across SDK,
/// RPC, retry, extension, and interactive execution surfaces.
pub(crate) fn append_dialect_repair_telemetry(
    session: &mut Session,
    repairs: &[crate::dialects::RepairEntry],
) {
    for entry in repairs {
        session.append_custom_entry(
            "dialect_repair".to_string(),
            Some(serde_json::json!({
                "tool": entry.tool,
                "strippedBytes": entry.stripped_bytes,
                "remainingTextBytes": entry.remaining_text_bytes,
            })),
        );
    }
}

/// Normalize a `before_provider_request` handler response into a proposed
/// request-body rewrite (gh #167 / bd-1q31s).
///
/// Accepted shapes, mirroring upstream pi: the rewritten payload object
/// directly, or `{ "payload": <object> }`. Null/undefined and non-object
/// responses mean "no rewrite".
fn normalize_before_provider_request_response(response: Value) -> Option<Value> {
    if response.is_null() {
        return None;
    }
    match response {
        Value::Object(mut object) => object.remove("payload").map_or_else(
            || Some(Value::Object(object)),
            |inner| (!inner.is_null()).then_some(inner),
        ),
        other => {
            tracing::warn!(
                "before_provider_request handler returned a non-object rewrite (ignored): {other}"
            );
            None
        }
    }
}

fn compatible_tool_parallelism_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let host_parallelism = std::thread::available_parallelism()
            .map_or(MIN_COMPATIBLE_TOOL_PARALLELISM, |parallelism| {
                parallelism.get()
            });
        resolve_compatible_tool_parallelism(
            std::env::var("PI_MAX_CONCURRENT_COMPATIBLE_TOOLS")
                .ok()
                .as_deref(),
            host_parallelism,
        )
    })
}

fn resolve_compatible_tool_parallelism(
    raw_override: Option<&str>,
    host_parallelism: usize,
) -> usize {
    let host_default = host_parallelism.clamp(
        MIN_COMPATIBLE_TOOL_PARALLELISM,
        MAX_AUTO_COMPATIBLE_TOOL_PARALLELISM,
    );

    let Some(raw) = raw_override.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return host_default;
    };

    match raw.parse::<usize>() {
        Ok(0) => {
            warn!(
                value = raw,
                "Ignoring PI_MAX_CONCURRENT_COMPATIBLE_TOOLS=0; using host-scaled default"
            );
            host_default
        }
        Ok(limit) => limit.clamp(1, MAX_CONFIGURED_COMPATIBLE_TOOL_PARALLELISM),
        Err(err) => {
            warn!(
                value = raw,
                error = %err,
                "Ignoring invalid PI_MAX_CONCURRENT_COMPATIBLE_TOOLS; using host-scaled default"
            );
            host_default
        }
    }
}

fn duration_millis_saturating(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn duration_micros_saturating(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn record_global_latency(counter: &crate::session_metrics::TimingCounter, duration: Duration) {
    if crate::session_metrics::global().enabled() {
        counter.record(duration_micros_saturating(duration));
    }
}

/// Nearest-rank tail percentile summary for a latency sample set.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LatencyPercentiles {
    /// Median latency, in milliseconds.
    pub p50_ms: u64,
    /// P95 latency, in milliseconds.
    pub p95_ms: u64,
    /// P99 latency, in milliseconds.
    pub p99_ms: u64,
    /// P99.9 latency, in milliseconds.
    pub p999_ms: u64,
}

impl LatencyPercentiles {
    fn from_samples(samples: &[u64]) -> Self {
        Self {
            p50_ms: percentile_nearest_rank(samples, 50),
            p95_ms: percentile_nearest_rank(samples, 95),
            p99_ms: percentile_nearest_rank(samples, 99),
            p999_ms: percentile_nearest_rank_per_mille(samples, 999),
        }
    }
}

/// Latency budget contribution for one component in a turn.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LatencyComponentBreakdown {
    /// Sum of all component samples in the turn, in milliseconds.
    pub duration_ms: u64,
    /// Number of samples recorded for the component in the turn.
    pub samples: usize,
    /// Tail percentiles for the component samples.
    pub tail_percentiles: LatencyPercentiles,
}

impl LatencyComponentBreakdown {
    /// Build a component breakdown from millisecond samples.
    #[must_use]
    pub fn from_millis_samples(samples: &[u64]) -> Self {
        Self {
            duration_ms: samples.iter().copied().fold(0u64, u64::saturating_add),
            samples: samples.len(),
            tail_percentiles: LatencyPercentiles::from_samples(samples),
        }
    }
}

/// Per-turn breakdown of provider, tool, extension hook, and persistence budgets.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnLatencyBreakdown {
    /// Versioned schema identifier for downstream evidence consumers.
    pub schema: &'static str,
    /// Total measured turn time in the core agent loop, in milliseconds.
    pub total_ms: u64,
    /// Provider streaming budget, including stream setup and drain time.
    pub provider_streaming: LatencyComponentBreakdown,
    /// Built-in/local tool execution budget.
    pub local_tools: LatencyComponentBreakdown,
    /// Extension hook dispatch budget around tool calls.
    pub extension_hostcalls: LatencyComponentBreakdown,
    /// Session persistence budget when measured by the current runtime path.
    pub persistence: LatencyComponentBreakdown,
    /// Component with the largest measured duration.
    pub dominant_component: String,
}

impl TurnLatencyBreakdown {
    /// Build a latency breakdown from component sample sets.
    #[must_use]
    pub fn from_component_samples(
        total_ms: u64,
        provider_streaming_ms: &[u64],
        local_tool_ms: &[u64],
        extension_hostcall_ms: &[u64],
        persistence_ms: &[u64],
    ) -> Self {
        let provider_streaming =
            LatencyComponentBreakdown::from_millis_samples(provider_streaming_ms);
        let local_tools = LatencyComponentBreakdown::from_millis_samples(local_tool_ms);
        let extension_hostcalls =
            LatencyComponentBreakdown::from_millis_samples(extension_hostcall_ms);
        let persistence = LatencyComponentBreakdown::from_millis_samples(persistence_ms);
        let dominant_component = dominant_latency_component(
            &provider_streaming,
            &local_tools,
            &extension_hostcalls,
            &persistence,
        );

        Self {
            schema: TURN_LATENCY_BREAKDOWN_SCHEMA_V1,
            total_ms,
            provider_streaming,
            local_tools,
            extension_hostcalls,
            persistence,
            dominant_component,
        }
    }
}

fn percentile_nearest_rank(samples: &[u64], percentile: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let len = sorted.len();
    let rank = percentile
        .saturating_mul(len)
        .div_ceil(100)
        .saturating_sub(1)
        .min(len.saturating_sub(1));
    sorted[rank]
}

fn percentile_nearest_rank_per_mille(samples: &[u64], permille: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let len = sorted.len();
    let rank = permille
        .saturating_mul(len)
        .div_ceil(1000)
        .saturating_sub(1)
        .min(len.saturating_sub(1));
    sorted[rank]
}

fn dominant_latency_component(
    provider_streaming: &LatencyComponentBreakdown,
    local_tools: &LatencyComponentBreakdown,
    extension_hostcalls: &LatencyComponentBreakdown,
    persistence: &LatencyComponentBreakdown,
) -> String {
    [
        ("provider_streaming", provider_streaming.duration_ms),
        ("local_tools", local_tools.duration_ms),
        ("extension_hostcalls", extension_hostcalls.duration_ms),
        ("persistence", persistence.duration_ms),
    ]
    .into_iter()
    .max_by_key(|(_, duration_ms)| *duration_ms)
    .filter(|(_, duration_ms)| *duration_ms > 0)
    .map_or_else(|| "none".to_string(), |(name, _)| name.to_string())
}

#[derive(Debug)]
struct TurnLatencyAccumulator {
    started_at: Instant,
    provider_streaming_ms: Vec<u64>,
    local_tool_ms: Vec<u64>,
    extension_hostcall_ms: Vec<u64>,
    persistence_ms: Vec<u64>,
}

impl TurnLatencyAccumulator {
    fn started() -> Self {
        Self {
            started_at: Instant::now(),
            provider_streaming_ms: Vec::new(),
            local_tool_ms: Vec::new(),
            extension_hostcall_ms: Vec::new(),
            persistence_ms: Vec::new(),
        }
    }

    fn snapshot(&self) -> TurnLatencyBreakdown {
        TurnLatencyBreakdown::from_component_samples(
            duration_millis_saturating(self.started_at.elapsed()),
            &self.provider_streaming_ms,
            &self.local_tool_ms,
            &self.extension_hostcall_ms,
            &self.persistence_ms,
        )
    }
}

type SharedTurnLatencyAccumulator = Arc<StdMutex<TurnLatencyAccumulator>>;

fn snapshot_turn_latency(
    latency: &SharedTurnLatencyAccumulator,
) -> Option<Box<TurnLatencyBreakdown>> {
    latency.lock().ok().map(|guard| Box::new(guard.snapshot()))
}

fn record_provider_streaming_latency(latency: &SharedTurnLatencyAccumulator, duration: Duration) {
    if let Ok(mut guard) = latency.lock() {
        guard
            .provider_streaming_ms
            .push(duration_millis_saturating(duration));
    }
    let metrics = crate::session_metrics::global();
    record_global_latency(&metrics.provider_streaming, duration);
}

fn record_local_tool_latency(latency: &SharedTurnLatencyAccumulator, duration: Duration) {
    if let Ok(mut guard) = latency.lock() {
        guard
            .local_tool_ms
            .push(duration_millis_saturating(duration));
    }
    let metrics = crate::session_metrics::global();
    record_global_latency(&metrics.local_tools, duration);
}

fn record_extension_hostcall_latency(latency: &SharedTurnLatencyAccumulator, duration: Duration) {
    if let Ok(mut guard) = latency.lock() {
        guard
            .extension_hostcall_ms
            .push(duration_millis_saturating(duration));
    }
    let metrics = crate::session_metrics::global();
    record_global_latency(&metrics.extension_hostcalls, duration);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToolEffectBatch {
    start: usize,
    end: usize,
}

/// Serializable evidence for one planned tool-effect batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEffectBatchEvidence {
    /// Inclusive start index in the original tool-call order.
    pub start: usize,
    /// Exclusive end index in the original tool-call order.
    pub end: usize,
    /// Number of tool calls covered by this batch.
    pub len: usize,
    /// Stable labels for the union of all effects in this batch.
    pub combined_effects: Vec<&'static str>,
    /// Whether this batch can be executed with compatible-tool parallelism.
    pub parallel_safe: bool,
    /// Fail-closed barrier reason when the batch is serialized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub barrier_reason: Option<&'static str>,
}

/// Serializable evidence for the full planned tool-effect batch layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEffectBatchPlanEvidence {
    /// Versioned schema identifier for downstream evidence consumers.
    pub schema: &'static str,
    /// Number of tool calls in the source plan.
    pub tool_count: usize,
    /// Parallelism cap that compatible batches will use at execution time.
    pub parallelism_cap: usize,
    /// Deterministic contiguous batch plan.
    pub batches: Vec<ToolEffectBatchEvidence>,
}

fn plan_tool_effect_batches(effects: &[ToolEffects]) -> Vec<ToolEffectBatch> {
    let Some((&first_effects, remaining_effects)) = effects.split_first() else {
        return Vec::new();
    };

    let mut batches = Vec::new();
    let mut start = 0;
    let mut active_effects = first_effects;

    for (offset, candidate_effects) in remaining_effects.iter().copied().enumerate() {
        let index = offset + 1;
        if active_effects.compatible_with(candidate_effects) {
            active_effects = active_effects.union(candidate_effects);
        } else {
            batches.push(ToolEffectBatch { start, end: index });
            start = index;
            active_effects = candidate_effects;
        }
    }

    batches.push(ToolEffectBatch {
        start,
        end: effects.len(),
    });
    batches
}

fn combined_tool_effects(effects: &[ToolEffects]) -> Option<ToolEffects> {
    effects.iter().copied().reduce(ToolEffects::union)
}

const fn tool_effect_barrier_reason(effects: ToolEffects) -> Option<&'static str> {
    if effects.parallel_safe() {
        return None;
    }
    match (effects.writes(), effects.appends(), effects.processes()) {
        (true, true, true) => Some("write_append_process_barrier"),
        (true, true, false) => Some("write_append_barrier"),
        (true, false, true) => Some("write_process_barrier"),
        (false, true, true) => Some("append_process_barrier"),
        (true, false, false) => Some("write_barrier"),
        (false, true, false) => Some("append_barrier"),
        (false, false, true) => Some("process_barrier"),
        (false, false, false) => Some("undeclared_effects_barrier"),
    }
}

/// Build deterministic machine-readable evidence for a tool-effect batch plan.
#[must_use]
pub fn tool_effect_batch_plan_evidence(
    effects: &[ToolEffects],
    parallelism_cap: usize,
) -> ToolEffectBatchPlanEvidence {
    let batches = plan_tool_effect_batches(effects)
        .into_iter()
        .map(|batch| {
            let combined_effects = effects
                .get(batch.start..batch.end)
                .and_then(combined_tool_effects)
                .unwrap_or_else(ToolEffects::read);
            ToolEffectBatchEvidence {
                start: batch.start,
                end: batch.end,
                len: batch.end.saturating_sub(batch.start),
                combined_effects: combined_effects.labels(),
                parallel_safe: combined_effects.parallel_safe(),
                barrier_reason: tool_effect_barrier_reason(combined_effects),
            }
        })
        .collect();

    ToolEffectBatchPlanEvidence {
        schema: TOOL_EFFECT_BATCH_PLAN_SCHEMA_V1,
        tool_count: effects.len(),
        parallelism_cap,
        batches,
    }
}

// ============================================================================
// Agent Configuration
// ============================================================================

/// Default cap for tool-call iterations per agent turn.
///
/// Override per-invocation via `--max-tool-iterations` / the
/// `PI_MAX_TOOL_ITERATIONS` env var, or programmatically by writing
/// [`AgentConfig::max_tool_iterations`] directly. Resolved through
/// [`resolve_max_tool_iterations`] which clamps invalid values back to this
/// default rather than failing the run.
pub const MAX_TOOL_ITERATIONS_DEFAULT: usize = 50;

/// Sanity ceiling for `max_tool_iterations` overrides.
///
/// Guards against runaway loops from a typo while still leaving plenty of
/// room for long, multi-step agentic tasks (large refactors, multi-phase
/// spec implementations).
pub const MAX_TOOL_ITERATIONS_CEILING: usize = 1_000;

/// Maximum automatic continuations after an Anthropic `pause_turn` stop.
///
/// Anthropic documents `pause_turn` as a successful, resumable response from
/// long-running server tools. Retrying indefinitely would let a broken remote
/// tool turn one user request into an unbounded loop, so keep the provider's
/// recommended retry budget explicit and small.
pub const MAX_PAUSE_TURN_CONTINUATIONS: usize = 3;

/// Threshold (as a fraction of `max_tool_iterations`) at which the runtime
/// emits a one-shot soft-handoff steering message so the agent can begin a
/// graceful incomplete-handoff rather than being silently killed at the cap.
/// Encoded as numerator/denominator to avoid floating-point in a hot loop.
const ITERATION_WARN_NUMERATOR: usize = 4;
const ITERATION_WARN_DENOMINATOR: usize = 5;

/// Below this absolute cap, the soft-handoff warning is suppressed — for
/// caps like 3 or 4, the warning would fire on the first iteration and add
/// noise rather than help.
const ITERATION_WARN_MIN_CAP: usize = 5;

/// Resolve the effective tool-iteration cap from `PI_MAX_TOOL_ITERATIONS`.
///
/// Falls back to [`MAX_TOOL_ITERATIONS_DEFAULT`] when unset/invalid. Used
/// by callers that build an [`AgentConfig`] without going through the CLI
/// parser (ACP server, SDK).
pub fn resolved_max_tool_iterations_default() -> usize {
    resolve_max_tool_iterations(std::env::var("PI_MAX_TOOL_ITERATIONS").ok().as_deref())
}

/// Pure resolver for `max_tool_iterations` string overrides.
///
/// Returns [`MAX_TOOL_ITERATIONS_DEFAULT`] when input is `None`, empty,
/// unparseable, zero, or above the ceiling — emitting a warning so a
/// misconfigured cap is observable in logs rather than silently lost.
pub fn resolve_max_tool_iterations(raw_override: Option<&str>) -> usize {
    let Some(raw) = raw_override.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return MAX_TOOL_ITERATIONS_DEFAULT;
    };
    match raw.parse::<usize>() {
        Ok(0) => {
            warn!(
                "PI_MAX_TOOL_ITERATIONS=0 is invalid; falling back to {}",
                MAX_TOOL_ITERATIONS_DEFAULT
            );
            MAX_TOOL_ITERATIONS_DEFAULT
        }
        Ok(n) if n > MAX_TOOL_ITERATIONS_CEILING => {
            warn!(
                "PI_MAX_TOOL_ITERATIONS={n} exceeds ceiling {MAX_TOOL_ITERATIONS_CEILING}; clamping to {MAX_TOOL_ITERATIONS_CEILING}"
            );
            MAX_TOOL_ITERATIONS_CEILING
        }
        Ok(n) => n,
        Err(err) => {
            warn!(
                "PI_MAX_TOOL_ITERATIONS={raw:?} is not a valid usize ({err}); falling back to {}",
                MAX_TOOL_ITERATIONS_DEFAULT
            );
            MAX_TOOL_ITERATIONS_DEFAULT
        }
    }
}

/// Clamp a CLI-parsed `Option<usize>` cap to the supported range.
///
/// Same semantics as [`resolve_max_tool_iterations`] but for values that
/// have already been parsed by clap. Returns the effective cap, clamped
/// to `[1, MAX_TOOL_ITERATIONS_CEILING]` with invalid values (None, 0)
/// falling back to [`MAX_TOOL_ITERATIONS_DEFAULT`].
pub fn clamp_max_tool_iterations(value: Option<usize>) -> usize {
    match value {
        None => MAX_TOOL_ITERATIONS_DEFAULT,
        Some(0) => {
            warn!(
                "--max-tool-iterations=0 is invalid; falling back to {}",
                MAX_TOOL_ITERATIONS_DEFAULT
            );
            MAX_TOOL_ITERATIONS_DEFAULT
        }
        Some(n) if n > MAX_TOOL_ITERATIONS_CEILING => {
            warn!(
                "--max-tool-iterations={n} exceeds ceiling {MAX_TOOL_ITERATIONS_CEILING}; clamping to {MAX_TOOL_ITERATIONS_CEILING}"
            );
            MAX_TOOL_ITERATIONS_CEILING
        }
        Some(n) => n,
    }
}

/// Pure predicate: should we emit the one-shot iteration-budget warning at
/// the current iteration, given the configured cap?
///
/// Fires when `current >= (max * 4) / 5` and `max >= ITERATION_WARN_MIN_CAP`.
/// Caller is responsible for tracking fire-once state so the steering message
/// only injects once per run-loop. Stateless and integer-only so it's safe to
/// call inside the hot loop. Uses `saturating_mul` so an SDK caller that
/// writes `AgentConfig::max_tool_iterations = usize::MAX` directly (bypassing
/// the resolvers' clamp) gets a sane "never warn" rather than wrap-around to
/// a tiny threshold.
pub const fn should_warn_at_iteration_threshold(current: usize, max: usize) -> bool {
    max >= ITERATION_WARN_MIN_CAP
        && current >= max.saturating_mul(ITERATION_WARN_NUMERATOR) / ITERATION_WARN_DENOMINATOR
}

/// Body of the one-shot soft-handoff steering message, formatted with the
/// current/max iteration counts. Kept as a free function so test fixtures
/// can pin the wording without instantiating a full agent.
pub fn iteration_handoff_steering_text(current: usize, max: usize) -> String {
    format!(
        "[runtime] Tool-iteration budget at >=80% (used {current} of {max}). \
         Per the iteration-aware-handoff protocol in your spec, begin graceful \
         handoff now: commit current work, post a one-line status note, and \
         write an incomplete-handoff envelope with what's done / what remains \
         / next-agent starting position. Do NOT compress remaining work into \
         the last few iterations."
    )
}

/// Recognize only the synthetic successful boundary message emitted by the
/// run-time cap. Frontends must publish it after AgentSession persistence has
/// succeeded, rather than rendering its pre-save MessageStart/MessageEnd.
pub(crate) fn time_cap_marker(message: &AssistantMessage) -> Option<&str> {
    if message.stop_reason != StopReason::Stop
        || !message.api.is_empty()
        || !message.provider.is_empty()
        || !message.model.is_empty()
        || message.error_message.is_some()
    {
        return None;
    }
    let [crate::model::ContentBlock::Text(text)] = message.content.as_slice() else {
        return None;
    };
    let seconds = text
        .text
        .strip_prefix("[time cap reached] time cap reached after ")?
        .strip_suffix("s (--max-time); stopping at the turn boundary")?;
    if seconds.is_empty() || !seconds.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    seconds.parse::<u64>().ok()?;
    Some(text.text.as_str())
}

/// Configuration for the agent.
#[derive(Clone)]
pub struct AgentConfig {
    /// System prompt to use for all requests.
    pub system_prompt: Option<String>,

    /// Maximum tool call iterations before stopping.
    pub max_tool_iterations: usize,

    /// Default stream options.
    pub stream_options: StreamOptions,

    /// Whether the active model accepts image inputs.
    /// When false, native images become omission explanations in outbound
    /// context and snapcompact frames are stripped with a logged reason.
    /// Session history retains the original images for later model switches.
    pub model_accepts_images: bool,

    /// Strip image blocks before sending context to providers.
    pub block_images: bool,

    /// Fail closed when extension tool hooks error or time out.
    pub fail_closed_hooks: bool,

    /// Optional approval gate invoked before a tool executes.
    pub tool_approval: Option<ToolApprovalHandler>,

    /// Magic-keyword settings (bd-cv653.3.6): per-keyword toggles plus
    /// custom words. None = all three built-ins enabled, no customs.
    pub keyword_settings: Option<crate::magic_keywords::KeywordSettings>,

    /// Wall-clock cap for a run (bd-cv653.3.7): at the next turn boundary
    /// after the deadline the agent stops with a 'time cap reached' marker
    /// instead of starting another turn.
    pub max_time: Option<std::time::Duration>,

    /// Auto-continue policy for unexpected mid-task stops (bd-cv653.3.15).
    pub turn_recovery: crate::turn_recovery::TurnRecoveryMode,

    /// Graduated tool approval mode state (bd-cv653.3.19).
    pub approval_state: Option<crate::approval::ApprovalState>,

    /// Bash mediation settings for hard policy gating (bd-cv653.1.7).
    pub bash_settings: Option<crate::config::BashSettings>,

    /// Secrets vault settings (bd-cv653.7.9): mode + user patterns.
    pub secrets: Option<crate::secrets::SecretsSettings>,
}

impl fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentConfig")
            .field("system_prompt", &self.system_prompt)
            .field("max_tool_iterations", &self.max_tool_iterations)
            .field("stream_options", &self.stream_options)
            .field("block_images", &self.block_images)
            .field("model_accepts_images", &self.model_accepts_images)
            .field("fail_closed_hooks", &self.fail_closed_hooks)
            .field("tool_approval", &self.tool_approval.is_some())
            .field("keyword_settings", &self.keyword_settings)
            .field("max_time", &self.max_time)
            .field("turn_recovery", &self.turn_recovery)
            .field("approval_state", &self.approval_state)
            .field("bash_settings", &self.bash_settings)
            .field("secrets", &self.secrets)
            .finish()
    }
}

/// Details for a pending tool approval request.
#[derive(Debug, Clone)]
pub struct ToolApprovalRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

/// Decision returned by a tool approval handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    Allow,
    Deny { reason: String },
}

impl ToolApprovalDecision {
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }
}

pub type ToolApprovalHandler =
    Arc<dyn Fn(ToolApprovalRequest) -> BoxFuture<'static, ToolApprovalDecision> + Send + Sync>;

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_tool_iterations: resolved_max_tool_iterations_default(),
            stream_options: StreamOptions::default(),
            block_images: false,
            model_accepts_images: true,
            fail_closed_hooks: false,
            tool_approval: None,
            keyword_settings: None,
            max_time: None,
            turn_recovery: crate::turn_recovery::TurnRecoveryMode::default(),
            approval_state: None,
            bash_settings: None,
            secrets: None,
        }
    }
}

/// Opt-in semantic context bundle controls for a single agent session.
#[derive(Debug, Clone)]
pub struct SemanticContextBundleInjection {
    pub enabled: bool,
    pub bundle: SemanticContextBundle,
    pub max_prompt_items: usize,
    pub max_prompt_bytes: u64,
    pub include_exclusion_summary: bool,
    pub include_validation_commands: bool,
}

impl SemanticContextBundleInjection {
    pub fn enabled(bundle: SemanticContextBundle) -> Self {
        let max_prompt_items = bundle
            .budget
            .max_items
            .min(DEFAULT_SEMANTIC_CONTEXT_PROMPT_MAX_ITEMS);
        let max_prompt_bytes = bundle
            .budget
            .max_bytes
            .min(DEFAULT_SEMANTIC_CONTEXT_PROMPT_MAX_BYTES);
        Self {
            enabled: true,
            bundle,
            max_prompt_items,
            max_prompt_bytes,
            include_exclusion_summary: true,
            include_validation_commands: true,
        }
    }

    pub const fn disabled(bundle: SemanticContextBundle) -> Self {
        Self {
            enabled: false,
            bundle,
            max_prompt_items: DEFAULT_SEMANTIC_CONTEXT_PROMPT_MAX_ITEMS,
            max_prompt_bytes: DEFAULT_SEMANTIC_CONTEXT_PROMPT_MAX_BYTES,
            include_exclusion_summary: true,
            include_validation_commands: true,
        }
    }

    #[must_use]
    pub const fn with_prompt_budget(mut self, max_items: usize, max_bytes: u64) -> Self {
        self.max_prompt_items = max_items;
        self.max_prompt_bytes = max_bytes;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticContextPromptShape {
    CustomUserMessage,
    SystemPromptAppend,
}

#[derive(Debug, Clone)]
struct PreparedSemanticContextPrompt {
    prompt: String,
    revision: String,
    shape: SemanticContextPromptShape,
    details: Value,
}

#[derive(Debug, Clone, Copy)]
struct SemanticContextPromptBudget {
    max_items: usize,
    max_bytes: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct SemanticContextPromptStats {
    selected_items_included: usize,
    selected_items_omitted: usize,
    validation_commands_included: usize,
    validation_commands_omitted: usize,
    exclusions_included: usize,
    exclusions_omitted: usize,
    truncated: bool,
}

/// Ephemeral queued-message envelope.
///
/// `message` is the provider-visible payload. `keyword_scan_source` is present
/// only when a human-authored source string is known; generated host,
/// extension, job, and peer messages leave it absent so their payload bytes
/// can never activate magic keywords accidentally. This provenance is runtime
/// state only and is deliberately not serialized into [`Message`]. The stable
/// entry identity likewise stays on the envelope so terminal RPC persistence
/// can replay a cancelled writer without creating a second durable branch.
#[derive(Debug, Clone)]
pub struct QueuedAgentMessage {
    message: Message,
    keyword_scan_source: Option<String>,
    persistence_identity: Arc<OnceLock<QueuedPersistenceIdentity>>,
}

#[derive(Debug, Clone)]
struct QueuedPersistenceIdentity {
    entry_id: String,
    timestamp: String,
    parent_id: Option<String>,
}

impl QueuedAgentMessage {
    /// Pair a provider-visible message with the exact user-authored source
    /// that is eligible for magic-keyword scanning.
    #[must_use]
    pub fn authored(message: Message, keyword_scan_source: impl Into<String>) -> Self {
        let keyword_scan_source =
            matches!(&message, Message::User(_)).then(|| keyword_scan_source.into());
        Self {
            message,
            keyword_scan_source,
            persistence_identity: Arc::new(OnceLock::new()),
        }
    }

    /// Treat a user message as its own authored scan source.
    ///
    /// Queue boundaries that expand templates or attachments must use
    /// [`Self::authored`] with the pre-expansion source. This constructor is
    /// for direct public API callers, where preserving the historical behavior
    /// means treating every text block they supplied as authored.
    #[must_use]
    pub fn from_authored_message(message: Message) -> Self {
        let keyword_scan_source = match &message {
            Message::User(UserMessage { content, .. }) => Some(match content {
                UserContent::Text(text) => text.clone(),
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            }),
            _ => None,
        };
        Self {
            message,
            keyword_scan_source,
            persistence_identity: Arc::new(OnceLock::new()),
        }
    }

    /// Wrap a generated/internal message. Its provider-visible text is never
    /// eligible for magic-keyword activation.
    #[must_use]
    pub fn generated(message: Message) -> Self {
        Self {
            message,
            keyword_scan_source: None,
            persistence_identity: Arc::new(OnceLock::new()),
        }
    }

    #[must_use]
    pub const fn message(&self) -> &Message {
        &self.message
    }

    /// Stable identity used if acknowledged queued input must be persisted
    /// after a terminal RPC failure. Once bound, clones retain the complete
    /// entry base so a cancelled writer retry is idempotent.
    #[must_use]
    pub(crate) fn bind_persistence_identity(
        &self,
        parent_id: Option<String>,
    ) -> (String, String, Option<String>) {
        let identity = self
            .persistence_identity
            .get_or_init(|| QueuedPersistenceIdentity {
                entry_id: uuid::Uuid::new_v4().to_string(),
                timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                parent_id,
            });
        (
            identity.entry_id.clone(),
            identity.timestamp.clone(),
            identity.parent_id.clone(),
        )
    }

    #[must_use]
    pub(crate) fn persistence_entry_id(&self) -> Option<&str> {
        self.persistence_identity
            .get()
            .map(|identity| identity.entry_id.as_str())
    }

    fn shares_persistence_identity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.persistence_identity, &other.persistence_identity)
    }

    #[must_use]
    pub fn keyword_scan_source(&self) -> Option<&str> {
        self.keyword_scan_source.as_deref()
    }

    /// Text suitable for queue previews/editor restoration. Prefer the raw
    /// authored source, falling back to provider-visible text for explicitly
    /// suppressed generated entries.
    #[must_use]
    pub fn text_for_display(&self) -> Option<&str> {
        self.keyword_scan_source().or_else(|| match &self.message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => Some(text.as_str()),
                UserContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                }),
            },
            _ => None,
        })
    }

    #[must_use]
    pub fn into_message(self) -> Message {
        self.message
    }
}

/// Async fetcher for queued messages (steering or follow-up).
pub type MessageFetcher =
    Arc<dyn Fn() -> BoxFuture<'static, Vec<QueuedAgentMessage>> + Send + Sync + 'static>;

/// One additive message source. Permanent RPC/extension sources carry no
/// lifetime; turn-scoped SDK sources carry a weak lease and are removed before
/// the next poll once their owning turn retires.
struct RegisteredMessageFetcher {
    fetcher: MessageFetcher,
    lifetime: Option<Weak<()>>,
}

impl RegisteredMessageFetcher {
    fn permanent(fetcher: MessageFetcher) -> Self {
        Self {
            fetcher,
            lifetime: None,
        }
    }

    fn scoped(fetcher: MessageFetcher, lifetime: &Arc<()>) -> Self {
        Self {
            fetcher,
            lifetime: Some(Arc::downgrade(lifetime)),
        }
    }

    fn is_live(&self) -> bool {
        self.lifetime
            .as_ref()
            .is_none_or(|lifetime| lifetime.strong_count() > 0)
    }
}

type AgentEventHandler = Arc<dyn Fn(AgentEvent) + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    All,
    OneAtATime,
}

impl QueueMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    Interactive,
    Rpc,
    Extension,
}

impl InputSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Rpc => "rpc",
            Self::Extension => "extension",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum QueueKind {
    Steering,
    FollowUp,
}

#[derive(Debug, Clone)]
struct SequencedQueuedMessage {
    seq: u64,
    enqueued_at: i64,
    /// Session that owns a background-job completion. Ordinary queue entries
    /// remain unscoped so only generated job notices are filtered when the
    /// owning surface switches sessions between staging and delivery.
    job_owner_session_id: Option<String>,
    delivery: QueuedAgentMessage,
}

#[derive(Debug)]
struct MessageQueue {
    steering: VecDeque<SequencedQueuedMessage>,
    follow_up: VecDeque<SequencedQueuedMessage>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    next_seq: u64,
}

impl MessageQueue {
    const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode,
            follow_up_mode,
            next_seq: 0,
        }
    }

    const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }

    fn pending_count(&self) -> usize {
        self.steering.len() + self.follow_up.len()
    }

    fn next_entry(
        &mut self,
        delivery: QueuedAgentMessage,
        job_owner_session_id: Option<String>,
    ) -> SequencedQueuedMessage {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        SequencedQueuedMessage {
            seq,
            enqueued_at: Utc::now().timestamp_millis(),
            job_owner_session_id,
            delivery,
        }
    }

    fn push(&mut self, kind: QueueKind, delivery: QueuedAgentMessage) -> u64 {
        let entry = self.next_entry(delivery, None);
        let seq = entry.seq;
        match kind {
            QueueKind::Steering => {
                if self.steering.len() >= MAX_STEERING_QUEUE_SIZE {
                    tracing::warn!(
                        "Steering queue full ({} messages), dropping oldest message",
                        MAX_STEERING_QUEUE_SIZE
                    );
                    self.steering.pop_front();
                }
                self.steering.push_back(entry);
            }
            QueueKind::FollowUp => {
                let ordinary_count = self
                    .follow_up
                    .iter()
                    .filter(|entry| entry.job_owner_session_id.is_none())
                    .count();
                if ordinary_count >= MAX_FOLLOW_UP_QUEUE_SIZE {
                    tracing::warn!(
                        "Follow-up queue full ({} messages), dropping oldest message",
                        MAX_FOLLOW_UP_QUEUE_SIZE
                    );
                    if let Some(oldest) = self
                        .follow_up
                        .iter()
                        .position(|entry| entry.job_owner_session_id.is_none())
                    {
                        let _ = self.follow_up.remove(oldest);
                    }
                }
                self.follow_up.push_back(entry);
            }
        }
        seq
    }

    fn push_steering(&mut self, delivery: QueuedAgentMessage) -> u64 {
        self.push(QueueKind::Steering, delivery)
    }

    fn push_steering_lossless(&mut self, delivery: QueuedAgentMessage) -> u64 {
        let entry = self.next_entry(delivery, None);
        let seq = entry.seq;
        self.steering.push_back(entry);
        seq
    }

    fn restore_steering_front_lossless(&mut self, deliveries: Vec<QueuedAgentMessage>) {
        let mut restored = deliveries
            .into_iter()
            .map(|delivery| self.next_entry(delivery, None))
            .collect::<VecDeque<_>>();
        restored.append(&mut self.steering);
        self.steering = restored;
    }

    fn push_follow_up(&mut self, delivery: QueuedAgentMessage) -> u64 {
        self.push(QueueKind::FollowUp, delivery)
    }

    fn push_follow_up_lossless(&mut self, delivery: QueuedAgentMessage) -> u64 {
        let entry = self.next_entry(delivery, None);
        let seq = entry.seq;
        self.follow_up.push_back(entry);
        seq
    }

    fn push_job_follow_up_lossless(
        &mut self,
        owner_session_id: String,
        delivery: QueuedAgentMessage,
    ) -> u64 {
        let entry = self.next_entry(delivery, Some(owner_session_id));
        let seq = entry.seq;
        self.follow_up.push_back(entry);
        seq
    }

    fn has_job_follow_up(&self) -> bool {
        self.follow_up
            .iter()
            .any(|entry| entry.job_owner_session_id.is_some())
    }

    /// Remove staged job notices that do not belong to the selected session.
    /// The caller restores the returned entries to the bounded job registry so
    /// switching sessions cannot leak or silently discard an owner's notice.
    fn take_job_follow_ups_except(
        &mut self,
        owner_session_id: Option<&str>,
    ) -> Vec<(String, QueuedAgentMessage)> {
        let mut retained = VecDeque::with_capacity(self.follow_up.len());
        let mut released = Vec::new();
        while let Some(entry) = self.follow_up.pop_front() {
            let should_release = entry
                .job_owner_session_id
                .as_deref()
                .is_some_and(|owner| Some(owner) != owner_session_id);
            if should_release {
                if let Some(owner) = entry.job_owner_session_id {
                    released.push((owner, entry.delivery));
                }
            } else {
                retained.push_back(entry);
            }
        }
        self.follow_up = retained;
        released
    }

    fn pop_steering(&mut self) -> Vec<QueuedAgentMessage> {
        self.pop_kind(QueueKind::Steering)
    }

    fn pop_follow_up(&mut self) -> Vec<QueuedAgentMessage> {
        self.pop_kind(QueueKind::FollowUp)
    }

    fn follow_up_batch_len(&self) -> usize {
        match self.follow_up_mode {
            QueueMode::All => self.follow_up.len(),
            QueueMode::OneAtATime => usize::from(!self.follow_up.is_empty()),
        }
    }

    fn pop_kind(&mut self, kind: QueueKind) -> Vec<QueuedAgentMessage> {
        let (queue, mode) = match kind {
            QueueKind::Steering => (&mut self.steering, self.steering_mode),
            QueueKind::FollowUp => (&mut self.follow_up, self.follow_up_mode),
        };

        match mode {
            QueueMode::All => queue.drain(..).map(|entry| entry.delivery).collect(),
            QueueMode::OneAtATime => queue
                .pop_front()
                .into_iter()
                .map(|entry| entry.delivery)
                .collect(),
        }
    }

    fn discard_persistence_ids(&mut self, entry_ids: &std::collections::HashSet<String>) -> usize {
        let before = self.pending_count();
        self.steering.retain(|entry| {
            !entry
                .delivery
                .persistence_entry_id()
                .is_some_and(|id| entry_ids.contains(id))
        });
        self.follow_up.retain(|entry| {
            !entry
                .delivery
                .persistence_entry_id()
                .is_some_and(|id| entry_ids.contains(id))
        });
        before.saturating_sub(self.pending_count())
    }

    fn contains_delivery(&self, delivery: &QueuedAgentMessage) -> bool {
        self.steering
            .iter()
            .chain(&self.follow_up)
            .any(|entry| entry.delivery.shares_persistence_identity(delivery))
    }
}

// ============================================================================
// Agent Event
// ============================================================================

impl AgentEvent {
    /// Serialize this event as one line of a JSON event stream (`--mode json`,
    /// gh #222).
    ///
    /// `message_update` records are delta-only: they omit the cumulative
    /// `message` field and `assistantMessageEvent.partial`, both of which carry
    /// the whole accumulated assistant message and would make the stream
    /// quadratic in the response length (measured at ~500 MB of stdout for a
    /// 50 KB reply). `message_start` / `message_end` still carry the full
    /// message, so consumers reconstruct text by concatenating deltas or by
    /// reading `message_end`. This matches upstream pi's 0.84 JSON contract.
    /// Every other event serializes exactly like the plain `Serialize` impl.
    pub fn to_json_stream_line(&self) -> serde_json::Result<String> {
        #[derive(Serialize)]
        struct DeltaOnlyMessageUpdate<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            #[serde(rename = "assistantMessageEvent")]
            assistant_message_event: crate::model::AssistantMessageEventDelta<'a>,
        }

        match self {
            Self::MessageUpdate {
                assistant_message_event,
                ..
            } => serde_json::to_string(&DeltaOnlyMessageUpdate {
                kind: "message_update",
                assistant_message_event: assistant_message_event.delta_only(),
            }),
            other => serde_json::to_string(other),
        }
    }
}

/// Events emitted by the agent during execution.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Agent lifecycle start.
    AgentStart {
        #[serde(rename = "sessionId")]
        session_id: Arc<str>,
    },
    /// Agent lifecycle end with all new messages.
    AgentEnd {
        #[serde(rename = "sessionId")]
        session_id: Arc<str>,
        messages: Vec<Message>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Turn lifecycle start (assistant response + tool calls).
    TurnStart {
        #[serde(rename = "sessionId")]
        session_id: Arc<str>,
        #[serde(rename = "turnIndex")]
        turn_index: usize,
        timestamp: i64,
    },
    /// Turn lifecycle end with tool results.
    TurnEnd {
        #[serde(rename = "sessionId")]
        session_id: Arc<str>,
        #[serde(rename = "turnIndex")]
        turn_index: usize,
        message: Message,
        #[serde(rename = "toolResults")]
        tool_results: Vec<Message>,
        #[serde(rename = "latencyBreakdown", skip_serializing_if = "Option::is_none")]
        latency_breakdown: Option<Box<TurnLatencyBreakdown>>,
    },
    /// Message lifecycle start (user, assistant, or tool result).
    MessageStart { message: Message },
    /// Message update (assistant streaming).
    MessageUpdate {
        message: Message,
        #[serde(rename = "assistantMessageEvent")]
        assistant_message_event: AssistantMessageEvent,
    },
    /// Message lifecycle end.
    MessageEnd { message: Message },
    /// Tool execution start.
    ToolExecutionStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        args: serde_json::Value,
    },
    /// Tool execution update.
    ToolExecutionUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        args: serde_json::Value,
        #[serde(rename = "partialResult")]
        partial_result: ToolOutput,
    },
    /// Tool execution end.
    ToolExecutionEnd {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        result: ToolOutput,
        #[serde(rename = "isError")]
        is_error: bool,
    },
    /// Auto-compaction lifecycle start.
    AutoCompactionStart { reason: String },
    /// Auto-compaction lifecycle end.
    AutoCompactionEnd {
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        aborted: bool,
        #[serde(rename = "willRetry")]
        will_retry: bool,
        #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    /// Auto-retry lifecycle start.
    AutoRetryStart {
        attempt: u32,
        #[serde(rename = "maxAttempts")]
        max_attempts: u32,
        #[serde(rename = "delayMs")]
        delay_ms: u64,
        #[serde(rename = "errorMessage")]
        error_message: String,
    },
    /// Auto-retry lifecycle end.
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        #[serde(rename = "finalError", skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    /// Cross-model failover start (bd-cv653.3.2): the active model is being
    /// swapped to a fallback-chain entry after a classified transient failure.
    FailoverStart {
        #[serde(rename = "fromProvider")]
        from_provider: String,
        #[serde(rename = "fromModel")]
        from_model: String,
        #[serde(rename = "toProvider")]
        to_provider: String,
        #[serde(rename = "toModel")]
        to_model: String,
        /// Failure class that triggered the failover (quota/overload/transient).
        class: String,
        /// Which successful swap this is within the logical turn, 1-based.
        ///
        /// This is the number `retry.maxFailoversPerTurn` is counted against,
        /// so a consumer can say "swap 2 of at most 3" without tracking state.
        /// It used to carry the chain index instead, which stopped meaning
        /// anything once the walk began skipping the current model, duplicates
        /// and unusable entries (bd-oqo03.1) — the number then jumped by more
        /// than one per swap, or not at all, with nothing to say why. The index
        /// is still reported, as `chain_index`.
        attempt: u32,
        /// Position in the fallback chain of the entry being swapped to,
        /// 0-based.
        ///
        /// Durable across turns, unlike `attempt`: the walk resumes from where
        /// the previous turn left it. Reported separately because the two
        /// answer different questions — `attempt` is budget, this is
        /// provenance — and conflating them is what bd-oqo03 was opened for.
        #[serde(rename = "chainIndex")]
        chain_index: u32,
    },
    /// Cross-model failover end (bd-cv653.3.2): the turn completed on a
    /// failover entry, or the primary was restored after cooldown.
    FailoverEnd {
        success: bool,
        provider: String,
        model: String,
        #[serde(rename = "restoredPrimary")]
        restored_primary: bool,
    },
    /// Advisor verdict recorded for the session, before queued delivery (bd-cv653.3.3).
    AdvisorNote { level: String, rationale: String },
    /// A provider request failed and ended the turn (#209). Emitted right
    /// before the `TurnEnd`/`AgentEnd` pair so consumers get the provider,
    /// HTTP status, and retryability as structured fields rather than having
    /// to parse `AgentEnd.error`. Retry lifecycle (`AutoRetryStart`/`End`)
    /// is reported separately by the caller that owns the retry budget.
    ProviderError {
        #[serde(rename = "sessionId")]
        session_id: Arc<str>,
        provider: String,
        model: String,
        #[serde(flatten)]
        summary: crate::error::ProviderErrorSummary,
        message: String,
    },
    /// Extension error during event dispatch or execution.
    ExtensionError {
        #[serde(rename = "extensionId", skip_serializing_if = "Option::is_none")]
        extension_id: Option<String>,
        event: String,
        error: String,
    },
}

// ============================================================================
// Agent
// ============================================================================

/// Handle to request an abort of an in-flight agent run.
#[derive(Debug, Clone)]
pub struct AbortHandle {
    inner: Arc<AbortSignalInner>,
}

/// Signal for observing abort requests.
#[derive(Debug, Clone)]
pub struct AbortSignal {
    inner: Arc<AbortSignalInner>,
}

#[derive(Debug)]
struct AbortSignalInner {
    aborted: AtomicBool,
    notify: Notify,
}

impl AbortHandle {
    /// Create a new abort handle + signal pair.
    #[must_use]
    pub fn new() -> (Self, AbortSignal) {
        let inner = Arc::new(AbortSignalInner {
            aborted: AtomicBool::new(false),
            notify: Notify::new(),
        });
        (
            Self {
                inner: Arc::clone(&inner),
            },
            AbortSignal { inner },
        )
    }

    /// Trigger an abort.
    pub fn abort(&self) {
        if !self.inner.aborted.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }
}

impl AbortSignal {
    /// Check if an abort has already been requested.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        // Broadcast notifications are edge-triggered. Sample their generation
        // before checking the latched flag, so an abort between that check and
        // waiter registration cannot strand provider or tool cancellation.
        self.inner.notify.wait_until(|| self.is_aborted()).await;
    }
}

/// Compaction window for a model entry, with a reported zero treated as
/// "unknown" rather than "no room".
///
/// Print mode and the RPC server each had a private copy of this, one of them
/// silently — the warning is worth having wherever it happens (bd-u2qv4).
#[must_use]
pub fn context_window_tokens_for_entry(entry: &crate::models::ModelEntry) -> u32 {
    if entry.model.context_window == 0 {
        tracing::warn!(
            "Model {} reported context_window=0; falling back to default compaction window",
            entry.model.id
        );
        return ResolvedCompactionSettings::default().context_window_tokens;
    }
    entry.model.context_window
}

/// A walk of a fallback chain looking for an entry worth swapping to.
pub struct FailoverSwapAttempt<'a> {
    /// The chain resolved for the live model.
    pub chain: &'a crate::failover::FailoverChain,
    /// Where to resume the walk, from the caller's cross-turn state.
    pub start_position: usize,
    /// Admitted destinations after availability and provider filtering. The
    /// session's full registry, when present, establishes provider identity.
    pub available_models: &'a [crate::models::ModelEntry],
    /// Credential store consulted for each candidate.
    pub auth: &'a crate::auth::AuthStorage,
    /// An explicit `--api-key`, which pins and never rotates.
    pub cli_api_key: Option<&'a str>,
    /// Why the swap is happening, recorded in the transcript.
    pub class: crate::failover::FailoverClass,
    /// The level a candidate is clamped against; see
    /// [`FailoverSwapRequest::thinking_level_to_clamp`].
    pub thinking_level_to_clamp: crate::model::ThinkingLevel,
    /// Whether a completed error response must have left a revertible tail.
    pub require_incomplete_tail: bool,
    /// The primary identity before failover started (bd-gm481.2).
    pub primary: Option<&'a crate::failover::FailoverPrimary>,
    /// Cooldown duration in seconds (bd-gm481.2).
    pub cooldown_secs: Option<u64>,
    /// Unique lifecycle ID across hops in this failover cycle (bd-gm481.2).
    pub lifecycle_id: Option<&'a str>,
}

/// A swap that committed, with what the caller needs for its own events and
/// cross-turn bookkeeping.
#[derive(Debug, Clone)]
pub struct CommittedFailover {
    /// Where the installed entry sits in the chain. Note this is the entry's
    /// index, not the resume position — reporting the latter is off by one
    /// (bd-oqo03).
    pub entry_index: usize,
    /// Provider left behind.
    pub from_provider: String,
    /// Model left behind.
    pub from_model: String,
    /// Provider now installed.
    pub to_provider: String,
    /// Model now installed.
    pub to_model: String,
    /// Exact durable branch record captured before releasing the commit lock.
    /// Recovery callers use it to preserve the live monotonic cooldown.
    pub(crate) source: Option<crate::failover::FailoverSource>,
}

/// The result of a chain walk, swapped or not.
#[derive(Debug, Clone)]
pub struct FailoverSwapOutcome {
    /// Where the NEXT walk resumes. Record it either way, so a later turn does
    /// not re-walk entries this one already rejected.
    pub next_position: usize,
    /// `None` means the chain held nothing installable.
    pub committed: Option<CommittedFailover>,
}

/// One fallback-chain swap, as [`AgentSession::commit_failover_swap`] needs it.
///
/// The caller has already classified the failure, walked the chain, resolved a
/// credential and constructed the provider; what remains is the persisted
/// transition, which is identical on every surface.
pub struct FailoverSwapRequest<'a> {
    /// The chain entry being installed.
    pub entry: &'a crate::models::ModelEntry,
    /// Credential resolved for that entry, `None` only where none is required.
    pub api_key: Option<String>,
    /// Provider constructed for the entry.
    pub provider: Arc<dyn Provider>,
    /// The identity being left, recorded in the transcript.
    pub from_provider: &'a str,
    /// The model being left, recorded in the transcript.
    pub from_model: &'a str,
    /// Why the swap is happening, recorded in the transcript.
    pub class: crate::failover::FailoverClass,
    /// Chain index recorded with the transcript entry.
    pub chain_position: usize,
    /// The level the entry is clamped against: the level ORIGINALLY REQUESTED,
    /// captured before any swap — never the live level (bd-jk057, settled).
    ///
    /// A clamp exists to respect a MODEL's limit, not to make one model's limit
    /// sticky across models. Passing the live level ratchets it down through
    /// whatever the worst model the chain happened to touch allowed, and never
    /// recovers it — for the turn and, because the level is written into the
    /// session header, for the session.
    ///
    /// Every surface gets this value the same way, from
    /// `FailoverState::primary_for_swap`, which yields the recorded primary
    /// while a chain is in flight and the live identity otherwise. Print mode
    /// passed the live level until bd-jk057; RPC and the SDK never did.
    pub thinking_level_to_clamp: crate::model::ThinkingLevel,
    /// Whether a completed error response must have left a revertible tail.
    pub require_incomplete_tail: bool,
    /// The primary identity before failover started (bd-gm481.2).
    pub primary: Option<&'a crate::failover::FailoverPrimary>,
    /// Cooldown duration in seconds (bd-gm481.2).
    pub cooldown_secs: Option<u64>,
    /// Unique lifecycle ID across hops in this failover cycle (bd-gm481.2).
    pub lifecycle_id: Option<&'a str>,
}

/// One restoration of the captured primary after a failover cooldown, as
/// [`AgentSession::restore_primary_swap`] needs it.
///
/// The mirror image of [`FailoverSwapRequest`]: same staged-candidate
/// transition, but the transcript records `primary_restore` rather than
/// `failover`, and there is no incomplete tail to revert because the turn that
/// ran on the fallback completed.
pub struct PrimaryRestoreRequest<'a> {
    /// The primary captured at the first swap, to be reinstalled.
    pub primary: &'a crate::failover::FailoverPrimary,
    /// The fallback the caller believes is live, as `(provider, model)`. Both
    /// the runtime and the persisted Session must still agree with it, or the
    /// caller's model of the session is stale and nothing is installed.
    pub active: &'a (String, String),
    /// Whether the cooldown has actually elapsed. Passed in rather than
    /// computed here because the cooldown lives in each surface's own
    /// cross-turn state, and it is checked at the point RPC checks it so a
    /// stale-runtime session is still reported before this short-circuits.
    pub cooldown_elapsed: bool,
    /// Models the primary can resolve against.
    pub available_models: &'a [crate::models::ModelEntry],
    /// Credential store consulted for the primary.
    pub auth: &'a crate::auth::AuthStorage,
    /// An explicit `--api-key`, which pins and never rotates.
    pub cli_api_key: Option<&'a str>,
    /// How a refusal is reported, which is the one place print and RPC
    /// genuinely disagree.
    ///
    /// RPC treats a mismatch between its recorded fallback and the live
    /// runtime or the persisted Session as a corrupted model of the session:
    /// it blocks provider admission and fails the call, because re-entering a
    /// provider against a session nobody can describe is how work gets
    /// re-billed against a record that no longer matches it. Print has no
    /// admission gate and no long-lived state to corrupt, so it declines and
    /// runs the next prompt on the fallback, which is a correct outcome there.
    ///
    /// Preserved rather than unified, because unifying it would silently
    /// change a surface people depend on; the divergence is tracked
    /// separately.
    pub strict_invariants: bool,
    /// Whether to discard an in-flight background compaction.
    ///
    /// RPC, the SDK, and print mode discard an in-flight background compaction across
    /// primary restoration (bd-uyqkk). The restoration changes the model and the context
    /// window in the same transition, so a compaction computed against the fallback's
    /// window is stale by construction.
    pub invalidate_background_compaction: bool,
}

/// What a restoration actually installed.
#[derive(Debug, Clone)]
pub struct RestoredPrimary {
    /// Provider now installed.
    pub provider: String,
    /// Model now installed.
    pub model: String,
}

/// The agent runtime that orchestrates LLM calls and tool execution.
pub struct Agent {
    /// The LLM provider.
    provider: Arc<dyn Provider>,

    /// Tool registry, shared with the extension hosts (see
    /// [`crate::tools::SharedToolRegistry`]).
    tools: crate::tools::SharedToolRegistry,

    /// Agent configuration.
    config: AgentConfig,

    /// Optional extension manager for tool/event hooks.
    extensions: Option<ExtensionManager>,

    /// Message history.
    messages: Vec<Message>,

    /// Authored queued inputs that reached the live transcript but have not
    /// yet reached Session persistence. Absolute, sorted message indices retain
    /// provenance after the ephemeral delivery envelope is consumed.
    authored_queue_delivery_indices: Vec<usize>,

    /// Fetchers for queued steering messages (interrupts).
    steering_fetchers: Vec<RegisteredMessageFetcher>,

    /// Owning surface's bounded follow-up source used for resumed handoffs.
    initial_follow_up_fetcher: Option<MessageFetcher>,

    /// Fetchers for queued follow-up messages (idle).
    follow_up_fetchers: Vec<RegisteredMessageFetcher>,

    /// Whether queue sources may be polled and dispatched automatically.
    /// Hosts can pause either lane and explicitly take already staged batches.
    automatic_steering_dispatch: bool,
    automatic_follow_up_dispatch: bool,

    /// Live owner resolver for background completions. Notices retain the
    /// resolved owner across the registry-to-Agent handoff and are checked
    /// again immediately before provider delivery.
    job_session_scope: crate::jobs::JobSessionScope,

    /// Internal queue for steering/follow-up messages.
    message_queue: MessageQueue,

    /// Cached tool definitions. Invalidated when tools change via `extend_tools`.
    /// The generation counter covers mid-session `xdev` promotions (bd-cv653.1.6).
    cached_tool_defs: Option<(u64, Vec<ToolDef>)>,

    /// Path-scoped imported workspace rules (bd-cv653.6.2), delivered as
    /// steering messages the first time a tool call touches a matching path.
    scoped_rules: Option<ScopedRuleState>,

    /// Discoverable tools promoted into the live schema mid-session via
    /// `xdev promote` (bd-cv653.1.6). Interior-mutable because tool
    /// execution takes `&self`.
    promoted_tools: Arc<StdMutex<std::collections::HashSet<String>>>,
    /// Bumped on promotion; `cached_tool_defs` rebuilds when its stored
    /// generation is stale.
    tool_defs_generation: Arc<std::sync::atomic::AtomicU64>,

    /// Plan-mode state (bd-cv653.3.5): shared with the `submit_plan` tool;
    /// the executor consults the gate before every tool call.
    plan_state: crate::plan::PlanState,

    /// Dialect repair ledger (bd-cv653.7.8): every text→tool-call fixup this
    /// session, drained by the session layer into session Custom entries.
    repair_ledger: Arc<StdMutex<crate::dialects::RepairLedger>>,

    /// Session-local sequence used to keep synthesized tool-call IDs unique
    /// across multiple repaired assistant messages.
    dialect_repair_sequence: AtomicU64,

    /// Explicit catalog-selected tool-call dialect. Native is the fail-closed
    /// default; model-name heuristics never enable runtime repair.
    tool_call_dialect: crate::dialects::Dialect,

    /// Magic-keyword activations this run (bd-cv653.3.6), drained by the
    /// session wrapper into session Custom entries for auditability.
    keyword_ledger: Vec<crate::magic_keywords::KeywordActivation>,

    /// Activations retained only while an incomplete turn is retryable. A
    /// resume has no new prompt to scan, so these reapply the same turn-local
    /// effects without duplicating durable telemetry.
    retry_keyword_activations: Vec<crate::magic_keywords::KeywordActivation>,

    /// One-shot, caller-supplied prose provenance for the next prompt. This
    /// keeps generated attachment wrappers and file bytes visible to the
    /// model while excluding them from behavior-changing keyword scans.
    magic_keyword_scan_override: Option<String>,

    /// Highest effort the active model can accept. The session wrapper keeps
    /// this synchronized with the model registry so `ultrathink` cannot send
    /// a raw unsupported `Max` request.
    keyword_max_thinking_level: crate::model::ThinkingLevel,

    /// Session-scoped secrets vault (bd-cv653.7.9): placeholder map lives in
    /// memory and dies with the session — never persisted raw.
    secrets_vault: crate::secrets::SecretVault,
    /// Extension compaction cannot borrow the running Agent. Its shared
    /// snapshot is refreshed at each successful vault update and Session reset.
    compaction_privacy_state: Arc<StdMutex<compaction::CompactionPrivacy>>,
}

/// Restore temporary keyword settings even if an in-flight run is dropped.
/// Capture these only at agent-loop entry, after session model selection has
/// installed the model's supported baseline thinking level.
struct AgentTurnSettingsGuard<'a> {
    agent: &'a mut Agent,
    thinking_level: Option<crate::model::ThinkingLevel>,
    system_prompt: Option<String>,
}

impl<'a> AgentTurnSettingsGuard<'a> {
    fn new(agent: &'a mut Agent) -> Self {
        let thinking_level = agent.config.stream_options.thinking_level;
        let system_prompt = agent.config.system_prompt.clone();
        Self {
            agent,
            thinking_level,
            system_prompt,
        }
    }
}

impl Drop for AgentTurnSettingsGuard<'_> {
    fn drop(&mut self) {
        self.agent.config.stream_options.thinking_level = self.thinking_level;
        self.agent.config.system_prompt = self.system_prompt.take();
    }
}

/// Activation state for glob-scoped foreign rules (bd-cv653.6.2).
struct ScopedRuleState {
    rules: Vec<crate::context_files::ForeignRule>,
    matcher: crate::context_files::ScopedRuleMatcher,
    workspace_root: PathBuf,
    activated: std::collections::HashSet<usize>,
}

/// JSON keys of tool inputs that name filesystem paths, used to decide when
/// a scoped rule's globs are touched. Budget-capped: only string values and
/// first-level arrays of strings are inspected.
const PATH_LIKE_INPUT_KEYS: [&str; 6] =
    ["path", "file_path", "filePath", "file", "directory", "cwd"];

/// Extract path-like strings from a tool call's arguments (borrowed;
/// allocation-free). These are matcher *inputs*, never joined onto a
/// filesystem path.
fn path_like_inputs(arguments: &Value) -> Vec<&str> {
    let Some(object) = arguments.as_object() else {
        return Vec::new();
    };
    PATH_LIKE_INPUT_KEYS
        .iter()
        .filter_map(|key| object.get(*key))
        .flat_map(|value| match value {
            Value::String(_) => std::slice::from_ref(value).iter(),
            Value::Array(entries) => entries.iter(),
            _ => [].iter(),
        })
        .filter_map(Value::as_str)
        .filter(|candidate| !candidate.is_empty())
        .collect()
}

impl Agent {
    /// Create a new agent with the given provider and tools.
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry, config: AgentConfig) -> Self {
        Self::with_shared_tools(
            provider,
            crate::tools::SharedToolRegistry::new(tools),
            config,
        )
    }

    /// The registry handle this agent resolves and mounts tools through.
    /// Hand the same handle to an extension runtime so `pi.tool` hostcalls
    /// resolve against the live registry, including tools mounted later.
    #[must_use]
    pub fn shared_tools(&self) -> crate::tools::SharedToolRegistry {
        self.tools.clone()
    }

    /// Create a new agent over an already-shared tool registry.
    pub fn with_shared_tools(
        provider: Arc<dyn Provider>,
        tools: crate::tools::SharedToolRegistry,
        config: AgentConfig,
    ) -> Self {
        let keyword_max_thinking_level = config
            .stream_options
            .thinking_level
            .unwrap_or(crate::model::ThinkingLevel::Off);
        let registry = tools.snapshot();
        let job_session_scope = registry.job_session_scope();
        registry
            .model_scope()
            .set(provider.name(), provider.model_id());
        let compaction_privacy_state = Arc::new(StdMutex::new(
            compaction::CompactionPrivacy::from_settings(config.secrets.as_ref()),
        ));
        Self {
            provider,
            tools,
            config,
            extensions: None,
            messages: Vec::new(),
            authored_queue_delivery_indices: Vec::new(),
            steering_fetchers: Vec::new(),
            initial_follow_up_fetcher: None,
            follow_up_fetchers: Vec::new(),
            automatic_steering_dispatch: true,
            automatic_follow_up_dispatch: true,
            job_session_scope,
            message_queue: MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime),
            cached_tool_defs: None,
            scoped_rules: None,
            promoted_tools: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            tool_defs_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            plan_state: crate::plan::PlanState::new(),
            repair_ledger: Arc::new(StdMutex::new(crate::dialects::RepairLedger::default())),
            dialect_repair_sequence: AtomicU64::new(0),
            tool_call_dialect: crate::dialects::Dialect::Native,
            keyword_ledger: Vec::new(),
            retry_keyword_activations: Vec::new(),
            magic_keyword_scan_override: None,
            keyword_max_thinking_level,
            secrets_vault: crate::secrets::SecretVault::default(),
            compaction_privacy_state,
        }
    }

    /// Dialect repair (bd-cv653.7.8): when a weak model emits its tool call
    /// as text, extract and synthesize it into a real ToolCall block so the
    /// turn continues. Guards: explicitly repairable dialect, Stop reason, no
    /// structured calls already present, tools enabled, one repair per
    /// message, candidate names must be registered tools.
    fn maybe_repair_dialect_tool_calls(&self, msg: AssistantMessage) -> AssistantMessage {
        use crate::dialects::{Dialect, extract_text_tool_calls, strip_candidates};

        if !extract_tool_calls(&msg.content).is_empty() {
            return msg; // structured calls present — nothing to repair
        }
        if !matches!(msg.stop_reason, StopReason::Stop) {
            return msg;
        }
        if self.tools.snapshot().tools().is_empty() {
            return msg;
        }
        if self.tool_call_dialect != Dialect::Xmlish {
            return msg;
        }

        let candidate_block =
            msg.content
                .iter()
                .enumerate()
                .find_map(|(index, block)| match block {
                    ContentBlock::Text(text) => {
                        let candidates = extract_text_tool_calls(&text.text, &|name| {
                            self.tools.snapshot().get(name).is_some()
                        });
                        (!candidates.is_empty()).then_some((index, candidates))
                    }
                    _ => None,
                });
        let Some((block_index, candidates)) = candidate_block else {
            return msg;
        };

        let ContentBlock::Text(original_text) = &msg.content[block_index] else {
            unreachable!("candidate search only returns text blocks");
        };
        let remaining = strip_candidates(&original_text.text, &candidates);
        let repair_sequence = self.dialect_repair_sequence.fetch_add(1, Ordering::Relaxed);
        let mut replacement = Vec::with_capacity(1 + candidates.len());
        if !remaining.is_empty() {
            // The candidate block changed, so its provider signature no longer
            // authenticates the bytes. Preserve every untouched block and its
            // metadata, but clear the modified block's now-invalid signature.
            replacement.push(ContentBlock::Text(TextContent::new(remaining.clone())));
        }
        for (index, candidate) in candidates.iter().enumerate() {
            replacement.push(ContentBlock::ToolCall(ToolCall {
                id: format!("dialect-repair-{repair_sequence}-{index}"),
                name: candidate.name.clone(),
                arguments: candidate.arguments.clone(),
                thought_signature: None,
            }));
        }
        let mut content = msg.content.clone();
        content.splice(block_index..=block_index, replacement);
        if let Ok(mut ledger) = self.repair_ledger.lock() {
            for candidate in &candidates {
                ledger.record(
                    &candidate.name,
                    candidate.end - candidate.start,
                    remaining.len(),
                );
            }
        }
        tracing::info!(
            event = "pi.dialect.repair",
            tools = ?candidates.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            remaining_text_bytes = remaining.len(),
            "Repaired text-emitted tool call into structured call"
        );
        AssistantMessage { content, ..msg }
    }

    /// The shared plan-mode state (bd-cv653.3.5).
    #[must_use]
    pub fn plan_state(&self) -> crate::plan::PlanState {
        self.plan_state.clone()
    }

    /// Reset memory-only state that belongs to the active Session identity.
    ///
    /// Secret placeholders are intentionally predictable within one Session,
    /// so carrying their raw-value map into another Session would let a stale
    /// placeholder recover data that the new Session never observed. Plan
    /// state is shared with the submit-plan tool and must be reset in place.
    pub fn reset_session_scoped_state(&mut self, plan_mode: crate::plan::PlanMode) {
        self.secrets_vault = crate::secrets::SecretVault::default();
        self.refresh_compaction_privacy();
        self.plan_state.reset_for_session(plan_mode);
    }

    /// The shared tool approval state (bd-cv653.3.19).
    #[must_use]
    pub fn approval_state(&self) -> Option<crate::approval::ApprovalState> {
        self.config.approval_state.clone()
    }

    /// Credential rules shared with project-memory command projections.
    #[must_use]
    pub(crate) const fn secrets_settings(&self) -> Option<&crate::secrets::SecretsSettings> {
        self.config.secrets.as_ref()
    }

    /// Report whether the dialect-repair audit ledger has pending entries.
    pub fn repair_ledger_is_empty(&self) -> Result<bool> {
        self.repair_ledger
            .lock()
            .map(|ledger| ledger.entries.is_empty())
            .map_err(|_| Error::session("dialect repair ledger mutex poisoned"))
    }

    /// Take all pending dialect-repair audit entries.
    pub fn drain_repair_ledger(&self) -> Result<Vec<crate::dialects::RepairEntry>> {
        self.repair_ledger
            .lock()
            .map(|mut ledger| std::mem::take(&mut ledger.entries))
            .map_err(|_| Error::session("dialect repair ledger mutex poisoned"))
    }

    /// Drain magic-keyword activations (bd-cv653.3.6) for the session layer
    /// to persist as Custom entries.
    pub fn drain_keyword_ledger(&mut self) -> Vec<crate::magic_keywords::KeywordActivation> {
        std::mem::take(&mut self.keyword_ledger)
    }

    /// Install glob-scoped imported workspace rules (bd-cv653.6.2). Each rule
    /// is delivered once, as a steering message, the first time a tool call
    /// touches a path matching its globs.
    pub fn set_foreign_scoped_rules(
        &mut self,
        rules: Vec<crate::context_files::ForeignRule>,
        workspace_root: PathBuf,
    ) {
        let matcher = crate::context_files::ScopedRuleMatcher::new(&rules);
        if matcher.is_empty() {
            self.scoped_rules = None;
            return;
        }
        self.scoped_rules = Some(ScopedRuleState {
            rules,
            matcher,
            workspace_root,
            activated: std::collections::HashSet::new(),
        });
    }

    /// Match `tool_calls` against scoped imported rules and queue newly
    /// activated rule content as steering messages (delivered at the next
    /// boundary, before the following provider request).
    fn activate_scoped_rules_for_tool_calls(&mut self, tool_calls: &[ToolCall]) {
        let Some(state) = &mut self.scoped_rules else {
            return;
        };
        let mut newly_activated = Vec::new();
        for tool_call in tool_calls {
            for path in path_like_inputs(&tool_call.arguments) {
                for index in state
                    .matcher
                    .matching_rules(Path::new(&path), &state.workspace_root)
                {
                    if state.activated.insert(index) {
                        newly_activated.push(index);
                    }
                }
            }
        }
        for index in newly_activated {
            let Some(rule) = state.rules.get(index) else {
                continue;
            };
            let text = format!(
                "<imported-rule source=\"{}\" format=\"{}\">\n{}\n</imported-rule>\nThis workspace rule applies to files you are now working with. Follow it for the rest of the session.",
                rule.source,
                rule.format.label(),
                rule.content.trim()
            );
            tracing::info!(
                event = "pi.context_files.scoped_rule_activated",
                source = %rule.source,
                format = rule.format.label(),
                "imported scoped rule activated"
            );
            self.message_queue
                .push_steering(QueuedAgentMessage::generated(Message::User(UserMessage {
                    content: UserContent::Text(text),
                    timestamp: Utc::now().timestamp_millis(),
                })));
        }
    }

    /// Get the current message history.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Clear the message history.
    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.authored_queue_delivery_indices.clear();
    }

    /// Truncate the active message history to `len` (rewind/retry keep the
    /// full span in the session tree — bd-cv653.3.7).
    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
        self.authored_queue_delivery_indices
            .retain(|index| *index < len);
    }

    /// Used by terminal recovery when a dropped Agent future left completed
    /// tool work in memory. Generated user-role messages never qualify.
    pub(crate) fn is_authored_queue_delivery(&self, index: usize) -> bool {
        self.authored_queue_delivery_indices
            .binary_search(&index)
            .is_ok()
    }

    /// Add a message to the history.
    pub fn add_message(&mut self, message: Message) {
        if self.messages.len() >= MAX_AGENT_MESSAGES {
            tracing::warn!(
                "Agent message history full ({} messages), dropping oldest message",
                MAX_AGENT_MESSAGES
            );
            self.messages.remove(0);
            self.authored_queue_delivery_indices.retain_mut(|index| {
                if let Some(shifted) = index.checked_sub(1) {
                    *index = shifted;
                    true
                } else {
                    false
                }
            });
        }
        self.messages.push(message);
    }

    /// Replace the message history.
    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.authored_queue_delivery_indices.clear();
    }

    /// Replace the provider implementation (used for model/provider switching).
    ///
    /// Bare providers have no catalog metadata. This resets image support and
    /// thinking to disabled and the tool-call dialect to native; catalog-aware
    /// callers must install the selected model's capabilities afterwards.
    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = provider;
        self.tools
            .snapshot()
            .model_scope()
            .set(self.provider.name(), self.provider.model_id());
        // A bare provider object does not carry registry capability metadata.
        // Reset fail-closed so a new or external call site cannot accidentally
        // carry capabilities from the previous model. Registry-aware switch
        // paths immediately install the target model's thinking and image caps.
        self.keyword_max_thinking_level = crate::model::ThinkingLevel::Off;
        self.tool_call_dialect = crate::dialects::Dialect::Native;
        self.config.model_accepts_images = false;
    }

    /// Set the model-clamped target used when a turn contains `ultrathink`.
    pub const fn set_keyword_max_thinking_level(&mut self, level: crate::model::ThinkingLevel) {
        self.keyword_max_thinking_level = level;
    }

    /// Override the prose scanned for magic keywords on the next non-resume
    /// run. Attachment-aware callers pass only the user-authored/template
    /// prose, excluding generated `<file>` wrappers and attached file bytes.
    pub fn set_magic_keyword_scan_override(&mut self, source: Option<String>) {
        self.magic_keyword_scan_override = source;
    }

    /// Install the model-catalog-selected tool-call dialect.
    pub const fn set_tool_call_dialect(&mut self, dialect: crate::dialects::Dialect) {
        self.tool_call_dialect = dialect;
    }

    /// Install whether the active model accepts image inputs.
    pub const fn set_model_accepts_images(&mut self, accepts_images: bool) {
        self.config.model_accepts_images = accepts_images;
    }

    /// Whether the active model accepts image inputs.
    #[must_use]
    pub const fn model_accepts_images(&self) -> bool {
        self.config.model_accepts_images
    }

    /// The model-catalog-selected tool-call dialect.
    #[must_use]
    pub const fn tool_call_dialect(&self) -> crate::dialects::Dialect {
        self.tool_call_dialect
    }

    /// Register async fetchers for queued steering/follow-up messages.
    ///
    /// This is additive: multiple sources (e.g. RPC, extensions) can register
    /// fetchers, and the agent will poll all of them.
    pub fn register_message_fetchers(
        &mut self,
        steering: Option<MessageFetcher>,
        follow_up: Option<MessageFetcher>,
    ) {
        if let Some(fetcher) = steering {
            self.steering_fetchers
                .push(RegisteredMessageFetcher::permanent(fetcher));
        }
        if let Some(fetcher) = follow_up {
            self.follow_up_fetchers
                .push(RegisteredMessageFetcher::permanent(fetcher));
        }
    }

    /// Register message sources whose lifetime is owned by one SDK turn.
    ///
    /// The Agent keeps the closures only until their lease expires. This keeps
    /// long-lived sessions from accumulating inert readers while leaving
    /// permanent RPC/extension sources additive.
    pub(crate) fn register_scoped_message_fetchers(
        &mut self,
        lifetime: &Arc<()>,
        steering: Option<MessageFetcher>,
        follow_up: Option<MessageFetcher>,
    ) {
        if let Some(fetcher) = steering {
            self.steering_fetchers
                .push(RegisteredMessageFetcher::scoped(fetcher, lifetime));
        }
        if let Some(fetcher) = follow_up {
            self.follow_up_fetchers
                .push(RegisteredMessageFetcher::scoped(fetcher, lifetime));
        }
    }

    /// Register the owning surface's bounded follow-up source for a resumed
    /// handoff. Unlike additive follow-up sources, this source is fetched on
    /// its own before the first provider request.
    pub(crate) fn register_initial_follow_up_fetcher(&mut self, fetcher: MessageFetcher) {
        self.initial_follow_up_fetcher = Some(fetcher);
    }

    pub(crate) fn has_staged_follow_up(&self) -> bool {
        self.message_queue.follow_up_batch_len() > 0
    }

    /// Extend the tool registry with additional tools (e.g. extension-registered tools).
    pub fn extend_tools<I>(&mut self, tools: I)
    where
        I: IntoIterator<Item = Box<dyn Tool>>,
    {
        // Publishes a new snapshot, so extension hostcalls see the mounted
        // tools on their next lookup as well.
        self.tools.update(|registry| registry.extend(tools));
        self.cached_tool_defs = None; // Invalidate cache when tools change
    }

    /// Install (or clear) the tool-approval prompt handler after
    /// construction.
    ///
    /// Hosts wire their prompt surface later than `AgentConfig` is built —
    /// the ask tool that backs the interactive approval card (issue #196) is
    /// only created once the registry exists — so this mirrors
    /// [`Self::extend_tools`]' post-construction shape.
    pub fn set_tool_approval(&mut self, handler: Option<ToolApprovalHandler>) {
        self.config.tool_approval = handler;
    }

    /// Whether a tool implementation is enabled for this agent session.
    ///
    /// Interactive host commands use this to inherit opt-in tool gates
    /// without exposing the registry itself.
    #[must_use]
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.snapshot().get(name).is_some()
    }

    /// Queue a steering message (delivered after tool completion).
    pub fn queue_steering(&mut self, message: Message) -> u64 {
        self.message_queue
            .push_steering(QueuedAgentMessage::from_authored_message(message))
    }

    /// Queue a follow-up message (delivered when agent becomes idle).
    pub fn queue_follow_up(&mut self, message: Message) -> u64 {
        self.message_queue
            .push_follow_up(QueuedAgentMessage::from_authored_message(message))
    }

    fn queue_generated_steering(&mut self, message: Message) -> u64 {
        self.message_queue
            .push_steering(QueuedAgentMessage::generated(message))
    }

    /// Configure queue delivery modes.
    pub const fn set_queue_modes(&mut self, steering: QueueMode, follow_up: QueueMode) {
        self.message_queue.set_modes(steering, follow_up);
    }

    pub const fn queue_modes(&self) -> (QueueMode, QueueMode) {
        (
            self.message_queue.steering_mode,
            self.message_queue.follow_up_mode,
        )
    }

    /// Enable or pause automatic queue dispatch independently of batch mode.
    ///
    /// Pausing a lane prevents the agent from polling that lane's external
    /// fetchers and from consuming already staged messages automatically.
    /// Direct tool-call continuations are unaffected. Pausing follow-ups also
    /// suppresses synthetic turn-recovery nudges, so an idle/truncated turn
    /// cannot trigger an unexpected provider request.
    pub const fn set_automatic_queue_dispatch(&mut self, steering: bool, follow_up: bool) {
        self.automatic_steering_dispatch = steering;
        self.automatic_follow_up_dispatch = follow_up;
    }

    #[must_use]
    pub const fn automatic_queue_dispatch(&self) -> (bool, bool) {
        (
            self.automatic_steering_dispatch,
            self.automatic_follow_up_dispatch,
        )
    }

    /// Explicitly take one configured steering batch without polling fetchers.
    pub fn take_queued_steering_batch(&mut self) -> Vec<QueuedAgentMessage> {
        self.message_queue.pop_steering()
    }

    /// Explicitly take one configured follow-up batch without polling fetchers.
    ///
    /// Background-job notices are still owner-validated before they leave the
    /// queue; stale notices are restored to their owning session registry.
    pub async fn take_queued_follow_up_batch(&mut self) -> Vec<QueuedAgentMessage> {
        self.pop_follow_up_for_current_session().await
    }

    /// Count queued messages (steering + follow-up).
    #[must_use]
    pub fn queued_message_count(&self) -> usize {
        self.message_queue.pending_count()
    }

    pub(crate) fn discard_queued_persistence_ids(
        &mut self,
        entry_ids: &std::collections::HashSet<String>,
    ) -> usize {
        self.message_queue.discard_persistence_ids(entry_ids)
    }

    pub(crate) fn has_staged_delivery(&self, delivery: &QueuedAgentMessage) -> bool {
        self.message_queue.contains_delivery(delivery)
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    /// The session undo recorder attached to the tool registry, if any
    /// (bd-cv653.3.13).
    #[must_use]
    pub fn mutation_recorder(&self) -> Option<Arc<crate::undo::FileMutationRecorder>> {
        self.tools.snapshot().mutation_recorder()
    }

    pub const fn stream_options(&self) -> &StreamOptions {
        &self.config.stream_options
    }

    pub const fn stream_options_mut(&mut self) -> &mut StreamOptions {
        &mut self.config.stream_options
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.config.system_prompt.as_deref()
    }

    pub fn set_system_prompt(&mut self, system_prompt: Option<String>) {
        self.config.system_prompt = system_prompt;
    }

    /// What the next completion request would carry: the system prompt,
    /// tool definitions and messages after the same filtering a real request
    /// applies. OMP `/dump` writes this beside the transcript.
    pub fn request_context_json(&mut self) -> serde_json::Value {
        let context = self.build_context();
        let tools: Vec<serde_json::Value> = context
            .tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect();
        serde_json::json!({
            "systemPrompt": context.system_prompt,
            "tools": tools,
            "messages": context.messages,
        })
    }

    /// Build context for a completion request.
    fn build_context(&mut self) -> Context<'_> {
        // `display` governs TUI rendering only: a hidden custom message still
        // reaches the provider, which is how extension hooks inject context
        // the model must see without showing it to the user (the
        // before_agent_start contract in extension_events.rs). The one
        // exception is pi's own provenance records (see
        // `context_excluded_custom_message`): they are persisted hidden for
        // audit and resume, and their payload reaches the model another way
        // (the semantic bundle rides the system prompt), so replaying them
        // would only spend context budget. Until 2026-09-02 every hidden
        // custom message was dropped here, which silently discarded hidden
        // hook injections.
        let has_excluded = self
            .messages
            .iter()
            .any(|m| matches!(m, Message::Custom(c) if context_excluded_custom_message(c)));
        let needs_image_filter = (self.config.block_images || !self.config.model_accepts_images)
            && self.messages.iter().any(message_has_images);
        let messages: Cow<'_, [Message]> = if needs_image_filter || has_excluded {
            let mut msgs = self.messages.clone();
            msgs.retain(|m| match m {
                Message::Custom(c) => !context_excluded_custom_message(c),
                _ => true,
            });
            apply_image_input_policy(
                &mut msgs,
                self.config.block_images,
                self.config.model_accepts_images,
            );
            Cow::Owned(msgs)
        } else {
            Cow::Borrowed(self.messages.as_slice())
        };

        // Borrow cached tool defs if available; otherwise build + cache + borrow.
        // Load modes (bd-cv653.1.6): discoverable-tier tools are excluded
        // until promoted; the generation counter invalidates on promotion.
        // The registry version covers every published swap, including the
        // ones made from extension hostcalls (`setActiveTools`), so those
        // reach the next provider request without out-of-band invalidation.
        let generation = self
            .tool_defs_generation
            .load(std::sync::atomic::Ordering::SeqCst)
            ^ self.tools.version().rotate_left(32);
        let cache_fresh = self
            .cached_tool_defs
            .as_ref()
            .is_some_and(|cached| cached.0 == generation);
        if !cache_fresh {
            let promoted = self
                .promoted_tools
                .lock()
                .map(|set| set.clone())
                .unwrap_or_default();
            let registry = self.tools.snapshot();
            let defs: Vec<ToolDef> = registry
                .tools()
                .iter()
                .filter(|t| !registry.is_discoverable(t.name()) || promoted.contains(t.name()))
                .map(|t| ToolDef {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: t.parameters(),
                })
                .collect();
            self.cached_tool_defs = Some((generation, defs));
        }
        let tools = Cow::Borrowed(
            self.cached_tool_defs
                .as_ref()
                .map(|(_, defs)| defs.as_slice())
                .expect("cache populated above"),
        );

        Context {
            system_prompt: self.config.system_prompt.as_deref().map(Cow::Borrowed),
            messages,
            tools,
        }
    }

    /// Run the agent with a user message.
    ///
    /// Returns a stream of events and the final assistant message.
    pub async fn run(
        &mut self,
        user_input: impl Into<String>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_with_abort(user_input, None, on_event).await
    }

    /// Run the agent with a user message and abort support.
    pub async fn run_with_abort(
        &mut self,
        user_input: impl Into<String>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        // Add user message
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(user_input.into()),
            timestamp: Utc::now().timestamp_millis(),
        });

        // Run the agent loop
        self.run_loop(vec![user_message], Arc::new(on_event), abort)
            .await
    }

    /// Run the agent with structured content (text + images).
    pub async fn run_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_with_content_with_abort(content, None, on_event)
            .await
    }

    /// Run the agent with structured content (text + images) and abort support.
    pub async fn run_with_content_with_abort(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        // Add user message
        let user_message = Message::User(UserMessage {
            content: UserContent::Blocks(content),
            timestamp: Utc::now().timestamp_millis(),
        });

        // Run the agent loop
        self.run_loop(vec![user_message], Arc::new(on_event), abort)
            .await
    }

    /// Run the agent with a pre-constructed user message and abort support.
    pub async fn run_with_message_with_abort(
        &mut self,
        message: Message,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop(vec![message], Arc::new(on_event), abort)
            .await
    }

    /// Run the agent with a pre-constructed prompt list and abort support.
    pub async fn run_with_messages_with_abort(
        &mut self,
        messages: Vec<Message>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop(messages, Arc::new(on_event), abort).await
    }

    /// Continue the agent loop without adding a new prompt message (used for retries).
    pub async fn run_continue_with_abort(
        &mut self,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop(Vec::new(), Arc::new(on_event), abort).await
    }

    /// Continue without a new top-level prompt, draining follow-ups before the
    /// first provider request. RPC uses this when a follow-up was accepted in
    /// the narrow interval after the prior run's final queue drain.
    pub async fn run_continue_with_follow_up_with_abort(
        &mut self,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_continue_with_follow_up_on_ready_with_abort(abort, || true, on_event)
            .await
    }

    pub(crate) async fn run_continue_with_follow_up_on_ready_with_abort(
        &mut self,
        abort: Option<AbortSignal>,
        on_ready: impl FnOnce() -> bool + Send + 'static,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_loop_with_initial_follow_up(
            Vec::new(),
            true,
            Some(Box::new(on_ready)),
            Arc::new(on_event),
            abort,
        )
        .await
    }

    /// Outbound secret screening at the primary provider boundary.
    ///
    /// The whole request is transactional: all mutations happen against a
    /// cloned vault and owned context, and the live vault is installed only
    /// after every field succeeds. This prevents a late refusal from learning
    /// credentials or consuming placeholder identities.
    #[allow(clippy::too_many_lines)] // one transaction; splitting it would scatter the commit point
    fn apply_secrets_outbound(
        &mut self,
        mut context: Context<'static>,
    ) -> Result<Context<'static>> {
        let mode = crate::secrets::SecretsMode::from_setting(
            self.config.secrets.as_ref().and_then(|s| s.mode.as_deref()),
        );
        if mode == crate::secrets::SecretsMode::Off {
            return Ok(context);
        }
        let extra = crate::secrets::compile_extra_patterns(
            self.config
                .secrets
                .as_ref()
                .and_then(|s| s.extra_patterns.as_deref())
                .unwrap_or(&[]),
        );
        let mut staged_vault = self.secrets_vault.clone();

        // Discover across the complete screenable request before rewriting any
        // one field. A later assignment such as {"api_key": "..."} can
        // therefore identify an earlier bare echo of the same opaque value.
        // Binary image/media bytes and provider-opaque redacted reasoning are
        // deliberately absent from this projection. Block mode refuses here on
        // any detection, signed and paused content included; discovery never
        // rewrites, so it cannot fail on content that is replayed verbatim.
        let discovery = Self::secrets_discovery_projection(&context);
        crate::secrets::discover_outbound_json(&discovery, &mut staged_vault, mode, &extra)?;

        let mut total = 0usize;
        let mut labels: Vec<String> = Vec::new();

        if let Some(prompt) = context.system_prompt.as_deref() {
            context.system_prompt = Some(Cow::Owned(Self::secrets_transform_text(
                prompt,
                &mut staged_vault,
                mode,
                &extra,
                &mut total,
                &mut labels,
            )?));
        }

        for message in context.messages.to_mut().iter_mut() {
            match message {
                Message::User(user) => Self::secrets_transform_user_content(
                    &mut user.content,
                    &mut staged_vault,
                    mode,
                    &extra,
                    &mut total,
                    &mut labels,
                )?,
                Message::Assistant(assistant) => {
                    let same_origin = assistant.api == self.provider.api()
                        && assistant.provider == self.provider.name()
                        && assistant.model == self.provider.model_id();
                    // Paused server-tool responses and signed blocks must remain
                    // byte/structure stable when replayed to their original
                    // provider, API and model. After a switch, these bytes are
                    // ordinary history: screen them and discard signatures so
                    // the new provider cannot receive a now-known raw secret
                    // or a signature over rewritten content. Block mode has
                    // already refused detections in either case above.
                    if same_origin && assistant.stop_reason == StopReason::PauseTurn {
                        // Typed request-wide discovery above covers the
                        // screenable text, arguments and media metadata.
                        // Serializing this whole message for another scan
                        // would incorrectly treat payloads and signatures as
                        // credential-bearing text in block mode.
                        continue;
                    }

                    let assistant_mut = Arc::make_mut(assistant);
                    for block in &mut assistant_mut.content {
                        Self::secrets_transform_content_block(
                            block,
                            same_origin,
                            &mut staged_vault,
                            mode,
                            &extra,
                            &mut total,
                            &mut labels,
                        )?;
                    }
                    if let Some(error_message) = assistant_mut.error_message.as_mut() {
                        *error_message = Self::secrets_transform_text(
                            error_message,
                            &mut staged_vault,
                            mode,
                            &extra,
                            &mut total,
                            &mut labels,
                        )?;
                    }
                    if let Some(details) = assistant_mut.stop_details.as_mut()
                        && let Some(explanation) = details.explanation.as_mut()
                    {
                        *explanation = Self::secrets_transform_text(
                            explanation,
                            &mut staged_vault,
                            mode,
                            &extra,
                            &mut total,
                            &mut labels,
                        )?;
                    }
                }
                Message::ToolResult(result) => {
                    let result_mut = Arc::make_mut(result);
                    for block in &mut result_mut.content {
                        Self::secrets_transform_content_block(
                            block,
                            false,
                            &mut staged_vault,
                            mode,
                            &extra,
                            &mut total,
                            &mut labels,
                        )?;
                    }
                    if let Some(details) = result_mut.details.as_mut() {
                        *details = Self::secrets_transform_json(
                            details,
                            &mut staged_vault,
                            mode,
                            &extra,
                            &mut total,
                            &mut labels,
                        )?;
                    }
                }
                Message::Custom(custom) => {
                    custom.content = Self::secrets_transform_text(
                        &custom.content,
                        &mut staged_vault,
                        mode,
                        &extra,
                        &mut total,
                        &mut labels,
                    )?;
                    if let Some(details) = custom.details.as_mut() {
                        *details = Self::secrets_transform_json(
                            details,
                            &mut staged_vault,
                            mode,
                            &extra,
                            &mut total,
                            &mut labels,
                        )?;
                    }
                }
            }
        }

        for tool in context.tools.to_mut().iter_mut() {
            // Tool names are routing identifiers. A replacement here would
            // advertise a name the local registry cannot execute, so names are
            // never rewritten. Only block mode refuses; obfuscate mode sends
            // the name as registered (MCP server keys are user configuration).
            let screened_name = Self::secrets_transform_text(
                &tool.name,
                &mut staged_vault,
                mode,
                &extra,
                &mut total,
                &mut labels,
            )?;
            if screened_name != tool.name && mode == crate::secrets::SecretsMode::Block {
                return Err(Error::validation(
                    "PI_SECRET_IDENTIFIER: tool name contains secret material and cannot be rewritten safely"
                        .to_string(),
                ));
            }
            tool.description = Self::secrets_transform_text(
                &tool.description,
                &mut staged_vault,
                mode,
                &extra,
                &mut total,
                &mut labels,
            )?;
            tool.parameters = Self::secrets_transform_json(
                &tool.parameters,
                &mut staged_vault,
                mode,
                &extra,
                &mut total,
                &mut labels,
            )?;
        }

        self.secrets_vault = staged_vault;
        self.refresh_compaction_privacy();
        if total > 0 {
            tracing::info!(
                event = "pi.secrets.outbound",
                detections = total,
                rules = ?labels,
                "secrets screened in outbound provider context (redacted)"
            );
        }
        Ok(context)
    }

    fn secrets_discovery_projection(context: &Context<'_>) -> Value {
        let mut fields = Vec::new();
        if let Some(prompt) = context.system_prompt.as_deref() {
            fields.push(Value::String(prompt.to_string()));
        }
        for message in context.messages.iter() {
            match message {
                Message::User(user) => match &user.content {
                    UserContent::Text(text) => fields.push(Value::String(text.clone())),
                    UserContent::Blocks(blocks) => {
                        Self::secrets_discovery_blocks(blocks, &mut fields);
                    }
                },
                Message::Assistant(assistant) => {
                    Self::secrets_discovery_blocks(&assistant.content, &mut fields);
                    if let Some(error) = &assistant.error_message {
                        fields.push(Value::String(error.clone()));
                    }
                    if let Some(explanation) = assistant
                        .stop_details
                        .as_ref()
                        .and_then(|details| details.explanation.as_ref())
                    {
                        fields.push(Value::String(explanation.clone()));
                    }
                }
                Message::ToolResult(result) => {
                    Self::secrets_discovery_blocks(&result.content, &mut fields);
                    if let Some(details) = &result.details {
                        fields.push(details.clone());
                    }
                }
                Message::Custom(custom) => {
                    fields.push(Value::String(custom.content.clone()));
                    if let Some(details) = &custom.details {
                        fields.push(details.clone());
                    }
                }
            }
        }
        for tool in context.tools.iter() {
            fields.push(Value::String(tool.name.clone()));
            fields.push(Value::String(tool.description.clone()));
            fields.push(tool.parameters.clone());
        }
        Value::Array(fields)
    }

    fn secrets_discovery_blocks(blocks: &[ContentBlock], fields: &mut Vec<Value>) {
        for block in blocks {
            match block {
                ContentBlock::Text(text) => fields.push(Value::String(text.text.clone())),
                ContentBlock::Thinking(thinking) => {
                    fields.push(Value::String(thinking.thinking.clone()));
                }
                ContentBlock::ToolCall(call) => fields.push(call.arguments.clone()),
                ContentBlock::Image(image) => {
                    fields.push(Value::String(image.mime_type.clone()));
                }
                ContentBlock::Media(media) => {
                    fields.push(Value::String(media.mime_type.clone()));
                    if let Some(name) = &media.name {
                        fields.push(Value::String(name.clone()));
                    }
                }
                ContentBlock::RedactedThinking(_) => {}
            }
        }
    }

    fn secrets_add_audit(
        audit: crate::secrets::TransformAudit,
        total: &mut usize,
        labels: &mut Vec<String>,
    ) {
        *total = total.saturating_add(audit.detections);
        for rule in audit.rules {
            if !labels.contains(&rule) {
                labels.push(rule);
            }
        }
    }

    fn secrets_transform_json(
        value: &Value,
        vault: &mut crate::secrets::SecretVault,
        mode: crate::secrets::SecretsMode,
        extra: &[regex::Regex],
        total: &mut usize,
        labels: &mut Vec<String>,
    ) -> Result<Value> {
        let (output, audit) = crate::secrets::transform_outbound_json(value, vault, mode, extra)?;
        Self::secrets_add_audit(audit, total, labels);
        Ok(output)
    }

    fn secrets_transform_text(
        text: &str,
        vault: &mut crate::secrets::SecretVault,
        mode: crate::secrets::SecretsMode,
        extra: &[regex::Regex],
        total: &mut usize,
        labels: &mut Vec<String>,
    ) -> Result<String> {
        let value = Value::String(text.to_string());
        let output = Self::secrets_transform_json(&value, vault, mode, extra, total, labels)?;
        output.as_str().map(ToString::to_string).ok_or_else(|| {
            Error::validation(
                "PI_SECRET_JSON_PRIMITIVE: text screening changed the JSON value type".to_string(),
            )
        })
    }

    /// Media payload bytes stay opaque, but names and MIME labels can appear
    /// in provider-visible text. MIME labels also control native transport
    /// routing, so replacing a secret there cannot safely produce a usable
    /// identifier. Refuse instead of sending the secret or a malformed type.
    fn secrets_transform_media_metadata(
        mime_type: &str,
        name: Option<&mut String>,
        vault: &mut crate::secrets::SecretVault,
        mode: crate::secrets::SecretsMode,
        extra: &[regex::Regex],
        total: &mut usize,
        labels: &mut Vec<String>,
    ) -> Result<()> {
        if Self::secrets_transform_text(mime_type, vault, mode, extra, total, labels)? != mime_type
        {
            return Err(Error::validation(
                "PI_SECRET_MEDIA_MIME: media MIME type contains secret material and cannot be rewritten without changing transport; provide a nonsecret MIME type",
            ));
        }
        if let Some(name) = name {
            *name = Self::secrets_transform_text(name, vault, mode, extra, total, labels)?;
        }
        Ok(())
    }

    /// `same_origin_assistant` is true only for assistant output from the
    /// current provider, API and model. Foreign history, user and tool-result
    /// blocks receive neither signature nor primitive replay exceptions (an
    /// extension can set a signature on any JSON).
    fn secrets_transform_content_block(
        block: &mut ContentBlock,
        same_origin_assistant: bool,
        vault: &mut crate::secrets::SecretVault,
        mode: crate::secrets::SecretsMode,
        extra: &[regex::Regex],
        total: &mut usize,
        labels: &mut Vec<String>,
    ) -> Result<()> {
        // A signed assistant block (a real provider signature, or a plain item
        // id such as OpenAI's `fc_`/`msg_` that replay is keyed on) stays intact
        // only for its original destination outside block mode. Skipping the
        // rewrite (not just discarding it) also keeps a type-changing
        // replacement from refusing the request.
        let keep_signed = same_origin_assistant && mode != crate::secrets::SecretsMode::Block;
        match block {
            ContentBlock::Text(text) => {
                if keep_signed && text.text_signature.is_some() {
                    return Ok(());
                }
                if !same_origin_assistant {
                    text.text_signature = None;
                }
                text.text =
                    Self::secrets_transform_text(&text.text, vault, mode, extra, total, labels)?;
            }
            ContentBlock::Thinking(thinking) => {
                if keep_signed && thinking.thinking_signature.is_some() {
                    return Ok(());
                }
                if !same_origin_assistant {
                    thinking.thinking_signature = None;
                }
                thinking.thinking = Self::secrets_transform_text(
                    &thinking.thinking,
                    vault,
                    mode,
                    extra,
                    total,
                    labels,
                )?;
            }
            ContentBlock::ToolCall(call) => {
                if keep_signed && call.thought_signature.is_some() {
                    return Ok(());
                }
                if !same_origin_assistant {
                    call.thought_signature = None;
                }
                call.arguments = if same_origin_assistant {
                    let (output, audit) = crate::secrets::transform_assistant_replay_json(
                        &call.arguments,
                        vault,
                        mode,
                        extra,
                    )?;
                    Self::secrets_add_audit(audit, total, labels);
                    output
                } else {
                    Self::secrets_transform_json(
                        &call.arguments,
                        vault,
                        mode,
                        extra,
                        total,
                        labels,
                    )?
                };
            }
            ContentBlock::Image(image) => Self::secrets_transform_media_metadata(
                &image.mime_type,
                None,
                vault,
                mode,
                extra,
                total,
                labels,
            )?,
            ContentBlock::Media(media) => Self::secrets_transform_media_metadata(
                &media.mime_type,
                media.name.as_mut(),
                vault,
                mode,
                extra,
                total,
                labels,
            )?,
            ContentBlock::RedactedThinking(_) => {}
        }
        Ok(())
    }

    /// Outbound secret hygiene for user content, including structured blocks.
    fn secrets_transform_user_content(
        content: &mut UserContent,
        vault: &mut crate::secrets::SecretVault,
        mode: crate::secrets::SecretsMode,
        extra: &[regex::Regex],
        total: &mut usize,
        labels: &mut Vec<String>,
    ) -> Result<()> {
        match content {
            UserContent::Text(text) => {
                *text = Self::secrets_transform_text(text, vault, mode, extra, total, labels)?;
            }
            UserContent::Blocks(blocks) => {
                for block in blocks {
                    Self::secrets_transform_content_block(
                        block, false, vault, mode, extra, total, labels,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn build_abort_message(&self, partial: Option<&AssistantMessage>) -> AssistantMessage {
        let mut message = partial.cloned().unwrap_or_else(|| AssistantMessage {
            content: Vec::new(),
            api: self.provider.api().to_string(),
            provider: self.provider.name().to_string(),
            model: self.provider.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Aborted,
            stop_details: None,
            error_message: Some("Aborted".to_string()),
            timestamp: Utc::now().timestamp_millis(),
        });
        message.stop_reason = StopReason::Aborted;
        message.error_message = Some("Aborted".to_string());
        message.timestamp = Utc::now().timestamp_millis();
        message
    }

    fn build_error_message(
        &self,
        partial: Option<&AssistantMessage>,
        error_message: impl Into<String>,
    ) -> AssistantMessage {
        let error_message = error_message.into();
        let mut message = partial.cloned().unwrap_or_else(|| AssistantMessage {
            content: Vec::new(),
            api: self.provider.api().to_string(),
            provider: self.provider.name().to_string(),
            model: self.provider.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            stop_details: None,
            error_message: Some(error_message.clone()),
            timestamp: Utc::now().timestamp_millis(),
        });
        message.stop_reason = StopReason::Error;
        message.error_message = Some(error_message);
        message.timestamp = Utc::now().timestamp_millis();
        message
    }

    /// Structured turn-ending provider failure (#209), classified from the
    /// flattened error text and stamped with the provider the request went to.
    fn build_provider_error_event(&self, session_id: &Arc<str>, message: &str) -> AgentEvent {
        let provider = self.provider.name().to_string();
        AgentEvent::ProviderError {
            session_id: Arc::clone(session_id),
            summary: crate::error::ProviderErrorSummary::from_error_text(Some(&provider), message),
            provider,
            model: self.provider.model_id().to_string(),
            message: message.to_string(),
        }
    }

    /// The main agent loop. Magic keywords (bd-cv653.3.6) mutate the
    /// thinking level and system prompt for *this turn only*; snapshot and
    /// restore them here so a single `ultrathink` does not pin every later
    /// turn at max thinking and directives do not accrete forever.
    async fn run_loop(
        &mut self,
        prompts: Vec<Message>,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
    ) -> Result<AssistantMessage> {
        self.run_loop_with_initial_follow_up(prompts, false, None, on_event, abort)
            .await
    }

    async fn run_loop_with_initial_follow_up(
        &mut self,
        prompts: Vec<Message>,
        initial_follow_up: bool,
        initial_queue_ready: Option<Box<dyn FnOnce() -> bool + Send>>,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
    ) -> Result<AssistantMessage> {
        if !prompts.is_empty() || initial_follow_up {
            self.retry_keyword_activations.clear();
        }
        let settings_scope = AgentTurnSettingsGuard::new(self);
        let result = settings_scope
            .agent
            .run_loop_inner(
                prompts,
                initial_follow_up,
                initial_queue_ready,
                on_event,
                abort,
            )
            .await;
        drop(settings_scope);
        if result.as_ref().is_ok_and(|message| {
            !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
        }) {
            self.retry_keyword_activations.clear();
        }
        result
    }

    fn apply_magic_keyword_effects(&mut self, hits: &[crate::magic_keywords::KeywordActivation]) {
        let keyword_max_thinking_level = self.keyword_max_thinking_level;
        for hit in hits {
            if hit.action == "ultrathink" {
                self.stream_options_mut().thinking_level = Some(keyword_max_thinking_level);
            }
        }
        let directives =
            crate::magic_keywords::directives_for(hits, self.config.keyword_settings.as_ref());
        if !directives.is_empty() {
            let block = directives.join("\n");
            match &mut self.config.system_prompt {
                Some(existing) => {
                    existing.push_str("\n\n");
                    existing.push_str(&block);
                }
                none_slot => {
                    *none_slot = Some(block);
                }
            }
        }
    }

    /// Apply magic-keyword effects for one user-authored message delivered
    /// during the current logical turn. The caller owns the turn-wide
    /// de-duplication set so the initial prompt and steering messages cannot
    /// inject the same directive repeatedly. Follow-ups begin a fresh scope.
    fn apply_magic_keywords_for_message(
        &mut self,
        message: &Message,
        turn_keyword_words: &mut std::collections::HashSet<String>,
    ) {
        let Message::User(user) = message else {
            return;
        };
        let scan_text = match &user.content {
            UserContent::Text(text) => Cow::Borrowed(text.as_str()),
            UserContent::Blocks(blocks) => Cow::Owned(
                blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        };
        self.apply_magic_keywords_for_text(&scan_text, turn_keyword_words);
    }

    fn apply_magic_keywords_for_delivery(
        &mut self,
        delivery: &QueuedAgentMessage,
        turn_keyword_words: &mut std::collections::HashSet<String>,
    ) {
        if let Some(source) = delivery.keyword_scan_source() {
            self.apply_magic_keywords_for_text(source, turn_keyword_words);
        }
    }

    /// Consume a delivery immediately before appending its message. Every
    /// normal and terminal drain uses this boundary so persistence can still
    /// distinguish authored input from generated user-role messages.
    fn consume_queued_delivery(&mut self, delivery: QueuedAgentMessage) -> Message {
        if delivery.keyword_scan_source().is_some() {
            self.authored_queue_delivery_indices.push(self.messages.len());
        }
        delivery.into_message()
    }

    fn apply_magic_keywords_for_text(
        &mut self,
        scan_text: &str,
        turn_keyword_words: &mut std::collections::HashSet<String>,
    ) {
        let hits: Vec<_> =
            crate::magic_keywords::detect(scan_text, self.config.keyword_settings.as_ref())
                .into_iter()
                .filter(|hit| turn_keyword_words.insert(hit.word.clone()))
                .collect();
        if hits.is_empty() {
            return;
        }

        self.apply_magic_keyword_effects(&hits);
        self.retry_keyword_activations.extend(hits.iter().cloned());
        self.keyword_ledger.extend(hits);
    }

    #[allow(clippy::too_many_lines)]
    async fn run_loop_inner(
        &mut self,
        prompts: Vec<Message>,
        initial_follow_up: bool,
        mut initial_queue_ready: Option<Box<dyn FnOnce() -> bool + Send>>,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
    ) -> Result<AssistantMessage> {
        let loop_cx = crate::agent_cx::AgentCx::for_current_or_request();
        let session_id: Arc<str> = self
            .config
            .stream_options
            .session_id
            .as_deref()
            .unwrap_or("")
            .into();
        let mut iterations = 0usize;
        let mut pause_turn_continuations = 0usize;
        let mut warned_at_handoff_threshold = false;
        let mut turn_index: usize = 0;
        let mut new_messages: Vec<Message> = Vec::with_capacity(prompts.len() + 8);
        let mut last_assistant: Option<Arc<AssistantMessage>> = None;
        let mut turn_keyword_words = std::collections::HashSet::new();
        let turn_baseline_thinking = self.config.stream_options.thinking_level;
        let turn_baseline_system_prompt = self.config.system_prompt.clone();

        if prompts.is_empty() && !self.retry_keyword_activations.is_empty() {
            let retry_hits = self.retry_keyword_activations.clone();
            turn_keyword_words.extend(retry_hits.iter().map(|hit| hit.word.clone()));
            self.apply_magic_keyword_effects(&retry_hits);
        }

        // A resumed follow-up must fetch and validate the owning surface before
        // emitting lifecycle events or consuming any older staged follow-up.
        // Always poll the owning source: a private staged batch does not prove
        // that the exact RPC batch which triggered this continuation was fetched.
        let mut follow_up_staged = false;
        let mut required_initial_follow_up_pending = initial_follow_up;
        if initial_follow_up {
            self.fetch_initial_follow_up_messages().await;
            follow_up_staged = self.message_queue.follow_up_batch_len() > 0;
            if !follow_up_staged {
                return Err(Error::session(
                    "accepted follow-up was unavailable at continuation boundary",
                ));
            }
            if let Some(on_ready) = initial_queue_ready.take()
                && !on_ready()
            {
                return Err(Error::session(
                    "required follow-up source was unavailable at continuation boundary",
                ));
            }
        }

        let agent_start_event = AgentEvent::AgentStart {
            session_id: session_id.clone(),
        };
        self.dispatch_extension_lifecycle_event(&agent_start_event)
            .await;
        on_event(agent_start_event);

        let mut keyword_scan_override = self.magic_keyword_scan_override.take();
        for prompt in prompts {
            // Magic keywords (bd-cv653.3.6): pre-send prose scan of the user
            // message. ultrathink raises the turn's thinking level to the
            // active model's pre-clamped maximum; orchestrate/workflowz/custom words
            // append their directive to the system prompt (appended, never
            // inserted, so provider prompt caches stay valid).
            if matches!(prompt, Message::User(_))
                && let Some(source) = keyword_scan_override.take()
            {
                self.apply_magic_keywords_for_text(&source, &mut turn_keyword_words);
            } else {
                self.apply_magic_keywords_for_message(&prompt, &mut turn_keyword_words);
            }
            self.messages.push(prompt.clone());
            on_event(AgentEvent::MessageStart {
                message: prompt.clone(),
            });
            on_event(AgentEvent::MessageEnd {
                message: prompt.clone(),
            });
            new_messages.push(prompt);
        }

        // Delivery boundary: ordinary continuation starts with steering; the
        // RPC late-follow-up repair explicitly starts with follow-ups instead.
        let mut pending_messages = if initial_follow_up {
            Vec::new()
        } else {
            self.drain_steering_messages().await
        };
        let mut turn_recovery =
            crate::turn_recovery::TurnRecoveryState::new(self.config.turn_recovery);

        // Wall-clock run cap (bd-cv653.3.7, --max-time): checked at turn
        // boundaries — never mid-tool-call. On expiry the run stops with a
        // marker instead of starting another turn.
        let run_started = std::time::Instant::now();
        let max_time = self.config.max_time;

        'agent: loop {
            let mut has_more_tool_calls = true;
            let mut steering_after_tools: Option<Vec<QueuedAgentMessage>> = None;

            while has_more_tool_calls || !pending_messages.is_empty() {
                if !required_initial_follow_up_pending
                    && let Some(cap) = max_time
                    && run_started.elapsed() >= cap
                {
                    // The owning surface already acknowledged these messages.
                    // Put the undelivered batch back at the front before
                    // returning the time-cap marker so a later run can resume
                    // it in the original order.
                    self.message_queue
                        .restore_steering_front_lossless(std::mem::take(&mut pending_messages));
                    let marker = format!(
                        "time cap reached after {}s (--max-time); stopping at the turn boundary",
                        cap.as_secs()
                    );
                    tracing::info!("{marker}");
                    let assistant = Arc::new(AssistantMessage {
                        content: vec![crate::model::ContentBlock::Text(TextContent::new(format!(
                            "[time cap reached] {marker}"
                        )))],
                        timestamp: Utc::now().timestamp_millis(),
                        ..AssistantMessage::default()
                    });
                    let marker_message = Message::Assistant(Arc::clone(&assistant));
                    self.messages.push(marker_message.clone());
                    new_messages.push(marker_message.clone());
                    on_event(AgentEvent::MessageStart {
                        message: marker_message.clone(),
                    });
                    on_event(AgentEvent::MessageEnd {
                        message: marker_message,
                    });
                    last_assistant = Some(assistant);
                    break 'agent;
                }

                let delivering_follow_up = follow_up_staged;
                if delivering_follow_up {
                    let current_follow_up = self.pop_follow_up_for_current_session().await;
                    follow_up_staged = false;
                    required_initial_follow_up_pending = false;
                    if current_follow_up.is_empty() {
                        // The session changed after staging and every queued
                        // job notice belonged to the previous owner. Reach the
                        // normal idle restaging boundary without emitting a
                        // lifecycle start or invoking the provider empty.
                        break;
                    }
                    // Follow-ups are new logical user turns. Restore the
                    // caller's baseline before scanning their keywords so
                    // steering-only effort and directives cannot leak across
                    // the boundary.
                    self.config.stream_options.thinking_level = turn_baseline_thinking;
                    self.config
                        .system_prompt
                        .clone_from(&turn_baseline_system_prompt);
                    turn_keyword_words.clear();
                    pending_messages = current_follow_up;
                }

                let current_turn_index = turn_index;
                let turn_latency = Arc::new(StdMutex::new(TurnLatencyAccumulator::started()));
                let turn_start_event = AgentEvent::TurnStart {
                    session_id: session_id.clone(),
                    turn_index: current_turn_index,
                    timestamp: Utc::now().timestamp_millis(),
                };
                self.dispatch_extension_lifecycle_event(&turn_start_event)
                    .await;
                on_event(turn_start_event);

                for delivery in std::mem::take(&mut pending_messages) {
                    self.apply_magic_keywords_for_delivery(&delivery, &mut turn_keyword_words);
                    let message = self.consume_queued_delivery(delivery);
                    self.messages.push(message.clone());
                    on_event(AgentEvent::MessageStart {
                        message: message.clone(),
                    });
                    on_event(AgentEvent::MessageEnd {
                        message: message.clone(),
                    });
                    new_messages.push(message);
                }

                if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                    let abort_message = self.build_abort_message(None);
                    let message = Message::assistant(abort_message.clone());

                    self.messages.push(message.clone());
                    new_messages.push(message.clone());
                    on_event(AgentEvent::MessageStart {
                        message: message.clone(),
                    });
                    on_event(AgentEvent::MessageEnd {
                        message: message.clone(),
                    });

                    let turn_end_event = AgentEvent::TurnEnd {
                        session_id: session_id.clone(),
                        turn_index: current_turn_index,
                        message,
                        tool_results: Vec::new(),
                        latency_breakdown: snapshot_turn_latency(&turn_latency),
                    };
                    self.dispatch_extension_lifecycle_event(&turn_end_event)
                        .await;
                    on_event(turn_end_event);
                    let agent_end_event = AgentEvent::AgentEnd {
                        session_id: session_id.clone(),
                        messages: std::mem::take(&mut new_messages),
                        error: Some(
                            abort_message
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "Aborted".to_string()),
                        ),
                    };
                    self.dispatch_extension_lifecycle_event(&agent_end_event)
                        .await;
                    on_event(agent_end_event);
                    return Ok(abort_message);
                }

                let provider_streaming_started_at = Instant::now();
                let assistant_result = self
                    .stream_assistant_response(Arc::clone(&on_event), abort.clone(), &loop_cx)
                    .await;
                record_provider_streaming_latency(
                    &turn_latency,
                    provider_streaming_started_at.elapsed(),
                );

                let assistant_message = match assistant_result {
                    Ok(msg) => msg,
                    Err(err) => {
                        let err_string = err.to_string();
                        let steering_to_add = self.drain_steering_messages().await;
                        for delivery in steering_to_add {
                            self.apply_magic_keywords_for_delivery(
                                &delivery,
                                &mut turn_keyword_words,
                            );
                            let message = self.consume_queued_delivery(delivery);
                            self.messages.push(message.clone());
                            on_event(AgentEvent::MessageStart {
                                message: message.clone(),
                            });
                            on_event(AgentEvent::MessageEnd {
                                message: message.clone(),
                            });
                            new_messages.push(message);
                        }

                        let error_message = self.build_error_message(None, err_string.clone());
                        let assistant_event_message = Message::assistant(error_message.clone());
                        self.messages.push(assistant_event_message.clone());
                        new_messages.push(assistant_event_message.clone());
                        on_event(AgentEvent::MessageStart {
                            message: assistant_event_message.clone(),
                        });
                        on_event(AgentEvent::MessageEnd {
                            message: assistant_event_message.clone(),
                        });
                        on_event(self.build_provider_error_event(&session_id, &err_string));

                        let turn_end_event = AgentEvent::TurnEnd {
                            session_id: session_id.clone(),
                            turn_index: current_turn_index,
                            message: assistant_event_message,
                            tool_results: Vec::new(),
                            latency_breakdown: snapshot_turn_latency(&turn_latency),
                        };
                        self.dispatch_extension_lifecycle_event(&turn_end_event)
                            .await;
                        on_event(turn_end_event);

                        let agent_end_event = AgentEvent::AgentEnd {
                            session_id: session_id.clone(),
                            messages: std::mem::take(&mut new_messages),
                            error: Some(err_string),
                        };
                        self.dispatch_extension_lifecycle_event(&agent_end_event)
                            .await;
                        on_event(agent_end_event);
                        return Err(err);
                    }
                };
                // Wrap in Arc once; share via Arc::clone (O(1)) instead of deep
                // cloning the full AssistantMessage for every consumer.
                let assistant_arc = Arc::new(assistant_message);
                last_assistant = Some(Arc::clone(&assistant_arc));

                let assistant_event_message = Message::Assistant(Arc::clone(&assistant_arc));
                new_messages.push(assistant_event_message.clone());

                if matches!(
                    assistant_arc.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    let steering_to_add = self.drain_steering_messages().await;
                    for delivery in steering_to_add {
                        self.apply_magic_keywords_for_delivery(&delivery, &mut turn_keyword_words);
                        let message = self.consume_queued_delivery(delivery);
                        self.messages.push(message.clone());
                        on_event(AgentEvent::MessageStart {
                            message: message.clone(),
                        });
                        on_event(AgentEvent::MessageEnd {
                            message: message.clone(),
                        });
                        new_messages.push(message);
                    }

                    // A provider-side failure (stream `Error` event, dropped
                    // stream, truncated tool call) surfaces as a stop reason of
                    // `Error`; an abort is the user's doing and not reported
                    // as a provider error.
                    if assistant_arc.stop_reason == StopReason::Error {
                        let message = assistant_arc
                            .error_message
                            .clone()
                            .unwrap_or_else(|| "Request failed".to_string());
                        on_event(self.build_provider_error_event(&session_id, &message));
                    }

                    let turn_end_event = AgentEvent::TurnEnd {
                        session_id: session_id.clone(),
                        turn_index: current_turn_index,
                        message: assistant_event_message.clone(),
                        tool_results: Vec::new(),
                        latency_breakdown: snapshot_turn_latency(&turn_latency),
                    };
                    self.dispatch_extension_lifecycle_event(&turn_end_event)
                        .await;
                    on_event(turn_end_event);
                    let agent_end_event = AgentEvent::AgentEnd {
                        session_id: session_id.clone(),
                        messages: std::mem::take(&mut new_messages),
                        error: assistant_arc.error_message.clone(),
                    };
                    self.dispatch_extension_lifecycle_event(&agent_end_event)
                        .await;
                    on_event(agent_end_event);
                    return Ok(Arc::unwrap_or_clone(assistant_arc));
                }

                let tool_calls = extract_tool_calls(&assistant_arc.content);
                let pause_turn = assistant_arc.stop_reason == StopReason::PauseTurn;
                // A paused Anthropic server-tool turn must be resubmitted
                // verbatim. Its content can contain tool-use blocks, but those
                // are not a request to execute our local tool registry.
                let execute_local_tools = matches!(
                    assistant_arc.stop_reason,
                    StopReason::Stop | StopReason::ToolUse | StopReason::Length
                ) && !tool_calls.is_empty();
                has_more_tool_calls = execute_local_tools;
                if pause_turn {
                    pause_turn_continuations = pause_turn_continuations.saturating_add(1);
                    if pause_turn_continuations <= MAX_PAUSE_TURN_CONTINUATIONS {
                        // The completed assistant message is already in history. The
                        // next stream request therefore resubmits it verbatim with
                        // the same model, tools, and stream options, as Anthropic
                        // requires. No synthetic user message is introduced.
                        has_more_tool_calls = true;
                    } else {
                        tracing::warn!(
                            pause_turn_continuations,
                            max = MAX_PAUSE_TURN_CONTINUATIONS,
                            "pause_turn continuation limit reached"
                        );
                    }
                }

                let mut tool_results: Vec<Arc<ToolResultMessage>> = Vec::new();
                if execute_local_tools {
                    iterations += 1;
                    // Soft handoff: at >=80% of the cap, push a one-shot
                    // steering message so the agent has room to write an
                    // incomplete-handoff envelope before the hard stop. The
                    // queue drains at the next loop iteration via
                    // drain_steering_messages, so the agent observes the
                    // steering before its next assistant turn rather than
                    // after the cap fires.
                    if !warned_at_handoff_threshold
                        && should_warn_at_iteration_threshold(
                            iterations,
                            self.config.max_tool_iterations,
                        )
                    {
                        warned_at_handoff_threshold = true;
                        let warning = Message::User(UserMessage {
                            content: UserContent::Text(iteration_handoff_steering_text(
                                iterations,
                                self.config.max_tool_iterations,
                            )),
                            timestamp: Utc::now().timestamp_millis(),
                        });
                        self.message_queue
                            .push_steering(QueuedAgentMessage::generated(warning));
                        tracing::warn!(
                            iterations,
                            max = self.config.max_tool_iterations,
                            "tool-iteration budget at >=80%; injected handoff steering message"
                        );
                    }
                    if iterations > self.config.max_tool_iterations {
                        let error_message = format!(
                            "Maximum tool iterations ({}) exceeded",
                            self.config.max_tool_iterations
                        );
                        let mut stop_message = (*assistant_arc).clone();
                        stop_message.stop_reason = StopReason::Error;
                        stop_message.error_message = Some(error_message.clone());

                        // Strip dangling tool calls to prevent sequence mismatch on next user prompt.
                        stop_message
                            .content
                            .retain(|b| !matches!(b, crate::model::ContentBlock::ToolCall(_)));

                        let stop_arc = Arc::new(stop_message.clone());
                        let stop_event_message = Message::Assistant(Arc::clone(&stop_arc));

                        // Keep in-memory transcript and event payloads aligned with the
                        // error stop result returned to callers.
                        if let Some(last @ Message::Assistant(_)) = self
                            .messages
                            .iter_mut()
                            .rev()
                            .find(|m| matches!(m, Message::Assistant(_)))
                        {
                            *last = stop_event_message.clone();
                        }
                        if let Some(last @ Message::Assistant(_)) = new_messages.last_mut() {
                            *last = stop_event_message.clone();
                        }

                        let steering_to_add = self.drain_steering_messages().await;
                        for delivery in steering_to_add {
                            self.apply_magic_keywords_for_delivery(
                                &delivery,
                                &mut turn_keyword_words,
                            );
                            let message = self.consume_queued_delivery(delivery);
                            self.messages.push(message.clone());
                            on_event(AgentEvent::MessageStart {
                                message: message.clone(),
                            });
                            on_event(AgentEvent::MessageEnd {
                                message: message.clone(),
                            });
                            new_messages.push(message);
                        }

                        let turn_end_event = AgentEvent::TurnEnd {
                            session_id: session_id.clone(),
                            turn_index: current_turn_index,
                            message: stop_event_message,
                            tool_results: Vec::new(),
                            latency_breakdown: snapshot_turn_latency(&turn_latency),
                        };
                        self.dispatch_extension_lifecycle_event(&turn_end_event)
                            .await;
                        on_event(turn_end_event);

                        let agent_end_event = AgentEvent::AgentEnd {
                            session_id: session_id.clone(),
                            messages: std::mem::take(&mut new_messages),
                            error: Some(error_message),
                        };
                        self.dispatch_extension_lifecycle_event(&agent_end_event)
                            .await;
                        on_event(agent_end_event);

                        return Ok(stop_message);
                    }

                    let outcome = match self
                        .execute_tool_calls(
                            &tool_calls,
                            Arc::clone(&on_event),
                            &mut new_messages,
                            abort.clone(),
                            Arc::clone(&turn_latency),
                        )
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            let steering_to_add = self.drain_steering_messages().await;
                            for delivery in steering_to_add {
                                self.apply_magic_keywords_for_delivery(
                                    &delivery,
                                    &mut turn_keyword_words,
                                );
                                let message = self.consume_queued_delivery(delivery);
                                self.messages.push(message.clone());
                                on_event(AgentEvent::MessageStart {
                                    message: message.clone(),
                                });
                                on_event(AgentEvent::MessageEnd {
                                    message: message.clone(),
                                });
                                new_messages.push(message);
                            }

                            let turn_end_event = AgentEvent::TurnEnd {
                                session_id: session_id.clone(),
                                turn_index: current_turn_index,
                                message: assistant_event_message.clone(),
                                tool_results: Vec::new(),
                                latency_breakdown: snapshot_turn_latency(&turn_latency),
                            };
                            self.dispatch_extension_lifecycle_event(&turn_end_event)
                                .await;
                            on_event(turn_end_event);

                            let agent_end_event = AgentEvent::AgentEnd {
                                session_id: session_id.clone(),
                                messages: std::mem::take(&mut new_messages),
                                error: Some(err.to_string()),
                            };
                            self.dispatch_extension_lifecycle_event(&agent_end_event)
                                .await;
                            on_event(agent_end_event);
                            return Err(err);
                        }
                    };
                    tool_results = outcome.tool_results;
                    steering_after_tools = outcome.steering_messages;
                }

                let tool_messages = tool_results
                    .iter()
                    .map(|r| Message::ToolResult(Arc::clone(r)))
                    .collect::<Vec<_>>();

                let turn_end_event = AgentEvent::TurnEnd {
                    session_id: session_id.clone(),
                    turn_index: current_turn_index,
                    message: assistant_event_message.clone(),
                    tool_results: tool_messages,
                    latency_breakdown: snapshot_turn_latency(&turn_latency),
                };
                self.dispatch_extension_lifecycle_event(&turn_end_event)
                    .await;
                on_event(turn_end_event);

                turn_index = turn_index.saturating_add(1);

                if let Some(steering) = steering_after_tools.take() {
                    pending_messages = steering;
                } else {
                    // Delivery boundary: after assistant completion (no tool calls).
                    pending_messages = self.drain_steering_messages().await;
                }

                // Turn recovery (bd-cv653.3.15): with nothing queued and no
                // tool calls pending, an unexpected mid-task stop earns one
                // synthetic continue nudge (hard-capped) instead of a silent
                // end. The nudge flows through pending_messages so it is
                // evented and persisted like any user message.
                if self.automatic_follow_up_dispatch
                    && pending_messages.is_empty()
                    && !has_more_tool_calls
                {
                    let text = assistant_text_content(&assistant_arc.content);
                    if let Some(action) = turn_recovery.evaluate(assistant_arc.stop_reason, &text) {
                        pending_messages =
                            vec![QueuedAgentMessage::generated(Message::User(UserMessage {
                                content: UserContent::Text(action.nudge_text),
                                timestamp: Utc::now().timestamp_millis(),
                            }))];
                    }
                }
            }

            // Delivery boundary: agent idle (after all tool calls + steering).
            // Check the wall-clock cap before fetching the owning surface's
            // follow-up queue. Accepted RPC messages then remain visible to
            // that surface and can be resumed follow-up-first by its normal
            // late-queue handoff instead of becoming private staged state.
            if let Some(cap) = max_time
                && run_started.elapsed() >= cap
            {
                let marker = format!(
                    "time cap reached after {}s (--max-time); stopping at the turn boundary",
                    cap.as_secs()
                );
                tracing::info!("{marker}");
                let assistant = Arc::new(AssistantMessage {
                    content: vec![crate::model::ContentBlock::Text(TextContent::new(format!(
                        "[time cap reached] {marker}"
                    )))],
                    timestamp: Utc::now().timestamp_millis(),
                    ..AssistantMessage::default()
                });
                let marker_message = Message::Assistant(Arc::clone(&assistant));
                self.messages.push(marker_message.clone());
                new_messages.push(marker_message.clone());
                on_event(AgentEvent::MessageStart {
                    message: marker_message.clone(),
                });
                on_event(AgentEvent::MessageEnd {
                    message: marker_message,
                });
                last_assistant = Some(assistant);
                break;
            }
            follow_up_staged = self.stage_follow_up_messages().await;
            if !follow_up_staged {
                break;
            }
        }

        let Some(final_arc) = last_assistant else {
            return Err(Error::api("Agent completed without assistant message"));
        };

        let agent_end_event = AgentEvent::AgentEnd {
            session_id: session_id.clone(),
            messages: new_messages,
            error: None,
        };
        self.dispatch_extension_lifecycle_event(&agent_end_event)
            .await;
        on_event(agent_end_event);
        Ok(Arc::unwrap_or_clone(final_arc))
    }

    async fn fetch_messages(&self, fetcher: Option<&MessageFetcher>) -> Vec<QueuedAgentMessage> {
        if let Some(fetcher) = fetcher {
            (fetcher)().await
        } else {
            Vec::new()
        }
    }

    async fn dispatch_extension_lifecycle_event(&self, event: &AgentEvent) {
        let Some(extensions) = &self.extensions else {
            return;
        };

        let name = match event {
            AgentEvent::AgentStart { .. } => ExtensionEventName::AgentStart,
            AgentEvent::AgentEnd { .. } => ExtensionEventName::AgentEnd,
            AgentEvent::TurnStart { .. } => ExtensionEventName::TurnStart,
            AgentEvent::TurnEnd { .. } => ExtensionEventName::TurnEnd,
            _ => return,
        };

        let payload = match serde_json::to_value(event) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::warn!("failed to serialize agent lifecycle event (fail-open): {err}");
                return;
            }
        };

        if let Err(err) = extensions.dispatch_event(name, Some(payload)).await {
            tracing::warn!("agent lifecycle extension hook failed (fail-open): {err}");
        }
    }

    /// Build the `before_provider_request` interceptor handed to providers
    /// via [`StreamOptions`] (gh #167 / bd-1q31s). See
    /// [`normalize_before_provider_request_response`] for the accepted
    /// handler-response shapes.
    ///
    /// The extension handler receives the fully-built provider request body
    /// (never auth headers) and may return a rewritten body — either the
    /// payload object directly (upstream pi convention) or wrapped as
    /// `{ "payload": ... }`. Dispatch failures and null/undefined responses
    /// fail open: the provider keeps its original request.
    fn build_before_provider_request_hook(
        extensions: ExtensionManager,
    ) -> crate::provider::BeforeProviderRequestHook {
        crate::provider::BeforeProviderRequestHook::new(move |event| {
            let extensions = extensions.clone();
            Box::pin(async move {
                let payload = match serde_json::to_value(&event) {
                    Ok(payload) => payload,
                    Err(err) => {
                        tracing::warn!(
                            "failed to serialize before_provider_request event (fail-open): {err}"
                        );
                        return None;
                    }
                };
                let response = match extensions
                    .dispatch_event_with_response(
                        ExtensionEventName::BeforeProviderRequest,
                        Some(payload),
                        EXTENSION_EVENT_TIMEOUT_MS,
                    )
                    .await
                {
                    Ok(response) => response?,
                    Err(err) => {
                        tracing::warn!(
                            "before_provider_request extension hook failed (fail-open): {err}"
                        );
                        return None;
                    }
                };
                normalize_before_provider_request_response(response)
            })
        })
    }

    async fn dispatch_context_event(&self, messages: &[Message]) -> Option<Vec<Message>> {
        let Some(extensions) = &self.extensions else {
            return None;
        };

        let payload = json!({ "messages": messages });
        let response = extensions
            .dispatch_event_with_response(
                ExtensionEventName::Context,
                Some(payload),
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await
            .ok()?;

        let value = response?;

        if value.is_null() {
            return None;
        }

        let messages_value = if let Some(obj) = value.as_object() {
            obj.get("messages").cloned()?
        } else if value.is_array() {
            value
        } else {
            return None;
        };

        if messages_value.is_null() {
            return Some(Vec::new());
        }

        match serde_json::from_value(messages_value) {
            Ok(messages) => Some(messages),
            Err(err) => {
                tracing::warn!("context extension hook returned invalid messages: {err}");
                None
            }
        }
    }

    async fn drain_steering_messages(&mut self) -> Vec<QueuedAgentMessage> {
        if !self.automatic_steering_dispatch {
            return Vec::new();
        }
        self.steering_fetchers
            .retain(RegisteredMessageFetcher::is_live);
        for registered in &self.steering_fetchers {
            let fetched = self.fetch_messages(Some(&registered.fetcher)).await;
            for message in fetched {
                // Fetchers own their admission and queue bounds. Preserve the
                // complete accepted batch at the synchronous handoff instead
                // of applying the smaller direct-input queue limit a second
                // time (RPC legally admits up to MAX_RPC_PENDING_MESSAGES).
                self.message_queue.push_steering_lossless(message);
            }
        }
        self.message_queue.pop_steering()
    }

    async fn stage_follow_up_messages(&mut self) -> bool {
        if !self.automatic_follow_up_dispatch {
            return false;
        }
        let mut owning_surface_ready = self.message_queue.follow_up_batch_len() > 0;
        if !owning_surface_ready {
            let owning_surface = self
                .fetch_messages(self.initial_follow_up_fetcher.as_ref())
                .await;
            owning_surface_ready = !owning_surface.is_empty();
            for message in owning_surface {
                // The owning surface already applied its queue mode and
                // admission bound. Preserve its complete accepted batch until
                // the owner-validated delivery point immediately before
                // TurnStart.
                self.message_queue.push_follow_up_lossless(message);
            }
        }

        if owning_surface_ready {
            // Completion notices are already bounded per session by the jobs
            // registry. Poll them even while the owning source stays busy so a
            // continuously replenished RPC queue cannot starve the notice
            // registry until its oldest completions are evicted.
            self.fetch_job_follow_up_messages().await;
            return self.message_queue.follow_up_batch_len() > 0;
        }

        self.fetch_additive_follow_up_messages().await;
        self.message_queue.follow_up_batch_len() > 0
    }

    async fn fetch_additive_follow_up_messages(&mut self) {
        self.follow_up_fetchers
            .retain(RegisteredMessageFetcher::is_live);
        for registered in &self.follow_up_fetchers {
            let fetched = self.fetch_messages(Some(&registered.fetcher)).await;
            for message in fetched {
                self.message_queue.push_follow_up(message);
            }
        }
        self.fetch_job_follow_up_messages().await;
    }

    async fn fetch_job_follow_up_messages(&mut self) {
        let Ok(owner_session_id) = self.job_session_scope.session_id().await else {
            self.restore_job_follow_ups_except(None);
            return;
        };
        self.restore_job_follow_ups_except(Some(&owner_session_id));
        // One registry drain is already bounded per session. Do not drain a
        // second batch until every staged job notice from the first batch has
        // reached the model; otherwise OneAtATime mode grows by roughly one
        // registry batch per turn boundary.
        if self.message_queue.has_job_follow_up() {
            return;
        }
        for notice in crate::jobs::take_completion_notices(&owner_session_id) {
            self.message_queue.push_job_follow_up_lossless(
                owner_session_id.clone(),
                QueuedAgentMessage::generated(notice),
            );
        }
    }

    async fn pop_follow_up_for_current_session(&mut self) -> Vec<QueuedAgentMessage> {
        let owner_session_id = self.job_session_scope.session_id().await.ok();
        self.restore_job_follow_ups_except(owner_session_id.as_deref());
        self.message_queue.pop_follow_up()
    }

    fn restore_job_follow_ups_except(&mut self, owner_session_id: Option<&str>) {
        let stale = self
            .message_queue
            .take_job_follow_ups_except(owner_session_id);
        if stale.is_empty() {
            return;
        }
        crate::jobs::restore_completion_notices(
            stale
                .into_iter()
                .map(|(owner, delivery)| (owner, delivery.into_message()))
                .collect(),
        );
    }

    async fn fetch_initial_follow_up_messages(&mut self) {
        let fetched = self
            .fetch_messages(self.initial_follow_up_fetcher.as_ref())
            .await;
        for message in fetched {
            // The owning surface has already admitted and bounded this batch.
            // Preserve it across the handoff even when that source permits a
            // larger batch than the ordinary internal queue.
            self.message_queue.push_follow_up_lossless(message);
        }
    }

    /// Stream an assistant response and emit message events.
    #[allow(clippy::too_many_lines)]
    async fn stream_assistant_response(
        &mut self,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
        checkpoint_cx: &crate::agent_cx::AgentCx,
    ) -> Result<AssistantMessage> {
        // Refresh admitted MCP catalogs before capturing the provider's tool
        // schema, under the same owner and cancellation as this request.
        crate::mcp::refresh_agent_tools(&self.tools, abort.as_ref(), checkpoint_cx).await;
        // Build context and stream completion
        let provider = Arc::clone(&self.provider);
        let mut stream_options = self.config.stream_options.clone();
        if let Some(extensions) = &self.extensions {
            stream_options.before_provider_request =
                Some(Self::build_before_provider_request_hook(extensions.clone()));
        }
        let (system_prompt, tools, base_messages) = {
            let context = self.build_context();
            (
                context.system_prompt.as_deref().map(str::to_string),
                context.tools.to_vec(),
                context.messages.to_vec(),
            )
        };
        let mut messages = self
            .dispatch_context_event(&base_messages)
            .await
            .unwrap_or(base_messages);
        // Context hooks can return new native image blocks. Enforce the active
        // model's policy on the final request as well as the history projection;
        // the durable messages keep their bytes for later vision-model turns.
        apply_image_input_policy(
            &mut messages,
            self.config.block_images,
            self.config.model_accepts_images,
        );
        let context = Context::owned(system_prompt, messages, tools);
        // Secrets vault (bd-cv653.7.9): credential shapes in the outbound
        // context become stable placeholders (obfuscate) or a named refusal
        // (block) before any provider sees them. The vault is in-memory and
        // dies with the session.
        let context = self.apply_secrets_outbound(context)?;

        if checkpoint_cx.checkpoint().is_err()
            || abort.as_ref().is_some_and(AbortSignal::is_aborted)
        {
            let abort_arc = Arc::new(self.build_abort_message(None));
            on_event(AgentEvent::MessageStart {
                message: Message::Assistant(Arc::clone(&abort_arc)),
            });
            self.messages
                .push(Message::Assistant(Arc::clone(&abort_arc)));
            on_event(AgentEvent::MessageUpdate {
                message: Message::Assistant(Arc::clone(&abort_arc)),
                assistant_message_event: AssistantMessageEvent::Error {
                    reason: StopReason::Aborted,
                    error: Arc::clone(&abort_arc),
                },
            });
            return Ok(self.finalize_assistant_message(
                Arc::try_unwrap(abort_arc).unwrap_or_else(|a| (*a).clone()),
                &on_event,
                true,
            ));
        }

        let stream_fut = provider.stream(&context, &stream_options);
        let mut stream = if let Some(signal) = abort.as_ref() {
            let abort_fut = signal.wait().fuse();
            let stream_fut = stream_fut.fuse();
            futures::pin_mut!(abort_fut, stream_fut);
            match futures::future::select(abort_fut, stream_fut).await {
                futures::future::Either::Left(((), _)) => {
                    let abort_arc = Arc::new(self.build_abort_message(None));
                    on_event(AgentEvent::MessageStart {
                        message: Message::Assistant(Arc::clone(&abort_arc)),
                    });
                    self.messages
                        .push(Message::Assistant(Arc::clone(&abort_arc)));
                    on_event(AgentEvent::MessageUpdate {
                        message: Message::Assistant(Arc::clone(&abort_arc)),
                        assistant_message_event: AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: Arc::clone(&abort_arc),
                        },
                    });
                    return Ok(self.finalize_assistant_message(
                        Arc::try_unwrap(abort_arc).unwrap_or_else(|a| (*a).clone()),
                        &on_event,
                        true,
                    ));
                }
                futures::future::Either::Right((res, _)) => res?,
            }
        } else {
            stream_fut.await?
        };

        let mut added_partial = false;
        // Track whether we've already emitted `MessageStart` for this streaming response.
        // Avoids cloning the full message on every event just to re-emit a redundant start.
        let mut sent_start = false;
        // #126: raw accumulated tool-call argument fragments, keyed by content
        // index, for THIS streaming response. Providers keep their own partial's
        // `arguments` growing (#124), but the partial that RPC/ACP clients
        // actually receive is rebuilt here from `StreamEvent`s — so the
        // accumulation and best-effort JSON completion must happen here too,
        // uniformly for every provider.
        let mut tool_call_raw_args: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        // #148: count distinct streamed calls, including any unfinished call
        // later omitted from the terminal snapshot. Repeated updates at one
        // content index still describe one call.
        let mut streamed_tool_call_indices = std::collections::HashSet::new();

        'stream: loop {
            if checkpoint_cx.checkpoint().is_err()
                || abort.as_ref().is_some_and(AbortSignal::is_aborted)
            {
                let last_partial = if added_partial {
                    match self
                        .messages
                        .iter()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        Some(Message::Assistant(a)) => Some(a.as_ref()),
                        _ => None,
                    }
                } else {
                    None
                };
                let abort_arc = Arc::new(self.build_abort_message(last_partial));
                if !sent_start {
                    on_event(AgentEvent::MessageStart {
                        message: Message::Assistant(Arc::clone(&abort_arc)),
                    });
                    self.messages
                        .push(Message::Assistant(Arc::clone(&abort_arc)));
                    added_partial = true;
                }
                on_event(AgentEvent::MessageUpdate {
                    message: Message::Assistant(Arc::clone(&abort_arc)),
                    assistant_message_event: AssistantMessageEvent::Error {
                        reason: StopReason::Aborted,
                        error: Arc::clone(&abort_arc),
                    },
                });
                return Ok(self.finalize_assistant_message(
                    Arc::try_unwrap(abort_arc).unwrap_or_else(|a| (*a).clone()),
                    &on_event,
                    added_partial,
                ));
            }

            let event_result = if let Some(signal) = abort.as_ref() {
                let abort_fut = signal.wait().fuse();
                let event_fut = stream.next().fuse();
                futures::pin_mut!(abort_fut, event_fut);

                match futures::future::select(abort_fut, event_fut).await {
                    futures::future::Either::Left(((), _event_fut)) => {
                        let last_partial = if added_partial {
                            match self
                                .messages
                                .iter()
                                .rev()
                                .find(|m| matches!(m, Message::Assistant(_)))
                            {
                                Some(Message::Assistant(a)) => Some(a.as_ref()),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        let abort_arc = Arc::new(self.build_abort_message(last_partial));
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&abort_arc)),
                            });
                            self.messages
                                .push(Message::Assistant(Arc::clone(&abort_arc)));
                            added_partial = true;
                            // We do NOT set sent_start = true here because we are returning immediately,
                            // but setting added_partial = true prevents finalize_assistant_message from
                            // emitting a second MessageStart.
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&abort_arc)),
                            assistant_message_event: AssistantMessageEvent::Error {
                                reason: StopReason::Aborted,
                                error: Arc::clone(&abort_arc),
                            },
                        });
                        return Ok(self.finalize_assistant_message(
                            Arc::try_unwrap(abort_arc).unwrap_or_else(|a| (*a).clone()),
                            &on_event,
                            added_partial,
                        ));
                    }
                    futures::future::Either::Right((event, _abort_fut)) => event,
                }
            } else {
                let event_fut = stream.next().fuse();
                futures::pin_mut!(event_fut);
                loop {
                    let now = checkpoint_cx
                        .cx()
                        .timer_driver()
                        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
                    let tick_fut =
                        asupersync::time::sleep(now, std::time::Duration::from_millis(25)).fuse();
                    futures::pin_mut!(tick_fut);

                    match futures::future::select(tick_fut, &mut event_fut).await {
                        futures::future::Either::Left(((), _event_fut)) => {
                            if checkpoint_cx.checkpoint().is_err() {
                                continue 'stream;
                            }
                        }
                        futures::future::Either::Right((result, _tick_fut)) => break result,
                    }
                }
            };

            let Some(event_result) = event_result else {
                break;
            };
            let event = match event_result {
                Ok(e) => e,
                Err(err) => {
                    let partial = if added_partial {
                        match self
                            .messages
                            .iter()
                            .rev()
                            .find(|m| matches!(m, Message::Assistant(_)))
                        {
                            Some(Message::Assistant(a)) => Some(a.as_ref()),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    let msg = self.build_error_message(partial, err.to_string());

                    // If we never sent a Start event, finalize_assistant_message handles it.
                    // But if sent_start is true and added_partial is somehow false,
                    // finalize_assistant_message will emit a second Start. That shouldn't happen.
                    return Ok(self.finalize_assistant_message(msg, &on_event, added_partial));
                }
            };

            match event {
                StreamEvent::Start { partial } => {
                    if added_partial {
                        if let Some(Message::Assistant(msg_arc)) = self
                            .messages
                            .iter_mut()
                            .rev()
                            .find(|m| matches!(m, Message::Assistant(_)))
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.is_empty() {
                                *msg = partial;
                            } else {
                                msg.api = partial.api;
                                msg.provider = partial.provider;
                                msg.model = partial.model;
                                msg.usage = partial.usage;
                                msg.stop_reason = partial.stop_reason;
                                msg.error_message = partial.error_message;
                                msg.timestamp = partial.timestamp;
                            }
                            let shared = Arc::clone(msg_arc);
                            if !sent_start {
                                on_event(AgentEvent::MessageStart {
                                    message: Message::Assistant(Arc::clone(&shared)),
                                });
                                sent_start = true;
                            }
                            on_event(AgentEvent::MessageUpdate {
                                message: Message::Assistant(Arc::clone(&shared)),
                                assistant_message_event: AssistantMessageEvent::Start {
                                    partial: shared,
                                },
                            });
                        } else {
                            let shared = Arc::new(partial);
                            self.update_partial_message(Arc::clone(&shared), &mut added_partial);
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                            on_event(AgentEvent::MessageUpdate {
                                message: Message::Assistant(Arc::clone(&shared)),
                                assistant_message_event: AssistantMessageEvent::Start {
                                    partial: shared,
                                },
                            });
                        }
                    } else {
                        let shared = Arc::new(partial);
                        self.update_partial_message(Arc::clone(&shared), &mut added_partial);
                        on_event(AgentEvent::MessageStart {
                            message: Message::Assistant(Arc::clone(&shared)),
                        });
                        sent_start = true;
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::Start {
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::TextStart { content_index, .. } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        let msg = Arc::make_mut(msg_arc);
                        if content_index == msg.content.len() {
                            msg.content.push(ContentBlock::Text(TextContent::new("")));
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::TextStart {
                                content_index,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::TextDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::Text(TextContent::new("")));
                            }
                            if let Some(ContentBlock::Text(text)) =
                                msg.content.get_mut(content_index)
                            {
                                text.text.push_str(&delta);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::TextDelta {
                                content_index,
                                delta,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::TextEnd {
                    content_index,
                    content,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::Text(TextContent::new("")));
                            }
                            if let Some(ContentBlock::Text(text)) =
                                msg.content.get_mut(content_index)
                            {
                                text.text.clone_from(&content);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::TextEnd {
                                content_index,
                                content,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ThinkingStart { content_index, .. } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        let msg = Arc::make_mut(msg_arc);
                        if content_index == msg.content.len() {
                            msg.content.push(ContentBlock::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: None,
                            }));
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ThinkingStart {
                                content_index,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ThinkingDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::Thinking(ThinkingContent {
                                    thinking: String::new(),
                                    thinking_signature: None,
                                }));
                            }
                            if let Some(ContentBlock::Thinking(thinking)) =
                                msg.content.get_mut(content_index)
                            {
                                thinking.thinking.push_str(&delta);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                                content_index,
                                delta,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ThinkingEnd {
                    content_index,
                    content,
                    ..
                } => {
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::Thinking(ThinkingContent {
                                    thinking: String::new(),
                                    thinking_signature: None,
                                }));
                            }
                            if let Some(ContentBlock::Thinking(thinking)) =
                                msg.content.get_mut(content_index)
                            {
                                thinking.thinking.clone_from(&content);
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ThinkingEnd {
                                content_index,
                                content,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ToolCallStart {
                    content_index,
                    id,
                    name,
                } => {
                    streamed_tool_call_indices.insert(content_index);
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        let msg = Arc::make_mut(msg_arc);
                        // #129: seed `id`/`name` from the start event so every
                        // emitted partial carries the correlation key from the
                        // first `toolcall_delta`, not only at `toolcall_end`.
                        if content_index == msg.content.len() {
                            msg.content.push(ContentBlock::ToolCall(ToolCall {
                                id,
                                name,
                                arguments: serde_json::Value::Null,
                                thought_signature: None,
                            }));
                        } else if let Some(ContentBlock::ToolCall(tc)) =
                            msg.content.get_mut(content_index)
                        {
                            if tc.id.is_empty() {
                                tc.id = id;
                            }
                            if tc.name.is_empty() {
                                tc.name = name;
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ToolCallStart {
                                content_index,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ToolCallDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    streamed_tool_call_indices.insert(content_index);
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::ToolCall(ToolCall {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: serde_json::Value::Null,
                                    thought_signature: None,
                                }));
                            }
                            // #126: grow this partial's `arguments` as deltas
                            // arrive so snapshot-based clients (RPC/ACP IDE
                            // frontends) render a large tool call streaming in,
                            // like text, instead of pause-then-pop-in. The #124
                            // provider-side update mutates the provider's OWN
                            // partial, which is not the one emitted to clients —
                            // this one is, so the accumulated prefix must be
                            // completed here. On an un-completable fragment,
                            // `complete_partial_json` returns `None` and we keep
                            // the last good value (never wrong data). The
                            // terminal `ToolCallEnd` still sets the fully-parsed
                            // arguments.
                            let raw = tool_call_raw_args.entry(content_index).or_default();
                            raw.push_str(&delta);
                            if let Some(partial_args) =
                                crate::providers::openai::complete_partial_json(raw)
                                && let Some(ContentBlock::ToolCall(tc)) =
                                    msg.content.get_mut(content_index)
                            {
                                tc.arguments = partial_args;
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ToolCallDelta {
                                content_index,
                                delta,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    ..
                } => {
                    streamed_tool_call_indices.insert(content_index);
                    self.seed_partial_message_if_missing(&mut added_partial);
                    if let Some(Message::Assistant(msg_arc)) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::Assistant(_)))
                    {
                        {
                            let msg = Arc::make_mut(msg_arc);
                            if msg.content.get(content_index).is_none()
                                && content_index == msg.content.len()
                            {
                                msg.content.push(ContentBlock::ToolCall(ToolCall {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: serde_json::Value::Null,
                                    thought_signature: None,
                                }));
                            }
                            if let Some(ContentBlock::ToolCall(tc)) =
                                msg.content.get_mut(content_index)
                            {
                                *tc = tool_call.clone();
                            }
                        }
                        let shared = Arc::clone(msg_arc);
                        if !sent_start {
                            on_event(AgentEvent::MessageStart {
                                message: Message::Assistant(Arc::clone(&shared)),
                            });
                            sent_start = true;
                        }
                        on_event(AgentEvent::MessageUpdate {
                            message: Message::Assistant(Arc::clone(&shared)),
                            assistant_message_event: AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call,
                                partial: shared,
                            },
                        });
                    }
                }
                StreamEvent::Done {
                    reason,
                    mut message,
                } => {
                    // Failures, refusals, and server-tool pauses must win over
                    // a stale success reason in the final message snapshot.
                    let terminal_override = match reason {
                        StopReason::Error | StopReason::Aborted | StopReason::Refusal => {
                            !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
                        }
                        StopReason::PauseTurn => matches!(
                            message.stop_reason,
                            StopReason::Stop | StopReason::ToolUse | StopReason::Length
                        ),
                        StopReason::Length => {
                            matches!(message.stop_reason, StopReason::Stop | StopReason::ToolUse)
                        }
                        _ => false,
                    };
                    if terminal_override {
                        message.stop_reason = reason;
                    }
                    // #148: a token-limited snapshot may omit an unfinished
                    // call even while completed peers remain. Report the
                    // truncation before any peer executes, through the existing
                    // Error/AgentEnd path.
                    if is_truncated_before_tool_call(&message, streamed_tool_call_indices.len()) {
                        message.stop_reason = StopReason::Error;
                        message.error_message = Some(TRUNCATED_TOOL_CALL_ERROR.to_string());
                    }
                    return Ok(self.finalize_assistant_message(message, &on_event, added_partial));
                }
                StreamEvent::Error { reason, mut error } => {
                    // Custom providers can leave the partial snapshot stamped
                    // Stop/ToolUse. An Error envelope never authorizes tools.
                    if !matches!(error.stop_reason, StopReason::Error | StopReason::Aborted) {
                        error.stop_reason = if reason == StopReason::Aborted {
                            StopReason::Aborted
                        } else {
                            StopReason::Error
                        };
                    }
                    return Ok(self.finalize_assistant_message(error, &on_event, added_partial));
                }
            }
        }

        // If the stream ends without a Done/Error event, we may have a partial message.
        // Instead of discarding it, we finalize it with an error state so the user/session
        // retains the partial content.
        if added_partial
            && let Some(Message::Assistant(last_msg)) = self
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::Assistant(_)))
        {
            let mut final_msg = (**last_msg).clone();
            // #148: same truncation check as the `Done` arm. A provider can
            // stamp `Length` on a partial (via `Start`/deltas) and then drop
            // the connection without a terminal event; the resulting error
            // should name the truncated tool call rather than the generic
            // missing-`Done` condition.
            let truncated_tool_call =
                is_truncated_before_tool_call(&final_msg, streamed_tool_call_indices.len());
            final_msg.stop_reason = StopReason::Error;
            final_msg.error_message = Some(if truncated_tool_call {
                TRUNCATED_TOOL_CALL_ERROR.to_string()
            } else {
                "Stream ended without Done event".to_string()
            });
            return Ok(self.finalize_assistant_message(final_msg, &on_event, true));
        }
        Err(Error::api("Stream ended without Done event"))
    }

    /// Ensure we have a fresh assistant message for the current stream.
    ///
    /// Some providers/extensions can emit deltas without a Start event; without
    /// this guard we would mutate the previous assistant message instead.
    fn seed_partial_message_if_missing(&mut self, added_partial: &mut bool) {
        if *added_partial {
            return;
        }

        let message = AssistantMessage {
            content: Vec::new(),
            api: self.provider.api().to_string(),
            provider: self.provider.name().to_string(),
            model: self.provider.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: Utc::now().timestamp_millis(),
        };
        self.messages.push(Message::Assistant(Arc::new(message)));
        *added_partial = true;
    }

    /// Update the partial assistant message in `self.messages`.
    ///
    /// Takes an `Arc<AssistantMessage>` and moves it into the message list
    /// (one Arc move, zero deep-copies).
    fn update_partial_message(
        &mut self,
        partial: Arc<AssistantMessage>,
        added_partial: &mut bool,
    ) -> bool {
        if *added_partial {
            if let Some(target) = self
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m, Message::Assistant(_)))
            {
                *target = Message::Assistant(partial);
            } else {
                // Defensive: added_partial is true but no Assistant message found.
                // Push as new message rather than silently dropping the update.
                tracing::warn!("update_partial_message: expected an Assistant message in history");
                self.messages.push(Message::Assistant(partial));
            }
            false
        } else {
            self.messages.push(Message::Assistant(partial));
            *added_partial = true;
            true
        }
    }

    fn finalize_assistant_message(
        &mut self,
        mut message: AssistantMessage,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        added_partial: bool,
    ) -> AssistantMessage {
        // Repair before the execution check and terminal publication so the
        // persisted message, MessageEnd, and local executor see one batch.
        message = self.maybe_repair_dialect_tool_calls(message);
        if let Some(error) = terminal_tool_batch_error(&message) {
            message.stop_reason = StopReason::Error;
            // Preserve the provider's original diagnostic if it supplied one.
            message.error_message.get_or_insert(error);
        }
        // gh #221: price the finished turn from the catalog rates. Transports
        // only fill token counts (plus OpenRouter's billed total); without
        // this every `usage.cost` stayed 0.0 on every provider.
        if let Some(rates) = self.provider.model_cost() {
            rates.price_usage(&mut message.usage);
        }
        let arc = Arc::new(message);
        if added_partial {
            if let Some(target) = self
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m, Message::Assistant(_)))
            {
                *target = Message::Assistant(Arc::clone(&arc));
            } else {
                // Defensive: added_partial is true but no Assistant message found.
                // Push as new message rather than overwriting an unrelated message.
                tracing::warn!(
                    "finalize_assistant_message: expected an Assistant message in history"
                );
                self.messages.push(Message::Assistant(Arc::clone(&arc)));
                on_event(AgentEvent::MessageStart {
                    message: Message::Assistant(Arc::clone(&arc)),
                });
            }
        } else {
            self.messages.push(Message::Assistant(Arc::clone(&arc)));
            on_event(AgentEvent::MessageStart {
                message: Message::Assistant(Arc::clone(&arc)),
            });
        }

        on_event(AgentEvent::MessageEnd {
            message: Message::Assistant(Arc::clone(&arc)),
        });
        Arc::try_unwrap(arc).unwrap_or_else(|a| (*a).clone())
    }

    async fn execute_tool_batch(
        &self,
        batch: Vec<(usize, ToolCall)>,
        on_event: AgentEventHandler,
        abort: Option<AbortSignal>,
        latency: SharedTurnLatencyAccumulator,
    ) -> Vec<(usize, (ToolOutput, bool))> {
        let parallelism = compatible_tool_parallelism_limit();
        let futures = batch.into_iter().map(|(idx, tc)| {
            let on_event = Arc::clone(&on_event);
            let latency = Arc::clone(&latency);
            async move { (idx, self.execute_tool_owned(tc, on_event, latency).await) }
        });

        if let Some(signal) = abort.as_ref() {
            use futures::future::{Either, select};
            let pending = stream::iter(futures).buffer_unordered(parallelism);
            let abort_fut = signal.wait().fuse();
            futures::pin_mut!(pending, abort_fut);
            // Completed work belongs to the transcript even when a sibling
            // remains blocked. Racing cancellation against collect() drops
            // its already-collected outputs and later misreports those calls
            // as aborted. Keep each observed completion outside the future
            // that cancellation drops; only unfinished calls receive the
            // synthetic cancellation results in execute_tool_calls.
            let mut completed = Vec::new();
            while !signal.is_aborted() {
                let next = pending.next().fuse();
                futures::pin_mut!(next);
                match select(next, abort_fut.as_mut()).await {
                    Either::Left((Some(result), _)) => completed.push(result),
                    Either::Left((None, _)) | Either::Right(_) => break,
                }
            }
            completed
        } else {
            stream::iter(futures)
                .buffer_unordered(parallelism)
                .collect::<Vec<_>>()
                .await
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        on_event: AgentEventHandler,
        new_messages: &mut Vec<Message>,
        abort: Option<AbortSignal>,
        latency: SharedTurnLatencyAccumulator,
    ) -> Result<ToolExecutionOutcome> {
        let mut results = Vec::new();
        let mut steering_messages: Option<Vec<QueuedAgentMessage>> = None;

        // Phase 1: Emit start events for ALL tools up front.
        for tool_call in tool_calls {
            // Crash-bundle context (bd-cv653.7.12): the ring is redacted at
            // capture, so tool names + argument shape are safe and give a
            // bundle its "what was happening" trail.
            crate::crash::record_operation(format!(
                "tool {} ({} arg bytes)",
                tool_call.name,
                tool_call.arguments.to_string().len()
            ));
            on_event(AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            });
        }

        // Phase 2: Execute tools in contiguous compatible-effect batches.
        let effect_plan = tool_calls
            .iter()
            .map(|tool_call| {
                self.tools
                    .snapshot()
                    .get(&tool_call.name)
                    .map_or_else(ToolEffects::write, Tool::effects)
            })
            .collect::<Vec<_>>();
        let effect_batches = plan_tool_effect_batches(&effect_plan);
        let mut recorded_results: Vec<Option<Arc<ToolResultMessage>>> =
            vec![None; tool_calls.len()];

        for effect_batch in effect_batches {
            if abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                break;
            }

            let steering = self.drain_steering_messages().await;
            if !steering.is_empty() {
                steering_messages = Some(steering);
                break;
            }

            let batch_len = effect_batch.end.saturating_sub(effect_batch.start);
            let batch = tool_calls
                .iter()
                .cloned()
                .enumerate()
                .skip(effect_batch.start)
                .take(batch_len)
                .collect();
            let mut batch_results = self
                .execute_tool_batch(
                    batch,
                    Arc::clone(&on_event),
                    abort.clone(),
                    Arc::clone(&latency),
                )
                .await;
            batch_results.sort_by_key(|(idx, _)| *idx);
            for (idx, (output, is_error)) in batch_results {
                if let (Some(tool_call), Some(recorded_result)) =
                    (tool_calls.get(idx), recorded_results.get_mut(idx))
                {
                    *recorded_result = Some(self.record_tool_result(
                        tool_call,
                        output,
                        is_error,
                        &on_event,
                        new_messages,
                    ));
                }
            }
        }

        // Imported scoped rules (bd-cv653.6.2): activation is decided from
        // the tool calls' path inputs AFTER the batches ran (queuing earlier
        // would trip the steering check above and skip the tools). Newly
        // activated rules ride the steering queue, so the model sees them
        // alongside these tool results, before its next request.
        self.activate_scoped_rules_for_tool_calls(tool_calls);

        // Phase 3: Process results sequentially and handle skips.
        for (index, tool_call) in tool_calls.iter().enumerate() {
            // Check for new steering if we haven't already found some.
            // This catches steering messages that arrived during the *last* tool's execution.
            if steering_messages.is_none() && !abort.as_ref().is_some_and(AbortSignal::is_aborted) {
                let steering = self.drain_steering_messages().await;
                if !steering.is_empty() {
                    steering_messages = Some(steering);
                }
            }

            // If a result was recorded during execution, keep outcome ordering
            // without re-emitting lifecycle events or duplicating transcript entries.
            if let Some(tool_result) = recorded_results.get_mut(index).and_then(Option::take) {
                results.push(tool_result);
            } else if steering_messages.is_some() {
                // Skipped due to steering.
                results.push(self.skip_tool_call(tool_call, &on_event, new_messages));
            } else {
                // Aborted or otherwise failed to run (e.g. abort signal).
                let output = ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(
                        "Tool execution aborted",
                    ))],
                    details: Some(Self::tool_cancellation_details(
                        &tool_call.name,
                        "abort_signal",
                    )),
                    is_error: true,
                };

                on_event(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    args: tool_call.arguments.clone(),
                    partial_result: ToolOutput {
                        content: output.content.clone(),
                        details: output.details.clone(),
                        is_error: true,
                    },
                });

                on_event(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    result: ToolOutput {
                        content: output.content.clone(),
                        details: output.details.clone(),
                        is_error: true,
                    },
                    is_error: true,
                });

                let tool_result = Arc::new(ToolResultMessage {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    content: output.content,
                    details: output.details,
                    is_error: true,
                    timestamp: Utc::now().timestamp_millis(),
                });

                let msg = Message::ToolResult(Arc::clone(&tool_result));
                self.messages.push(msg.clone());
                on_event(AgentEvent::MessageStart {
                    message: msg.clone(),
                });
                let end_msg = msg.clone();
                new_messages.push(msg);
                on_event(AgentEvent::MessageEnd { message: end_msg });

                results.push(tool_result);
            }
        }

        Ok(ToolExecutionOutcome {
            tool_results: results,
            steering_messages,
        })
    }

    fn record_tool_result(
        &mut self,
        tool_call: &ToolCall,
        output: ToolOutput,
        is_error: bool,
        on_event: &AgentEventHandler,
        new_messages: &mut Vec<Message>,
    ) -> Arc<ToolResultMessage> {
        on_event(AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
            partial_result: ToolOutput {
                content: output.content.clone(),
                details: output.details.clone(),
                is_error,
            },
        });

        let tool_result = Arc::new(ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: output.content,
            details: output.details,
            is_error,
            timestamp: Utc::now().timestamp_millis(),
        });

        on_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_result.tool_call_id.clone(),
            tool_name: tool_result.tool_name.clone(),
            result: ToolOutput {
                content: tool_result.content.clone(),
                details: tool_result.details.clone(),
                is_error,
            },
            is_error,
        });

        let msg = Message::ToolResult(Arc::clone(&tool_result));
        self.messages.push(msg.clone());
        on_event(AgentEvent::MessageStart {
            message: msg.clone(),
        });
        new_messages.push(msg.clone());
        on_event(AgentEvent::MessageEnd { message: msg });

        tool_result
    }

    async fn execute_tool(
        &self,
        tool_call: ToolCall,
        on_event: AgentEventHandler,
        latency: SharedTurnLatencyAccumulator,
    ) -> (ToolOutput, bool) {
        let extensions = self.extensions.clone();

        // Inbound secrets restore (bd-cv653.7.9): placeholders in tool-call
        // arguments become the real values before approval/execution (the
        // operator approves the REAL command), and restored values are
        // masked again in the result heading back to the model.
        let tool_call = match self.restore_secrets_inbound(tool_call) {
            Ok(tool_call) => tool_call,
            Err(error) => {
                return (Self::xdev_text_output(&error.to_string(), true), true);
            }
        };

        // Plan-mode gate (bd-cv653.3.5): Planning/PendingApproval reject any
        // tool whose effects intersect the mutation/process BARRIER set before
        // soliciting approval or running extensions.
        if !self
            .plan_state
            .allows_effects(self.effects_for_call(&tool_call))
        {
            return (
                Self::xdev_text_output(
                    &crate::plan::PlanState::block_message(&tool_call.name),
                    true,
                ),
                true,
            );
        }

        let approval_denied_output = self
            .request_tool_approval(&tool_call, Arc::clone(&on_event))
            .await;

        let (mut output, is_error) = if let Some(output) = approval_denied_output {
            (output, true)
        } else if let Some(extensions) = &extensions {
            let hook_started_at = Instant::now();
            let hook_outcome = Self::dispatch_tool_call_hook(
                extensions,
                &tool_call,
                self.config.fail_closed_hooks,
            )
            .await;
            record_extension_hostcall_latency(&latency, hook_started_at.elapsed());

            if let Some(blocked_output) = hook_outcome {
                (blocked_output, true)
            } else {
                let tool_started_at = Instant::now();
                let outcome = self
                    .execute_tool_without_hooks(&tool_call, Arc::clone(&on_event))
                    .await;
                record_local_tool_latency(&latency, tool_started_at.elapsed());
                outcome
            }
        } else {
            let tool_started_at = Instant::now();
            let outcome = self
                .execute_tool_without_hooks(&tool_call, Arc::clone(&on_event))
                .await;
            record_local_tool_latency(&latency, tool_started_at.elapsed());
            outcome
        };

        if let Some(extensions) = &extensions {
            let hook_started_at = Instant::now();
            Self::apply_tool_result_hook(extensions, &tool_call, &mut output, is_error).await;
            record_extension_hostcall_latency(&latency, hook_started_at.elapsed());
        }

        // Mask any restored real values in the result heading back to the
        // model (echo hygiene: a bash echo of the value shows the
        // placeholder).
        self.mask_secrets_in_output(&mut output);

        (output, is_error)
    }

    /// Inbound restore (bd-cv653.7.9): placeholders in tool-call arguments
    /// become real values before approval/execution.
    ///
    /// # Errors
    /// Refuses excessive nesting or distinct object keys that would restore
    /// to the same key. Ambiguous arguments must never reach approval or a tool.
    pub fn restore_secrets_inbound(&self, mut tool_call: ToolCall) -> Result<ToolCall> {
        if crate::secrets::SecretsMode::from_setting(
            self.config.secrets.as_ref().and_then(|s| s.mode.as_deref()),
        ) == crate::secrets::SecretsMode::Off
        {
            return Ok(tool_call);
        }
        tool_call.arguments =
            crate::secrets::restore_inbound_json(&tool_call.arguments, &self.secrets_vault)?;
        Ok(tool_call)
    }

    /// Echo hygiene (bd-cv653.7.9): restored real values in tool output are
    /// masked back to placeholders before the model sees the result.
    pub fn mask_secrets_in_output(&self, output: &mut ToolOutput) {
        if crate::secrets::SecretsMode::from_setting(
            self.config.secrets.as_ref().and_then(|s| s.mode.as_deref()),
        ) == crate::secrets::SecretsMode::Off
        {
            return;
        }
        for block in &mut output.content {
            if let ContentBlock::Text(t) = block {
                t.text = self.secrets_vault.mask(&t.text);
            }
        }
        if let Some(details) = &mut output.details {
            *details = crate::secrets::mask_known_json(details, &self.secrets_vault)
                .unwrap_or_else(|error| {
                    // A diagnostic cannot retain raw output as a fallback.
                    // Keep the tool's execution outcome, but replace unsafe
                    // details with an explicit, value-free omission record.
                    json!({
                        "schema": crate::secrets::SECRETS_SCHEMA,
                        "detailsOmitted": true,
                        "reason": error.to_string(),
                    })
                });
        }
    }

    /// Outbound hygiene for auxiliary provider calls (e.g. /btw side
    /// questions): apply the same mode gate + obfuscation the main request
    /// path uses, so text derived from the live message list never carries
    /// raw secrets to a provider. Errors in block mode when a secret shape
    /// is present.
    pub fn secrets_transform_outbound_text(&mut self, text: &str) -> Result<String> {
        let mode = crate::secrets::SecretsMode::from_setting(
            self.config.secrets.as_ref().and_then(|s| s.mode.as_deref()),
        );
        if mode == crate::secrets::SecretsMode::Off {
            return Ok(text.to_string());
        }
        let extra = crate::secrets::compile_extra_patterns(
            self.config
                .secrets
                .as_ref()
                .and_then(|s| s.extra_patterns.as_deref())
                .unwrap_or(&[]),
        );
        let mut total = 0usize;
        let mut labels: Vec<String> = Vec::new();
        let result = Self::secrets_transform_text(
            text,
            &mut self.secrets_vault,
            mode,
            &extra,
            &mut total,
            &mut labels,
        );
        if result.is_ok() {
            self.refresh_compaction_privacy();
        }
        result
    }

    /// Capture the policy and remembered values for one compaction request.
    #[must_use]
    pub fn compaction_privacy(&self) -> compaction::CompactionPrivacy {
        compaction::CompactionPrivacy::with_vault(self.config.secrets.as_ref(), &self.secrets_vault)
    }

    fn refresh_compaction_privacy(&self) {
        if let Ok(mut privacy) = self.compaction_privacy_state.lock() {
            *privacy = self.compaction_privacy();
        }
        // A poisoned snapshot is never used: extension callers propagate the
        // lock error before either provider entry point is admitted.
    }

    /// Screen bounded auxiliary fields against both configured policy and
    /// credentials learned earlier in this conversation. The disposable
    /// projection never mutates the live vault or exports its reversible IDs.
    pub(crate) fn project_auxiliary_inputs(
        &self,
        parts: &[&str],
        privacy: &crate::text_completion::AuxiliaryPrivacy,
    ) -> Result<Vec<String>> {
        crate::text_completion::redact_inputs_with_vault(parts, privacy, &self.secrets_vault)
    }

    /// Export hygiene (bd-cv653.7.9): mask known secret values in arbitrary
    /// text (e.g. transcript exports or shares) back to their placeholders.
    /// A no-op when the secrets vault is disabled or empty.
    #[must_use]
    pub fn mask_secrets_text(&self, text: &str) -> String {
        if crate::secrets::SecretsMode::from_setting(
            self.config.secrets.as_ref().and_then(|s| s.mode.as_deref()),
        ) == crate::secrets::SecretsMode::Off
        {
            return text.to_string();
        }
        self.secrets_vault.mask(text)
    }

    #[allow(clippy::too_many_lines)]
    async fn request_tool_approval(
        &self,
        tool_call: &ToolCall,
        on_event: AgentEventHandler,
    ) -> Option<ToolOutput> {
        // 1. If approval_state is configured, evaluate graduated gating first (bd-cv653.3.19)
        if let Some(approval_state) = &self.config.approval_state {
            let effects = self.effects_for_call(tool_call);
            let evaluation = approval_state.evaluate(
                &tool_call.name,
                &tool_call.arguments,
                effects,
                Some(&self.plan_state),
                self.config.bash_settings.as_ref(),
            );

            match evaluation {
                crate::approval::ApprovalEvaluation::HardBlocked { reason } => {
                    return Some(Self::tool_approval_denied_output(&format!(
                        "Refused by policy gate: {reason}"
                    )));
                }
                crate::approval::ApprovalEvaluation::AutoApproved { mode, reason } => {
                    let audit_details = crate::approval::ApprovalState::audit_payload(
                        &tool_call.id,
                        &tool_call.name,
                        &crate::approval::ApprovalEvaluation::AutoApproved { mode, reason },
                    );
                    on_event(AgentEvent::ToolExecutionUpdate {
                        tool_call_id: tool_call.id.clone(),
                        tool_name: tool_call.name.clone(),
                        args: tool_call.arguments.clone(),
                        partial_result: ToolOutput {
                            content: Vec::new(),
                            details: Some(audit_details),
                            is_error: false,
                        },
                    });
                    return None;
                }
                crate::approval::ApprovalEvaluation::RequiresApproval {
                    mode,
                    reason,
                    is_dual_confirm,
                    danger_classes: _,
                } => {
                    if let Some(approval) = &self.config.tool_approval {
                        let request = ToolApprovalRequest {
                            tool_call_id: tool_call.id.clone(),
                            tool_name: tool_call.name.clone(),
                            arguments: tool_call.arguments.clone(),
                        };
                        match approval(request).await {
                            ToolApprovalDecision::Allow => {
                                if is_dual_confirm {
                                    let cmd = tool_call
                                        .arguments
                                        .get("command")
                                        .or_else(|| tool_call.arguments.get("cmd"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    let token = format!("{}:{cmd}", tool_call.name);
                                    approval_state.record_confirmation(&token);
                                }
                                on_event(AgentEvent::ToolExecutionUpdate {
                                    tool_call_id: tool_call.id.clone(),
                                    tool_name: tool_call.name.clone(),
                                    args: tool_call.arguments.clone(),
                                    partial_result: ToolOutput {
                                        content: Vec::new(),
                                        details: Some(json!({
                                            "schema": TOOL_APPROVAL_STATUS_SCHEMA_V1,
                                            "status": "approved",
                                            "mode": mode.as_str(),
                                        })),
                                        is_error: false,
                                    },
                                });
                                return None;
                            }
                            ToolApprovalDecision::Deny { reason } => {
                                return Some(Self::tool_approval_denied_output(&reason));
                            }
                        }
                    }
                    return Some(Self::tool_approval_denied_output(&format!(
                        "Approval required in {mode} mode: {reason}"
                    )));
                }
            }
        }

        // 2. Legacy / default path when approval_state is not configured
        let Some(approval) = &self.config.tool_approval else {
            return None;
        };

        let request = ToolApprovalRequest {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
        };

        match approval(request).await {
            ToolApprovalDecision::Allow => {
                on_event(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    args: tool_call.arguments.clone(),
                    partial_result: ToolOutput {
                        content: Vec::new(),
                        details: Some(json!({
                            "schema": TOOL_APPROVAL_STATUS_SCHEMA_V1,
                            "status": "approved",
                        })),
                        is_error: false,
                    },
                });
                None
            }
            ToolApprovalDecision::Deny { reason } => {
                Some(Self::tool_approval_denied_output(&reason))
            }
        }
    }

    async fn execute_tool_owned(
        &self,
        tool_call: ToolCall,
        on_event: AgentEventHandler,
        latency: SharedTurnLatencyAccumulator,
    ) -> (ToolOutput, bool) {
        self.execute_tool(tool_call, on_event, latency).await
    }

    /// xdev dispatcher interception (bd-cv653.1.6). `run` executes the inner
    /// tool through the normal `Tool::execute` contract (its own input
    /// validation produces the named errors); `promote` moves the tool into
    /// the live schema and invalidates the def cache. Returns `None` for
    /// `list`/`describe` (the XdevTool handles those itself).
    async fn dispatch_xdev(&self, tool_call: &ToolCall) -> Option<ToolOutput> {
        let args = &tool_call.arguments;
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "run" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return Some(Self::xdev_text_output("xdev run requires a `name`", true));
                }
                let registry = self.tools.snapshot();
                if !registry.is_discoverable(name) {
                    return Some(Self::xdev_text_output(
                        &if registry.get(name).is_some() {
                            format!("{name:?} is already in the live schema — call it directly.")
                        } else {
                            format!("No discoverable tool named {name:?}; use xdev list.")
                        },
                        true,
                    ));
                }
                let inner_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
                let Some(inner) = registry.get(name) else {
                    return Some(Self::xdev_text_output(
                        &format!("Tool {name:?} not registered"),
                        true,
                    ));
                };
                let protect = |parts: &[&str]| {
                    let privacy = crate::text_completion::AuxiliaryPrivacy::from_settings(
                        self.config.secrets.as_ref(),
                    );
                    self.project_auxiliary_inputs(parts, &privacy)
                };
                let mut output = inner
                    .execute_with_auxiliary_privacy(&tool_call.id, inner_args, None, &protect)
                    .await
                    .unwrap_or_else(|err| Self::xdev_text_output(&err.to_string(), true));
                output.details = Some(Self::xdev_dispatch_details(name, output.details.take()));
                Some(output)
            }
            "promote" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return Some(Self::xdev_text_output(
                        "xdev promote requires a `name`",
                        true,
                    ));
                }
                let registry = self.tools.snapshot();
                if !registry.is_discoverable(name) {
                    return Some(Self::xdev_text_output(
                        &if registry.get(name).is_some() {
                            format!("{name:?} is already in the live schema.")
                        } else {
                            format!("No discoverable tool named {name:?}; use xdev list.")
                        },
                        true,
                    ));
                }
                if let Ok(mut set) = self.promoted_tools.lock() {
                    set.insert(name.to_string());
                }
                self.tool_defs_generation
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(Self::xdev_text_output(
                    &format!(
                        "Promoted {name:?} into the live schema for the rest of this session. You can now call it directly."
                    ),
                    false,
                ))
            }
            _ => None,
        }
    }

    fn xdev_text_output(text: &str, is_error: bool) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error,
        }
    }

    fn xdev_dispatch_details(name: &str, original: Option<Value>) -> Value {
        let mut details = match original {
            Some(Value::Object(fields))
                if !fields.contains_key("dispatchedVia") && !fields.contains_key("tool") =>
            {
                fields
            }
            // Preserve non-object details and objects owning reserved routing
            // keys intact, including any pre-existing resultDetails field.
            Some(value) => serde_json::Map::from_iter([("resultDetails".to_string(), value)]),
            None => serde_json::Map::new(),
        };
        details.insert("dispatchedVia".to_string(), json!("xdev"));
        details.insert("tool".to_string(), json!(name));
        Value::Object(details)
    }

    /// Effective side effects for a tool call (bd-cv653.3.5): for `xdev run`
    /// dispatches, the INNER tool's actual arguments decide (read-only runs
    /// stay allowed in plan mode). Unknown names default to read-only here;
    /// the not-found path produces its own error downstream.
    fn effects_for_call(&self, tool_call: &ToolCall) -> crate::tools::ToolEffects {
        if tool_call.name == "xdev" {
            let action = tool_call
                .arguments
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("list");
            if action != "run" {
                return crate::tools::ToolEffects::read();
            }
            let inner = tool_call
                .arguments
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Match dispatch_xdev's default input exactly. Non-object inputs
            // reach the tool unchanged so its conservative validation applies.
            let default_args = json!({});
            let inner_args = tool_call.arguments.get("args").unwrap_or(&default_args);
            return self
                .tools
                .snapshot()
                .get(inner)
                .map_or_else(crate::tools::ToolEffects::read, |tool| {
                    tool.effects_for_input(inner_args)
                });
        }
        self.tools
            .snapshot()
            .get(&tool_call.name)
            .map_or_else(crate::tools::ToolEffects::read, |tool| {
                tool.effects_for_input(&tool_call.arguments)
            })
    }

    async fn execute_tool_without_hooks(
        &self,
        tool_call: &ToolCall,
        on_event: AgentEventHandler,
    ) -> (ToolOutput, bool) {
        // Plan-mode gate (bd-cv653.3.5): Planning/PendingApproval reject any
        // tool whose effects intersect the mutation/process BARRIER set. For
        // xdev run calls the INNER tool's effects decide, not the union.
        if !self
            .plan_state
            .allows_effects(self.effects_for_call(tool_call))
        {
            return (
                Self::xdev_text_output(
                    &crate::plan::PlanState::block_message(&tool_call.name),
                    true,
                ),
                true,
            );
        }

        // Load modes (bd-cv653.1.6): intercept the xdev dispatcher's
        // run/promote actions so the inner tool executes through the normal
        // path (effects/approval already applied to this outer call).
        if tool_call.name == "xdev"
            && let Some(output) = self.dispatch_xdev(tool_call).await
        {
            let is_error = output.is_error;
            return (output, is_error);
        }

        // Find the tool in the current registry snapshot (kept alive for the
        // duration of the call so the handle outlives the await below).
        let registry = self.tools.snapshot();
        let Some(tool) = registry.get(&tool_call.name) else {
            return (Self::tool_not_found_output(&tool_call.name), true);
        };

        let tool_name = tool_call.name.clone();
        let tool_id = tool_call.id.clone();
        let tool_args = tool_call.arguments.clone();
        let on_event = Arc::clone(&on_event);

        let update_callback = move |update: ToolUpdate| {
            on_event(AgentEvent::ToolExecutionUpdate {
                tool_call_id: tool_id.clone(),
                tool_name: tool_name.clone(),
                args: tool_args.clone(),
                partial_result: ToolOutput {
                    content: update.content,
                    details: update.details,
                    is_error: false,
                },
            });
        };

        let _artifact_session_guard =
            self.config
                .stream_options
                .session_id
                .as_deref()
                .map(|session_id| {
                    crate::tools::register_tool_output_artifact_session(&tool_call.id, session_id)
                });

        // Tool-free secondary requests need the discoveries from this exact
        // Agent invocation. A shared tool handle must never retain another
        // session's vault or a snapshot taken before its latest discoveries.
        let protect = |parts: &[&str]| {
            let privacy = crate::text_completion::AuxiliaryPrivacy::from_settings(
                self.config.secrets.as_ref(),
            );
            self.project_auxiliary_inputs(parts, &privacy)
        };
        match tool
            .execute_with_auxiliary_privacy(
                &tool_call.id,
                tool_call.arguments.clone(),
                Some(Box::new(update_callback)),
                &protect,
            )
            .await
        {
            Ok(output) => {
                let is_error = output.is_error;
                (output, is_error)
            }
            Err(e) => (
                ToolOutput {
                    content: vec![ContentBlock::Text(TextContent::new(format!("Error: {e}")))],
                    details: None,
                    is_error: true,
                },
                true,
            ),
        }
    }

    fn tool_not_found_output(tool_name: &str) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Error: Tool '{tool_name}' not found"
            )))],
            details: None,
            is_error: true,
        }
    }

    fn tool_cancellation_details(tool_name: &str, reason: &str) -> Value {
        json!({
            "schema": TOOL_CANCELLATION_SCHEMA_V1,
            "status": "cancelled",
            "reason": reason,
            "toolName": tool_name,
            "cleanup": "tool_result_recorded_no_success",
        })
    }

    async fn dispatch_tool_call_hook(
        extensions: &ExtensionManager,
        tool_call: &ToolCall,
        fail_closed_hooks: bool,
    ) -> Option<ToolOutput> {
        match extensions
            .dispatch_tool_call(tool_call, EXTENSION_EVENT_TIMEOUT_MS)
            .await
        {
            Ok(Some(result)) if result.block => {
                Some(Self::tool_call_blocked_output(result.reason.as_deref()))
            }
            Ok(_) => None,
            Err(err) => {
                if fail_closed_hooks {
                    tracing::warn!(
                        error = ?err,
                        "tool_call extension hook failed (fail-closed)"
                    );
                    Some(Self::tool_call_blocked_output(Some(
                        "extension hook failed",
                    )))
                } else {
                    tracing::warn!("tool_call extension hook failed (fail-open): {err}");
                    None
                }
            }
        }
    }

    fn tool_call_blocked_output(reason: Option<&str>) -> ToolOutput {
        let reason = reason.map(str::trim).filter(|reason| !reason.is_empty());
        let message = reason.map_or_else(
            || "Tool execution was blocked by an extension".to_string(),
            |reason| format!("Tool execution blocked: {reason}"),
        );

        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(message))],
            details: None,
            is_error: true,
        }
    }

    fn tool_approval_denied_output(reason: &str) -> ToolOutput {
        let reason = reason.trim();
        let reason = if reason.is_empty() {
            "tool approval denied"
        } else {
            reason
        };

        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Tool execution denied: {reason}"
            )))],
            details: Some(json!({
                "schema": TOOL_APPROVAL_DENIED_SCHEMA_V1,
                "status": "denied",
                "reason": reason,
            })),
            is_error: true,
        }
    }

    async fn apply_tool_result_hook(
        extensions: &ExtensionManager,
        tool_call: &ToolCall,
        output: &mut ToolOutput,
        is_error: bool,
    ) {
        match extensions
            .dispatch_tool_result(tool_call, &*output, is_error, EXTENSION_EVENT_TIMEOUT_MS)
            .await
        {
            Ok(Some(result)) => {
                if let Some(content) = result.content {
                    output.content = content;
                }
                if let Some(details) = result.details {
                    output.details = Some(details);
                }
            }
            Ok(None) => {}
            Err(err) => tracing::warn!("tool_result extension hook failed (fail-open): {err}"),
        }
    }

    fn skip_tool_call(
        &mut self,
        tool_call: &ToolCall,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        new_messages: &mut Vec<Message>,
    ) -> Arc<ToolResultMessage> {
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                "Skipped due to queued user message.",
            ))],
            details: None,
            is_error: true,
        };

        // Note: Phase 1 already emitted ToolExecutionStart for all tools,
        // so we only emit Update and End here.
        on_event(AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
            partial_result: output.clone(),
        });
        on_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            result: output.clone(),
            is_error: true,
        });

        let tool_result = Arc::new(ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: output.content,
            details: output.details,
            is_error: true,
            timestamp: Utc::now().timestamp_millis(),
        });

        let msg = Message::ToolResult(Arc::clone(&tool_result));
        self.messages.push(msg.clone());
        new_messages.push(msg.clone());

        on_event(AgentEvent::MessageStart {
            message: msg.clone(),
        });
        on_event(AgentEvent::MessageEnd { message: msg });

        tool_result
    }
}

// ============================================================================
// Agent Session (Agent + Session persistence)
// ============================================================================

struct ToolExecutionOutcome {
    tool_results: Vec<Arc<ToolResultMessage>>,
    steering_messages: Option<Vec<QueuedAgentMessage>>,
}

/// Pre-created extension runtime state for overlapping startup I/O.
///
/// By spawning runtime boot as a background task *before* session creation and
/// model selection, expensive runtime startup can overlap with other work.
pub struct PreWarmedExtensionRuntime {
    /// The extension manager (already has `cwd` and risk config set).
    pub manager: ExtensionManager,
    /// The booted runtime handle.
    pub runtime: ExtensionRuntimeHandle,
    /// The tool registry passed to the runtime during boot; the same handle
    /// the agent is later constructed over (bd-4t6oz).
    pub tools: crate::tools::SharedToolRegistry,
}

/// Host bridges that must exist while extensions register and run startup
/// hooks, rather than being attached after initialization has completed.
#[derive(Clone)]
pub struct ExtensionHostConfiguration {
    /// Direct host UI bridge used by `ctx.ui` and capability prompts.
    pub ui_handler: Option<Arc<dyn crate::extension_dispatcher::ExtensionUiHandler>>,
    /// Whether capability decisions may persist beyond this session.
    pub persist_permission_decisions: bool,
    /// Parsed extension flags, applied after registration and before startup.
    pub cli_flags: Vec<crate::cli::ExtensionCliFlag>,
}

impl Default for ExtensionHostConfiguration {
    fn default() -> Self {
        Self {
            ui_handler: None,
            persist_permission_decisions: true,
            cli_flags: Vec::new(),
        }
    }
}

/// RAII guard that resets an `AtomicBool` to `false` on drop, ensuring the
/// flag is cleared even if the enclosing async task is cancelled.
struct AtomicBoolGuard(Option<Arc<AtomicBool>>);

impl AtomicBoolGuard {
    fn activate(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(Some(Arc::clone(flag)))
    }

    /// Transfer responsibility for clearing the flag to an independently
    /// running operation (for example, a registered background worker).
    fn keep_active(mut self) {
        self.0.take();
    }
}

impl Drop for AtomicBoolGuard {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(false, Ordering::SeqCst);
        }
    }
}

pub struct AgentSession {
    pub agent: Agent,
    pub session: Arc<Mutex<Session>>,
    save_enabled: bool,
    input_source: InputSource,
    /// Extension lifecycle region — ensures the JS runtime thread is shut
    /// down when the session ends.
    pub extensions: Option<ExtensionRegion>,
    /// MCP client registry serving this session, when MCP is enabled. Turn
    /// runners that own the session (RPC loop, classic TUI) use it to pick up
    /// servers extensions register after startup (`registerMcpServer`).
    mcp_manager: Option<Arc<crate::mcp::McpManager>>,
    extensions_is_streaming: Arc<AtomicBool>,
    extensions_is_compacting: Arc<AtomicBool>,
    extensions_turn_active: Arc<AtomicBool>,
    extensions_pending_idle_actions: Arc<StdMutex<VecDeque<PendingIdleAction>>>,
    extension_queue_modes: Option<Arc<StdMutex<ExtensionQueueModeState>>>,
    extension_injected_queue: Option<Arc<StdMutex<ExtensionInjectedQueue>>>,
    extension_ai_completion: Arc<StdMutex<ExtensionAiCompletionHostState>>,
    compaction_settings: ResolvedCompactionSettings,
    compaction_runtime: Option<Runtime>,
    /// The advisor runtime (bd-cv653.3.3): Some only when the advisor role
    /// resolved a model. None = zero-overhead path (no digest is built).
    pub advisor: Option<crate::advisor::AdvisorRuntime>,
    runtime_handle: Option<RuntimeHandle>,
    compaction_worker: CompactionWorkerState,
    model_registry: Option<ModelRegistry>,
    auth_storage: Option<AuthStorage>,
    api_key_override: Option<String>,
    semantic_context_bundle: Option<SemanticContextBundleInjection>,
    /// One process-local authority for provider admission and transition
    /// quarantine. Extension hostcalls hold its permit across the complete
    /// provider future; model/session transitions hold it across persistence
    /// and live installation, so a check cannot race provider entry.
    provider_admission: ProviderAdmissionGate,
    /// Serializes extension-authored Session actions with Session identity
    /// replacement. The generation rejects an action that began before a
    /// replacement but only acquired the permit after the replacement.
    session_action_admission: SessionActionAdmissionGate,
}

const MAX_ADVISOR_CONTINUATIONS: u64 = 2;
const ADVISOR_CONTINUATION_ENTRY: &str = "advisor_continuation";

/// Couple an authored queued input to its advisor budget boundary. The stable
/// identity also lets terminal RPC recovery replay a cancelled candidate save
/// without creating another reset or a divergent session branch.
pub(crate) fn append_advisor_input_reset(
    session: &mut Session,
    message_entry_id: &str,
) -> Result<String> {
    let timestamp = match session.get_entry(message_entry_id) {
        Some(crate::session::SessionEntry::Message(entry))
            if matches!(&entry.message, crate::session::SessionMessage::User { .. }) =>
        {
            entry.base.timestamp.clone()
        }
        _ => {
            return Err(Error::session(
                "advisor input reset requires a persisted user message",
            ));
        }
    };
    session.append_custom_entry_with_identity(
        ADVISOR_CONTINUATION_ENTRY.to_string(),
        Some(json!({"count": 0})),
        &format!("advisor-input-{message_entry_id}"),
        &timestamp,
        Some(message_entry_id),
    )
}

/// One host-visible turn can contain a completed attempt plus advisor-driven
/// continuations. Keep its terminal event until the final durable result;
/// ordinary message/tool events and native extension lifecycle still stream.
struct AdvisorTurn {
    on_event: AgentEventHandler,
    terminal: Option<Arc<StdMutex<Option<AgentEvent>>>>,
    started: Instant,
    max_time: Option<Duration>,
}

impl AdvisorTurn {
    fn new(session: &AgentSession, on_event: AgentEventHandler) -> Self {
        Self {
            on_event,
            terminal: session
                .advisor
                .as_ref()
                .map(|_| Arc::new(StdMutex::new(None))),
            started: Instant::now(),
            max_time: session.agent.config.max_time,
        }
    }

    fn event_handler(&self) -> AgentEventHandler {
        let Some(terminal) = &self.terminal else {
            return Arc::clone(&self.on_event);
        };
        let terminal = Arc::clone(terminal);
        let on_event = Arc::clone(&self.on_event);
        Arc::new(move |event| {
            if let AgentEvent::AgentEnd {
                session_id,
                mut messages,
                error,
            } = event
            {
                let mut held = terminal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(AgentEvent::AgentEnd {
                    messages: previous,
                    error: previous_error,
                    ..
                }) = held.as_mut()
                {
                    previous.append(&mut messages);
                    *previous_error = error;
                } else {
                    *held = Some(AgentEvent::AgentEnd {
                        session_id,
                        messages,
                        error,
                    });
                }
            } else {
                on_event(event);
            }
        })
    }

    fn remaining_time(&self) -> Option<Duration> {
        self.max_time
            .map(|budget| budget.saturating_sub(self.started.elapsed()))
    }

    fn time_exhausted(&self) -> bool {
        self.remaining_time().is_some_and(|time| time.is_zero())
    }

    fn may_continue(&self, agent: &Agent, abort: Option<&AbortSignal>) -> bool {
        !self.time_exhausted()
            && !abort.is_some_and(AbortSignal::is_aborted)
            && !crate::advisor::ADVISOR_PAUSED.load(Ordering::SeqCst)
            && agent.automatic_queue_dispatch() == (true, true)
            && matches!(
                agent.plan_state.mode(),
                crate::plan::PlanMode::Off | crate::plan::PlanMode::Approved
            )
    }

    fn finish(self, result: Result<AssistantMessage>) -> Result<AssistantMessage> {
        if let Some(terminal) = &self.terminal {
            let held = terminal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(mut event) = held {
                if let Err(failure) = &result
                    && let AgentEvent::AgentEnd { error, .. } = &mut event
                {
                    *error = Some(failure.to_string());
                }
                (self.on_event)(event);
            }
        }
        result
    }
}

impl Drop for AdvisorTurn {
    fn drop(&mut self) {
        let Some(terminal) = &self.terminal else {
            return;
        };
        let held = terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut event) = held {
            // The native attempt already produced this terminal, but its
            // owning future was abandoned during review or publication.
            if let AgentEvent::AgentEnd { error, .. } = &mut event {
                error.get_or_insert_with(|| "Aborted".to_string());
            }
            (self.on_event)(event);
        }
    }
}

/// Keep an extension-selected prompt local to one provider attempt, including
/// when its future is dropped inside the agent loop or session persistence.
/// Owning the session borrow lets `Drop` restore state without an async lock.
struct SessionTurnPromptGuard<'a> {
    session: &'a mut AgentSession,
    base_system_prompt: Option<String>,
    base_max_time: Option<Duration>,
}

impl<'a> SessionTurnPromptGuard<'a> {
    fn new(session: &'a mut AgentSession, system_prompt: Option<String>) -> Self {
        let base_system_prompt = session.agent.system_prompt().map(str::to_string);
        let base_max_time = session.agent.config.max_time;
        session.agent.set_system_prompt(system_prompt);
        Self {
            session,
            base_system_prompt,
            base_max_time,
        }
    }
}

impl Drop for SessionTurnPromptGuard<'_> {
    fn drop(&mut self) {
        self.session
            .agent
            .set_system_prompt(self.base_system_prompt.take());
        self.session.agent.config.max_time = self.base_max_time;
        // The agent normally consumes this at loop entry. Early returns and
        // cancellation must not hand this turn's provenance to another input.
        let _ = self.session.agent.magic_keyword_scan_override.take();
    }
}

/// Extension-generated idle turns temporarily change input provenance.
struct SessionInputSourceGuard<'a> {
    session: &'a mut AgentSession,
    previous_source: InputSource,
}

impl<'a> SessionInputSourceGuard<'a> {
    const fn new(session: &'a mut AgentSession, source: InputSource) -> Self {
        let previous_source = std::mem::replace(&mut session.input_source, source);
        Self {
            session,
            previous_source,
        }
    }
}

impl Drop for SessionInputSourceGuard<'_> {
    fn drop(&mut self) {
        self.session.input_source = self.previous_source;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderAdmissionGate {
    permit: Arc<Mutex<()>>,
    reason: Arc<StdMutex<Option<String>>>,
}

impl Default for ProviderAdmissionGate {
    fn default() -> Self {
        Self {
            permit: Arc::new(Mutex::new(())),
            reason: Arc::new(StdMutex::new(None)),
        }
    }
}

impl ProviderAdmissionGate {
    pub(crate) fn reason(&self) -> Option<String> {
        self.reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn ensure_allowed(&self) -> Result<()> {
        if let Some(reason) = self.reason() {
            return Err(Error::session_persistence(format!(
                "provider re-entry is quarantined after an indeterminate Session transition: {reason}"
            )));
        }
        Ok(())
    }

    pub(crate) fn block(&self, reason: String) {
        *self
            .reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
    }

    pub(crate) fn clear(&self) {
        *self
            .reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub(crate) async fn acquire(&self, cx: &asupersync::Cx) -> Result<OwnedMutexGuard<()>> {
        OwnedMutexGuard::lock(Arc::clone(&self.permit), cx)
            .await
            .map_err(|err| Error::session(format!("provider admission lock failed: {err}")))
    }

    /// Reserve provider admission before acquiring Session-action authority.
    /// Preparing or declining a swap does not quarantine the session; the
    /// shared transition code marks uncertainty at its persistence boundary.
    pub(crate) async fn acquire_transition_authority(
        &self,
        cx: &asupersync::Cx,
    ) -> Result<ProviderTransitionAuthority> {
        let permit = self.acquire(cx).await?;
        self.ensure_allowed()?;
        Ok(ProviderTransitionAuthority {
            gate: self.clone(),
            _permit: permit,
        })
    }

    pub(crate) async fn begin_transition(
        &self,
        reason: String,
        cx: &asupersync::Cx,
    ) -> Result<OwnedMutexGuard<()>> {
        let permit = self.acquire(cx).await?;
        self.ensure_allowed()?;
        self.block(reason);
        Ok(permit)
    }
}

/// Proof that a caller owns this gate's provider permit. Recovery keeps this
/// authority while acquiring Session-action admission, then uses the same
/// transition machinery without recursively acquiring the provider mutex.
pub(crate) struct ProviderTransitionAuthority {
    gate: ProviderAdmissionGate,
    _permit: OwnedMutexGuard<()>,
}

#[derive(Clone, Copy)]
enum ProviderSwapAdmission<'a> {
    Gate(&'a ProviderAdmissionGate),
    Held(&'a ProviderTransitionAuthority),
}

impl<'a> ProviderSwapAdmission<'a> {
    const fn gate(self) -> &'a ProviderAdmissionGate {
        match self {
            Self::Gate(gate) => gate,
            Self::Held(authority) => &authority.gate,
        }
    }

    async fn begin_transition(
        self,
        reason: String,
        cx: &asupersync::Cx,
    ) -> Result<Option<OwnedMutexGuard<()>>> {
        match self {
            Self::Gate(gate) => gate.begin_transition(reason, cx).await.map(Some),
            Self::Held(authority) => {
                authority.gate.ensure_allowed()?;
                authority.gate.block(reason);
                Ok(None)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SessionActionAdmissionGate {
    permit: Arc<Mutex<()>>,
    origin_source: SessionActionOriginSource,
    #[cfg(test)]
    pending_generation_checks: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
struct PendingSessionActionGenerationCheck {
    count: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl Drop for PendingSessionActionGenerationCheck {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Default for SessionActionAdmissionGate {
    fn default() -> Self {
        Self {
            permit: Arc::new(Mutex::new(())),
            origin_source: SessionActionOriginSource::default(),
            #[cfg(test)]
            pending_generation_checks: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl SessionActionAdmissionGate {
    pub(crate) fn generation(&self) -> u64 {
        self.origin_source.generation()
    }

    pub(crate) fn origin_source(&self) -> SessionActionOriginSource {
        self.origin_source.clone()
    }

    pub(crate) fn capture_origin(&self) -> SessionActionOrigin {
        self.origin_source.capture()
    }

    pub(crate) async fn acquire(&self, cx: &asupersync::Cx) -> Result<OwnedMutexGuard<()>> {
        OwnedMutexGuard::lock(Arc::clone(&self.permit), cx)
            .await
            .map_err(|err| Error::session(format!("session action admission lock failed: {err}")))
    }

    async fn acquire_origin(&self, origin: &SessionActionOrigin) -> Result<OwnedMutexGuard<()>> {
        #[cfg(test)]
        let _pending_generation_check = {
            self.pending_generation_checks
                .fetch_add(1, Ordering::SeqCst);
            PendingSessionActionGenerationCheck {
                count: Arc::clone(&self.pending_generation_checks),
            }
        };
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let permit = self.acquire(cx.cx()).await?;
        if !self.origin_source.accepts(origin) {
            return Err(Error::session(
                "active Session changed before the extension action could be applied",
            ));
        }
        Ok(permit)
    }

    #[cfg(test)]
    fn pending_generation_check_count(&self) -> usize {
        self.pending_generation_checks.load(Ordering::SeqCst)
    }

    pub(crate) fn advance_generation(&self) {
        self.origin_source.advance();
    }
}

struct PreparedModelSelection {
    entry: Option<ModelEntry>,
    provider: Option<Arc<dyn Provider>>,
    resolved_key: Option<String>,
    provider_id: String,
    model_id: String,
    thinking_level: crate::model::ThinkingLevel,
}

#[derive(Debug, Clone, Copy)]
struct ExtensionQueueModeState {
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl ExtensionQueueModeState {
    const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering_mode,
            follow_up_mode,
        }
    }

    const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }
}

#[derive(Debug)]
struct ExtensionInjectedQueue {
    steering: VecDeque<Message>,
    follow_up: VecDeque<Message>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl ExtensionInjectedQueue {
    const fn new(steering_mode: QueueMode, follow_up_mode: QueueMode) -> Self {
        Self {
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            steering_mode,
            follow_up_mode,
        }
    }

    const fn set_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.steering_mode = steering_mode;
        self.follow_up_mode = follow_up_mode;
    }

    fn push_steering(&mut self, message: Message) {
        if self.steering.len() >= MAX_STEERING_QUEUE_SIZE {
            tracing::warn!(
                "Extension steering queue full ({} messages), dropping oldest message",
                MAX_STEERING_QUEUE_SIZE
            );
            self.steering.pop_front();
        }
        self.steering.push_back(message);
    }

    fn push_follow_up(&mut self, message: Message) {
        if self.follow_up.len() >= MAX_FOLLOW_UP_QUEUE_SIZE {
            tracing::warn!(
                "Extension follow-up queue full ({} messages), dropping oldest message",
                MAX_FOLLOW_UP_QUEUE_SIZE
            );
            self.follow_up.pop_front();
        }
        self.follow_up.push_back(message);
    }

    fn pop_steering(&mut self) -> Vec<Message> {
        match self.steering_mode {
            QueueMode::All => self.steering.drain(..).collect(),
            QueueMode::OneAtATime => self.steering.pop_front().into_iter().collect(),
        }
    }

    fn pop_follow_up(&mut self) -> Vec<Message> {
        match self.follow_up_mode {
            QueueMode::All => self.follow_up.drain(..).collect(),
            QueueMode::OneAtATime => self.follow_up.pop_front().into_iter().collect(),
        }
    }
}

impl Default for ExtensionInjectedQueue {
    fn default() -> Self {
        Self::new(QueueMode::OneAtATime, QueueMode::OneAtATime)
    }
}

#[derive(Debug)]
enum PendingIdleAction {
    CustomMessage(Message),
    UserText(String),
}

#[derive(Clone)]
struct AgentSessionHostActions {
    session: Arc<Mutex<Session>>,
    injected: Arc<StdMutex<ExtensionInjectedQueue>>,
    is_streaming: Arc<AtomicBool>,
    is_turn_active: Arc<AtomicBool>,
    pending_idle_actions: Arc<StdMutex<VecDeque<PendingIdleAction>>>,
    ai_completion: Arc<StdMutex<ExtensionAiCompletionHostState>>,
    provider_admission: ProviderAdmissionGate,
    session_action_admission: SessionActionAdmissionGate,
}

#[derive(Clone)]
struct ExtensionAiCompletionHostState {
    provider: Arc<dyn Provider>,
    stream_options: StreamOptions,
    models: Vec<Value>,
    compaction_models: ExtensionCompactionModelState,
    compaction_privacy: Arc<StdMutex<compaction::CompactionPrivacy>>,
}

#[derive(Clone, Default)]
struct ExtensionCompactionModelState {
    registry: Option<Arc<ModelRegistry>>,
    auth: Option<Arc<AuthStorage>>,
    js_provider_ids: Vec<String>,
}

impl ExtensionCompactionModelState {
    fn resolve(
        &self,
        current_provider: Arc<dyn Provider>,
        mut stream_options: StreamOptions,
        options: &ExtensionCompactionOptions,
    ) -> Result<(Arc<dyn Provider>, StreamOptions)> {
        let (provider_id, model_id) = options.model.as_ref().map_or_else(
            || (current_provider.name(), current_provider.model_id()),
            |model| (model.provider.trim(), model.id.trim()),
        );
        if provider_id.is_empty() || model_id.is_empty() {
            return Err(Error::validation(
                "compact: model provider and id must be non-empty strings",
            ));
        }
        let entry = self
            .registry
            .as_ref()
            .and_then(|registry| registry.find(provider_id, model_id));
        let (resolved_provider, resolved_model) =
            entry.as_ref().map_or((provider_id, model_id), |entry| {
                (entry.model.provider.as_str(), entry.model.id.as_str())
            });
        if self
            .js_provider_ids
            .iter()
            .any(|id| id == resolved_provider)
        {
            return Err(Error::validation(
                "compact: extension streamSimple models cannot be called from a compaction hook",
            ));
        }

        let same_provider = resolved_provider == current_provider.name();
        let same_model = same_provider && resolved_model == current_provider.model_id();
        let requested_thinking = options
            .thinking_level
            .or(stream_options.thinking_level)
            .unwrap_or(crate::model::ThinkingLevel::High);
        stream_options.thinking_level = Some(entry.as_ref().map_or(requested_thinking, |entry| {
            entry.clamp_thinking_level(requested_thinking)
        }));
        // The normal provider hook can itself be the extension awaiting this
        // request. The summary engine also clears it for ordinary callers.
        stream_options.before_provider_request = None;
        if same_model {
            return Ok((current_provider, stream_options));
        }

        let mut entry = entry.ok_or_else(|| {
            Error::validation(format!(
                "compact: model {provider_id}/{model_id} is not registered"
            ))
        })?;
        let target_key = same_provider
            .then(|| normalize_api_key_opt(stream_options.api_key.clone()))
            .flatten()
            .or_else(|| {
                self.auth.as_ref().and_then(|auth| {
                    normalize_api_key_opt(auth.resolve_api_key(&entry.model.provider, None))
                })
            })
            .or_else(|| normalize_api_key_opt(entry.api_key.clone()));
        // Some providers bind credentials during construction (Copilot's
        // token exchange). They must see the same resolved target credential
        // as providers that read it from StreamOptions for each request.
        entry.api_key.clone_from(&target_key);
        let provider =
            crate::providers::create_provider_with_auth(&entry, None, self.auth.as_deref())?;
        if model_requires_configured_credential(&entry)
            && target_key.is_none()
            && !Self::has_registered_header_auth(&entry, provider.as_ref())
        {
            return Err(Error::auth(format!(
                "compact: missing credentials for {}/{}",
                entry.model.provider, entry.model.id
            )));
        }
        // A model override never carries the source provider's key or
        // authorization headers into a different provider. Endpoints and
        // target headers come exclusively from the registered host entry.
        stream_options.api_key = target_key;
        stream_options.headers.clone_from(&entry.headers);
        stream_options.max_tokens = Some(entry.model.max_tokens);
        Ok((provider, stream_options))
    }

    fn has_registered_header_auth(entry: &ModelEntry, provider: &dyn Provider) -> bool {
        if crate::provider_metadata::canonical_provider_id(provider.name())
            == Some("github-copilot")
        {
            // Copilot reports openai-completions but requires its own GitHub
            // token exchange before it can send a completion request.
            return false;
        }
        let names: &[&str] = match provider.api() {
            "openai-completions"
            | "openai-responses"
            | "openai-codex-responses"
            | "google-vertex" => &["authorization"],
            "anthropic-messages" => &["authorization", "x-api-key"],
            "azure-openai" => &["authorization", "api-key"],
            "google-generative-ai" => &["authorization", "x-goog-api-key"],
            // Gemini CLI and other providers still require their native
            // credential payloads; unrelated headers do not make them ready.
            _ => return false,
        };
        let has_auth = |headers| {
            crate::providers::first_non_empty_header_value_case_insensitive(headers, names)
                .is_some()
        };
        has_auth(&entry.headers)
            || entry
                .compat
                .as_ref()
                .and_then(|compat| compat.custom_headers.as_ref())
                .is_some_and(has_auth)
    }
}

impl AgentSessionHostActions {
    async fn acquire_provider_admission(&self) -> Result<OwnedMutexGuard<()>> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let permit = self.provider_admission.acquire(cx.cx()).await?;
        self.provider_admission.ensure_allowed()?;
        Ok(permit)
    }

    async fn acquire_session_action_admission(
        &self,
        origin: Option<SessionActionOrigin>,
    ) -> Result<OwnedMutexGuard<()>> {
        let origin = origin.ok_or_else(|| {
            Error::session("extension Session action is missing trusted task provenance")
        })?;
        self.session_action_admission.acquire_origin(&origin).await
    }

    fn enqueue(&self, deliver_as: Option<ExtensionDeliverAs>, message: Message) {
        let deliver_as = deliver_as.unwrap_or(ExtensionDeliverAs::Steer);
        let Ok(mut queue) = self.injected.lock() else {
            tracing::error!("injected queue mutex poisoned; dropping extension message");
            return;
        };
        match deliver_as {
            ExtensionDeliverAs::FollowUp => {
                queue.push_follow_up(message);
            }
            ExtensionDeliverAs::Steer | ExtensionDeliverAs::NextTurn => {
                queue.push_steering(message);
            }
        }
    }

    async fn append_to_session(&self, message: Message) -> Result<()> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        session.append_model_message(message);
        Ok(())
    }

    fn queue_pending_idle_action(&self, action: PendingIdleAction) {
        let Ok(mut actions) = self.pending_idle_actions.lock() else {
            tracing::error!("pending idle actions mutex poisoned; dropping idle action");
            return;
        };
        actions.push_back(action);
    }
}

#[async_trait]
impl ExtensionHostActions for AgentSessionHostActions {
    async fn send_message(
        &self,
        message: ExtensionSendMessage,
        origin: Option<SessionActionOrigin>,
    ) -> Result<()> {
        let _session_action_permit = self.acquire_session_action_admission(origin).await?;
        let custom_message = Message::Custom(CustomMessage {
            content: message.content,
            custom_type: message.custom_type,
            display: message.display,
            details: message.details,
            timestamp: Utc::now().timestamp_millis(),
        });

        if matches!(message.deliver_as, Some(ExtensionDeliverAs::NextTurn)) {
            return self.append_to_session(custom_message).await;
        }

        if self.is_streaming.load(Ordering::SeqCst) {
            self.enqueue(message.deliver_as, custom_message);
            return Ok(());
        }

        if self.is_turn_active.load(Ordering::SeqCst) {
            return self.append_to_session(custom_message).await;
        }

        if message.trigger_turn {
            self.queue_pending_idle_action(PendingIdleAction::CustomMessage(custom_message));
            return Ok(());
        }

        self.append_to_session(custom_message).await
    }

    async fn send_user_message(
        &self,
        message: ExtensionSendUserMessage,
        origin: Option<SessionActionOrigin>,
    ) -> Result<()> {
        let _session_action_permit = self.acquire_session_action_admission(origin).await?;
        let text = message.text;
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(text.clone()),
            timestamp: Utc::now().timestamp_millis(),
        });

        if self.is_streaming.load(Ordering::SeqCst) {
            self.enqueue(message.deliver_as, user_message);
            return Ok(());
        }

        if self.is_turn_active.load(Ordering::SeqCst) {
            return self.append_to_session(user_message).await;
        }

        self.queue_pending_idle_action(PendingIdleAction::UserText(text));
        Ok(())
    }

    async fn complete_ai(&self, request: ExtensionAiCompletionRequest) -> Result<Value> {
        let _provider_admission = self.acquire_provider_admission().await?;
        let (provider, mut stream_options) = {
            let state = self.ai_completion.lock().map_err(|_| {
                Error::extension("extension completion host state mutex poisoned".to_string())
            })?;
            (Arc::clone(&state.provider), state.stream_options.clone())
        };

        apply_pi_ai_completion_options(&request.options, &mut stream_options)?;
        let context = build_pi_ai_completion_context(&request)?;
        let provider_name = provider.name().to_string();
        let mut events = provider.stream(&context, &stream_options).await?;
        let mut streamed_text = String::new();

        while let Some(event) = events.next().await {
            match event.map_err(|err| Error::provider(provider_name.clone(), err.to_string()))? {
                StreamEvent::TextDelta { delta, .. } => streamed_text.push_str(&delta),
                StreamEvent::TextEnd { content, .. } => {
                    streamed_text.push_str(&content);
                }
                StreamEvent::Done { message, .. } => {
                    if message.stop_reason == StopReason::Error {
                        return Err(Error::provider(
                            provider_name,
                            pi_ai_assistant_error_message(&message),
                        ));
                    }
                    return pi_ai_completion_response(&message, request.simple);
                }
                StreamEvent::Error { error, .. } => {
                    return Err(Error::provider(
                        provider_name,
                        pi_ai_assistant_error_message(&error),
                    ));
                }
                StreamEvent::Start { .. }
                | StreamEvent::TextStart { .. }
                | StreamEvent::ThinkingStart { .. }
                | StreamEvent::ThinkingDelta { .. }
                | StreamEvent::ThinkingEnd { .. }
                | StreamEvent::ToolCallStart { .. }
                | StreamEvent::ToolCallDelta { .. }
                | StreamEvent::ToolCallEnd { .. } => {}
            }
        }

        let suffix = if streamed_text.is_empty() {
            String::new()
        } else {
            format!(" after streaming {} text bytes", streamed_text.len())
        };
        Err(Error::provider(
            provider_name,
            format!("pi-ai completion stream ended without Done event{suffix}"),
        ))
    }

    async fn list_ai_models(&self) -> Result<Value> {
        let state = self.ai_completion.lock().map_err(|_| {
            Error::extension("extension completion host state mutex poisoned".to_string())
        })?;
        if state.models.is_empty() {
            return Ok(json!([{
                "id": state.provider.model_id(),
                "name": state.provider.model_id(),
                "api": state.provider.api(),
                "provider": state.provider.name(),
            }]));
        }
        Ok(Value::Array(state.models.clone()))
    }

    async fn compact_session(
        &self,
        preparation: Value,
        options: ExtensionCompactionOptions,
    ) -> Result<Value> {
        let _provider_admission = self.acquire_provider_admission().await?;
        // gh #167 / bd-i28yz: bridge ctx.compact() / pi-coding-agent
        // compact() to the native compaction engine. The preparation JSON is
        // untrusted extension input; the strict deserializer rejects
        // malformed shapes instead of defaulting them.
        let preparation = crate::compaction::compaction_preparation_from_value(&preparation)?;

        // Lock order / re-entrancy: this briefly locks only the
        // ai-completion `StdMutex` (exactly like `complete_ai` /
        // `list_ai_models`) and never touches the session mutex, while
        // `dispatch_before_compact` holds neither when awaiting extension
        // handlers -- so an extension calling compact() from inside
        // `session_before_compact` cannot deadlock.
        let (provider, stream_options, models, privacy) = {
            let state = self.ai_completion.lock().map_err(|_| {
                Error::extension("extension completion host state mutex poisoned".to_string())
            })?;
            (
                Arc::clone(&state.provider),
                state.stream_options.clone(),
                state.compaction_models.clone(),
                Arc::clone(&state.compaction_privacy),
            )
        };
        // Registry/auth resolution can evaluate a host-configured credential
        // command, so it runs after releasing the shared completion mutex.
        let (provider, stream_options) = models.resolve(provider, stream_options, &options)?;

        let privacy = privacy
            .lock()
            .map_err(|_| {
                Error::extension("extension compaction privacy state mutex poisoned".to_string())
            })?
            .clone();
        let result = crate::compaction::compact(
            preparation,
            provider,
            &stream_options,
            options.custom_instructions.as_deref(),
            &privacy,
        )
        .await?;
        serde_json::to_value(&result).map_err(|err| {
            Error::extension(format!("serialize extension compaction result: {err}"))
        })
    }

    async fn compact_session_native(&self, preparation: Value, request: Value) -> Result<Value> {
        let _provider_admission = self.acquire_provider_admission().await?;
        // gh #167: host-mediated native-Responses compaction. The extension
        // composes the compact request (so replay chains of previously
        // stored windows keep working) but the host owns endpoint + auth:
        // the request is reduced to an allowlist, credential-shaped keys are
        // rejected loudly, the model is pinned to the session's own, and the
        // POST happens inside the provider with the session's credentials.
        // Every failure is an `Err` so the calling extension fails open to
        // pi's default compaction (never a fabricated result).
        let mut preparation = crate::compaction::compaction_preparation_from_value(&preparation)?;

        let (provider, stream_options, privacy) = {
            let state = self.ai_completion.lock().map_err(|_| {
                Error::extension("extension completion host state mutex poisoned".to_string())
            })?;
            (
                Arc::clone(&state.provider),
                state.stream_options.clone(),
                Arc::clone(&state.compaction_privacy),
            )
        };

        if provider.api() != "openai-responses" {
            return Err(Error::validation(format!(
                "native compaction requires the session's provider to use the openai-responses API (current: {})",
                provider.api()
            )));
        }

        let sanitized = sanitize_native_compact_request(&request, provider.model_id())?;
        let mut privacy = privacy
            .lock()
            .map_err(|_| {
                Error::extension("extension compaction privacy state mutex poisoned".to_string())
            })?
            .clone();
        let sanitized = privacy.screen_native_request(&mut preparation, &sanitized)?;
        let response = provider.compact_native(&sanitized, &stream_options).await?;

        let (read_files, modified_files) =
            crate::compaction::compute_file_lists(&preparation.file_ops);
        let result = shape_native_compact_result(
            &response,
            &preparation.first_kept_entry_id,
            preparation.tokens_before,
            read_files,
            modified_files,
        )?;
        privacy.screen_native_result(result)
    }
}

/// Top-level fields an extension-composed native compact request may carry
/// across the bridge (gh #167). Everything else is dropped; credential-shaped
/// keys are rejected loudly first (see
/// [`sanitize_native_compact_request`]). Mirrors the optional extras
/// pi-better-compaction copies from the previous live provider request.
const NATIVE_COMPACT_REQUEST_ALLOWLIST: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "parallel_tool_calls",
    "reasoning",
    "service_tier",
    "prompt_cache_key",
    "text",
];

/// Keys that suggest an attempt to smuggle credentials or auth material
/// through the native compact bridge. Rejected with a hard error (not
/// silently dropped) so a confused extension gets a diagnosable failure.
const NATIVE_COMPACT_FORBIDDEN_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "authorization",
    "auth",
    "bearer",
    "credentials",
    "header",
    "headers",
    "key",
    "token",
];

/// Validate and reduce an untrusted, extension-composed native compact
/// request (gh #167): object shape enforced, credential-shaped keys rejected,
/// `model` pinned to the session's own model id, `input` required to be an
/// array, and only [`NATIVE_COMPACT_REQUEST_ALLOWLIST`] fields retained.
fn sanitize_native_compact_request(request: &Value, session_model_id: &str) -> Result<Value> {
    let Some(obj) = request.as_object() else {
        return Err(Error::validation(
            "native compact request must be a JSON object".to_string(),
        ));
    };

    for key in obj.keys() {
        let lowered = key.to_ascii_lowercase();
        if NATIVE_COMPACT_FORBIDDEN_KEYS.contains(&lowered.as_str()) {
            return Err(Error::validation(format!(
                "native compact request must not carry credential material (offending key: `{key}`); the host supplies auth from the session"
            )));
        }
    }

    match obj.get("model") {
        Some(Value::String(model)) if model == session_model_id => {}
        Some(Value::String(model)) => {
            return Err(Error::validation(format!(
                "native compact request model `{model}` does not match the session model `{session_model_id}`; the bridge always compacts with the session's own model"
            )));
        }
        _ => {
            return Err(Error::validation(
                "native compact request must carry a string `model` matching the session model"
                    .to_string(),
            ));
        }
    }

    if !obj.get("input").is_some_and(Value::is_array) {
        return Err(Error::validation(
            "native compact request must carry an `input` array".to_string(),
        ));
    }

    if let Some(instructions) = obj.get("instructions")
        && !(instructions.is_string() || instructions.is_null())
    {
        return Err(Error::validation(
            "native compact request `instructions` must be a string when present".to_string(),
        ));
    }

    let mut sanitized = serde_json::Map::new();
    let mut dropped: Vec<&str> = Vec::new();
    for (key, value) in obj {
        if NATIVE_COMPACT_REQUEST_ALLOWLIST.contains(&key.as_str()) {
            if !value.is_null() {
                sanitized.insert(key.clone(), value.clone());
            }
        } else {
            dropped.push(key.as_str());
        }
    }
    if !dropped.is_empty() {
        tracing::debug!(
            dropped = ?dropped,
            "native compact request: dropped non-allowlisted fields"
        );
    }
    Ok(Value::Object(sanitized))
}

/// Fallback summary recorded when the native compact output carries no
/// assistant `output_text` (matches pi-better-compaction's own fallback).
const NATIVE_COMPACT_FALLBACK_SUMMARY: &str = "[OpenAI native compaction checkpoint]";

/// Shape the raw native compact response into the
/// `session_before_compact`-compatible result (gh #167): `output` is
/// required and becomes `details.compactedWindow` verbatim; assistant
/// `output_text` (when present) becomes the human-readable summary.
fn shape_native_compact_result(
    response: &Value,
    first_kept_entry_id: &str,
    tokens_before: u64,
    read_files: Vec<String>,
    modified_files: Vec<String>,
) -> Result<Value> {
    let Some(output) = response.get("output").and_then(Value::as_array) else {
        return Err(Error::provider(
            "openai-responses".to_string(),
            "native compact response is missing the `output` array".to_string(),
        ));
    };

    let mut summary = String::new();
    for item in output {
        if item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("assistant")
            && let Some(content) = item.get("content").and_then(Value::as_array)
        {
            for part in content {
                if part.get("type").and_then(Value::as_str) == Some("output_text")
                    && let Some(text) = part.get("text").and_then(Value::as_str)
                {
                    if !summary.is_empty() {
                        summary.push('\n');
                    }
                    summary.push_str(text);
                }
            }
        }
    }
    if summary.trim().is_empty() {
        summary = NATIVE_COMPACT_FALLBACK_SUMMARY.to_string();
    }

    let mut details = serde_json::Map::new();
    details.insert(
        "strategy".to_string(),
        Value::String("openai-responses-native".to_string()),
    );
    details.insert("compactedWindow".to_string(), Value::Array(output.clone()));
    if let Some(id) = response.get("id").and_then(Value::as_str)
        && !id.is_empty()
    {
        details.insert(
            "compactResponseId".to_string(),
            Value::String(id.to_string()),
        );
    }
    if let Some(created_at) = response.get("created_at")
        && !created_at.is_null()
    {
        details.insert("createdAt".to_string(), created_at.clone());
    }
    details.insert(
        "readFiles".to_string(),
        Value::Array(read_files.into_iter().map(Value::String).collect()),
    );
    details.insert(
        "modifiedFiles".to_string(),
        Value::Array(modified_files.into_iter().map(Value::String).collect()),
    );

    Ok(serde_json::json!({
        "summary": summary,
        "firstKeptEntryId": first_kept_entry_id,
        "tokensBefore": tokens_before,
        "details": Value::Object(details),
    }))
}

fn pi_ai_model_entry_value(entry: &ModelEntry) -> Value {
    json!({
        "id": entry.model.id,
        "name": entry.model.name,
        "api": entry.model.api,
        "provider": entry.model.provider,
        "baseUrl": entry.model.base_url,
        "reasoning": entry.model.reasoning,
        "input": entry.model.input,
        "cost": entry.model.cost,
        "contextWindow": entry.model.context_window,
        "maxTokens": entry.model.max_tokens,
        "authHeader": entry.auth_header,
        "hasCredentials": entry.api_key.is_some(),
    })
}

fn pi_ai_model_registry_values(registry: &ModelRegistry) -> Vec<Value> {
    registry
        .models()
        .iter()
        .map(pi_ai_model_entry_value)
        .collect()
}

fn apply_pi_ai_completion_options(
    options: &Value,
    stream_options: &mut StreamOptions,
) -> Result<()> {
    if let Some(value) = options
        .get("temperature")
        .or_else(|| options.get("temp"))
        .filter(|value| !value.is_null())
    {
        let temperature = serde_json::from_value::<f32>(value.clone()).map_err(|err| {
            Error::validation(format!(
                "pi-ai completion temperature must be numeric: {err}"
            ))
        })?;
        if !(0.0..=2.0).contains(&temperature) {
            return Err(Error::validation(
                "pi-ai completion temperature must be between 0 and 2".to_string(),
            ));
        }
        stream_options.temperature = Some(temperature);
    }

    if let Some(value) = options
        .get("maxTokens")
        .or_else(|| options.get("max_tokens"))
        .filter(|value| !value.is_null())
    {
        let raw = value.as_u64().ok_or_else(|| {
            Error::validation("pi-ai completion maxTokens must be an unsigned integer".to_string())
        })?;
        let max_tokens = u32::try_from(raw).map_err(|_| {
            Error::validation("pi-ai completion maxTokens exceeds u32::MAX".to_string())
        })?;
        if max_tokens == 0 {
            return Err(Error::validation(
                "pi-ai completion maxTokens must be greater than zero".to_string(),
            ));
        }
        stream_options.max_tokens = Some(max_tokens);
    }

    Ok(())
}

fn build_pi_ai_completion_context(
    request: &ExtensionAiCompletionRequest,
) -> Result<Context<'static>> {
    let mut system_prompts = Vec::new();
    let mut messages = Vec::new();
    collect_pi_ai_context_messages(&request.context, &mut system_prompts, &mut messages)?;

    if messages.is_empty() {
        return Err(Error::validation(
            "@mariozechner/pi-ai completion requires at least one user or assistant message"
                .to_string(),
        ));
    }

    let system_prompt = system_prompts
        .into_iter()
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Context::owned(
        if system_prompt.is_empty() {
            None
        } else {
            Some(system_prompt)
        },
        messages,
        Vec::new(),
    ))
}

fn collect_pi_ai_context_messages(
    value: &Value,
    system_prompts: &mut Vec<String>,
    messages: &mut Vec<Message>,
) -> Result<()> {
    match value {
        Value::Null => {}
        Value::String(text) => push_pi_ai_user_message(text, messages),
        Value::Array(items) => {
            for item in items {
                push_pi_ai_message(item, system_prompts, messages)?;
            }
        }
        Value::Object(map) => {
            if let Some(system) = map
                .get("systemPrompt")
                .or_else(|| map.get("system_prompt"))
                .or_else(|| map.get("system"))
                .and_then(pi_ai_text_from_value)
            {
                system_prompts.push(system);
            }

            if let Some(items) = map.get("messages").and_then(Value::as_array) {
                for item in items {
                    push_pi_ai_message(item, system_prompts, messages)?;
                }
            } else if let Some(prompt) = map
                .get("prompt")
                .or_else(|| map.get("input"))
                .or_else(|| map.get("message"))
                .and_then(pi_ai_text_from_value)
            {
                push_pi_ai_user_message(&prompt, messages);
            } else if map.contains_key("role") {
                push_pi_ai_message(value, system_prompts, messages)?;
            }
        }
        Value::Bool(_) | Value::Number(_) => push_pi_ai_user_message(&value.to_string(), messages),
    }
    Ok(())
}

fn push_pi_ai_message(
    value: &Value,
    system_prompts: &mut Vec<String>,
    messages: &mut Vec<Message>,
) -> Result<()> {
    let Value::Object(map) = value else {
        if let Some(text) = pi_ai_text_from_value(value) {
            push_pi_ai_user_message(&text, messages);
        }
        return Ok(());
    };

    let role = map
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .trim()
        .to_ascii_lowercase();
    let content = map
        .get("content")
        .or_else(|| map.get("text"))
        .and_then(pi_ai_text_from_value)
        .unwrap_or_default();

    match role.as_str() {
        "system" => {
            if !content.trim().is_empty() {
                system_prompts.push(content);
            }
        }
        "user" => push_pi_ai_user_message(&content, messages),
        "assistant" => push_pi_ai_assistant_message(&content, messages),
        other => {
            return Err(Error::validation(format!(
                "@mariozechner/pi-ai completion does not support {other:?} context messages"
            )));
        }
    }
    Ok(())
}

fn push_pi_ai_user_message(text: &str, messages: &mut Vec<Message>) {
    messages.push(Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: Utc::now().timestamp_millis(),
    }));
}

fn push_pi_ai_assistant_message(text: &str, messages: &mut Vec<Message>) {
    messages.push(Message::assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new(text.to_string()))],
        timestamp: Utc::now().timestamp_millis(),
        stop_details: None,
        ..AssistantMessage::default()
    }));
}

fn pi_ai_text_from_value(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(_) | Value::Number(_) => Some(value.to_string()),
        Value::Array(items) => {
            let mut text = String::new();
            for item in items {
                if let Some(part) = pi_ai_text_from_value(item)
                    && !part.is_empty()
                {
                    text.push_str(&part);
                }
            }
            Some(text)
        }
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .or_else(|| map.get("delta"))
            .and_then(pi_ai_text_from_value),
    }
}

fn pi_ai_assistant_text(message: &AssistantMessage) -> String {
    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Text(text_block) = block {
            text.push_str(&text_block.text);
        }
    }
    text
}

fn pi_ai_assistant_error_message(message: &AssistantMessage) -> String {
    message
        .error_message
        .clone()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| {
            let text = pi_ai_assistant_text(message);
            if text.trim().is_empty() {
                "provider returned an error without a message".to_string()
            } else {
                text
            }
        })
}

fn pi_ai_completion_response(message: &AssistantMessage, simple: bool) -> Result<Value> {
    let text = pi_ai_assistant_text(message);
    if simple {
        return Ok(Value::String(text));
    }

    Ok(json!({
        "message": serde_json::to_value(message)?,
        "content": serde_json::to_value(&message.content)?,
        "text": text,
        "usage": serde_json::to_value(&message.usage)?,
        "model": message.model,
        "provider": message.provider,
        "api": message.api,
        "stopReason": message.stop_reason,
    }))
}

#[cfg(test)]
mod message_queue_tests {
    use super::*;

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn queued_user_message(text: &str) -> QueuedAgentMessage {
        QueuedAgentMessage::from_authored_message(user_message(text))
    }

    #[test]
    fn direct_authored_block_message_preserves_all_text_for_scanning() {
        let queued = QueuedAgentMessage::from_authored_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![
                ContentBlock::Text(TextContent::new("please orchestrate")),
                ContentBlock::Image(crate::model::ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                }),
                ContentBlock::Text(TextContent::new("then workflowz")),
            ]),
            timestamp: 0,
        }));

        assert_eq!(
            queued.keyword_scan_source(),
            Some("please orchestrate\nthen workflowz")
        );
    }

    #[test]
    fn message_queue_one_at_a_time() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(queued_user_message("a"));
        queue.push_steering(queued_user_message("b"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert!(matches!(
            first.first().map(QueuedAgentMessage::message),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "a")
        ));

        let second = queue.pop_steering();
        assert_eq!(second.len(), 1);
        assert!(matches!(
            second.first().map(QueuedAgentMessage::message),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "b")
        ));

        assert!(queue.pop_steering().is_empty());
    }

    #[test]
    fn message_queue_all_mode() {
        let mut queue = MessageQueue::new(QueueMode::All, QueueMode::OneAtATime);
        queue.push_steering(queued_user_message("a"));
        queue.push_steering(queued_user_message("b"));

        let drained = queue.pop_steering();
        assert_eq!(drained.len(), 2);
        assert!(queue.pop_steering().is_empty());
    }

    #[test]
    fn message_queue_separates_kinds() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(queued_user_message("steer"));
        queue.push_follow_up(queued_user_message("follow"));

        let steering = queue.pop_steering();
        assert_eq!(steering.len(), 1);
        assert_eq!(queue.pending_count(), 1);

        let follow = queue.pop_follow_up();
        assert_eq!(follow.len(), 1);
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn queued_message_clones_share_lazy_persistence_identity_for_cleanup() {
        let delivery = queued_user_message("durably acknowledged");
        let staged_clone = delivery.clone();
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(staged_clone);
        assert!(queue.contains_delivery(&delivery));

        let (entry_id, timestamp, parent_id) =
            delivery.bind_persistence_identity(Some("parent-entry".to_string()));
        assert_eq!(parent_id.as_deref(), Some("parent-entry"));
        assert!(!timestamp.is_empty());
        assert_eq!(
            queue.steering[0].delivery.persistence_entry_id(),
            Some(entry_id.as_str()),
            "a staged Agent clone must observe the identity bound by RPC durability"
        );

        let entry_ids = std::collections::HashSet::from([entry_id]);
        assert_eq!(queue.discard_persistence_ids(&entry_ids), 1);
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn message_queue_seq_increments() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        let first = queue.push_steering(queued_user_message("a"));
        let second = queue.push_follow_up(queued_user_message("b"));
        assert!(second > first);
    }

    #[test]
    fn message_queue_seq_saturates_at_u64_max() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.next_seq = u64::MAX;

        let first = queue.push_steering(queued_user_message("a"));
        let second = queue.push_follow_up(queued_user_message("b"));

        assert_eq!(first, u64::MAX);
        assert_eq!(second, u64::MAX);
        assert_eq!(queue.pending_count(), 2);
    }

    #[test]
    fn message_queue_follow_up_all_mode_drains_entire_queue_in_order() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::All);
        queue.push_follow_up(queued_user_message("f1"));
        queue.push_follow_up(queued_user_message("f2"));

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 2);
        assert!(matches!(
            follow_up.first().map(QueuedAgentMessage::message),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "f1")
        ));
        assert!(matches!(
            follow_up.get(1).map(QueuedAgentMessage::message),
            Some(Message::User(UserMessage { content, .. }))
                if matches!(content, UserContent::Text(text) if text == "f2")
        ));
        assert!(queue.pop_follow_up().is_empty());
    }

    #[test]
    fn ordinary_overflow_does_not_evict_an_older_job_notice() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::All);
        queue.push_job_follow_up_lossless(
            "owner-a".to_string(),
            QueuedAgentMessage::generated(user_message("job-notice")),
        );
        for index in 0..=MAX_FOLLOW_UP_QUEUE_SIZE {
            queue.push_follow_up(queued_user_message(&format!("ordinary-{index}")));
        }

        assert_eq!(
            queue
                .follow_up
                .iter()
                .filter(|entry| entry.job_owner_session_id.is_none())
                .count(),
            MAX_FOLLOW_UP_QUEUE_SIZE
        );
        assert_eq!(
            queue
                .follow_up
                .iter()
                .filter(|entry| entry.job_owner_session_id.as_deref() == Some("owner-a"))
                .count(),
            1,
            "ordinary admission pressure must evict only ordinary follow-ups"
        );
        let drained = queue.pop_follow_up();
        assert!(drained.iter().any(|delivery| {
            matches!(
                delivery.message(),
                Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                }) if text == "job-notice"
            )
        }));
    }
}

#[cfg(test)]
mod compatible_tool_parallelism_tests {
    use super::*;

    #[test]
    fn compatible_tool_parallelism_preserves_historical_floor() {
        assert_eq!(resolve_compatible_tool_parallelism(None, 1), 8);
        assert_eq!(resolve_compatible_tool_parallelism(None, 8), 8);
    }

    #[test]
    fn compatible_tool_parallelism_scales_on_many_core_hosts() {
        assert_eq!(resolve_compatible_tool_parallelism(None, 32), 32);
        assert_eq!(resolve_compatible_tool_parallelism(None, 64), 64);
        assert_eq!(resolve_compatible_tool_parallelism(None, 128), 64);
    }

    #[test]
    fn compatible_tool_parallelism_accepts_bounded_override() {
        assert_eq!(resolve_compatible_tool_parallelism(Some("16"), 4), 16);
        assert_eq!(resolve_compatible_tool_parallelism(Some("512"), 64), 256);
        assert_eq!(resolve_compatible_tool_parallelism(Some("1"), 64), 1);
    }

    #[test]
    fn compatible_tool_parallelism_ignores_invalid_override() {
        assert_eq!(
            resolve_compatible_tool_parallelism(Some("not-a-number"), 24),
            24
        );
        assert_eq!(resolve_compatible_tool_parallelism(Some("0"), 24), 24);
        assert_eq!(resolve_compatible_tool_parallelism(Some(" "), 24), 24);
    }
}

#[cfg(test)]
mod tool_effect_batch_planning_tests {
    use super::*;

    #[derive(Debug, Clone, Copy)]
    enum SyntheticOutcome {
        Success,
        Error,
    }

    #[derive(Debug, Clone)]
    struct SyntheticToolCase {
        id: String,
        name: String,
        registered_effects: Option<ToolEffects>,
        outcome: SyntheticOutcome,
    }

    #[derive(Debug, Clone, Copy)]
    enum BatchArrivalOrder {
        Forward,
        Reverse,
        RotateLeft(usize),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TranscriptEntry {
        tool_call_id: String,
        tool_name: String,
        text: String,
        details: serde_json::Value,
        is_error: bool,
    }

    fn batch_ranges(effects: &[ToolEffects]) -> Vec<(usize, usize)> {
        plan_tool_effect_batches(effects)
            .into_iter()
            .map(|batch| (batch.start, batch.end))
            .collect()
    }

    fn batch_plan_json(effects: &[ToolEffects], parallelism_cap: usize) -> serde_json::Value {
        serde_json::to_value(tool_effect_batch_plan_evidence(effects, parallelism_cap))
            .expect("tool-effect batch evidence should serialize")
    }

    fn synthetic_tool_case(
        index: usize,
        name: impl Into<String>,
        registered_effects: Option<ToolEffects>,
        outcome: SyntheticOutcome,
    ) -> SyntheticToolCase {
        SyntheticToolCase {
            id: format!("call-{index:03}"),
            name: name.into(),
            registered_effects,
            outcome,
        }
    }

    fn effect_plan(cases: &[SyntheticToolCase]) -> Vec<ToolEffects> {
        cases
            .iter()
            .map(|case| case.registered_effects.unwrap_or_else(ToolEffects::write))
            .collect()
    }

    fn make_tool_result(case: &SyntheticToolCase, index: usize) -> ToolResultMessage {
        let (content, is_error) = match case.outcome {
            SyntheticOutcome::Success => (format!("ok:{}", case.name), false),
            SyntheticOutcome::Error => (format!("error:{}", case.name), true),
        };
        ToolResultMessage {
            tool_call_id: case.id.clone(),
            tool_name: case.name.clone(),
            content: vec![ContentBlock::Text(TextContent::new(content))],
            details: Some(serde_json::json!({
                "ordinal": index,
                "tool": case.name,
                "status": if is_error { "error" } else { "ok" },
            })),
            is_error,
            timestamp: 42,
        }
    }

    fn transcript_entry(message: &ToolResultMessage) -> TranscriptEntry {
        assert_eq!(message.content.len(), 1, "synthetic result content drifted");
        let text = message
            .content
            .first()
            .and_then(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "non-text synthetic result".to_string());
        TranscriptEntry {
            tool_call_id: message.tool_call_id.clone(),
            tool_name: message.tool_name.clone(),
            text,
            details: message.details.clone().unwrap_or(serde_json::Value::Null),
            is_error: message.is_error,
        }
    }

    fn sequential_oracle(cases: &[SyntheticToolCase]) -> Vec<TranscriptEntry> {
        cases
            .iter()
            .enumerate()
            .map(|(index, case)| transcript_entry(&make_tool_result(case, index)))
            .collect()
    }

    fn reorder_batch(indices: &mut [usize], order: BatchArrivalOrder) {
        match order {
            BatchArrivalOrder::Forward => {}
            BatchArrivalOrder::Reverse => indices.reverse(),
            BatchArrivalOrder::RotateLeft(amount) => {
                if !indices.is_empty() {
                    indices.rotate_left(amount % indices.len());
                }
            }
        }
    }

    fn scheduled_transcript(
        cases: &[SyntheticToolCase],
        order: BatchArrivalOrder,
    ) -> Vec<TranscriptEntry> {
        let effects = effect_plan(cases);
        let batches = plan_tool_effect_batches(&effects);
        let mut recorded_results: Vec<Option<ToolResultMessage>> = vec![None; cases.len()];

        for batch in batches {
            let mut completion_order = (batch.start..batch.end).collect::<Vec<_>>();
            reorder_batch(&mut completion_order, order);
            let mut batch_results = completion_order
                .into_iter()
                .filter_map(|index| {
                    cases
                        .get(index)
                        .map(|case| (index, make_tool_result(case, index)))
                })
                .collect::<Vec<_>>();
            batch_results.sort_by_key(|(index, _)| *index);
            for (index, result) in batch_results {
                if let Some(slot) = recorded_results.get_mut(index) {
                    *slot = Some(result);
                }
            }
        }

        assert!(
            recorded_results.iter().all(Option::is_some),
            "scheduled execution should record every result"
        );
        recorded_results
            .into_iter()
            .flatten()
            .map(|result| transcript_entry(&result))
            .collect()
    }

    fn assert_barrier_effects_are_singleton_batches(cases: &[SyntheticToolCase]) {
        let effects = effect_plan(cases);
        for batch in plan_tool_effect_batches(&effects) {
            let batch_effects = effects
                .get(batch.start..batch.end)
                .unwrap_or(&[])
                .iter()
                .copied()
                .fold(ToolEffects::read(), ToolEffects::union);
            if !batch_effects.parallel_safe() {
                assert_eq!(
                    batch.end - batch.start,
                    1,
                    "barrier batch must serialize original index {}",
                    batch.start
                );
            }
        }
    }

    #[test]
    fn read_and_network_effects_share_compatible_batch() {
        let ranges = batch_ranges(&[
            ToolEffects::read(),
            ToolEffects::network(),
            ToolEffects::read(),
        ]);

        assert_eq!(ranges, vec![(0, 3)]);
    }

    #[test]
    fn evidence_records_64_plus_compatible_batch_with_parallelism_cap() {
        let effects = (0..72)
            .map(|index| {
                if index % 3 == 0 {
                    ToolEffects::network()
                } else {
                    ToolEffects::read()
                }
            })
            .collect::<Vec<_>>();

        assert_eq!(
            batch_plan_json(&effects, 64),
            serde_json::json!({
                "schema": TOOL_EFFECT_BATCH_PLAN_SCHEMA_V1,
                "toolCount": 72,
                "parallelismCap": 64,
                "batches": [
                    {
                        "start": 0,
                        "end": 72,
                        "len": 72,
                        "combinedEffects": ["read", "network"],
                        "parallelSafe": true
                    }
                ]
            })
        );
    }

    #[test]
    fn write_effect_creates_deterministic_barrier() {
        let ranges = batch_ranges(&[
            ToolEffects::read(),
            ToolEffects::read(),
            ToolEffects::write(),
            ToolEffects::read(),
        ]);

        assert_eq!(ranges, vec![(0, 2), (2, 3), (3, 4)]);
    }

    #[test]
    fn append_and_process_effects_remain_serialized() {
        let ranges = batch_ranges(&[
            ToolEffects::append(),
            ToolEffects::append(),
            ToolEffects::process(),
            ToolEffects::read(),
        ]);

        assert_eq!(ranges, vec![(0, 1), (1, 2), (2, 3), (3, 4)]);
    }

    #[test]
    fn combined_process_write_effect_is_exclusive() {
        let ranges = batch_ranges(&[
            ToolEffects::read(),
            ToolEffects::process().union(ToolEffects::write()),
            ToolEffects::network(),
        ]);

        assert_eq!(ranges, vec![(0, 1), (1, 2), (2, 3)]);
    }

    #[test]
    fn evidence_records_barrier_reasons_for_mixed_effects() {
        let effects = [
            ToolEffects::read(),
            ToolEffects::network(),
            ToolEffects::write(),
            ToolEffects::append(),
            ToolEffects::process(),
            ToolEffects::read(),
            ToolEffects::process().union(ToolEffects::write()),
        ];

        assert_eq!(
            batch_plan_json(&effects, 32),
            serde_json::json!({
                "schema": TOOL_EFFECT_BATCH_PLAN_SCHEMA_V1,
                "toolCount": 7,
                "parallelismCap": 32,
                "batches": [
                    {
                        "start": 0,
                        "end": 2,
                        "len": 2,
                        "combinedEffects": ["read", "network"],
                        "parallelSafe": true
                    },
                    {
                        "start": 2,
                        "end": 3,
                        "len": 1,
                        "combinedEffects": ["write"],
                        "parallelSafe": false,
                        "barrierReason": "write_barrier"
                    },
                    {
                        "start": 3,
                        "end": 4,
                        "len": 1,
                        "combinedEffects": ["append"],
                        "parallelSafe": false,
                        "barrierReason": "append_barrier"
                    },
                    {
                        "start": 4,
                        "end": 5,
                        "len": 1,
                        "combinedEffects": ["process"],
                        "parallelSafe": false,
                        "barrierReason": "process_barrier"
                    },
                    {
                        "start": 5,
                        "end": 6,
                        "len": 1,
                        "combinedEffects": ["read"],
                        "parallelSafe": true
                    },
                    {
                        "start": 6,
                        "end": 7,
                        "len": 1,
                        "combinedEffects": ["write", "process"],
                        "parallelSafe": false,
                        "barrierReason": "write_process_barrier"
                    }
                ]
            })
        );
    }

    #[test]
    fn metamorphic_empty_tool_batch_matches_sequential_oracle() {
        let cases = Vec::new();

        assert!(plan_tool_effect_batches(&effect_plan(&cases)).is_empty());
        assert_eq!(
            scheduled_transcript(&cases, BatchArrivalOrder::Forward),
            sequential_oracle(&cases)
        );
    }

    #[test]
    fn metamorphic_mixed_effect_batches_match_sequential_oracle() {
        let cases = vec![
            synthetic_tool_case(
                0,
                "read",
                Some(ToolEffects::read()),
                SyntheticOutcome::Success,
            ),
            synthetic_tool_case(
                1,
                "network",
                Some(ToolEffects::network()),
                SyntheticOutcome::Success,
            ),
            synthetic_tool_case(
                2,
                "write",
                Some(ToolEffects::write()),
                SyntheticOutcome::Success,
            ),
            synthetic_tool_case(
                3,
                "read",
                Some(ToolEffects::read()),
                SyntheticOutcome::Success,
            ),
            synthetic_tool_case(
                4,
                "append",
                Some(ToolEffects::append()),
                SyntheticOutcome::Error,
            ),
            synthetic_tool_case(
                5,
                "network",
                Some(ToolEffects::network()),
                SyntheticOutcome::Success,
            ),
            synthetic_tool_case(
                6,
                "process",
                Some(ToolEffects::process()),
                SyntheticOutcome::Success,
            ),
            synthetic_tool_case(
                7,
                "read",
                Some(ToolEffects::read()),
                SyntheticOutcome::Error,
            ),
            synthetic_tool_case(8, "unknown", None, SyntheticOutcome::Success),
            synthetic_tool_case(
                9,
                "network",
                Some(ToolEffects::network()),
                SyntheticOutcome::Success,
            ),
        ];

        assert_eq!(
            batch_ranges(&effect_plan(&cases)),
            vec![
                (0, 2),
                (2, 3),
                (3, 4),
                (4, 5),
                (5, 6),
                (6, 7),
                (7, 8),
                (8, 9),
                (9, 10)
            ]
        );
        let evidence = tool_effect_batch_plan_evidence(&effect_plan(&cases), 16);
        assert_eq!(evidence.schema, TOOL_EFFECT_BATCH_PLAN_SCHEMA_V1);
        assert_eq!(evidence.parallelism_cap, 16);
        assert_eq!(evidence.batches.len(), 9);
        assert!(evidence.batches.iter().any(|batch| {
            batch.barrier_reason == Some("append_barrier") && batch.combined_effects == ["append"]
        }));
        assert!(
            cases
                .iter()
                .any(|case| matches!(case.outcome, SyntheticOutcome::Error)),
            "mixed-effect fixture must include failure cases"
        );
        assert_barrier_effects_are_singleton_batches(&cases);

        let oracle = sequential_oracle(&cases);
        assert_eq!(
            scheduled_transcript(&cases, BatchArrivalOrder::Reverse),
            oracle
        );
        assert_eq!(
            scheduled_transcript(&cases, BatchArrivalOrder::RotateLeft(1)),
            oracle
        );
    }

    #[test]
    fn metamorphic_high_count_batches_keep_transcript_deterministic() {
        let cases = (0..96)
            .map(|index| match index % 12 {
                0 => synthetic_tool_case(
                    index,
                    format!("process-{index}"),
                    Some(ToolEffects::process()),
                    SyntheticOutcome::Success,
                ),
                5 => synthetic_tool_case(
                    index,
                    format!("append-{index}"),
                    Some(ToolEffects::append()),
                    SyntheticOutcome::Success,
                ),
                9 => synthetic_tool_case(
                    index,
                    format!("unknown-{index}"),
                    None,
                    SyntheticOutcome::Error,
                ),
                3 | 7 => synthetic_tool_case(
                    index,
                    format!("network-{index}"),
                    Some(ToolEffects::network()),
                    SyntheticOutcome::Success,
                ),
                _ => synthetic_tool_case(
                    index,
                    format!("read-{index}"),
                    Some(ToolEffects::read()),
                    SyntheticOutcome::Success,
                ),
            })
            .collect::<Vec<_>>();

        assert_barrier_effects_are_singleton_batches(&cases);
        let oracle = sequential_oracle(&cases);
        assert_eq!(
            scheduled_transcript(&cases, BatchArrivalOrder::Forward),
            oracle
        );
        assert_eq!(
            scheduled_transcript(&cases, BatchArrivalOrder::Reverse),
            oracle
        );
        assert_eq!(
            scheduled_transcript(&cases, BatchArrivalOrder::RotateLeft(3)),
            oracle
        );
    }
}

#[cfg(test)]
mod extensions_integration_tests {
    use super::*;

    use crate::session::Session;
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    async fn wait_for_session_action_generation_capture(gate: &SessionActionAdmissionGate) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while gate.pending_generation_check_count() == 0 {
            assert!(
                Instant::now() < deadline,
                "session action did not capture its source generation before the deadline"
            );
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(1)).await;
        }
    }

    async fn wait_for_session_action_generation_release(gate: &SessionActionAdmissionGate) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while gate.pending_generation_check_count() != 0 {
            assert!(
                Instant::now() < deadline,
                "cancelled Session action did not release its pending admission before the deadline"
            );
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(1)).await;
        }
    }

    /// bd-cv653.6.2: a glob-scoped imported rule is queued as steering the
    /// first time a tool call touches a matching path — exactly once — and
    /// non-matching tool calls queue nothing.
    #[test]
    fn scoped_rules_activate_once_per_rule_via_tool_call_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let provider: Arc<dyn Provider> = Arc::new(NoopProvider);
        let tools = ToolRegistry::new(&[], tmp.path(), None);
        let mut agent = Agent::new(provider, tools, AgentConfig::default());
        agent.set_foreign_scoped_rules(
            vec![crate::context_files::ForeignRule {
                content: "Use strict TypeScript.".to_string(),
                globs: vec!["*.ts".to_string()],
                always_apply: false,
                description: None,
                source: ".cursor/rules/ts.mdc".to_string(),
                format: crate::context_files::ForeignRuleFormat::CursorMdc,
            }],
            tmp.path().to_path_buf(),
        );

        let tool_call = |path: &str| ToolCall {
            id: "call".to_string(),
            name: "read".to_string(),
            arguments: json!({ "path": path }),
            thought_signature: None,
        };

        agent.activate_scoped_rules_for_tool_calls(&[tool_call("README.md")]);
        assert!(
            agent.message_queue.pop_steering().is_empty(),
            "non-matching path must not activate the rule"
        );

        agent.activate_scoped_rules_for_tool_calls(&[tool_call("src/main.ts")]);
        let delivered = agent.message_queue.pop_steering();
        assert_eq!(delivered.len(), 1, "matching path activates the rule once");
        assert_eq!(delivered[0].keyword_scan_source(), None);
        let Message::User(user) = delivered[0].message() else {
            panic!("rule must arrive as a user steering message");
        };
        let UserContent::Text(text) = &user.content else {
            panic!("rule steering must be text");
        };
        assert!(text.contains("imported-rule"));
        assert!(text.contains(".cursor/rules/ts.mdc"));
        assert!(text.contains("Use strict TypeScript."));

        agent.activate_scoped_rules_for_tool_calls(&[tool_call("other/lib.ts")]);
        assert!(
            agent.message_queue.pop_steering().is_empty(),
            "an activated rule must not re-queue"
        );
    }

    /// bd-1q31s: handler responses accept the upstream shapes (rewritten
    /// payload object directly, or `{ payload: ... }`) and treat null /
    /// non-object responses as "no rewrite".
    #[test]
    fn before_provider_request_response_shapes_normalize() {
        let direct = json!({"model": "m", "input": []});
        assert_eq!(
            normalize_before_provider_request_response(direct.clone()),
            Some(direct)
        );

        let wrapped = json!({"payload": {"model": "m", "input": []}});
        assert_eq!(
            normalize_before_provider_request_response(wrapped),
            Some(json!({"model": "m", "input": []}))
        );

        assert_eq!(
            normalize_before_provider_request_response(json!({"payload": null})),
            None
        );
        assert_eq!(
            normalize_before_provider_request_response(Value::Null),
            None
        );
        assert_eq!(
            normalize_before_provider_request_response(json!("not an object")),
            None
        );
    }

    #[derive(Debug)]
    struct NoopProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for NoopProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[derive(Debug)]
    struct IdleCommandProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for IdleCommandProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            let done = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(
                    "resumed-response-0".to_string(),
                ))],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: done,
                }),
            ])))
        }
    }

    #[derive(Debug)]
    struct CountingTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "count_tool"
        }

        fn label(&self) -> &str {
            "count_tool"
        }

        fn description(&self) -> &str {
            "counting tool"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("ok"))],
                details: None,
                is_error: false,
            })
        }
    }

    #[derive(Debug)]
    struct ToolUseProvider {
        stream_calls: AtomicUsize,
    }

    impl ToolUseProvider {
        const fn new() -> Self {
            Self {
                stream_calls: AtomicUsize::new(0),
            }
        }

        fn assistant_message(
            &self,
            stop_reason: StopReason,
            content: Vec<ContentBlock>,
        ) -> AssistantMessage {
            AssistantMessage {
                content,
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolUseProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);

            let partial = self.assistant_message(StopReason::Stop, Vec::new());

            let (reason, message) = if call_index == 0 {
                let tool_calls = vec![
                    ToolCall {
                        id: "call-1".to_string(),
                        name: "count_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    },
                    ToolCall {
                        id: "call-2".to_string(),
                        name: "count_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    },
                ];

                (
                    StopReason::ToolUse,
                    self.assistant_message(
                        StopReason::ToolUse,
                        tool_calls
                            .into_iter()
                            .map(ContentBlock::ToolCall)
                            .collect::<Vec<_>>(),
                    ),
                )
            } else {
                (
                    StopReason::Stop,
                    self.assistant_message(
                        StopReason::Stop,
                        vec![ContentBlock::Text(TextContent::new("done"))],
                    ),
                )
            };

            let events = vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done { reason, message }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn agent_session_enable_extensions_registers_extension_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "hello_tool",
                    label: "hello_tool",
                    description: "test tool",
                    parameters: { type: "object", properties: { name: { type: "string" } } },
                    execute: async (_callId, input, _onUpdate, _abort, ctx) => {
                      const who = input && input.name ? String(input.name) : "world";
                      const cwd = ctx && ctx.cwd ? String(ctx.cwd) : "";
                      return {
                        content: [{ type: "text", text: `hello ${who}` }],
                        details: { from: "extension", cwd: cwd },
                        isError: false
                      };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let registry = agent_session.agent.tools.snapshot();
            let tool = registry.get("hello_tool").expect("hello_tool registered");

            let output = tool
                .execute("call-1", json!({ "name": "pi" }), None)
                .await
                .expect("execute tool");

            assert!(!output.is_error);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected single text content block, got {:?}",
                output.content
            );
            let [ContentBlock::Text(text)] = output.content.as_slice() else {
                return;
            };
            assert_eq!(text.text, "hello pi");

            let details = output.details.expect("details present");
            assert_eq!(
                details.get("from").and_then(serde_json::Value::as_str),
                Some("extension")
            );
        });
    }

    #[test]
    fn agent_session_enable_extensions_with_no_entries_clears_and_is_noop() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            // Manually inject a dummy extension state to verify clearing behavior.
            let dummy_manager = ExtensionManager::new();
            agent_session.extensions = Some(crate::extensions::ExtensionRegion::new(dummy_manager.clone()));
            agent_session.agent.extensions = Some(dummy_manager.clone());
            agent_session.extension_queue_modes = Some(Arc::new(std::sync::Mutex::new(ExtensionQueueModeState::new(
                QueueMode::OneAtATime,
                QueueMode::OneAtATime,
            ))));
            agent_session.extension_injected_queue = Some(Arc::new(std::sync::Mutex::new(ExtensionInjectedQueue::default())));

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[])
                .await
                .expect("empty extension list should be a no-op");

            assert!(
                agent_session.extensions.is_none(),
                "no extension region should be created (and existing should be cleared) for an empty extension list"
            );
            assert!(
                agent_session.agent.extensions.is_none(),
                "agent should not report extensions active when nothing was requested"
            );
            assert!(
                agent_session.extension_queue_modes.is_none(),
                "empty extension list should clear queue mode mirrors"
            );
            assert!(
                agent_session.extension_injected_queue.is_none(),
                "empty extension list should clear injected extension queues"
            );
        });
    }

    #[test]
    fn agent_session_enable_extensions_rejects_mixed_js_and_native_entries() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let js_entry = temp_dir.path().join("ext.mjs");
            let native_entry = temp_dir.path().join("ext.native.json");
            std::fs::write(
                &js_entry,
                r"
                export default function init(_pi) {}
                ",
            )
            .expect("write js extension entry");
            std::fs::write(&native_entry, "{}").expect("write native extension descriptor");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let err = agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[js_entry, native_entry])
                .await
                .expect_err("mixed extension runtimes should be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains("Mixed extension runtimes are not supported"),
                "unexpected mixed-runtime error message: {msg}"
            );
        });
    }

    #[test]
    fn extension_send_message_persists_custom_message_entry_when_idle() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "emit_message",
                    label: "emit_message",
                    description: "emit a custom message",
                    parameters: { type: "object" },
                    execute: async () => {
                      pi.sendMessage({
                        customType: "note",
                        content: "hello",
                        display: true,
                        details: { from: "test" }
                      }, {});
                      return { content: [{ type: "text", text: "ok" }], isError: false };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let registry = agent_session.agent.tools.snapshot();
            let tool = registry
                .get("emit_message")
                .expect("emit_message registered");

            let _ = tool
                .execute("call-1", json!({}), None)
                .await
                .expect("execute tool");

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session.lock(cx.cx()).await.expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Custom(CustomMessage { custom_type, content, display, details, .. })
                            if custom_type == "note"
                                && content == "hello"
                                && *display
                                && details
                                    .as_ref()
                                    .and_then(|v| v.get("from").and_then(Value::as_str))
                                    .is_some_and(|from| from.eq("test"))
                    )
                }),
                "expected custom message to be persisted, got {messages:?}"
            );
        });
    }

    #[test]
    fn extension_send_message_persists_custom_message_entry_when_idle_after_await() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "emit_message",
                    label: "emit_message",
                    description: "emit a custom message",
                    parameters: { type: "object" },
                    execute: async () => {
                      await Promise.resolve();
                      pi.sendMessage({
                        customType: "note",
                        content: "hello-after-await",
                        display: true,
                        details: { from: "test" }
                      }, {});
                      return { content: [{ type: "text", text: "ok" }], isError: false };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let registry = agent_session.agent.tools.snapshot();
            let tool = registry
                .get("emit_message")
                .expect("emit_message registered");

            let _ = tool
                .execute("call-1", json!({}), None)
                .await
                .expect("execute tool");

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session.lock(cx.cx()).await.expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Custom(CustomMessage { custom_type, content, display, details, .. })
                            if custom_type == "note"
                                && content == "hello-after-await"
                                && *display
                                && details
                                    .as_ref()
                                    .and_then(|v| v.get("from").and_then(Value::as_str))
                                    .is_some_and(|from| from.eq("test"))
                    )
                }),
                "expected custom message to be persisted, got {messages:?}"
            );
        });
    }

    #[test]
    fn agent_host_actions_send_message_inherits_cancelled_context_when_locked() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let session_action_admission = SessionActionAdmissionGate::default();
            let origin = session_action_admission.capture_origin();
            let actions = AgentSessionHostActions {
                session: Arc::clone(&session),
                injected: Arc::new(StdMutex::new(ExtensionInjectedQueue::default())),
                is_streaming: Arc::new(AtomicBool::new(false)),
                is_turn_active: Arc::new(AtomicBool::new(false)),
                pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
                ai_completion: Arc::new(StdMutex::new(ExtensionAiCompletionHostState {
                    provider: Arc::new(NoopProvider),
                    stream_options: StreamOptions::default(),
                    models: Vec::new(),
                    compaction_models: ExtensionCompactionModelState::default(),
                    compaction_privacy: Arc::new(StdMutex::new(
                        compaction::CompactionPrivacy::default(),
                    )),
                })),
                provider_admission: ProviderAdmissionGate::default(),
                session_action_admission,
            };

            let hold_cx = crate::agent_cx::AgentCx::for_request();
            let held_guard = session.lock(hold_cx.cx()).await.expect("lock session");

            let ambient_cx = asupersync::Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = asupersync::Cx::set_current(Some(ambient_cx));
            let inner = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_millis(100),
                actions.send_message(
                    ExtensionSendMessage {
                        extension_id: Some("ext".to_string()),
                        custom_type: "note".to_string(),
                        content: "blocked".to_string(),
                        display: false,
                        details: None,
                        deliver_as: Some(ExtensionDeliverAs::NextTurn),
                        trigger_turn: false,
                    },
                    Some(origin),
                ),
            )
            .await;
            let outcome = inner.expect("cancelled helper should finish before timeout");
            let err = outcome.expect_err("session append should fail under inherited cancellation");
            assert!(
                err.to_string().contains("mutex lock cancelled"),
                "unexpected error: {err}"
            );

            drop(held_guard);

            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = session.lock(cx.cx()).await.expect("lock session");
            assert!(
                guard.to_messages_for_current_path().is_empty(),
                "cancelled send_message should not append a message"
            );
        });
    }

    #[test]
    fn agent_host_actions_reject_message_delayed_across_session_transition() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let session_action_admission = SessionActionAdmissionGate::default();
            let actions = AgentSessionHostActions {
                session: Arc::clone(&session),
                injected: Arc::new(StdMutex::new(ExtensionInjectedQueue::default())),
                is_streaming: Arc::new(AtomicBool::new(false)),
                is_turn_active: Arc::new(AtomicBool::new(false)),
                pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
                ai_completion: Arc::new(StdMutex::new(ExtensionAiCompletionHostState {
                    provider: Arc::new(NoopProvider),
                    stream_options: StreamOptions::default(),
                    models: Vec::new(),
                    compaction_models: ExtensionCompactionModelState::default(),
                    compaction_privacy: Arc::new(StdMutex::new(
                        compaction::CompactionPrivacy::default(),
                    )),
                })),
                provider_admission: ProviderAdmissionGate::default(),
                session_action_admission: session_action_admission.clone(),
            };
            let transition_cx = crate::agent_cx::AgentCx::for_request();
            let transition_permit = session_action_admission
                .acquire(transition_cx.cx())
                .await
                .expect("transition admission");
            let source_origin = session_action_admission.capture_origin();

            let delayed_actions = actions.clone();
            let hostcall = runtime_handle.spawn(async move {
                delayed_actions
                    .send_message(
                        ExtensionSendMessage {
                            extension_id: Some("ext".to_string()),
                            custom_type: "note".to_string(),
                            content: "must wait for transition".to_string(),
                            display: false,
                            details: None,
                            deliver_as: Some(ExtensionDeliverAs::NextTurn),
                            trigger_turn: false,
                        },
                        Some(source_origin),
                    )
                    .await
            });
            wait_for_session_action_generation_capture(&session_action_admission).await;
            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let guard = session.lock(cx.cx()).await.expect("session lock");
                assert!(
                    guard.to_messages_for_current_path().is_empty(),
                    "host action must not mutate the Session while transition admission is held"
                );
            }

            session_action_admission.advance_generation();
            drop(transition_permit);
            let err = hostcall
                .await
                .expect_err("action from the previous Session generation must be rejected");
            assert!(
                err.to_string().contains("active Session changed"),
                "unexpected delayed-action error: {err}"
            );

            actions
                .send_message(
                    ExtensionSendMessage {
                        extension_id: Some("ext".to_string()),
                        custom_type: "note".to_string(),
                        content: "belongs to the new session".to_string(),
                        display: false,
                        details: None,
                        deliver_as: Some(ExtensionDeliverAs::NextTurn),
                        trigger_turn: false,
                    },
                    Some(session_action_admission.capture_origin()),
                )
                .await
                .expect("new-generation action");
            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = session.lock(cx.cx()).await.expect("session lock");
            // Bind the owned message vec first: iterating the temporary
            // directly would leave &str borrows pointing at a value dropped
            // at the end of this let statement.
            let messages = guard.to_messages_for_current_path();
            let custom_contents = messages
                .iter()
                .filter_map(|message| match message {
                    Message::Custom(CustomMessage { content, .. }) => Some(content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(custom_contents, vec!["belongs to the new session"]);
        });
    }

    #[test]
    fn session_action_admission_rejects_foreign_origin_at_same_generation() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let gate = SessionActionAdmissionGate::default();
            let foreign_gate = SessionActionAdmissionGate::default();
            assert_eq!(gate.generation(), foreign_gate.generation());

            let Err(err) = gate.acquire_origin(&foreign_gate.capture_origin()).await else {
                panic!("a same-counter token from another Session gate must be rejected")
            };
            assert!(
                err.to_string().contains("active Session changed"),
                "unexpected foreign-origin error: {err}"
            );

            let current_permit = gate
                .acquire_origin(&gate.capture_origin())
                .await
                .expect("the gate's own current origin must remain valid");
            drop(current_permit);
        });
    }

    #[derive(Debug, Default)]
    pub(super) struct PiAiCapturedProviderContext {
        pub(super) system_prompt: Option<String>,
        pub(super) messages: Vec<Message>,
        pub(super) options: StreamOptions,
    }

    #[derive(Debug)]
    pub(super) struct PiAiCaptureProvider {
        pub(super) calls: Arc<StdMutex<Vec<PiAiCapturedProviderContext>>>,
    }

    #[async_trait]
    impl Provider for PiAiCaptureProvider {
        fn name(&self) -> &'static str {
            "capturing-provider"
        }

        fn api(&self) -> &'static str {
            "test-api"
        }

        fn model_id(&self) -> &'static str {
            "capture-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            options: &StreamOptions,
        ) -> crate::error::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>,
            >,
        > {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(PiAiCapturedProviderContext {
                    system_prompt: context.system_prompt.as_ref().map(ToString::to_string),
                    messages: context.messages.iter().cloned().collect(),
                    options: options.clone(),
                });
            let final_message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("captured"))],
                api: "test-api".to_string(),
                provider: "capturing-provider".to_string(),
                model: "capture-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                },
            )])))
        }
    }

    #[test]
    fn agent_host_actions_complete_ai_streams_configured_provider() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let calls = Arc::new(StdMutex::new(Vec::new()));
            let provider = Arc::new(PiAiCaptureProvider {
                calls: Arc::clone(&calls),
            });
            let actions = AgentSessionHostActions {
                session,
                injected: Arc::new(StdMutex::new(ExtensionInjectedQueue::default())),
                is_streaming: Arc::new(AtomicBool::new(false)),
                is_turn_active: Arc::new(AtomicBool::new(false)),
                pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
                ai_completion: Arc::new(StdMutex::new(ExtensionAiCompletionHostState {
                    provider,
                    stream_options: StreamOptions::default(),
                    models: vec![json!({
                        "id": "capture-model",
                        "provider": "capturing-provider",
                        "api": "test-api",
                    })],
                    compaction_models: ExtensionCompactionModelState::default(),
                    compaction_privacy: Arc::new(StdMutex::new(
                        compaction::CompactionPrivacy::default(),
                    )),
                })),
                provider_admission: ProviderAdmissionGate::default(),
                session_action_admission: SessionActionAdmissionGate::default(),
            };

            let result = actions
                .complete_ai(ExtensionAiCompletionRequest {
                    model: json!({ "id": "capture-model" }),
                    context: json!({
                        "systemPrompt": "answer tersely",
                        "messages": [
                            { "role": "user", "content": "ping" }
                        ]
                    }),
                    options: json!({ "maxTokens": 16 }),
                    simple: false,
                })
                .await
                .expect("complete through provider");

            assert_eq!(result["text"], json!("captured"));
            assert_eq!(result["provider"], json!("capturing-provider"));
            assert_eq!(result["api"], json!("test-api"));

            let (captured_len, captured_system_prompt, captured_messages) = {
                let captured = match calls.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                (
                    captured.len(),
                    captured.first().and_then(|call| call.system_prompt.clone()),
                    captured
                        .first()
                        .map(|call| call.messages.clone())
                        .unwrap_or_default(),
                )
            };
            assert_eq!(captured_len, 1);
            assert_eq!(captured_system_prompt.as_deref(), Some("answer tersely"));
            assert_eq!(captured_messages.len(), 1);
            assert!(
                matches!(
                    captured_messages.first(),
                    Some(Message::User(UserMessage { content: UserContent::Text(text), .. }))
                        if text == "ping"
                ),
                "expected user message context, got {captured_messages:?}"
            );

            let models = actions.list_ai_models().await.expect("list models");
            assert_eq!(models[0]["id"], json!("capture-model"));

            actions
                .provider_admission
                .block("test quarantine".to_string());
            let blocked = actions
                .complete_ai(ExtensionAiCompletionRequest {
                    model: json!({ "id": "capture-model" }),
                    context: json!({
                        "messages": [{ "role": "user", "content": "blocked" }]
                    }),
                    options: json!({}),
                    simple: true,
                })
                .await
                .expect_err("quarantined host action must not call the provider");
            assert!(blocked.is_session_persistence());
            assert_eq!(
                calls.lock().expect("calls lock").len(),
                1,
                "quarantine must reject before provider.stream"
            );
        });
    }

    /// gh #167 / bd-i28yz: the compact_session host action deserializes a
    /// valid preparation, compacts with the session's own provider, and
    /// returns the {summary, firstKeptEntryId, tokensBefore, details} shape
    /// consumed by the session_before_compact replacement contract; malformed
    /// preparation is rejected with a validation error, never a fabricated
    /// summary.
    #[test]
    #[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
    fn agent_host_actions_compact_session_bridges_native_compaction() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let calls = Arc::new(StdMutex::new(Vec::new()));
            let provider = Arc::new(PiAiCaptureProvider {
                calls: Arc::clone(&calls),
            });
            let actions = AgentSessionHostActions {
                session,
                injected: Arc::new(StdMutex::new(ExtensionInjectedQueue::default())),
                is_streaming: Arc::new(AtomicBool::new(false)),
                is_turn_active: Arc::new(AtomicBool::new(false)),
                pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
                ai_completion: Arc::new(StdMutex::new(ExtensionAiCompletionHostState {
                    provider,
                    stream_options: StreamOptions {
                        api_key: Some("host-managed-key".to_string()),
                        headers: std::collections::HashMap::from([(
                            "x-compaction-route".to_string(),
                            "required-route".to_string(),
                        )]),
                        max_tokens: Some(4096),
                        thinking_level: Some(crate::model::ThinkingLevel::High),
                        ..StreamOptions::default()
                    },
                    models: Vec::new(),
                    compaction_models: ExtensionCompactionModelState::default(),
                    compaction_privacy: Arc::new(StdMutex::new(
                        compaction::CompactionPrivacy::default(),
                    )),
                })),
                provider_admission: ProviderAdmissionGate::default(),
                session_action_admission: SessionActionAdmissionGate::default(),
            };

            let preparation = json!({
                "firstKeptEntryId": "entry-9",
                "messagesToSummarize": [
                    { "role": "user", "content": "investigate the flaky scheduler test" }
                ],
                "turnPrefixMessages": [],
                "isSplitTurn": false,
                "tokensBefore": 4200,
                "previousSummary": "## Goal\nShip the scheduler fix",
                "fileOps": {
                    "read": ["src/lib.rs"],
                    "written": [],
                    "edited": ["src/agent.rs"]
                },
                "settings": {
                    "enabled": true,
                    "contextWindowTokens": 128_000,
                    "reserveTokens": 10_240,
                    "keepRecentTokens": 12_800
                }
            });

            let result = actions
                .compact_session(
                    preparation,
                    ExtensionCompactionOptions {
                        model: Some(crate::extensions::ExtensionCompactionModel {
                            provider: "capturing-provider".to_string(),
                            id: "capture-model".to_string(),
                        }),
                        custom_instructions: Some("Preserve the retry-count decision".to_string()),
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                    },
                )
                .await
                .expect("compact through session provider");

            let summary = result["summary"].as_str().expect("summary string");
            assert!(
                summary.contains("captured"),
                "summary must come from the session provider: {summary}"
            );
            assert_eq!(result["firstKeptEntryId"], json!("entry-9"));
            assert_eq!(result["tokensBefore"], json!(4200));
            assert_eq!(result["details"]["readFiles"], json!(["src/lib.rs"]));
            assert_eq!(result["details"]["modifiedFiles"], json!(["src/agent.rs"]));

            // Exactly one provider call, and it used the session's own
            // provider (never an extension-chosen endpoint).
            let captured_len = match calls.lock() {
                Ok(guard) => guard.len(),
                Err(poisoned) => poisoned.into_inner().len(),
            };
            assert_eq!(captured_len, 1);
            {
                let captured = calls.lock().expect("captured request");
                let call = &captured[0];
                assert_eq!(call.options.api_key.as_deref(), Some("host-managed-key"));
                assert_eq!(call.options.headers["x-compaction-route"], "required-route");
                assert_eq!(call.options.max_tokens, Some(4096));
                assert_eq!(
                    call.options.thinking_level,
                    Some(crate::model::ThinkingLevel::Low)
                );
                assert!(
                    serde_json::to_string(&call.messages)
                        .unwrap()
                        .contains("Preserve the retry-count decision")
                );
            }
            assert_eq!(
                actions
                    .ai_completion
                    .lock()
                    .unwrap()
                    .stream_options
                    .thinking_level,
                Some(crate::model::ThinkingLevel::High),
                "per-compaction options must not change the active session",
            );

            // Malformed preparation is rejected with a validation error.
            let err = actions
                .compact_session(
                    json!({ "firstKeptEntryId": "" }),
                    ExtensionCompactionOptions::default(),
                )
                .await
                .expect_err("empty firstKeptEntryId must be rejected");
            assert!(
                err.to_string()
                    .contains("`firstKeptEntryId` must be a non-empty string"),
                "unexpected error: {err}"
            );

            let err = actions
                .compact_session(
                    json!("not-an-object"),
                    ExtensionCompactionOptions::default(),
                )
                .await
                .expect_err("non-object preparation must be rejected");
            assert!(
                err.to_string()
                    .contains("compaction preparation must be a JSON object"),
                "unexpected error: {err}"
            );
        });
    }

    fn compaction_override_entry(provider: &str, id: &str, requires_key: bool) -> ModelEntry {
        ModelEntry {
            model: crate::provider::Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "openai-completions".to_string(),
                provider: provider.to_string(),
                base_url: "https://compaction.example.invalid/v1".to_string(),
                reasoning: false,
                input: vec![crate::provider::InputType::Text],
                cost: crate::provider::ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 32_768,
                max_tokens: 2048,
                headers: std::collections::HashMap::new(),
            },
            api_key: None,
            headers: std::collections::HashMap::from([(
                "x-target-route".to_string(),
                "target-only".to_string(),
            )]),
            auth_header: requires_key,
            compat: None,
            oauth_config: None,
        }
    }

    fn compaction_override(provider: &str, id: &str) -> ExtensionCompactionOptions {
        ExtensionCompactionOptions {
            model: Some(crate::extensions::ExtensionCompactionModel {
                provider: provider.to_string(),
                id: id.to_string(),
            }),
            thinking_level: Some(crate::model::ThinkingLevel::Max),
            ..ExtensionCompactionOptions::default()
        }
    }

    #[test]
    fn extension_compaction_override_uses_target_auth_headers_and_model_limits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut auth = AuthStorage::load(dir.path().join("auth.json")).expect("auth");
        auth.set(
            "compaction-target",
            crate::auth::AuthCredential::ApiKey {
                key: "target-auth-key".to_string(),
            },
        );
        let mut entry = compaction_override_entry("compaction-target", "summary-model", true);
        entry.api_key = Some("catalog-fallback-key".to_string());
        let mut registry = ModelRegistry::load_with_credential_resolver(None, |_| None);
        registry.merge_entries(vec![entry]);
        let state = ExtensionCompactionModelState {
            registry: Some(Arc::new(registry)),
            auth: Some(Arc::new(auth)),
            js_provider_ids: Vec::new(),
        };
        let current: Arc<dyn Provider> = Arc::new(PiAiCaptureProvider {
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let original = StreamOptions {
            api_key: Some("source-provider-key".to_string()),
            headers: std::collections::HashMap::from([(
                "authorization".to_string(),
                "Bearer source-private-header".to_string(),
            )]),
            max_tokens: Some(8192),
            ..StreamOptions::default()
        };
        let (provider, options) = state
            .resolve(
                Arc::clone(&current),
                original.clone(),
                &compaction_override("compaction-target", "summary-model"),
            )
            .expect("resolve registered model");
        assert_eq!(provider.name(), "compaction-target");
        assert_eq!(provider.model_id(), "summary-model");
        assert!(!Arc::ptr_eq(&provider, &current));
        assert_eq!(options.api_key.as_deref(), Some("target-auth-key"));
        assert_eq!(options.headers["x-target-route"], "target-only");
        assert!(!options.headers.contains_key("authorization"));
        assert_eq!(options.max_tokens, Some(2048));
        assert_eq!(
            options.thinking_level,
            Some(crate::model::ThinkingLevel::Off)
        );
        assert_eq!(original.api_key.as_deref(), Some("source-provider-key"));
        assert_eq!(current.name(), "capturing-provider");
    }

    #[test]
    fn extension_compaction_override_accepts_registered_header_auth_without_a_key() {
        let current: Arc<dyn Provider> = Arc::new(PiAiCaptureProvider {
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        for api in ["openai-completions", "openai-responses"] {
            for use_compat in [false, true] {
                for (header, value, ready) in [
                    ("AUTHORIZATION", "Bearer target-header", true),
                    ("Authorization", "   ", false),
                    ("x-tenant", "not-an-auth-header", false),
                ] {
                    let mut entry =
                        compaction_override_entry("compaction-header", "summary-model", true);
                    entry.model.api = api.to_string();
                    let headers =
                        std::collections::HashMap::from([(header.to_string(), value.to_string())]);
                    if use_compat {
                        entry.compat = Some(crate::models::CompatConfig {
                            custom_headers: Some(headers),
                            ..crate::models::CompatConfig::default()
                        });
                    } else {
                        entry.headers.extend(headers);
                    }
                    let mut registry = ModelRegistry::load_with_credential_resolver(None, |_| None);
                    registry.merge_entries(vec![entry]);
                    let state = ExtensionCompactionModelState {
                        registry: Some(Arc::new(registry)),
                        ..ExtensionCompactionModelState::default()
                    };
                    let result = state.resolve(
                        Arc::clone(&current),
                        StreamOptions {
                            api_key: Some("unrelated-source-key".to_string()),
                            headers: std::collections::HashMap::from([(
                                "x-source-private".to_string(),
                                "source-only".to_string(),
                            )]),
                            ..StreamOptions::default()
                        },
                        &compaction_override("compaction-header", "summary-model"),
                    );
                    if ready {
                        let (provider, options) = result.expect("registered header authentication");
                        assert_eq!(provider.api(), api);
                        assert!(options.api_key.is_none());
                        assert!(!options.headers.contains_key("x-source-private"));
                        if !use_compat {
                            assert_eq!(options.headers[header], value);
                        }
                    } else {
                        let error = result.err().expect("no usable target credential");
                        assert!(error.to_string().contains("missing credentials"));
                    }
                }
            }
        }
    }

    #[test]
    fn extension_compaction_override_supplies_fresh_constructor_auth() {
        const CHILD_ENV: &str = "PI_COMPACTION_CONSTRUCTOR_AUTH_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            // Copilot can otherwise hide a missing constructor credential by
            // falling back to the test runner's ambient GitHub credentials.
            let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "agent::extensions_integration_tests::extension_compaction_override_supplies_fresh_constructor_auth",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .env_remove("GITHUB_COPILOT_API_KEY")
                .env_remove("GITHUB_TOKEN")
                .output()
                .expect("run isolated constructor-auth regression");
            assert!(
                output.status.success(),
                "constructor-auth child failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let mut auth = AuthStorage::load(dir.path().join("auth.json")).expect("auth");
        auth.set(
            "github-copilot",
            crate::auth::AuthCredential::ApiKey {
                key: "fresh-target-github-token".to_string(),
            },
        );
        let entry = compaction_override_entry("github-copilot", "copilot-summary", true);
        assert!(
            entry.api_key.is_none(),
            "catalog intentionally has no credential"
        );
        let mut registry = ModelRegistry::load_with_credential_resolver(None, |_| None);
        registry.merge_entries(vec![entry]);
        let state = ExtensionCompactionModelState {
            registry: Some(Arc::new(registry)),
            auth: Some(Arc::new(auth)),
            js_provider_ids: Vec::new(),
        };
        let current: Arc<dyn Provider> = Arc::new(PiAiCaptureProvider {
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let (provider, options) = state
            .resolve(
                current,
                StreamOptions::default(),
                &compaction_override("github-copilot", "copilot-summary"),
            )
            .expect("factory receives auth resolved after the catalog was loaded");
        assert_eq!(provider.name(), "github-copilot");
        assert_eq!(
            options.api_key.as_deref(),
            Some("fresh-target-github-token")
        );
    }

    #[test]
    fn extension_compaction_override_rejects_unknown_uncredentialed_and_js_models() {
        let mut registry = ModelRegistry::load_with_credential_resolver(None, |_| None);
        registry.merge_entries(vec![
            compaction_override_entry("compaction-private", "requires-key", true),
            compaction_override_entry("compaction-js", "js-model", false),
            compaction_override_entry("compaction-local", "local-model", false),
        ]);
        let state = ExtensionCompactionModelState {
            registry: Some(Arc::new(registry)),
            auth: None,
            js_provider_ids: vec!["compaction-js".to_string()],
        };
        let current: Arc<dyn Provider> = Arc::new(PiAiCaptureProvider {
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        for (provider, model, expected) in [
            ("compaction-missing", "absent", "not registered"),
            ("compaction-private", "requires-key", "missing credentials"),
            ("compaction-js", "js-model", "streamSimple"),
            ("", "bad-id", "non-empty"),
        ] {
            let error = state
                .resolve(
                    Arc::clone(&current),
                    StreamOptions {
                        api_key: Some("unrelated-key".to_string()),
                        ..StreamOptions::default()
                    },
                    &compaction_override(provider, model),
                )
                .err()
                .expect("invalid target must fail before provider work");
            assert!(
                error.to_string().contains(expected),
                "unexpected error: {error}"
            );
        }
        let (_, options) = state
            .resolve(
                current,
                StreamOptions {
                    api_key: Some("unrelated-key".to_string()),
                    ..StreamOptions::default()
                },
                &compaction_override("compaction-local", "local-model"),
            )
            .expect("keyless registered model");
        assert!(
            options.api_key.is_none(),
            "keyless target must not inherit a source credential"
        );
    }

    #[test]
    fn extension_compaction_registry_and_auth_follow_session_updates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut auth = AuthStorage::load(dir.path().join("auth.json")).expect("auth");
        let provider: Arc<dyn Provider> = Arc::new(PiAiCaptureProvider {
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig::default(),
        );
        let mut session = AgentSession::new(
            agent,
            Arc::new(Mutex::new(Session::in_memory())),
            false,
            ResolvedCompactionSettings::default(),
        );
        let mut registry = ModelRegistry::load_with_credential_resolver(None, |_| None);
        registry.merge_entries(vec![compaction_override_entry(
            "compaction-live",
            "live-model",
            true,
        )]);

        // Without extensions nothing can read the compaction registry, so
        // setting the registry and auth does not build it.
        session.set_model_registry(registry.clone());
        session.set_auth_storage(auth.clone());
        assert!(
            session
                .extension_ai_completion
                .lock()
                .unwrap()
                .compaction_models
                .registry
                .is_none(),
            "the compaction registry is built only for sessions with extensions"
        );

        // With extensions it follows every registry and auth update.
        session.extensions = Some(crate::extensions::ExtensionRegion::new(
            crate::extensions::ExtensionManager::new(),
        ));
        session.set_model_registry(registry);
        for key in ["first-live-key", "replacement-live-key"] {
            auth.set(
                "compaction-live",
                crate::auth::AuthCredential::ApiKey {
                    key: key.to_string(),
                },
            );
            session.set_auth_storage(auth.clone());
            let state = session.extension_ai_completion.lock().unwrap().clone();
            let (_, options) = state
                .compaction_models
                .resolve(
                    state.provider,
                    state.stream_options,
                    &compaction_override("compaction-live", "live-model"),
                )
                .expect("updated target auth");
            assert_eq!(options.api_key.as_deref(), Some(key));
        }
        assert_eq!(session.agent.provider().name(), "capturing-provider");
    }

    struct NativePrivacyCapture {
        requests: Arc<StdMutex<Vec<Value>>>,
        response: Arc<StdMutex<Value>>,
    }

    #[async_trait]
    impl Provider for NativePrivacyCapture {
        fn name(&self) -> &'static str {
            "native-privacy"
        }

        fn api(&self) -> &'static str {
            "openai-responses"
        }

        fn model_id(&self) -> &'static str {
            "native-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>>
        {
            Err(Error::api(
                "native privacy fixture only supports native compaction",
            ))
        }

        async fn compact_native(&self, body: &Value, options: &StreamOptions) -> Result<Value> {
            assert_eq!(options.api_key.as_deref(), Some("auth-kept"));
            self.requests.lock().expect("requests").push(body.clone());
            Ok(self
                .response
                .lock()
                .expect("scripted native response") // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture response ownership must remain intact.
                .clone())
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn native_compaction_host_screens_configured_patterns_and_fresh_live_vault() {
        asupersync::test_utils::run_test(|| async {
            for mode in ["obfuscate", "block"] {
                let requests = Arc::new(StdMutex::new(Vec::new()));
                let provider = Arc::new(NativePrivacyCapture {
                    requests: Arc::clone(&requests),
                    response: Arc::new(StdMutex::new(json!({"output": [{
                        "type": "compaction", "id": "opaque-window", "encrypted_content": "opaque-data"
                    }]}))),
                });
                let agent = Agent::new(
                    provider,
                    ToolRegistry::without_builtins(None),
                    AgentConfig {
                        secrets: Some(crate::secrets::SecretsSettings {
                            mode: Some(mode.to_string()),
                            extra_patterns: Some(vec![r"^ACME-[0-9]{6}$".to_string()]),
                        }),
                        stream_options: StreamOptions {
                            api_key: Some("auth-kept".to_string()),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                );
                let mut session = AgentSession::new(
                    agent,
                    Arc::new(Mutex::new(Session::in_memory())),
                    false,
                    ResolvedCompactionSettings::default(),
                );
                let actions = AgentSessionHostActions {
                    session: Arc::clone(&session.session),
                    injected: Arc::new(StdMutex::new(ExtensionInjectedQueue::default())),
                    is_streaming: Arc::new(AtomicBool::new(false)),
                    is_turn_active: Arc::new(AtomicBool::new(false)),
                    pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
                    ai_completion: Arc::clone(&session.extension_ai_completion),
                    provider_admission: session.provider_admission_gate(),
                    session_action_admission: session.session_action_admission_gate(),
                };
                let known = "rememberedNativeCredential123456789";
                if mode == "obfuscate" {
                    // Learn AFTER the host action is constructed. A one-time
                    // host-state snapshot would leak this otherwise bare echo.
                    session
                        .agent
                        .secrets_transform_outbound_text(&format!("API_KEY={known}"))
                        .expect("learn current-session credential");
                }
                let preparation = compaction::compaction_preparation_to_value(
                    &compaction::CompactionPreparation {
                        first_kept_entry_id: "anchor-kept".to_string(),
                        messages_to_summarize: Vec::new(),
                        turn_prefix_messages: Vec::new(),
                        is_split_turn: false,
                        tokens_before: 100,
                        previous_summary: None,
                        file_ops: compaction::FileOperations::default(),
                        settings: ResolvedCompactionSettings::default(),
                    },
                );
                let request = json!({
                    "model": "native-model",
                    "instructions": "summarize this work",
                    "input": [
                        {"role": "user", "content": [
                            {"type": "input_text", "text": known},
                            {"type": "input_text", "text": "ACME-123456"}
                        ]},
                        {"type": "function_call", "id": "item-kept", "call_id": "call-kept",
                         "name": "configure", "arguments": "{\"project_token\":\"ACME-654321\"}"}
                    ]
                });
                let result = actions
                    .compact_session_native(preparation.clone(), request.clone())
                    .await;
                if mode == "block" {
                    assert!(
                        result
                            .expect_err("block before native dispatch")
                            .to_string()
                            .contains("PI_SECRET_BLOCK")
                    );
                    assert!(requests.lock().expect("requests").is_empty());
                    continue;
                }
                assert_eq!(
                    result.expect("native result")["firstKeptEntryId"],
                    "anchor-kept"
                );
                let captured = requests.lock().expect("requests")[0].clone();
                let rendered = captured.to_string();
                for secret in [known, "ACME-123456", "ACME-654321"] {
                    assert!(!rendered.contains(secret));
                }
                assert!(rendered.contains("<pi-secret:redacted>"));
                assert_eq!(captured["input"][1]["call_id"], "call-kept");
                assert_eq!(captured["input"][1]["id"], "item-kept");
                let arguments: Value = serde_json::from_str(
                    // ubs:ignore[rust.parsing.serde-unwrap] -- Parsing provider-observed fixture arguments verifies preserved native wire syntax.
                    captured["input"][1]["arguments"]
                        .as_str()
                        .expect("JSON arguments stay encoded"),
                )
                .expect("screened arguments remain valid JSON"); // ubs:ignore[rust.ownership.unwrap-expect] -- Malformed provider-observed fixture arguments must fail the regression.
                assert_eq!(arguments["project_token"], "<pi-secret:redacted>");
                assert_eq!(request["input"][0]["content"][0]["text"], known);

                session
                    .agent
                    .reset_session_scoped_state(crate::plan::PlanMode::default());
                actions
                    .compact_session_native(
                        preparation,
                        json!({
                            "model": "native-model",
                            "input": [{"role": "user", "content": known}]
                        }),
                    )
                    .await
                    .expect("a new Session must not inherit the old vault");
                assert_eq!(
                    requests.lock().expect("requests")[1]["input"][0]["content"],
                    known
                );
            }
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn native_compaction_screens_returned_windows_and_refuses_fragmented_text() {
        asupersync::test_utils::run_test(|| async {
            for mode in ["obfuscate", "block"] {
                let known = "rememberedNativeWindowCredential123456789";
                let discovered = "requestDiscoveredCredential987654321";
                let requests = Arc::new(StdMutex::new(Vec::new()));
                let response = Arc::new(StdMutex::new(json!({"output": [
                    {"type": "compaction", "id": "window-kept", "encrypted_content": "opaque-kept"},
                    {"type": "message", "role": "assistant", "id": "message-kept", "content": [
                        {"type": "output_text", "text": known},
                        {"type": "output_text", "text": discovered},
                        {"type": "output_text", "text": "ACME-123456"}
                    ]}
                ]})));
                let mut session = AgentSession::new(
                    Agent::new(
                        Arc::new(NativePrivacyCapture {
                            requests: Arc::clone(&requests),
                            response: Arc::clone(&response),
                        }),
                        ToolRegistry::without_builtins(None),
                        AgentConfig {
                            secrets: Some(crate::secrets::SecretsSettings {
                                mode: Some(mode.to_string()),
                                extra_patterns: Some(vec![
                                    r"^ACME-[0-9]{6}$".to_string(),
                                    r"^ACME-\n[0-9]{6}$".to_string(),
                                ]),
                            }),
                            stream_options: StreamOptions {
                                api_key: Some("auth-kept".to_string()),
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                    ),
                    Arc::new(Mutex::new(Session::in_memory())),
                    false,
                    ResolvedCompactionSettings::default(),
                );
                let actions = AgentSessionHostActions {
                    session: Arc::clone(&session.session),
                    injected: Arc::new(StdMutex::new(ExtensionInjectedQueue::default())),
                    is_streaming: Arc::new(AtomicBool::new(false)),
                    is_turn_active: Arc::new(AtomicBool::new(false)),
                    pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
                    ai_completion: Arc::clone(&session.extension_ai_completion),
                    provider_admission: session.provider_admission_gate(),
                    session_action_admission: session.session_action_admission_gate(),
                };
                if mode == "obfuscate" {
                    session
                        .agent
                        .secrets_transform_outbound_text(&format!("API_KEY={known}"))
                        .expect("learn old-window credential"); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture seeds a live credential before native replay.
                }
                let preparation = compaction::compaction_preparation_to_value(
                    &compaction::CompactionPreparation {
                        first_kept_entry_id: "ACME-654321".to_string(),
                        messages_to_summarize: Vec::new(),
                        turn_prefix_messages: Vec::new(),
                        is_split_turn: false,
                        tokens_before: 100,
                        previous_summary: None,
                        file_ops: compaction::FileOperations::default(),
                        settings: ResolvedCompactionSettings::default(),
                    },
                );
                let error = actions
                    .compact_session_native(
                        preparation.clone(),
                        json!({"model": "native-model", "input": [{"role": "user", "content": [
                            {"type": "input_text", "text": "ACME-"},
                            {"type": "input_text", "text": "123456"}
                        ]}]}),
                    )
                    .await
                    .expect_err("independent native fragments must not dispatch"); // ubs:ignore[rust.ownership.unwrap-expect] -- Empty preparation must not hide a fragmented native request.
                let refusal = if mode == "block" {
                    "PI_SECRET_BLOCK"
                } else {
                    "PI_COMPACTION_FRAGMENTED"
                };
                assert!(error.to_string().contains(refusal)); // ubs:ignore[rust.panic.assert-macros] -- The named refusal identifies the actual privacy boundary.
                assert!(requests.lock().expect("requests").is_empty()); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Provider observation proves pre-dispatch refusal.

                let mut input = vec![json!({
                    "type": "compaction", "id": "old-window", "encrypted_content": "old-opaque"
                })];
                if mode == "obfuscate" {
                    input.push(json!({
                        "type": "function_call", "id": "call-item", "call_id": "call-kept",
                        "name": "configure", "arguments": json!({"api_key": discovered}).to_string()
                    }));
                }
                let result = actions
                    .compact_session_native(
                        preparation.clone(),
                        json!({"model": "native-model", "input": input}),
                    )
                    .await;
                assert_eq!(requests.lock().expect("requests").len(), 1); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Clean replay is admitted exactly once before output validation.
                if mode == "block" {
                    let error = result.expect_err("blocked output cannot become a replay window"); // ubs:ignore[rust.ownership.unwrap-expect] -- Returned plaintext from an old opaque window must be refused.
                    assert!(error.to_string().contains("PI_SECRET_BLOCK")); // ubs:ignore[rust.panic.assert-macros] -- No native result is returned after blocked output.
                } else {
                    let result = result.expect("screen native replay result"); // ubs:ignore[rust.ownership.unwrap-expect] -- Safe native output remains usable after redaction.
                    let visible = json!({
                        "summary": result.get("summary"),
                        "window": result.pointer("/details/compactedWindow")
                    })
                    .to_string();
                    for secret in [known, discovered, "ACME-123456"] {
                        assert!(!visible.contains(secret)); // ubs:ignore[rust.panic.assert-macros] -- Both human summary and replayable native text must be screened.
                    }
                    assert_eq!(
                        // ubs:ignore[rust.panic.assert-macros] -- Cut-point IDs remain exact even when their spelling matches a pattern.
                        result.get("firstKeptEntryId"),
                        Some(&json!("ACME-654321"))
                    );
                    assert_eq!(
                        // ubs:ignore[rust.panic.assert-macros] -- Native replay identifiers are immutable protocol metadata.
                        result.pointer("/details/compactedWindow/0/id"),
                        Some(&json!("window-kept"))
                    );
                    assert_eq!(
                        // ubs:ignore[rust.panic.assert-macros] -- The opaque replay payload must not be rewritten.
                        result.pointer("/details/compactedWindow/0/encrypted_content"),
                        Some(&json!("opaque-kept"))
                    );
                    assert_eq!(
                        // ubs:ignore[rust.panic.assert-macros] -- Request discoveries remain private to this compaction transaction.
                        session.agent.mask_secrets_text(discovered),
                        discovered
                    );
                }

                *response.lock().expect("response") = json!({"output": [{ // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture switches to a fragmented native response after the first request.
                    "type": "message", "role": "assistant", "id": "message-kept", "content": [
                        {"type": "output_text", "text": "ACME-"},
                        {"type": "output_text", "text": "123456"}
                    ]
                }]});
                let error = actions
                    .compact_session_native(
                        preparation,
                        json!({"model": "native-model", "input": [{"role": "user", "content": "clean"}]}),
                    )
                    .await
                    .expect_err("fragmented native output cannot be replayed"); // ubs:ignore[rust.ownership.unwrap-expect] -- An unsafe assembled summary/window must not escape output validation.
                assert!(error.to_string().contains(refusal)); // ubs:ignore[rust.panic.assert-macros] -- Output refusal matches the configured block or preserve-or-refuse mode.
                assert_eq!(requests.lock().expect("requests").len(), 2); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Response refusal occurs after exactly one additional admitted request.
            }
        });
    }

    /// gh #167: the native compact request sanitizer pins the model, demands
    /// an input array, rejects credential-shaped keys loudly, and reduces the
    /// object to the allowlist.
    #[test]
    fn native_compact_request_sanitizer_enforces_contract() {
        // Credential-shaped keys: hard error naming the key.
        for key in ["apiKey", "api_key", "Authorization", "headers", "token"] {
            let request = json!({
                "model": "m-1",
                "input": [],
                (*key): "secret",
            });
            let err = sanitize_native_compact_request(&request, "m-1")
                .expect_err("credential key must be rejected");
            assert!(
                err.to_string().contains("credential material"),
                "unexpected error for {key}: {err}"
            );
        }

        // Model must match the session's model.
        let err = sanitize_native_compact_request(&json!({ "model": "other", "input": [] }), "m-1")
            .expect_err("model mismatch must be rejected");
        assert!(err.to_string().contains("does not match the session model"));
        let err = sanitize_native_compact_request(&json!({ "input": [] }), "m-1")
            .expect_err("missing model must be rejected");
        assert!(err.to_string().contains("string `model`"));

        // Input array required.
        let err = sanitize_native_compact_request(&json!({ "model": "m-1" }), "m-1")
            .expect_err("missing input must be rejected");
        assert!(err.to_string().contains("`input` array"));

        // Non-object rejected.
        assert!(sanitize_native_compact_request(&json!("nope"), "m-1").is_err());

        // Allowlisted extras survive; unknown benign fields are dropped.
        let sanitized = sanitize_native_compact_request(
            &json!({
                "model": "m-1",
                "instructions": "compact",
                "input": [{ "type": "message" }],
                "tools": [],
                "parallel_tool_calls": true,
                "reasoning": { "effort": "low" },
                "service_tier": "default",
                "prompt_cache_key": "cache-1",
                "text": { "verbosity": "low" },
                "stream": true,
                "store": false,
                "extension_junk": 42,
            }),
            "m-1",
        )
        .expect("valid request sanitizes");
        let sanitized = sanitized.as_object().expect("object");
        assert_eq!(sanitized.get("model"), Some(&json!("m-1")));
        assert_eq!(sanitized.get("prompt_cache_key"), Some(&json!("cache-1")));
        assert!(sanitized.contains_key("reasoning"));
        assert!(!sanitized.contains_key("stream"), "stream must be dropped");
        assert!(!sanitized.contains_key("store"), "store must be dropped");
        assert!(!sanitized.contains_key("extension_junk"));
    }

    /// gh #167: native compact response shaping — `output` verbatim as
    /// `details.compactedWindow`, assistant `output_text` as the summary
    /// (checkpoint fallback otherwise), id/created_at echoed when present.
    #[test]
    fn native_compact_result_shaping_round_trips_window() {
        let response = json!({
            "id": "resp_9",
            "created_at": 1_723_000_000,
            "output": [
                { "type": "message", "role": "user",
                  "content": [{ "type": "input_text", "text": "opaque item" }] },
                { "type": "message", "role": "assistant", "status": "completed",
                  "content": [{ "type": "output_text", "text": "native summary" }] }
            ]
        });
        let shaped = shape_native_compact_result(
            &response,
            "entry-7",
            120_000,
            vec!["src/lib.rs".to_string()],
            vec!["src/agent.rs".to_string()],
        )
        .expect("well-formed response shapes");
        assert_eq!(shaped["summary"], json!("native summary"));
        assert_eq!(shaped["firstKeptEntryId"], json!("entry-7"));
        assert_eq!(shaped["tokensBefore"], json!(120_000));
        assert_eq!(
            shaped["details"]["strategy"],
            json!("openai-responses-native")
        );
        assert_eq!(
            shaped["details"]["compactedWindow"], response["output"],
            "window must be echoed verbatim"
        );
        assert_eq!(shaped["details"]["compactResponseId"], json!("resp_9"));
        assert_eq!(shaped["details"]["createdAt"], json!(1_723_000_000));
        assert_eq!(shaped["details"]["readFiles"], json!(["src/lib.rs"]));
        assert_eq!(shaped["details"]["modifiedFiles"], json!(["src/agent.rs"]));

        // No assistant output_text -> checkpoint fallback summary.
        let opaque = json!({
            "output": [
                { "type": "message", "role": "user",
                  "content": [{ "type": "input_text", "text": "window 1" }] }
            ]
        });
        let shaped = shape_native_compact_result(&opaque, "entry-1", 10, vec![], vec![])
            .expect("opaque response shapes");
        assert_eq!(shaped["summary"], json!(NATIVE_COMPACT_FALLBACK_SUMMARY));
        assert!(shaped["details"].get("compactResponseId").is_none());

        // Missing output -> Err (plugin fails open).
        let err = shape_native_compact_result(&json!({ "id": "x" }), "entry-1", 10, vec![], vec![])
            .expect_err("missing output must error");
        assert!(err.to_string().contains("`output` array"));
    }

    /// gh #167: the host action refuses native compaction when the session's
    /// provider is not on the openai-responses API, before any sanitization
    /// or network activity.
    #[test]
    fn agent_host_actions_compact_session_native_requires_responses_api() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let calls = Arc::new(StdMutex::new(Vec::new()));
            let provider = Arc::new(PiAiCaptureProvider {
                calls: Arc::clone(&calls),
            });
            let actions = AgentSessionHostActions {
                session,
                injected: Arc::new(StdMutex::new(ExtensionInjectedQueue::default())),
                is_streaming: Arc::new(AtomicBool::new(false)),
                is_turn_active: Arc::new(AtomicBool::new(false)),
                pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
                ai_completion: Arc::new(StdMutex::new(ExtensionAiCompletionHostState {
                    provider,
                    stream_options: StreamOptions::default(),
                    models: Vec::new(),
                    compaction_models: ExtensionCompactionModelState::default(),
                    compaction_privacy: Arc::new(StdMutex::new(
                        compaction::CompactionPrivacy::default(),
                    )),
                })),
                provider_admission: ProviderAdmissionGate::default(),
                session_action_admission: SessionActionAdmissionGate::default(),
            };

            let preparation = json!({
                "firstKeptEntryId": "entry-9",
                "messagesToSummarize": [],
                "turnPrefixMessages": [],
                "isSplitTurn": false,
                "tokensBefore": 4200,
                "previousSummary": null,
                "fileOps": { "read": [], "written": [], "edited": [] },
                "settings": {
                    "enabled": true,
                    "contextWindowTokens": 128_000,
                    "reserveTokens": 10_240,
                    "keepRecentTokens": 12_800
                }
            });
            let request = json!({ "model": "capture-model", "input": [] });

            let err = actions
                .compact_session_native(preparation.clone(), request)
                .await
                .expect_err("non-responses API must be refused");
            assert!(
                err.to_string().contains("openai-responses"),
                "unexpected error: {err}"
            );

            // Malformed preparation is still rejected first.
            let err = actions
                .compact_session_native(json!({ "firstKeptEntryId": "" }), json!({}))
                .await
                .expect_err("malformed preparation must be rejected");
            assert!(
                err.to_string()
                    .contains("`firstKeptEntryId` must be a non-empty string"),
                "unexpected error: {err}"
            );

            // The capture provider was never streamed/POSTed.
            let captured_len = match calls.lock() {
                Ok(guard) => guard.len(),
                Err(poisoned) => poisoned.into_inner().len(),
            };
            assert_eq!(captured_len, 0, "no provider call may happen on refusal");
        });
    }

    #[test]
    fn extension_command_send_message_trigger_turn_runs_agent_turn_when_idle() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerCommand("emit-now", {
                    description: "emit a custom message and trigger a turn",
                    handler: async () => {
                      await pi.events("sendMessage", {
                        message: {
                          customType: "note",
                          content: "turn-now",
                          display: true
                        },
                        options: {
                          deliverAs: "steer",
                          triggerTurn: true
                        }
                      });
                      return "queued";
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(IdleCommandProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let value = agent_session
                .execute_extension_command("emit-now", "", 5_000, |_| {})
                .await
                .expect("execute extension command");
            assert_eq!(value.as_str(), Some("queued"));

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session.lock(cx.cx()).await.expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Custom(CustomMessage { custom_type, content, .. })
                            if custom_type == "note" && content == "turn-now"
                    )
                }),
                "expected custom message prompt in session, got {messages:?}"
            );
            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Assistant(assistant)
                            if assistant.content.iter().any(|block| matches!(
                                block,
                                ContentBlock::Text(TextContent { text, .. })
                                    if text.as_str().eq("resumed-response-0")
                            ))
                    )
                }),
                "expected assistant response after triggered turn, got {messages:?}"
            );
        });
    }

    fn extension_session_model_state() -> Arc<StdMutex<ExtensionAiCompletionHostState>> {
        Arc::new(StdMutex::new(ExtensionAiCompletionHostState {
            provider: Arc::new(NoopProvider),
            stream_options: StreamOptions::default(),
            models: Vec::new(),
            compaction_models: ExtensionCompactionModelState::default(),
            compaction_privacy: Arc::new(StdMutex::new(compaction::CompactionPrivacy::default())),
        }))
    }

    #[test]
    fn agent_extension_session_get_state_reports_agent_runtime_state() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let mut session = Session::in_memory();
            session.set_model_header(
                Some("test-provider".to_string()),
                Some("test-model".to_string()),
                Some("high".to_string()),
            );
            session.append_message(crate::session::SessionMessage::User {
                content: UserContent::Text("hello".to_string()),
                timestamp: Some(1),
            });
            let session = Arc::new(Mutex::new(session));

            let extension_session = AgentExtensionSession {
                handle: SessionHandle(Arc::clone(&session)),
                ai_completion: extension_session_model_state(),
                session_action_admission: SessionActionAdmissionGate::default(),
                is_streaming: Arc::new(AtomicBool::new(true)),
                is_compacting: Arc::new(AtomicBool::new(true)),
                queue_modes: Arc::new(StdMutex::new(ExtensionQueueModeState::new(
                    QueueMode::All,
                    QueueMode::OneAtATime,
                ))),
                auto_compaction_enabled: true,
            };

            let state = <AgentExtensionSession as crate::extensions::ExtensionSession>::get_state(
                &extension_session,
            )
            .await;

            assert_eq!(state["model"]["provider"], "test-provider");
            assert_eq!(state["model"]["id"], "test-model");
            assert_eq!(state["thinkingLevel"], "high");
            assert_eq!(state["isStreaming"], true);
            assert_eq!(state["isCompacting"], true);
            assert_eq!(state["steeringMode"], "all");
            assert_eq!(state["followUpMode"], "one-at-a-time");
            assert_eq!(state["autoCompactionEnabled"], true);
            assert_eq!(state["messageCount"], 1);
        });
    }

    #[test]
    fn agent_extension_session_get_state_uses_branch_local_model_and_thinking() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let mut session = Session::in_memory();
            let root_id = session.append_message(crate::session::SessionMessage::User {
                content: UserContent::Text("root".to_string()),
                timestamp: Some(1),
            });
            session.append_model_change("openai".to_string(), "gpt-4o".to_string());
            let branch_a_thinking = session.append_thinking_level_change("low".to_string());
            session.set_model_header(
                Some("openai".to_string()),
                Some("gpt-4o".to_string()),
                Some("low".to_string()),
            );

            assert!(session.create_branch_from(&root_id));
            session.append_model_change("anthropic".to_string(), "claude-sonnet-4-5".to_string());
            session.append_thinking_level_change("high".to_string());
            session.set_model_header(
                Some("anthropic".to_string()),
                Some("claude-sonnet-4-5".to_string()),
                Some("high".to_string()),
            );

            assert!(session.navigate_to(&branch_a_thinking));
            let session = Arc::new(Mutex::new(session));

            let extension_session = AgentExtensionSession {
                handle: SessionHandle(Arc::clone(&session)),
                ai_completion: extension_session_model_state(),
                session_action_admission: SessionActionAdmissionGate::default(),
                is_streaming: Arc::new(AtomicBool::new(false)),
                is_compacting: Arc::new(AtomicBool::new(false)),
                queue_modes: Arc::new(StdMutex::new(ExtensionQueueModeState::new(
                    QueueMode::OneAtATime,
                    QueueMode::OneAtATime,
                ))),
                auto_compaction_enabled: false,
            };

            let state = <AgentExtensionSession as crate::extensions::ExtensionSession>::get_state(
                &extension_session,
            )
            .await;

            assert_eq!(state["model"]["provider"], "openai");
            assert_eq!(state["model"]["id"], "gpt-4o");
            assert_eq!(state["thinkingLevel"], "low");
        });
    }

    #[test]
    fn agent_extension_session_rejects_mutation_delayed_across_session_transition() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let session_action_admission = SessionActionAdmissionGate::default();
            let extension_session = AgentExtensionSession {
                handle: SessionHandle(Arc::clone(&session)),
                ai_completion: extension_session_model_state(),
                session_action_admission: session_action_admission.clone(),
                is_streaming: Arc::new(AtomicBool::new(false)),
                is_compacting: Arc::new(AtomicBool::new(false)),
                queue_modes: Arc::new(StdMutex::new(ExtensionQueueModeState::new(
                    QueueMode::OneAtATime,
                    QueueMode::OneAtATime,
                ))),
                auto_compaction_enabled: false,
            };
            let transition_cx = crate::agent_cx::AgentCx::for_request();
            let transition_permit = session_action_admission
                .acquire(transition_cx.cx())
                .await
                .expect("transition admission");
            let source_origin = session_action_admission.capture_origin();

            let delayed_session = extension_session.clone();
            let hostcall = runtime_handle.spawn(async move {
                <AgentExtensionSession as crate::extensions::ExtensionSession>::append_custom_entry(
                    &delayed_session,
                    "stale-note".to_string(),
                    Some(json!({"owner": "source"})),
                    Some(source_origin),
                )
                .await
            });
            wait_for_session_action_generation_capture(&session_action_admission).await;

            let replacement_session_id = {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut guard = session.lock(cx.cx()).await.expect("session lock");
                *guard = Session::in_memory();
                guard.header.id.clone()
            };
            session_action_admission.advance_generation();
            drop(transition_permit);

            let err = hostcall
                .await
                .expect_err("mutation from the previous Session generation must be rejected");
            assert!(
                err.to_string().contains("active Session changed"),
                "unexpected delayed-mutation error: {err}"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = session
                .lock(cx.cx())
                .await
                .expect("replacement session lock");
            assert_eq!(guard.header.id, replacement_session_id);
            assert!(
                guard.entries_for_current_path().iter().all(|entry| {
                    !matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "stale-note"
                    )
                }),
                "stale extension mutation crossed into the replacement Session"
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn extension_command_rejects_real_js_timer_promise_mutation_delayed_across_transition() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("delayed-session-mutation.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerCommand("append-late", {
                    description: "append a custom Session entry",
                    handler: async () => {
                      await new Promise((resolve, reject) => {
                        setTimeout(() => {
                          Promise.resolve()
                            .then(() => pi.session("appendEntry", {
                              customType: "stale-js-note",
                              data: { owner: "source" }
                            }))
                            .then(resolve, reject);
                        }, 0);
                      });
                      return "appended";
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&[], temp_dir.path(), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );
            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable transition extension");

            let session_action_admission = agent_session.session_action_admission_gate();
            let extension_manager = agent_session
                .extensions
                .as_ref()
                .expect("extension region")
                .manager()
                .clone();
            let transition_cx = crate::agent_cx::AgentCx::for_request();
            let transition_permit = session_action_admission
                .acquire(transition_cx.cx())
                .await
                .expect("transition admission");

            let delayed_manager = extension_manager.clone();
            let hostcall = runtime_handle.spawn(async move {
                delayed_manager
                    .execute_command("append-late", "", 15_000)
                    .await
            });
            wait_for_session_action_generation_capture(&session_action_admission).await;

            let replacement_session_id = {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut guard = session.lock(cx.cx()).await.expect("session lock");
                *guard = Session::in_memory();
                guard.header.id.clone()
            };
            session_action_admission.advance_generation();
            drop(transition_permit);

            let err = hostcall
                .await
                .expect_err("stale real-JS timer/Promise Session mutation must be rejected");
            assert!(
                err.to_string().contains("active Session changed"),
                "unexpected real-JS timer/Promise mutation error: {err}"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = session
                .lock(cx.cx())
                .await
                .expect("replacement session lock");
            assert_eq!(guard.header.id, replacement_session_id);
            assert!(
                guard.entries_for_current_path().iter().all(|entry| {
                    !matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "stale-js-note"
                    )
                }),
                "stale real-JS mutation crossed into the replacement Session"
            );
            drop(guard);

            let current_result = extension_manager
                .execute_command("append-late", "", 5_000)
                .await
                .expect("current-generation real-JS mutation");
            assert_eq!(current_result, Value::String("appended".to_string()));

            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = session
                .lock(cx.cx())
                .await
                .expect("replacement session lock after current mutation");
            assert_eq!(guard.header.id, replacement_session_id);
            assert_eq!(
                guard
                    .entries_for_current_path()
                    .iter()
                    .filter(|entry| matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "stale-js-note"
                    ))
                    .count(),
                1,
                "the provenance fence must admit the current generation exactly once"
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn extension_command_deadline_cancels_blocked_session_hostcalls_and_reuses_shard() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let runtime_handle = runtime.handle();

        runtime.block_on(async move {
            for amac_enabled in [false, true] {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                let entry_path = temp_dir.path().join("deadline-session-mutation.mjs");
                std::fs::write(
                    &entry_path,
                    r#"
                    export default function init(pi) {
                      const state = {
                        resolved: 0,
                        rejected: 0,
                        finallyCount: 0,
                        code: null
                      };

                      pi.registerCommand("deadline-held", {
                        description: "attempt two Session mutations under one deadline",
                        handler: async () => {
                          try {
                            await Promise.all([
                              pi.session("appendEntry", {
                                customType: "deadline-note",
                                data: { slot: 1 }
                              }),
                              pi.session("appendEntry", {
                                customType: "deadline-note",
                                data: { slot: 2 }
                              })
                            ]);
                            state.resolved += 1;
                          } catch (error) {
                            state.rejected += 1;
                            state.code = error && error.code ? error.code : null;
                            throw error;
                          } finally {
                            state.finallyCount += 1;
                          }
                        }
                      });

                      pi.registerCommand("deadline-probe", {
                        description: "report deadline command settlement counts",
                        handler: async () => ({ ...state })
                      });

                      pi.registerCommand("reuse-probe", {
                        description: "prove the same shard remains usable",
                        handler: async () => "reused"
                      });
                    }
                    "#,
                )
                .expect("write extension entry");

                let provider = Arc::new(NoopProvider);
                let tools = ToolRegistry::new(&[], temp_dir.path(), None);
                let agent = Agent::new(provider, tools, AgentConfig::default());
                let session = Arc::new(Mutex::new(Session::in_memory()));
                let mut agent_session = AgentSession::new(
                    agent,
                    Arc::clone(&session),
                    false,
                    ResolvedCompactionSettings::default(),
                );
                agent_session
                    .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                    .await
                    .expect("enable deadline extension");

                let session_action_admission = agent_session.session_action_admission_gate();
                let extension_manager = agent_session
                    .extensions
                    .as_ref()
                    .expect("extension region")
                    .manager()
                    .clone();
                let js_runtime = extension_manager.js_runtime().expect("QuickJS runtime");
                js_runtime
                    .set_hostcall_amac_enabled_for_tests(amac_enabled)
                    .await
                    .expect("set hostcall AMAC mode");

                let transition_cx = crate::agent_cx::AgentCx::for_request();
                let transition_permit = session_action_admission
                    .acquire(transition_cx.cx())
                    .await
                    .expect("hold Session action admission");

                let delayed_manager = extension_manager.clone();
                let hostcall = runtime_handle.spawn(async move {
                    delayed_manager
                        .execute_command("deadline-held", "", 500)
                        .await
                });
                wait_for_session_action_generation_capture(&session_action_admission).await;

                let err = asupersync::time::timeout(
                    asupersync::time::wall_now(),
                    Duration::from_secs(5),
                    Box::pin(hostcall),
                )
                .await
                .expect("blocked Session hostcall deadline watchdog expired")
                .expect_err("blocked Session hostcalls must obey the root command deadline");
                let error_message = err.to_string();
                assert!(
                    error_message.contains("timeout") || error_message.contains("timed out"),
                    "unexpected deadline error with AMAC {amac_enabled}: {err}"
                );
                wait_for_session_action_generation_release(&session_action_admission).await;
                drop(transition_permit);

                let expected_state = json!({
                    "resolved": 0,
                    "rejected": 1,
                    "finallyCount": 1,
                    "code": "timeout"
                });
                let first_probe = extension_manager
                    .execute_command("deadline-probe", "", 5_000)
                    .await
                    .expect("first settlement probe");
                assert_eq!(
                    first_probe, expected_state,
                    "deadline Promise must settle exactly once with AMAC {amac_enabled}"
                );
                let second_probe = extension_manager
                    .execute_command("deadline-probe", "", 5_000)
                    .await
                    .expect("second settlement probe");
                assert_eq!(
                    second_probe, expected_state,
                    "late work changed settlement state with AMAC {amac_enabled}"
                );

                let cx = crate::agent_cx::AgentCx::for_request();
                let guard = session.lock(cx.cx()).await.expect("session lock");
                assert_eq!(
                    guard
                        .entries_for_current_path()
                        .iter()
                        .filter(|entry| matches!(
                            entry,
                            crate::session::SessionEntry::Custom(custom)
                                if custom.custom_type == "deadline-note"
                        ))
                        .count(),
                    0,
                    "cancelled hostcalls mutated the Session with AMAC {amac_enabled}"
                );
                drop(guard);

                let reuse = extension_manager
                    .execute_command("reuse-probe", "", 5_000)
                    .await
                    .expect("reuse command on the same shard");
                assert_eq!(reuse, Value::String("reused".to_string()));
                assert!(
                    js_runtime.shutdown(Duration::from_secs(1)).await,
                    "deadline test runtime did not shut down with AMAC {amac_enabled}"
                );
            }
        });
    }

    #[test]
    fn agent_session_set_queue_modes_updates_extension_delivery_state() {
        let provider = Arc::new(NoopProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let queue_modes = Arc::new(StdMutex::new(ExtensionQueueModeState::new(
            QueueMode::OneAtATime,
            QueueMode::OneAtATime,
        )));
        let injected_queue = Arc::new(StdMutex::new(ExtensionInjectedQueue::new(
            QueueMode::OneAtATime,
            QueueMode::OneAtATime,
        )));
        agent_session.extension_queue_modes = Some(Arc::clone(&queue_modes));
        agent_session.extension_injected_queue = Some(Arc::clone(&injected_queue));

        agent_session.set_queue_modes(QueueMode::All, QueueMode::All);

        assert_eq!(
            agent_session.agent.queue_modes(),
            (QueueMode::All, QueueMode::All)
        );
        let mirrored = queue_modes.lock().expect("lock queue mode mirror");
        assert_eq!(mirrored.steering_mode, QueueMode::All);
        assert_eq!(mirrored.follow_up_mode, QueueMode::All);
        drop(mirrored);

        let queued_follow_up_len = {
            let mut queue = injected_queue.lock().expect("lock injected queue");
            queue.push_follow_up(Message::User(UserMessage {
                content: UserContent::Text("first".to_string()),
                timestamp: 0,
            }));
            queue.push_follow_up(Message::User(UserMessage {
                content: UserContent::Text("second".to_string()),
                timestamp: 0,
            }));
            queue.pop_follow_up().len()
        };
        assert_eq!(
            queued_follow_up_len, 2,
            "updated queue modes should apply to extension-injected follow-ups"
        );
    }

    #[test]
    fn extension_command_send_user_message_runs_agent_turn_when_idle() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerCommand("inject-user", {
                    description: "inject a user message",
                    handler: async () => {
                      await pi.events("sendUserMessage", {
                        text: "Please review the changes"
                      });
                      return "queued";
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(IdleCommandProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let value = agent_session
                .execute_extension_command("inject-user", "", 5_000, |_| {})
                .await
                .expect("execute extension command");
            assert_eq!(value.as_str(), Some("queued"));

            let cx = crate::agent_cx::AgentCx::for_request();
            let session_guard = session.lock(cx.cx()).await.expect("lock session");
            let messages = session_guard.to_messages_for_current_path();

            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::User(UserMessage {
                            content: UserContent::Text(text),
                            ..
                        }) if text == "Please review the changes"
                    )
                }),
                "expected injected user message in session, got {messages:?}"
            );
            assert!(
                messages.iter().any(|msg| {
                    matches!(
                        msg,
                        Message::Assistant(assistant)
                            if assistant.content.iter().any(|block| matches!(
                                block,
                                ContentBlock::Text(TextContent { text, .. })
                                    if text.as_str().eq("resumed-response-0")
                            ))
                    )
                }),
                "expected assistant response after injected user turn, got {messages:?}"
            );
        });
    }

    #[test]
    fn send_user_message_steer_skips_remaining_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  let sent = false;
                  pi.on("tool_call", async (event) => {
                    if (sent) return {};
                    if (Object.is(event && event.toolName, "count_tool")) {
                      sent = true;
                      await pi.events("sendUserMessage", {
                        text: "steer-now",
                        options: { deliverAs: "steer" }
                      });
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(ToolUseProvider::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let _ = agent_session
                .run_text("go".to_string(), |_| {})
                .await
                .expect("run_text");

            // A steer message should short-circuit remaining tool dispatch.
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    /// bd-cv653.3.15: a Length-truncated reply mid-code-block earns one
    /// auto-continue nudge and the next call completes the turn; the nudge
    /// is recorded in history as a user message carrying the marker.
    struct TruncatingProvider {
        stream_calls: AtomicUsize,
        truncated_replies: usize,
    }

    impl TruncatingProvider {
        const fn new(truncated_replies: usize) -> Self {
            Self {
                stream_calls: AtomicUsize::new(0),
                truncated_replies,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for TruncatingProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);
            let (reason, text) = if call_index < self.truncated_replies {
                (StopReason::Length, "```rust\nfn main() {")
            } else {
                (StopReason::Stop, "}\n```\nAll done.")
            };
            let message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: reason,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            let partial = AssistantMessage {
                content: Vec::new(),
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            let events = vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done { reason, message }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn turn_recovery_auto_continues_budget_truncation() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let provider = Arc::new(TruncatingProvider::new(1));
            let tools = ToolRegistry::from_tools(vec![]);
            let provider_dyn: Arc<dyn Provider> = provider.clone();
            let mut agent = Agent::new(provider_dyn, tools, AgentConfig::default());

            let final_message = agent.run("write main", |_| {}).await.expect("run");
            assert_eq!(final_message.stop_reason, StopReason::Stop);
            assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 2);

            let nudges = agent
                .messages()
                .iter()
                .filter(|message| {
                    matches!(message, Message::User(user)
                        if matches!(&user.content, crate::model::UserContent::Text(text)
                            if text.contains("auto-continue")))
                })
                .count();
            assert_eq!(nudges, 1, "exactly one nudge recorded");
        });
    }

    /// bd-cv653.3.15: the cap allows two auto-continuations, then the run
    /// ends with the truncated message instead of looping forever.
    #[test]
    fn turn_recovery_cap_stops_after_two_continuations() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let provider = Arc::new(TruncatingProvider::new(10));
            let tools = ToolRegistry::from_tools(vec![]);
            let provider_dyn: Arc<dyn Provider> = provider.clone();
            let mut agent = Agent::new(provider_dyn, tools, AgentConfig::default());

            let final_message = agent.run("write main", |_| {}).await.expect("run");
            assert_eq!(final_message.stop_reason, StopReason::Length);
            assert_eq!(
                provider.stream_calls.load(Ordering::SeqCst),
                3,
                "initial call + two capped continuations"
            );
        });
    }

    /// bd-cv653.3.15: recovery off means a truncated stop ends the run
    /// untouched.
    #[test]
    fn turn_recovery_off_leaves_truncation_alone() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let provider = Arc::new(TruncatingProvider::new(10));
            let tools = ToolRegistry::from_tools(vec![]);
            let config = AgentConfig {
                turn_recovery: crate::turn_recovery::TurnRecoveryMode::Off,
                ..Default::default()
            };
            let provider_dyn: Arc<dyn Provider> = provider.clone();
            let mut agent = Agent::new(provider_dyn, tools, config);

            let final_message = agent.run("write main", |_| {}).await.expect("run");
            assert_eq!(final_message.stop_reason, StopReason::Length);
            assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn manual_follow_up_dispatch_keeps_staged_work_without_an_extra_provider_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let provider = Arc::new(TruncatingProvider::new(0));
            let provider_dyn: Arc<dyn Provider> = provider.clone();
            let mut agent = Agent::new(
                provider_dyn,
                ToolRegistry::from_tools(vec![]),
                AgentConfig::default(),
            );
            agent.set_automatic_queue_dispatch(true, false);
            agent.queue_follow_up(Message::User(UserMessage {
                content: UserContent::Text("manual follow-up".to_string()),
                timestamp: 0,
            }));

            let final_message = agent.run("initial", |_| {}).await.expect("run");
            assert_eq!(final_message.stop_reason, StopReason::Stop);
            assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
            assert_eq!(agent.queued_message_count(), 1);

            let batch = agent.take_queued_follow_up_batch().await;
            assert_eq!(batch.len(), 1);
            assert_eq!(agent.queued_message_count(), 0);
        });
    }

    #[test]
    fn manual_follow_up_dispatch_suppresses_turn_recovery_continuations() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let provider = Arc::new(TruncatingProvider::new(10));
            let provider_dyn: Arc<dyn Provider> = provider.clone();
            let mut agent = Agent::new(
                provider_dyn,
                ToolRegistry::from_tools(vec![]),
                AgentConfig::default(),
            );
            agent.set_automatic_queue_dispatch(true, false);

            let final_message = agent.run("write main", |_| {}).await.expect("run");
            assert_eq!(final_message.stop_reason, StopReason::Length);
            assert_eq!(
                provider.stream_calls.load(Ordering::SeqCst),
                1,
                "manual follow-up control must suppress synthetic recovery requests"
            );
            assert!(!agent.messages().iter().any(|message| {
                matches!(message, Message::User(user)
                    if matches!(&user.content, UserContent::Text(text)
                        if text.contains("auto-continue")))
            }));
        });
    }

    #[test]
    fn manual_steering_dispatch_never_polls_external_fetchers() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let provider = Arc::new(TruncatingProvider::new(0));
            let provider_dyn: Arc<dyn Provider> = provider.clone();
            // The fixture's final reply ("}\n```\nAll done.") reads as an
            // unclosed fence, so conservative turn recovery would add a
            // continuation call. Only steering is manual here (follow-up
            // dispatch, which suppresses recovery, stays automatic), so turn
            // recovery is switched off to isolate steering dispatch.
            let mut agent = Agent::new(
                provider_dyn,
                ToolRegistry::from_tools(vec![]),
                AgentConfig {
                    turn_recovery: crate::turn_recovery::TurnRecoveryMode::Off,
                    ..AgentConfig::default()
                },
            );
            let polls = Arc::new(AtomicUsize::new(0));
            let polls_for_fetcher = Arc::clone(&polls);
            agent.register_message_fetchers(
                Some(Arc::new(move || {
                    let polls = Arc::clone(&polls_for_fetcher);
                    Box::pin(async move {
                        polls.fetch_add(1, Ordering::SeqCst);
                        vec![QueuedAgentMessage::generated(Message::User(UserMessage {
                            content: UserContent::Text("should not be fetched".to_string()),
                            timestamp: 0,
                        }))]
                    })
                })),
                None,
            );
            agent.set_automatic_queue_dispatch(false, true);

            let final_message = agent.run("initial", |_| {}).await.expect("run");
            assert_eq!(final_message.stop_reason, StopReason::Stop);
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn explicit_manual_batch_take_respects_queue_modes_without_enabling_dispatch() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let provider: Arc<dyn Provider> = Arc::new(TruncatingProvider::new(0));
            let mut agent = Agent::new(
                provider,
                ToolRegistry::from_tools(vec![]),
                AgentConfig::default(),
            );
            agent.set_queue_modes(QueueMode::All, QueueMode::All);
            agent.set_automatic_queue_dispatch(false, false);
            assert_eq!(agent.automatic_queue_dispatch(), (false, false));

            for text in ["s1", "s2"] {
                agent.queue_steering(Message::User(UserMessage {
                    content: UserContent::Text(text.to_string()),
                    timestamp: 0,
                }));
            }
            for text in ["f1", "f2"] {
                agent.queue_follow_up(Message::User(UserMessage {
                    content: UserContent::Text(text.to_string()),
                    timestamp: 0,
                }));
            }
            assert_eq!(agent.queued_message_count(), 4);
            assert_eq!(agent.take_queued_steering_batch().len(), 2);
            assert_eq!(agent.take_queued_follow_up_batch().await.len(), 2);
            assert_eq!(agent.queued_message_count(), 0);
            assert_eq!(agent.automatic_queue_dispatch(), (false, false));
        });
    }

    #[test]
    fn send_user_message_follow_up_does_not_skip_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  let sent = false;
                  pi.on("tool_call", async (event) => {
                    if (sent) return {};
                    if (Object.is(event && event.toolName, "count_tool")) {
                      sent = true;
                      await pi.events("sendUserMessage", {
                        text: "follow-up",
                        options: { deliverAs: "followUp" }
                      });
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(ToolUseProvider::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let _ = agent_session
                .run_text("go".to_string(), |_| {})
                .await
                .expect("run_text");

            assert_eq!(calls.load(Ordering::SeqCst), 2);
        });
    }

    fn test_turn_latency() -> SharedTurnLatencyAccumulator {
        Arc::new(StdMutex::new(TurnLatencyAccumulator::started()))
    }

    #[test]
    fn latency_breakdown_reports_component_tail_percentiles() {
        let breakdown =
            TurnLatencyBreakdown::from_component_samples(250, &[10, 30, 20], &[40, 5], &[2], &[]);

        assert_eq!(breakdown.schema, TURN_LATENCY_BREAKDOWN_SCHEMA_V1);
        assert_eq!(breakdown.provider_streaming.duration_ms, 60);
        assert_eq!(breakdown.provider_streaming.samples, 3);
        assert_eq!(breakdown.provider_streaming.tail_percentiles.p50_ms, 20);
        assert_eq!(breakdown.provider_streaming.tail_percentiles.p95_ms, 30);
        assert_eq!(breakdown.provider_streaming.tail_percentiles.p99_ms, 30);
        assert_eq!(breakdown.provider_streaming.tail_percentiles.p999_ms, 30);
        assert_eq!(breakdown.local_tools.duration_ms, 45);
        assert_eq!(breakdown.extension_hostcalls.duration_ms, 2);
        assert_eq!(breakdown.persistence.duration_ms, 0);
        assert_eq!(breakdown.dominant_component, "provider_streaming");
    }

    #[test]
    fn latency_breakdown_serializes_without_provider_secrets() {
        let breakdown =
            TurnLatencyBreakdown::from_component_samples(125, &[100], &[20], &[5], &[0]);
        let serialized = serde_json::to_string(&breakdown).expect("serialize latency breakdown");

        assert!(serialized.contains(TURN_LATENCY_BREAKDOWN_SCHEMA_V1));
        assert!(serialized.contains("providerStreaming"));
        assert!(serialized.contains("localTools"));
        assert!(serialized.contains("extensionHostcalls"));
        assert!(serialized.contains("persistence"));
        assert!(!serialized.contains("api_key"));
        assert!(!serialized.contains("authorization"));
        assert!(!serialized.contains("bearer"));
        assert!(!serialized.contains("sk-"));
    }

    #[test]
    fn tool_call_hook_can_block_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (event) => {
                    if (Object.is(event && event.toolName, "count_tool")) {
                      return { block: true, reason: "blocked in test" };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            assert_eq!(output.details, None);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "Tool execution blocked: blocked in test");
            }
        });
    }

    #[test]
    fn tool_call_hook_errors_fail_open() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (_event) => {
                    throw new Error("boom");
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn tool_call_hook_errors_fail_closed_when_configured() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (_event) => {
                    throw new Error("boom");
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(
                provider,
                tools,
                AgentConfig {
                    fail_closed_hooks: true,
                    ..AgentConfig::default()
                },
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            let [ContentBlock::Text(text)] = output.content.as_slice() else {
                return;
            };
            assert_eq!(text.text, "Tool execution blocked: extension hook failed");
        });
    }

    #[test]
    fn tool_call_hook_absent_allows_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r"
                export default function init(_pi) {}
                ",
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn tool_approval_allow_executes_tool() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let approval_calls = Arc::new(AtomicUsize::new(0));
            let approval_counter = Arc::clone(&approval_calls);
            let agent = Agent::new(
                provider,
                tools,
                AgentConfig {
                    tool_approval: Some(Arc::new(move |request| {
                        assert_eq!(request.tool_call_id, "call-1");
                        assert_eq!(request.tool_name, "count_tool");
                        approval_counter.fetch_add(1, Ordering::SeqCst);
                        Box::pin(async { ToolApprovalDecision::Allow })
                    })),
                    ..AgentConfig::default()
                },
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let events = Arc::new(std::sync::Mutex::new(Vec::new()));
            let events_for_handler = Arc::clone(&events);
            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |event| {
                if let Ok(mut guard) = events_for_handler.lock() {
                    guard.push(event);
                }
            });
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(approval_calls.load(Ordering::SeqCst), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let saw_approval_update = events.lock().is_ok_and(|guard| {
                guard.iter().any(|event| {
                    matches!(
                        event,
                        AgentEvent::ToolExecutionUpdate {
                            partial_result,
                            ..
                        } if partial_result.details.as_ref().is_some_and(|details| {
                            details["schema"] == TOOL_APPROVAL_STATUS_SCHEMA_V1
                                && details["status"] == "approved"
                        })
                    )
                })
            });
            assert!(saw_approval_update);
        });
    }

    #[test]
    fn tool_approval_deny_blocks_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(
                provider,
                tools,
                AgentConfig {
                    tool_approval: Some(Arc::new(|request| {
                        assert_eq!(request.tool_name, "count_tool");
                        Box::pin(async { ToolApprovalDecision::deny("denied by approval test") })
                    })),
                    ..AgentConfig::default()
                },
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                output.details.as_ref().unwrap()["schema"],
                TOOL_APPROVAL_DENIED_SCHEMA_V1
            );
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(text)] if text
                    .text
                    .contains("denied by approval test"))
            );
        });
    }

    /// Issue #196: with graduated gating configured (`approval_state`
    /// present, the shipped CLI shape), calls the mode gates must consult
    /// the installed `tool_approval` prompt handler rather than denying.
    #[test]
    fn ask_mode_with_gating_state_consults_tool_approval_handler() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let approval_calls = Arc::new(AtomicUsize::new(0));
            let approval_counter = Arc::clone(&approval_calls);
            let agent = Agent::new(
                provider,
                tools,
                AgentConfig {
                    approval_state: Some(crate::approval::ApprovalState::new(
                        crate::approval::ApprovalMode::AlwaysAsk,
                        false,
                        Vec::new(),
                    )),
                    tool_approval: Some(Arc::new(move |request| {
                        assert_eq!(request.tool_name, "count_tool");
                        approval_counter.fetch_add(1, Ordering::SeqCst);
                        Box::pin(async { ToolApprovalDecision::Allow })
                    })),
                    ..AgentConfig::default()
                },
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(!is_error, "an approved gated call must execute");
            assert!(!output.is_error);
            assert_eq!(approval_calls.load(Ordering::SeqCst), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    /// Issue #196 regression pin: gating with NO prompt handler still fails
    /// closed, and the denial carries an explicit reason (the historical bug
    /// was that every interactive surface ran in this shape, so users saw
    /// silent denials with no prompt).
    #[test]
    fn ask_mode_without_handler_denies_with_explicit_reason() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(
                provider,
                tools,
                AgentConfig {
                    approval_state: Some(crate::approval::ApprovalState::new(
                        crate::approval::ApprovalMode::AlwaysAsk,
                        false,
                        Vec::new(),
                    )),
                    tool_approval: None,
                    ..AgentConfig::default()
                },
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(text)] if text
                    .text
                    .contains("Approval required"))
            );
        });
    }

    #[test]
    fn tool_call_hook_returns_empty_allows_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (_event) => ({}));
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn tool_call_hook_can_block_bash_tool_execution() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (event) => {
                    const name = event && event.toolName ? String(event.toolName) : "";
                    if (name === "bash") return { block: true, reason: "blocked bash in test" };
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::new(&["bash"], temp_dir.path(), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&["bash"], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: json!({ "command": "printf 'hi' > blocked.txt" }),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(output.details, None);
            assert!(
                !temp_dir.path().join("blocked.txt").exists(),
                "expected bash command not to run when blocked"
            );
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "Tool execution blocked: blocked bash in test");
            }
        });
    }

    #[test]
    fn tool_result_hook_can_modify_tool_output() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_result", async (event) => {
                    if (Object.is(event && event.toolName, "count_tool")) {
                      return {
                        content: [{ type: "text", text: "modified" }],
                        details: { from: "tool_result" }
                      };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(output.details, Some(json!({ "from": "tool_result" })));

            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "modified");
            }
        });
    }

    #[test]
    fn tool_result_hook_can_modify_tool_not_found_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_result", async (event) => {
                    if (Object.is(event && event.toolName, "missing_tool") && event.isError) {
                      return {
                        content: [{ type: "text", text: "overridden" }],
                        details: { handled: true }
                      };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let tools = ToolRegistry::from_tools(Vec::new());
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "missing_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(output.details, Some(json!({ "handled": true })));

            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "overridden");
            }
        });
    }

    #[test]
    fn tool_result_hook_errors_fail_open() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_result", async (_event) => {
                    throw new Error("boom");
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(!is_error);
            assert!(!output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            assert_eq!(output.details, None);
            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "ok");
            }
        });
    }

    #[test]
    fn tool_result_hook_runs_on_blocked_tool_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("tool_call", async (event) => {
                    if (Object.is(event && event.toolName, "count_tool")) {
                      return { block: true, reason: "blocked in test" };
                    }
                    return {};
                  });

                  pi.on("tool_result", async (event) => {
                    if (Object.is(event && event.toolName, "count_tool") && event.isError) {
                      return { content: [{ type: "text", text: "override" }] };
                    }
                    return {};
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let provider = Arc::new(NoopProvider);
            let calls = Arc::new(AtomicUsize::new(0));
            let tools = ToolRegistry::from_tools(vec![Box::new(CountingTool {
                calls: Arc::clone(&calls),
            })]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .enable_extensions(&[], temp_dir.path(), None, &[entry_path])
                .await
                .expect("enable extensions");

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "count_tool".to_string(),
                arguments: json!({}),
                thought_signature: None,
            };

            let on_event: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(|_| {});
            let (output, is_error) = agent_session
                .agent
                .execute_tool(tool_call, on_event, test_turn_latency())
                .await;

            assert!(is_error);
            assert!(output.is_error);
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            assert!(
                matches!(output.content.as_slice(), [ContentBlock::Text(_)]),
                "Expected text output, got {:?}",
                output.content
            );
            if let [ContentBlock::Text(text)] = output.content.as_slice() {
                assert_eq!(text.text, "override");
            }
        });
    }
}

#[cfg(test)]
mod abort_tests {
    use super::*;
    use crate::session::Session;
    use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context as TaskContext, Poll};

    #[derive(Default)]
    struct AbortWakeCounter(AtomicUsize);

    impl std::task::Wake for AbortWakeCounter {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn abort_signal_is_latched_before_waiter_subscription() {
        let (handle, signal) = AbortHandle::new();
        let mut created_before_abort = Box::pin(signal.wait());
        let waker = futures::task::noop_waker();
        let mut task = TaskContext::from_waker(&waker);

        handle.abort();
        assert!(std::future::Future::poll(created_before_abort.as_mut(), &mut task).is_ready());

        // Broadcasts do not retain permits for future subscribers. The latched
        // predicate must still complete a newly created waiter without a
        // second abort or any provider/tool progress to wake it.
        let late_signal = signal.clone();
        let mut created_after_abort = Box::pin(late_signal.wait());
        assert!(std::future::Future::poll(created_after_abort.as_mut(), &mut task).is_ready());
    }

    #[test]
    fn abort_signal_wakes_every_waiter_after_a_nonterminal_notification() {
        let (handle, signal) = AbortHandle::new();
        let cloned_signal = signal.clone();
        let mut first = Box::pin(signal.wait());
        let mut second = Box::pin(cloned_signal.wait());
        let first_wakes = Arc::new(AbortWakeCounter::default());
        let second_wakes = Arc::new(AbortWakeCounter::default());
        let first_waker = std::task::Waker::from(Arc::clone(&first_wakes));
        let second_waker = std::task::Waker::from(Arc::clone(&second_wakes));
        let mut first_task = TaskContext::from_waker(&first_waker);
        let mut second_task = TaskContext::from_waker(&second_waker);
        assert!(std::future::Future::poll(first.as_mut(), &mut first_task).is_pending());
        assert!(std::future::Future::poll(second.as_mut(), &mut second_task).is_pending());

        signal.inner.notify.notify_waiters();
        assert_eq!(first_wakes.0.load(Ordering::SeqCst), 1);
        assert_eq!(second_wakes.0.load(Ordering::SeqCst), 1);
        assert!(!signal.is_aborted());
        assert!(std::future::Future::poll(first.as_mut(), &mut first_task).is_pending());
        assert!(std::future::Future::poll(second.as_mut(), &mut second_task).is_pending());

        // Removing one subscriber must not consume another subscriber's abort.
        drop(first);
        let mut replacement = Box::pin(signal.wait());
        assert!(std::future::Future::poll(replacement.as_mut(), &mut first_task).is_pending());
        handle.abort();
        assert_eq!(first_wakes.0.load(Ordering::SeqCst), 2);
        assert_eq!(second_wakes.0.load(Ordering::SeqCst), 2);
        assert!(std::future::Future::poll(replacement.as_mut(), &mut first_task).is_ready());
        assert!(std::future::Future::poll(second.as_mut(), &mut second_task).is_ready());
        handle.abort();
        assert_eq!(first_wakes.0.load(Ordering::SeqCst), 2);
        assert_eq!(second_wakes.0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn abort_signal_registration_racing_another_thread_never_stays_pending() {
        const ATTEMPTS: usize = 256;
        let (sender, receiver) = std::sync::mpsc::sync_channel::<AbortHandle>(1);
        let rendezvous = Arc::new(std::sync::Barrier::new(2));
        let worker_rendezvous = Arc::clone(&rendezvous);
        let worker = std::thread::spawn(move || {
            for _ in 0..ATTEMPTS {
                let handle = receiver
                    .recv_timeout(Duration::from_secs(5))
                    .expect("next independent abort signal");
                worker_rendezvous.wait();
                handle.abort();
                worker_rendezvous.wait();
            }
        });
        let waker = futures::task::noop_waker();
        let mut task = TaskContext::from_waker(&waker);
        for _ in 0..ATTEMPTS {
            let (handle, signal) = AbortHandle::new();
            let mut waiting = Box::pin(signal.wait());
            sender.send(handle).expect("abort worker remains alive");
            rendezvous.wait();
            let first = std::future::Future::poll(waiting.as_mut(), &mut task);
            rendezvous.wait();
            assert!(signal.is_aborted());
            // Do not repoll a future that already completed. Otherwise, the
            // first poll raced the abort and must retain that notification.
            if first.is_pending() {
                assert!(std::future::Future::poll(waiting.as_mut(), &mut task).is_ready());
            }
        }
        worker.join().expect("abort worker completed");
    }

    struct StartThenPending {
        start: Option<StreamEvent>,
    }

    impl Stream for StartThenPending {
        type Item = crate::error::Result<StreamEvent>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<Option<Self::Item>> {
            if let Some(event) = self.start.take() {
                return Poll::Ready(Some(Ok(event)));
            }
            Poll::Pending
        }
    }

    #[derive(Debug)]
    struct HangingProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for HangingProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let partial = AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };

            Ok(Box::pin(StartThenPending {
                start: Some(StreamEvent::Start { partial }),
            }))
        }
    }

    #[derive(Debug)]
    struct CountingProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for CountingProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[derive(Debug)]
    struct PhasedProvider {
        pending_calls: usize,
        calls: AtomicUsize,
        system_prompts: StdMutex<Vec<Option<String>>>,
    }

    impl PhasedProvider {
        const fn new(pending_calls: usize) -> Self {
            Self {
                pending_calls,
                calls: AtomicUsize::new(0),
                system_prompts: StdMutex::new(Vec::new()),
            }
        }

        fn base_message() -> AssistantMessage {
            AssistantMessage {
                content: Vec::new(),
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for PhasedProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.system_prompts
                .lock()
                .unwrap()
                .push(context.system_prompt.as_deref().map(str::to_string));
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.pending_calls {
                return Ok(Box::pin(StartThenPending {
                    start: Some(StreamEvent::Start {
                        partial: Self::base_message(),
                    }),
                }));
            }

            let partial = Self::base_message();
            let mut done = Self::base_message();
            done.content = vec![ContentBlock::Text(TextContent::new(format!(
                "resumed-response-{call}"
            )))];

            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: done,
                }),
            ])))
        }
    }

    #[derive(Debug)]
    struct ToolCallProvider {
        tool_calls: Vec<ToolCall>,
        calls: AtomicUsize,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolCallProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let message = AssistantMessage {
                content: self
                    .tool_calls
                    .iter()
                    .cloned()
                    .map(ContentBlock::ToolCall)
                    .collect(),
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };

            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::Done {
                    reason: StopReason::ToolUse,
                    message,
                },
            )])))
        }
    }

    #[derive(Debug)]
    struct HangingTool;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hanging_tool"
        }

        fn label(&self) -> &str {
            "Hanging Tool"
        }

        fn description(&self) -> &str {
            "Never completes unless aborted by the host"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> crate::error::Result<ToolOutput> {
            futures::future::pending::<()>().await;
            unreachable!("hanging tool should be aborted by the agent")
        }
    }

    /// Exercise the real read tool while making its completion visible to the
    /// blocked sibling without a scheduler- or wall-clock-dependent delay.
    struct NotifyingReadTool {
        inner: crate::tools::ReadTool,
        completed: Arc<AtomicUsize>,
        ready: Arc<Notify>,
    }

    #[async_trait]
    impl Tool for NotifyingReadTool {
        fn name(&self) -> &str {
            self.inner.name()
        }

        fn label(&self) -> &str {
            self.inner.label()
        }

        fn description(&self) -> &str {
            self.inner.description()
        }

        fn parameters(&self) -> serde_json::Value {
            self.inner.parameters()
        }

        fn effects(&self) -> ToolEffects {
            self.inner.effects()
        }

        async fn execute(
            &self,
            tool_call_id: &str,
            input: serde_json::Value,
            on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> crate::error::Result<ToolOutput> {
            let result = self.inner.execute(tool_call_id, input, on_update).await;
            self.completed.fetch_add(1, Ordering::SeqCst);
            self.ready.notify_one();
            result
        }
    }

    struct BlockedToolDropGuard(Arc<AtomicBool>);

    impl Drop for BlockedToolDropGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct AbortAfterReadsTool {
        completed: Arc<AtomicUsize>,
        ready: Arc<Notify>,
        abort: AbortHandle,
        dropped: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for AbortAfterReadsTool {
        fn name(&self) -> &'static str {
            "blocked_network"
        }

        fn label(&self) -> &'static str {
            "Blocked network operation"
        }

        fn description(&self) -> &'static str {
            "Remains pending when the operator cancels after both reads complete"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }

        fn effects(&self) -> ToolEffects {
            ToolEffects::network()
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> crate::error::Result<ToolOutput> {
            let _drop_guard = BlockedToolDropGuard(Arc::clone(&self.dropped));
            loop {
                let ready = self.ready.notified();
                if self.completed.load(Ordering::SeqCst) == 2 {
                    break;
                }
                ready.await;
            }
            self.abort.abort();
            futures::future::pending().await
        }
    }

    fn event_tag(event: &AgentEvent) -> &'static str {
        match event {
            AgentEvent::AgentStart { .. } => "agent_start",
            AgentEvent::AgentEnd { error, .. } => {
                if error.as_deref() == Some("Aborted") {
                    "agent_end_aborted"
                } else {
                    "agent_end"
                }
            }
            AgentEvent::TurnStart { .. } => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match &assistant_message_event {
                AssistantMessageEvent::Error {
                    reason: StopReason::Aborted,
                    ..
                } => "assistant_error_aborted",
                AssistantMessageEvent::Done { .. } => "assistant_done",
                _ => "assistant_update",
            },
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_end",
            AgentEvent::AutoCompactionStart { .. } => "auto_compaction_start",
            AgentEvent::AutoCompactionEnd { .. } => "auto_compaction_end",
            AgentEvent::AutoRetryStart { .. } => "auto_retry_start",
            AgentEvent::AutoRetryEnd { .. } => "auto_retry_end",
            AgentEvent::FailoverStart { .. } => "failover_start",
            AgentEvent::FailoverEnd { .. } => "failover_end",
            AgentEvent::AdvisorNote { .. } => "advisor_note",
            AgentEvent::ProviderError { .. } => "provider_error",
            AgentEvent::ExtensionError { .. } => "extension_error",
        }
    }

    fn assert_abort_resume_message_sequence(persisted: &[Message]) {
        assert_eq!(
            persisted.len(),
            6,
            "expected three user+assistant pairs, got: {persisted:?}"
        );

        let assistant_states = persisted
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant.stop_reason),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            assistant_states,
            vec![StopReason::Aborted, StopReason::Aborted, StopReason::Stop]
        );
    }

    fn assert_abort_resume_timeline_boundaries(timeline: &[String]) {
        assert!(
            timeline
                .iter()
                .any(|event| event.as_str().eq("run0:agent_end_aborted")),
            "missing aborted boundary for first run: {timeline:?}"
        );
        assert!(
            timeline
                .iter()
                .any(|event| event.as_str().eq("run1:agent_end_aborted")),
            "missing aborted boundary for second run: {timeline:?}"
        );
        assert!(
            timeline
                .iter()
                .any(|event| event.as_str().eq("run2:agent_end")),
            "missing successful boundary for resumed run: {timeline:?}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn dropping_extension_custom_turn_restores_prompt_and_activity() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(Box::pin(async {
            let temp = tempfile::tempdir().unwrap();
            let entry = temp.path().join("custom-prompt.native.json");
            let descriptor = json!({
                "id": "custom-prompt-test",
                "name": "custom-prompt-test",
                "version": "1.0.0",
                "apiVersion": crate::extensions::PROTOCOL_VERSION,
                "eventHooks": ["before_agent_start"],
                "eventResponses": {
                    "before_agent_start": {"systemPrompt": "custom-hook-system"},
                },
            });
            std::fs::write(&entry, serde_json::to_vec(&descriptor).unwrap()).unwrap();
            let manager = ExtensionManager::new();
            manager.set_native_runtime(NativeRustExtensionRuntimeHandle::start().await.unwrap());
            manager
                .load_native_extensions(vec![
                    crate::extensions::NativeRustExtensionLoadSpec::from_entry_path(&entry)
                        .unwrap(),
                ])
                .await
                .unwrap();
            let provider = Arc::new(PhasedProvider::new(1));
            let agent = Agent::new(
                Arc::clone(&provider) as Arc<dyn Provider>,
                ToolRegistry::new(&[], Path::new("."), None),
                AgentConfig {
                    system_prompt: Some("base-system".to_string()),
                    ..AgentConfig::default()
                },
            );
            let mut agent_session = AgentSession::new(
                agent,
                Arc::new(Mutex::new(Session::in_memory())),
                false,
                ResolvedCompactionSettings::default(),
            );
            agent_session.extensions = Some(ExtensionRegion::new(manager));
            let active = Arc::clone(&agent_session.extensions_turn_active);
            let streaming = Arc::clone(&agent_session.extensions_is_streaming);
            let previous_source = agent_session.input_source;
            let message = Message::Custom(CustomMessage {
                content: "extension work".to_string(),
                custom_type: "note".to_string(),
                display: true,
                details: None,
                timestamp: 0,
            });
            agent_session
                .extensions_pending_idle_actions
                .lock()
                .unwrap()
                .push_back(PendingIdleAction::CustomMessage(message));
            let mut turn =
                Box::pin(agent_session.run_pending_idle_actions_with_abort(None, Arc::new(|_| {})));
            let reached_provider = futures::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(turn.as_mut(), cx).is_pending());
                if provider.calls.load(Ordering::SeqCst) == 1 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            });
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(5),
                reached_provider,
            )
            .await
            .expect("custom turn must reach its pending provider");
            assert!(active.load(Ordering::SeqCst));
            assert!(streaming.load(Ordering::SeqCst));
            drop(turn);
            assert!(!active.load(Ordering::SeqCst));
            assert!(!streaming.load(Ordering::SeqCst));
            assert_eq!(agent_session.agent.system_prompt(), Some("base-system"));
            assert_eq!(
                agent_session.input_source.as_str(),
                previous_source.as_str()
            );

            agent_session.extensions = None;
            let result = agent_session
                .run_text("next input".to_string(), |_| {})
                .await
                .unwrap();
            assert_eq!(result.stop_reason, StopReason::Stop);
            assert_eq!(
                *provider.system_prompts.lock().unwrap(),
                vec![
                    Some("custom-hook-system".to_string()),
                    Some("base-system".to_string()),
                ],
            );
        }));
    }

    #[test]
    fn abort_interrupts_in_flight_stream() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let started = Arc::new(Notify::new());
        let started_wait = started.notified();

        let (abort_handle, abort_signal) = AbortHandle::new();

        let provider = Arc::new(HangingProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let started_tx = Arc::clone(&started);
        let join = handle.spawn(async move {
            agent_session
                .run_text_with_abort("hello".to_string(), Some(abort_signal), move |event| {
                    if matches!(
                        event,
                        AgentEvent::MessageStart {
                            message: Message::Assistant(_)
                        }
                    ) {
                        started_tx.notify_one();
                    }
                })
                .await
        });

        runtime.block_on(async move {
            started_wait.await;
            abort_handle.abort();

            let message = join.await.expect("run_text_with_abort");
            assert_eq!(message.stop_reason, StopReason::Aborted);
            assert_eq!(message.error_message.as_deref(), Some("Aborted"));
        });
    }

    #[test]
    fn ambient_cancellation_interrupts_in_flight_stream() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let (started_tx, started_rx) = std::sync::mpsc::channel();

            let provider = Arc::new(HangingProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let ambient_cx = asupersync::Cx::for_testing();
            let cancel_cx = ambient_cx.clone();
            let _current = asupersync::Cx::set_current(Some(ambient_cx));

            let cancel_thread = std::thread::spawn(move || {
                started_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("stream start");
                cancel_cx.set_cancel_requested(true);
            });

            let message = agent_session
                .run_text_with_abort("hello".to_string(), None, move |event| {
                    if matches!(
                        event,
                        AgentEvent::MessageStart {
                            message: Message::Assistant(_)
                        }
                    ) {
                        let _ = started_tx.send(());
                    }
                })
                .await
                .expect("run_text_with_abort");

            cancel_thread.join().expect("cancel thread");

            assert_eq!(message.stop_reason, StopReason::Aborted);
            assert_eq!(message.error_message.as_deref(), Some("Aborted"));
        });
    }

    #[test]
    fn abort_before_run_skips_provider_stream_call() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        });
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let (abort_handle, abort_signal) = AbortHandle::new();
        abort_handle.abort();

        runtime.block_on(async move {
            let message = agent_session
                .run_text_with_abort("hello".to_string(), Some(abort_signal), |_| {})
                .await
                .expect("run_text_with_abort");
            assert_eq!(message.stop_reason, StopReason::Aborted);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn abort_then_resume_preserves_session_history() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(PhasedProvider::new(1));
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            let started = Arc::new(Notify::new());
            let (abort_handle, abort_signal) = AbortHandle::new();
            let started_for_abort = Arc::clone(&started);
            let abort_join = handle.spawn(async move {
                started_for_abort.notified().await;
                abort_handle.abort();
            });

            let aborted = agent_session
                .run_text_with_abort("first".to_string(), Some(abort_signal), {
                    let started = Arc::clone(&started);
                    move |event| {
                        if matches!(
                            event,
                            AgentEvent::MessageStart {
                                message: Message::Assistant(_)
                            }
                        ) {
                            started.notify_one();
                        }
                    }
                })
                .await
                .expect("first run");
            abort_join.await;

            assert_eq!(aborted.stop_reason, StopReason::Aborted);
            assert_eq!(aborted.error_message.as_deref(), Some("Aborted"));

            let resumed = agent_session
                .run_text("second".to_string(), |_| {})
                .await
                .expect("resumed run");
            assert_eq!(resumed.stop_reason, StopReason::Stop);
            assert!(resumed.error_message.is_none());

            let cx = crate::agent_cx::AgentCx::for_request();
            let persisted = session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .to_messages_for_current_path();

            assert_eq!(
                persisted.len(),
                4,
                "unexpected message history after abort+resume: {persisted:?}"
            );
            assert!(matches!(persisted.first(), Some(Message::User(_))));
            assert!(matches!(
                persisted.get(1),
                Some(Message::Assistant(assistant))
                    if matches!(assistant.stop_reason, StopReason::Aborted)
            ));
            assert!(matches!(persisted.get(2), Some(Message::User(_))));
            assert!(matches!(
                persisted.get(3),
                Some(Message::Assistant(assistant))
                    if matches!(assistant.stop_reason, StopReason::Stop)
                        && assistant.error_message.is_none()
            ));
        });
    }

    #[test]
    fn repeated_abort_then_resume_has_consistent_timeline_and_state() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(PhasedProvider::new(2));
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            let timeline = Arc::new(StdMutex::new(Vec::<String>::new()));

            for run_idx in 0..2 {
                let started = Arc::new(Notify::new());
                let (abort_handle, abort_signal) = AbortHandle::new();
                let started_for_abort = Arc::clone(&started);
                let abort_join = handle.spawn(async move {
                    started_for_abort.notified().await;
                    abort_handle.abort();
                });

                let run_timeline = Arc::clone(&timeline);
                let aborted = agent_session
                    .run_text_with_abort(format!("abort-run-{run_idx}"), Some(abort_signal), {
                        let started = Arc::clone(&started);
                        move |event| {
                            if let Ok(mut events) = run_timeline.lock() {
                                events.push(format!("run{run_idx}:{}", event_tag(&event)));
                            }
                            if matches!(
                                event,
                                AgentEvent::MessageStart {
                                    message: Message::Assistant(_)
                                }
                            ) {
                                started.notify_one();
                            }
                        }
                    })
                    .await
                    .expect("aborted run");
                abort_join.await;

                assert_eq!(
                    aborted.stop_reason,
                    StopReason::Aborted,
                    "run {run_idx} should abort cleanly"
                );
            }

            let run_timeline = Arc::clone(&timeline);
            let resumed = agent_session
                .run_text("final-run".to_string(), move |event| {
                    if let Ok(mut events) = run_timeline.lock() {
                        events.push(format!("run2:{}", event_tag(&event)));
                    }
                })
                .await
                .expect("final resumed run");
            assert_eq!(resumed.stop_reason, StopReason::Stop);
            assert!(resumed.error_message.is_none());

            let cx = crate::agent_cx::AgentCx::for_request();
            let persisted = session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .to_messages_for_current_path();

            assert_abort_resume_message_sequence(&persisted);

            let timeline = timeline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_abort_resume_timeline_boundaries(&timeline);
        });
    }

    #[test]
    fn abort_during_tool_execution_records_aborted_tool_result() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let provider = Arc::new(ToolCallProvider {
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "hanging_tool".to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                }],
                calls: AtomicUsize::new(0),
            });
            let tools = ToolRegistry::from_tools(vec![Box::new(HangingTool)]);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );

            let tool_started = Arc::new(Notify::new());
            let (abort_handle, abort_signal) = AbortHandle::new();
            let tool_started_for_abort = Arc::clone(&tool_started);
            let abort_join = handle.spawn(async move {
                tool_started_for_abort.notified().await;
                abort_handle.abort();
            });

            let result = agent_session
                .run_text_with_abort("trigger tool".to_string(), Some(abort_signal), {
                    let tool_started = Arc::clone(&tool_started);
                    move |event| {
                        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                            tool_started.notify_one();
                        }
                    }
                })
                .await
                .expect("tool-abort run");
            abort_join.await;
            assert_eq!(result.stop_reason, StopReason::Aborted);

            let cx = crate::agent_cx::AgentCx::for_request();
            let persisted = session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .to_messages_for_current_path();

            let tool_result = persisted
                .iter()
                .find_map(|message| match message {
                    Message::ToolResult(result) => Some(result),
                    _ => None,
                })
                .expect("expected tool result message");
            assert!(tool_result.is_error);
            assert!(
                tool_result.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text(text) if text.text.contains("Tool execution aborted")
                    )
                }),
                "missing aborted tool marker in tool output: {:?}",
                tool_result.content
            );
            let details = tool_result
                .details
                .as_ref()
                .expect("aborted tool result should include structured details");
            assert_eq!(details["schema"], TOOL_CANCELLATION_SCHEMA_V1);
            assert_eq!(details["status"], "cancelled");
            assert_eq!(details["reason"], "abort_signal");
            assert_eq!(details["toolName"], "hanging_tool");
            assert_eq!(details["cleanup"], "tool_result_recorded_no_success");
        });
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
    fn abort_preserves_completed_batch_results_in_persisted_history() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp = tempfile::tempdir().unwrap();
            std::fs::write(
                temp.path().join("completed.txt"),
                "completed before cancellation\n",
            )
            .unwrap();
            let completed = Arc::new(AtomicUsize::new(0));
            let ready = Arc::new(Notify::new());
            let dropped = Arc::new(AtomicBool::new(false));
            let (abort_handle, abort_signal) = AbortHandle::new();
            let tools = ToolRegistry::from_tools(vec![
                Box::new(NotifyingReadTool {
                    inner: crate::tools::ReadTool::new(temp.path()),
                    completed: Arc::clone(&completed),
                    ready: Arc::clone(&ready),
                }),
                Box::new(AbortAfterReadsTool {
                    completed: Arc::clone(&completed),
                    ready,
                    abort: abort_handle,
                    dropped: Arc::clone(&dropped),
                }),
                Box::new(crate::tools::WriteTool::new(temp.path())),
            ]);
            let provider = Arc::new(ToolCallProvider {
                tool_calls: [
                    ("read-complete", "read", json!({"path": "completed.txt"})),
                    (
                        "read-error",
                        "read",
                        json!({"path": "completed.txt", "limit": 0}),
                    ),
                    ("still-running", "blocked_network", json!({})),
                    (
                        "never-started",
                        "write",
                        json!({"path": "must-not-exist.txt", "content": "not admitted"}),
                    ),
                ]
                .into_iter()
                .map(|(id, name, arguments)| ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                    thought_signature: None,
                })
                .collect(),
                calls: AtomicUsize::new(0),
            });
            let agent = Agent::new(
                Arc::clone(&provider) as Arc<dyn Provider>,
                tools,
                AgentConfig::default(),
            );
            let session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                temp.path().join("sessions"),
            ))));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                true,
                ResolvedCompactionSettings::default(),
            );
            let events = Arc::new(StdMutex::new(Vec::new()));
            let observed = Arc::clone(&events);
            let result = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(30),
                agent_session.run_text_with_abort(
                    "read the file twice, then write".to_string(),
                    Some(abort_signal),
                    move |event| observed.lock().unwrap().push(event),
                ),
            )
            .await
            .expect("cancelling a blocked sibling must finish the turn")
            .expect("the cancelled turn must save completed work");
            assert_eq!(result.stop_reason, StopReason::Aborted);
            assert_eq!(completed.load(Ordering::SeqCst), 2);
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert!(!temp.path().join("must-not-exist.txt").exists());

            let cx = crate::agent_cx::AgentCx::for_request();
            let path = session
                .lock(cx.cx())
                .await
                .unwrap()
                .path
                .clone()
                .expect("the cancelled turn must have a persisted session");
            let reopened = Session::open(path.to_string_lossy().as_ref())
                .await
                .expect("reopen the cancelled session");
            let persisted = reopened.to_messages_for_current_path();
            let results = persisted
                .iter()
                .filter_map(|message| match message {
                    Message::ToolResult(result) => Some(result),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                results
                    .iter()
                    .map(|result| result.tool_call_id.as_str())
                    .collect::<Vec<_>>(),
                [
                    "read-complete",
                    "read-error",
                    "still-running",
                    "never-started"
                ],
            );
            assert!(!results[0].is_error);
            assert!(results[0].content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text(text) if text.text.contains("completed before cancellation")
                )
            }));
            assert!(results[1].is_error);
            assert!(results[1].content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text(text) if text.text.contains("`limit` must be greater than 0")
                )
            }));
            assert!(results[1].details.is_none());
            for result in &results[2..] {
                assert!(result.is_error);
                let details = result.details.as_ref().expect("cancellation details");
                assert_eq!(details["schema"], TOOL_CANCELLATION_SCHEMA_V1);
                assert_eq!(details["reason"], "abort_signal");
            }

            let events = events.lock().unwrap();
            for result in &results {
                let ends = events
                    .iter()
                    .filter_map(|event| match event {
                        AgentEvent::ToolExecutionEnd {
                            tool_call_id,
                            result: output,
                            is_error,
                            ..
                        } if tool_call_id == &result.tool_call_id => Some((output, is_error)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(ends.len(), 1, "one terminal event per tool call");
                assert_eq!(*ends[0].1, result.is_error);
                assert_eq!(
                    serde_json::to_value(&ends[0].0.content).unwrap(),
                    serde_json::to_value(&result.content).unwrap(),
                );
                assert_eq!(ends[0].0.details, result.details);
                let message_end_count = events
                    .iter()
                    .filter(|event| {
                        matches!(
                            event,
                            AgentEvent::MessageEnd {
                                message: Message::ToolResult(message),
                            } if message.tool_call_id == result.tool_call_id
                        )
                    })
                    .count();
                assert_eq!(message_end_count, 1, "one transcript event per tool result");
            }
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                    .count(),
                1,
            );
            assert!(matches!(
                events.last(),
                Some(AgentEvent::AgentEnd { error: Some(error), .. }) if error == "Aborted"
            ));
        });
    }
}

#[cfg(test)]
mod turn_event_tests {
    use super::*;
    use crate::session::Session;
    use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    // Note: Mutex from super::* is asupersync::sync::Mutex (for Session)
    // Use std::sync::Mutex directly for synchronous event capture

    fn assistant_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        }
    }

    struct SingleShotProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for SingleShotProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let partial = assistant_message("");
            let final_message = assistant_message("hello");
            let events = vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    struct PauseThenStopProvider {
        calls: AtomicUsize,
        pause_turns: usize,
        pause_has_tool_call: bool,
        contexts: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for PauseThenStopProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(context.messages.to_vec());
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let partial = assistant_message("");
            let mut done = if call < self.pause_turns {
                assistant_message("server tool is still working")
            } else {
                assistant_message("completed after pause")
            };
            done.stop_reason = if call < self.pause_turns {
                StopReason::PauseTurn
            } else {
                StopReason::Stop
            };
            if call < self.pause_turns && self.pause_has_tool_call {
                done.content = vec![ContentBlock::ToolCall(ToolCall {
                    id: "server-tool-1".to_string(),
                    name: "server_tool".to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                })];
            }

            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: done.stop_reason,
                    message: done,
                }),
            ])))
        }
    }

    struct StreamSetupErrorProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for StreamSetupErrorProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Err(Error::api("stream setup failed"))
        }
    }

    #[derive(Debug)]
    struct EchoTool;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }

        fn label(&self) -> &str {
            "echo_tool"
        }

        fn description(&self) -> &str {
            "echo test tool"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("tool-ok"))],
                details: None,
                is_error: false,
            })
        }
    }

    #[derive(Debug)]
    struct ToolTurnProvider {
        calls: AtomicUsize,
    }

    impl ToolTurnProvider {
        const fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn assistant_message_with(
            &self,
            stop_reason: StopReason,
            content: Vec<ContentBlock>,
        ) -> AssistantMessage {
            AssistantMessage {
                content,
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolTurnProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            let partial = self.assistant_message_with(StopReason::Stop, Vec::new());
            let done = if call_index == 0 {
                self.assistant_message_with(
                    StopReason::ToolUse,
                    vec![ContentBlock::ToolCall(ToolCall {
                        id: "tool-1".to_string(),
                        name: "echo_tool".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    })],
                )
            } else {
                self.assistant_message_with(
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new("final"))],
                )
            };

            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: done.stop_reason,
                    message: done,
                }),
            ])))
        }
    }

    #[test]
    fn pause_turn_resubmits_the_assistant_response_without_a_synthetic_user_message() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let provider = Arc::new(PauseThenStopProvider {
            calls: AtomicUsize::new(0),
            pause_turns: 1,
            pause_has_tool_call: false,
            contexts: std::sync::Mutex::new(Vec::new()),
        });
        let provider_for_assertions = Arc::clone(&provider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut agent = Agent::new(provider, tools, AgentConfig::default());

        runtime.block_on(async move {
            let result = agent
                .run("search for current news", |_| {})
                .await
                .expect("pause continuation succeeds");
            assert_eq!(result.stop_reason, StopReason::Stop);
            assert!(matches!(
                result.content.as_slice(),
                [ContentBlock::Text(text)] if text.text == "completed after pause"
            ));
        });

        assert_eq!(provider_for_assertions.calls.load(Ordering::SeqCst), 2);
        let contexts = provider_for_assertions
            .contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(contexts.len(), 2);
        assert!(matches!(contexts[0].as_slice(), [Message::User(_)]));
        assert!(matches!(
            contexts[1].as_slice(),
            [Message::User(_), Message::Assistant(message)]
                if message.stop_reason == StopReason::PauseTurn
                    && matches!(message.content.as_slice(), [ContentBlock::Text(text)] if text.text == "server tool is still working")
        ));
    }

    #[test]
    fn pause_turn_never_executes_its_tool_call_as_a_local_tool() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let provider = Arc::new(PauseThenStopProvider {
            calls: AtomicUsize::new(0),
            pause_turns: 1,
            pause_has_tool_call: true,
            contexts: std::sync::Mutex::new(Vec::new()),
        });
        let provider_for_assertions = Arc::clone(&provider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut agent = Agent::new(provider, tools, AgentConfig::default());

        runtime.block_on(async move {
            let result = agent
                .run("wait for the server tool", |_| {})
                .await
                .expect("pause continuation succeeds without local tool execution");
            assert_eq!(result.stop_reason, StopReason::Stop);
        });

        let contexts = provider_for_assertions
            .contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(matches!(
            contexts[1].as_slice(),
            [Message::User(_), Message::Assistant(message)]
                if matches!(message.content.as_slice(), [ContentBlock::ToolCall(call)] if call.name == "server_tool")
        ));
    }

    #[test]
    fn pause_turn_continuation_budget_stops_an_unbounded_server_tool_loop() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let provider = Arc::new(PauseThenStopProvider {
            calls: AtomicUsize::new(0),
            pause_turns: MAX_PAUSE_TURN_CONTINUATIONS + 1,
            pause_has_tool_call: false,
            contexts: std::sync::Mutex::new(Vec::new()),
        });
        let provider_for_assertions = Arc::clone(&provider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut agent = Agent::new(provider, tools, AgentConfig::default());

        runtime.block_on(async move {
            let result = agent
                .run("run the server tool", |_| {})
                .await
                .expect("bounded pause continuation succeeds");
            assert_eq!(result.stop_reason, StopReason::PauseTurn);
        });

        assert_eq!(
            provider_for_assertions.calls.load(Ordering::SeqCst),
            MAX_PAUSE_TURN_CONTINUATIONS + 1
        );
    }

    #[test]
    fn turn_events_wrap_assistant_response() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let provider = Arc::new(SingleShotProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_capture = Arc::clone(&events);

        let join = handle.spawn(async move {
            agent_session
                .run_text("hello".to_string(), move |event| {
                    events_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(event);
                })
                .await
                .expect("run_text")
        });

        runtime.block_on(async move {
            let message = join.await;
            assert_eq!(message.stop_reason, StopReason::Stop);

            let events = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let turn_start_indices = events
                .iter()
                .enumerate()
                .filter_map(|(idx, event)| {
                    matches!(event, AgentEvent::TurnStart { .. }).then_some(idx)
                })
                .collect::<Vec<_>>();
            let turn_end_indices = events
                .iter()
                .enumerate()
                .filter_map(|(idx, event)| {
                    matches!(event, AgentEvent::TurnEnd { .. }).then_some(idx)
                })
                .collect::<Vec<_>>();

            assert_eq!(turn_start_indices.len(), 1);
            assert_eq!(turn_end_indices.len(), 1);
            assert!(turn_start_indices[0] < turn_end_indices[0]);

            let assistant_message_end = events
                .iter()
                .enumerate()
                .find_map(|(idx, event)| match event {
                    AgentEvent::MessageEnd {
                        message: Message::Assistant(_),
                    } => Some(idx),
                    _ => None,
                })
                .expect("assistant message end");

            assert!(assistant_message_end < turn_end_indices[0]);

            let (message_is_assistant, tool_results_empty) = {
                let turn_end_event = &events[turn_end_indices[0]];
                assert!(
                    matches!(turn_end_event, AgentEvent::TurnEnd { .. }),
                    "Expected TurnEnd event, got {turn_end_event:?}"
                );
                match turn_end_event {
                    AgentEvent::TurnEnd {
                        message,
                        tool_results,
                        ..
                    } => (
                        matches!(message, Message::Assistant(_)),
                        tool_results.is_empty(),
                    ),
                    _ => (false, false),
                }
            };
            drop(events);
            assert!(message_is_assistant);
            assert!(tool_results_empty);
        });
    }

    #[test]
    fn stream_setup_errors_still_emit_turn_end_before_agent_end() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let provider = Arc::new(StreamSetupErrorProvider);
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_capture = Arc::clone(&events);

        let join = handle.spawn(async move {
            agent_session
                .run_text("hello".to_string(), move |event| {
                    events_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(event);
                })
                .await
                .expect_err("run_text should fail before streaming starts")
        });

        runtime.block_on(async move {
            let err = join.await;
            assert!(
                err.to_string().contains("stream setup failed"),
                "unexpected error: {err}"
            );

            let events = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let turn_start_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::TurnStart { turn_index: 0, .. }))
                .expect("turn start");
            let turn_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::TurnEnd { turn_index: 0, .. }))
                .expect("turn end");
            let agent_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                .expect("agent end");

            assert!(turn_start_idx < turn_end_idx);
            assert!(turn_end_idx < agent_end_idx);

            let assistant_message_end = events
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        AgentEvent::MessageEnd {
                            message: Message::Assistant(_),
                        }
                    )
                })
                .expect("assistant message end");
            assert!(assistant_message_end < turn_end_idx);

            match &events[turn_end_idx] {
                AgentEvent::TurnEnd {
                    message,
                    tool_results,
                    ..
                } => {
                    assert!(tool_results.is_empty());
                    assert!(
                        matches!(message, Message::Assistant(_)),
                        "expected assistant message in TurnEnd, got {message:?}"
                    );
                    let Message::Assistant(message) = message else {
                        return;
                    };
                    assert_eq!(message.stop_reason, StopReason::Error);
                    assert_eq!(
                        message.error_message.as_deref(),
                        Some("API error: stream setup failed")
                    );
                    assert_eq!(message.api, "test-api");
                    assert_eq!(message.provider, "test-provider");
                    assert_eq!(message.model, "test-model");
                }
                other => {
                    assert!(matches!(other, AgentEvent::TurnEnd { .. }));
                    return;
                }
            }

            match &events[agent_end_idx] {
                AgentEvent::AgentEnd { error, .. } => {
                    assert_eq!(error.as_deref(), Some("API error: stream setup failed"));
                }
                other => {
                    assert!(matches!(other, AgentEvent::AgentEnd { .. }));
                }
            }
        });
    }

    #[test]
    fn turn_events_include_tool_execution_and_tool_result_messages() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        let provider = Arc::new(ToolTurnProvider::new());
        let tools = ToolRegistry::from_tools(vec![Box::new(EchoTool)]);
        let agent = Agent::new(provider, tools, AgentConfig::default());
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let mut agent_session =
            AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_capture = Arc::clone(&events);

        let join = handle.spawn(async move {
            agent_session
                .run_text("hello".to_string(), move |event| {
                    events_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(event);
                })
                .await
                .expect("run_text")
        });

        runtime.block_on(async move {
            let message = join.await;
            assert_eq!(message.stop_reason, StopReason::Stop);

            let events = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let turn_start_count = events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnStart { .. }))
                .count();
            let turn_end_count = events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
                .count();
            assert_eq!(
                turn_start_count, 2,
                "expected one tool turn and one final turn"
            );
            assert_eq!(
                turn_end_count, 2,
                "expected one tool turn and one final turn"
            );

            let tool_start_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
                .expect("tool execution start event");
            let tool_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. }))
                .expect("tool execution end event");
            assert!(tool_start_idx < tool_end_idx);

            let first_turn_end_idx = events
                .iter()
                .position(|event| matches!(event, AgentEvent::TurnEnd { turn_index: 0, .. }))
                .expect("first turn end");
            assert!(
                tool_end_idx < first_turn_end_idx,
                "tool execution should complete before first turn end"
            );

            let first_turn_tool_results = events.iter().find_map(|event| match event {
                AgentEvent::TurnEnd {
                    turn_index,
                    tool_results,
                    ..
                } if turn_index.eq(&0) => Some(tool_results),
                _ => None,
            });

            let first_turn_tool_results =
                first_turn_tool_results.expect("expected tool results for first turn");
            assert_eq!(first_turn_tool_results.len(), 1);
            let first_result = first_turn_tool_results.first().unwrap();
            if let Message::ToolResult(tr) = first_result {
                assert_eq!(tr.tool_name, "echo_tool");
                assert!(!tr.is_error);
            } else {
                unreachable!("expected Message::ToolResult, got {:?}", first_result);
            }
            drop(events);
        });
    }
}

#[derive(Clone)]
struct AgentExtensionSession {
    handle: SessionHandle,
    ai_completion: Arc<StdMutex<ExtensionAiCompletionHostState>>,
    session_action_admission: SessionActionAdmissionGate,
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
    queue_modes: Arc<StdMutex<ExtensionQueueModeState>>,
    auto_compaction_enabled: bool,
}

impl AgentExtensionSession {
    async fn acquire_session_action_admission(
        &self,
        origin: Option<SessionActionOrigin>,
    ) -> Result<OwnedMutexGuard<()>> {
        let origin = origin.ok_or_else(|| {
            Error::session("extension Session action is missing trusted task provenance")
        })?;
        self.session_action_admission.acquire_origin(&origin).await
    }

    fn current_queue_modes(&self) -> (QueueMode, QueueMode) {
        self.queue_modes
            .lock()
            .map_or((QueueMode::OneAtATime, QueueMode::OneAtATime), |state| {
                (state.steering_mode, state.follow_up_mode)
            })
    }

    fn state_fallback(&self) -> Value {
        let (steering_mode, follow_up_mode) = self.current_queue_modes();
        json!({
            "model": null,
            "thinkingLevel": "off",
            "durabilityMode": "balanced",
            "isStreaming": self.is_streaming.load(std::sync::atomic::Ordering::SeqCst),
            "isCompacting": self.is_compacting.load(std::sync::atomic::Ordering::SeqCst),
            "steeringMode": steering_mode.as_str(),
            "followUpMode": follow_up_mode.as_str(),
            "sessionFile": null,
            "sessionId": "",
            "sessionName": null,
            "autoCompactionEnabled": self.auto_compaction_enabled,
            "messageCount": 0,
            "pendingMessageCount": 0,
        })
    }
}

#[async_trait]
impl crate::extensions::ExtensionSession for AgentExtensionSession {
    async fn get_state(&self) -> Value {
        let (steering_mode, follow_up_mode) = self.current_queue_modes();
        let mut state =
            <SessionHandle as crate::extensions::ExtensionSession>::get_state(&self.handle).await;
        let Some(object) = state.as_object_mut() else {
            return self.state_fallback();
        };

        object.insert(
            "isStreaming".to_string(),
            Value::Bool(self.is_streaming.load(std::sync::atomic::Ordering::SeqCst)),
        );
        object.insert(
            "isCompacting".to_string(),
            Value::Bool(self.is_compacting.load(std::sync::atomic::Ordering::SeqCst)),
        );
        object.insert(
            "steeringMode".to_string(),
            Value::String(steering_mode.as_str().to_string()),
        );
        object.insert(
            "followUpMode".to_string(),
            Value::String(follow_up_mode.as_str().to_string()),
        );
        object.insert(
            "autoCompactionEnabled".to_string(),
            Value::Bool(self.auto_compaction_enabled),
        );

        state
    }

    async fn get_messages(&self) -> Vec<crate::session::SessionMessage> {
        <SessionHandle as crate::extensions::ExtensionSession>::get_messages(&self.handle).await
    }

    async fn get_entries(&self) -> Vec<Value> {
        <SessionHandle as crate::extensions::ExtensionSession>::get_entries(&self.handle).await
    }

    async fn get_branch(&self) -> Vec<Value> {
        <SessionHandle as crate::extensions::ExtensionSession>::get_branch(&self.handle).await
    }

    async fn set_name(
        &self,
        name: String,
        origin: Option<SessionActionOrigin>,
    ) -> crate::error::Result<()> {
        let _session_action_permit = self.acquire_session_action_admission(origin).await?;
        <SessionHandle as crate::extensions::ExtensionSession>::set_name(&self.handle, name, None)
            .await
    }

    async fn append_message(
        &self,
        message: crate::session::SessionMessage,
        origin: Option<SessionActionOrigin>,
    ) -> crate::error::Result<()> {
        let _session_action_permit = self.acquire_session_action_admission(origin).await?;
        <SessionHandle as crate::extensions::ExtensionSession>::append_message(
            &self.handle,
            message,
            None,
        )
        .await
    }

    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<Value>,
        origin: Option<SessionActionOrigin>,
    ) -> crate::error::Result<()> {
        let _session_action_permit = self.acquire_session_action_admission(origin).await?;
        <SessionHandle as crate::extensions::ExtensionSession>::append_custom_entry(
            &self.handle,
            custom_type,
            data,
            None,
        )
        .await
    }

    async fn set_model(
        &self,
        provider: String,
        model_id: String,
        origin: Option<SessionActionOrigin>,
    ) -> crate::error::Result<()> {
        let _session_action_permit = self.acquire_session_action_admission(origin).await?;
        let provider = provider.trim();
        let model_id = model_id.trim();
        if provider.is_empty() || model_id.is_empty() {
            return Err(Error::validation(
                "setModel: provider and modelId must be non-empty strings",
            ));
        }
        let (registry, active_provider) = {
            let state = self
                .ai_completion
                .lock()
                .map_err(|_| Error::session("setModel: the live model catalog is unavailable"))?;
            (
                state.compaction_models.registry.clone(),
                Arc::clone(&state.provider),
            )
        };
        let resolved = registry
            .as_ref()
            .and_then(|registry| registry.find(provider, model_id));
        let (mut provider, mut model_id) = if let Some(entry) = resolved {
            (entry.model.provider, entry.model.id)
        } else if active_provider.name().eq_ignore_ascii_case(provider)
            && active_provider.model_id().eq_ignore_ascii_case(model_id)
        {
            // Embedders can supply an active ad-hoc provider without a catalog.
            // Re-selecting that same destination remains a valid explicit action.
            (
                active_provider.name().to_string(),
                active_provider.model_id().to_string(),
            )
        } else {
            return Err(Error::validation(format!(
                "setModel: unknown provider/model {provider}/{model_id}. Check the provider and model ID with ctx.modelRegistry.find(provider, modelId) before switching."
            )));
        };
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let mut session = self
            .handle
            .0
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(format!("Failed to lock session: {e}")))?;
        if let Some(registry) = registry
            && let Some((current_provider, current_model)) =
                session.effective_model_for_current_path()
            && let Some(current_entry) = registry.find(&current_provider, &current_model)
            && current_entry.model.provider.eq_ignore_ascii_case(&provider)
            && current_entry.model.id.eq_ignore_ascii_case(&model_id)
        {
            // The catalog proved that both names resolve to one destination.
            // Preserve historical alias spelling without merging separately
            // declared providers that happen to share a built-in alias family.
            provider = current_provider;
            model_id = current_model;
        }
        session.select_model(provider, model_id);
        Ok(())
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        <SessionHandle as crate::extensions::ExtensionSession>::get_model(&self.handle).await
    }

    async fn set_thinking_level(
        &self,
        level: String,
        origin: Option<SessionActionOrigin>,
    ) -> crate::error::Result<()> {
        let _session_action_permit = self.acquire_session_action_admission(origin).await?;
        <SessionHandle as crate::extensions::ExtensionSession>::set_thinking_level(
            &self.handle,
            level,
            None,
        )
        .await
    }

    async fn get_thinking_level(&self) -> Option<String> {
        <SessionHandle as crate::extensions::ExtensionSession>::get_thinking_level(&self.handle)
            .await
    }

    async fn set_label(
        &self,
        target_id: String,
        label: Option<String>,
        origin: Option<SessionActionOrigin>,
    ) -> crate::error::Result<()> {
        let _session_action_permit = self.acquire_session_action_admission(origin).await?;
        <SessionHandle as crate::extensions::ExtensionSession>::set_label(
            &self.handle,
            target_id,
            label,
            None,
        )
        .await
    }
}

/// Dropping a save future does not stop its blocking filesystem work. Arm this
/// guard only around the save itself: waiting for the Session lock or running a
/// provider does not yet make persistence indeterminate. Do not hold a provider
/// permit here; extension callbacks may already own it while awaiting Session.
struct TurnPersistenceGuard<'a> {
    admission: &'a ProviderAdmissionGate,
    completed: bool,
}

impl Drop for TurnPersistenceGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let mut reason = self
                .admission
                .reason
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reason.get_or_insert_with(|| {
                "turn persistence was interrupted before its durable result was observed"
                    .to_string()
            });
        }
    }
}

async fn flush_turn_autosave(
    session: &mut Session,
    admission: &ProviderAdmissionGate,
    trigger: AutosaveFlushTrigger,
) -> Result<()> {
    let mut guard = TurnPersistenceGuard {
        admission,
        completed: false,
    };
    let result = session.flush_autosave(trigger).await;
    // The caller handles an observed error with its original turn failure.
    // Never clear admission here: another operation may own its quarantine.
    guard.completed = true;
    result
}

/// A failed turn save leaves durability uncertain for every caller, including
/// surfaces that use [`AgentSession`] without the SDK recovery driver. Fence the
/// shared admission gate before returning the typed persistence error.
fn finish_turn_persistence<T>(
    admission: &ProviderAdmissionGate,
    result: Result<T>,
    persist_result: Result<()>,
) -> Result<T> {
    match persist_result {
        Ok(()) => result,
        Err(persist_err) => {
            let message = match result {
                Ok(_) => persist_err.to_string(),
                Err(primary_err) => {
                    format!("{persist_err}; primary provider/tool turn also failed: {primary_err}")
                }
            };
            admission.block(message.clone());
            Err(Error::session_persistence(message))
        }
    }
}

/// Persist mutations already protected by their Session lock using the same
/// interruption guard and returned-error fence as an AgentSession turn.
/// Extension callbacks cannot acquire the Agent while owning Session-action
/// authority, so they carry the shared admission gate to this boundary.
pub(crate) async fn persist_session_mutations(
    session: &mut Session,
    admission: &ProviderAdmissionGate,
    trigger: AutosaveFlushTrigger,
) -> Result<()> {
    let persist_result = flush_turn_autosave(session, admission, trigger).await;
    finish_turn_persistence(admission, Ok(()), persist_result)
}

#[cfg(test)]
mod finish_turn_persistence_tests {
    use super::*;

    #[test]
    fn pending_turn_save_does_not_block_or_replace_another_quarantine() {
        let admission = ProviderAdmissionGate::default();
        let guard = TurnPersistenceGuard {
            admission: &admission,
            completed: false,
        };
        admission
            .ensure_allowed()
            .expect("an active save must not reject an in-flight extension completion");
        admission.block("a separate transition failed".to_string());
        drop(guard);
        assert_eq!(
            admission.reason().as_deref(),
            Some("a separate transition failed")
        );
    }

    #[test]
    fn completed_no_op_turn_flush_preserves_existing_quarantine() {
        let admission = ProviderAdmissionGate::default();
        admission.block("an earlier save was interrupted".to_string());
        let mut session = Session::in_memory();
        futures::executor::block_on(flush_turn_autosave(
            &mut session,
            &admission,
            AutosaveFlushTrigger::Manual,
        ))
        .expect("an empty autosave queue needs no filesystem work");
        assert_eq!(session.autosave_metrics().flush_started, 0);
        assert_eq!(
            admission.reason().as_deref(),
            Some("an earlier save was interrupted")
        );
    }

    #[test]
    fn persistence_failure_is_terminal_without_hiding_primary_turn_error() {
        let admission = ProviderAdmissionGate::default();
        let err = finish_turn_persistence::<()>(
            &admission,
            Err(Error::provider("test", "provider failed")),
            Err(Error::session("disk flush failed")),
        )
        .expect_err("persistence failure must remain terminal");

        assert!(err.is_session_persistence());
        let message = err.to_string();
        assert!(message.contains("disk flush failed"));
        assert!(message.contains("provider failed"));
        let blocked = admission
            .ensure_allowed()
            .expect_err("every caller must retain the persistence fence");
        assert!(blocked.is_session_persistence());
        assert!(blocked.to_string().contains("provider failed"));
    }
}

impl AgentSession {
    fn job_session_id_resolver(session: &Arc<Mutex<Session>>) -> crate::jobs::JobSessionIdResolver {
        let job_session = Arc::clone(session);
        Arc::new(move || {
            let job_session = Arc::clone(&job_session);
            Box::pin(async move {
                let cx = crate::agent_cx::AgentCx::for_current_or_request();
                let session = OwnedMutexGuard::lock(job_session, cx.cx()).await.ok()?;
                Some(session.header.id.clone())
            })
        })
    }

    pub const fn runtime_repair_mode_from_policy_mode(mode: RepairPolicyMode) -> RepairMode {
        match mode {
            RepairPolicyMode::Off => RepairMode::Off,
            RepairPolicyMode::Suggest => RepairMode::Suggest,
            RepairPolicyMode::AutoSafe => RepairMode::AutoSafe,
            RepairPolicyMode::AutoStrict => RepairMode::AutoStrict,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_js_extension_runtime(
        stage: &'static str,
        cwd: &std::path::Path,
        tools: crate::tools::SharedToolRegistry,
        manager: ExtensionManager,
        policy: ExtensionPolicy,
        repair_mode: RepairMode,
        memory_limit_bytes: usize,
    ) -> Result<ExtensionRuntimeHandle> {
        let mut config = PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            repair_mode,
            ..PiJsRuntimeConfig::default()
        };
        config.limits.memory_limit_bytes = Some(memory_limit_bytes).filter(|bytes| *bytes > 0);

        let runtime =
            JsExtensionRuntimeHandle::start_with_policy(config, tools, manager, policy).await?;
        tracing::info!(
            event = "pi.extension_runtime.engine_decision",
            stage,
            requested = "quickjs",
            selected = "quickjs",
            fallback = false,
            "Extension runtime engine selected (legacy JS/TS)"
        );
        Ok(ExtensionRuntimeHandle::Js(runtime))
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_native_extension_runtime(
        stage: &'static str,
        _cwd: &std::path::Path,
        _tools: crate::tools::SharedToolRegistry,
        _manager: ExtensionManager,
        _policy: ExtensionPolicy,
        _repair_mode: RepairMode,
        _memory_limit_bytes: usize,
    ) -> Result<ExtensionRuntimeHandle> {
        let runtime = NativeRustExtensionRuntimeHandle::start().await?;
        tracing::info!(
            event = "pi.extension_runtime.engine_decision",
            stage,
            requested = "native-rust",
            selected = "native-rust",
            fallback = false,
            "Extension runtime engine selected (native-rust)"
        );
        Ok(ExtensionRuntimeHandle::NativeRust(runtime))
    }

    pub fn new(
        agent: Agent,
        session: Arc<Mutex<Session>>,
        save_enabled: bool,
        compaction_settings: ResolvedCompactionSettings,
    ) -> Self {
        // The job scope is shared by every snapshot of the registry, so
        // binding through the current one binds the live registry.
        agent
            .tools
            .snapshot()
            .bind_job_session_resolver(Self::job_session_id_resolver(&session));
        let extension_ai_completion = Arc::new(StdMutex::new(ExtensionAiCompletionHostState {
            provider: agent.provider(),
            stream_options: agent.stream_options().clone(),
            models: Vec::new(),
            compaction_models: ExtensionCompactionModelState::default(),
            compaction_privacy: Arc::clone(&agent.compaction_privacy_state),
        }));

        Self {
            agent,
            session,
            save_enabled,
            input_source: InputSource::Interactive,
            extensions: None,
            mcp_manager: None,
            extensions_is_streaming: Arc::new(AtomicBool::new(false)),
            extensions_is_compacting: Arc::new(AtomicBool::new(false)),
            extensions_turn_active: Arc::new(AtomicBool::new(false)),
            extensions_pending_idle_actions: Arc::new(StdMutex::new(VecDeque::new())),
            extension_queue_modes: None,
            extension_injected_queue: None,
            extension_ai_completion,
            compaction_settings,
            compaction_runtime: None,
            advisor: None,
            runtime_handle: None,
            compaction_worker: CompactionWorkerState::new(CompactionQuota::default()),
            model_registry: None,
            auth_storage: None,
            api_key_override: None,
            semantic_context_bundle: None,
            provider_admission: ProviderAdmissionGate::default(),
            session_action_admission: SessionActionAdmissionGate::default(),
        }
    }

    pub const fn set_input_source(&mut self, source: InputSource) {
        self.input_source = source;
    }

    /// Attach the MCP client registry that serves this session so its turn
    /// runner can sync servers extensions register after startup.
    pub fn set_mcp_manager(&mut self, manager: Arc<crate::mcp::McpManager>) {
        self.mcp_manager = Some(manager);
    }

    #[must_use]
    pub fn mcp_manager(&self) -> Option<Arc<crate::mcp::McpManager>> {
        self.mcp_manager.clone()
    }

    #[must_use]
    pub fn with_runtime_handle(mut self, runtime_handle: RuntimeHandle) -> Self {
        self.compaction_runtime = None;
        self.runtime_handle = Some(runtime_handle);
        self
    }

    /// The runtime this session dispatches background work on, if it has one.
    ///
    /// Set either explicitly by [`Self::with_runtime_handle`] or lazily by
    /// `compaction_runtime_handle` when background compaction first needs a
    /// runtime and none was supplied. So a `Some` here does not imply a caller
    /// provided it, and a `None` means nothing has needed one yet — not that
    /// none will ever exist.
    ///
    /// Exposed for surfaces that need to route agent events to extensions:
    /// `EventCoalescer::dispatch_agent_event_lazy` spawns onto a runtime, and
    /// a surface holding only an `AgentSession` had no way to reach one. See
    /// bd-82331, where the default interactive stack silently delivered no
    /// observation events to extensions because its event path could not build
    /// a coalescer.
    #[must_use]
    pub const fn runtime_handle(&self) -> Option<&RuntimeHandle> {
        self.runtime_handle.as_ref()
    }

    #[must_use]
    pub fn with_model_registry(mut self, registry: ModelRegistry) -> Self {
        self.set_model_registry(registry);
        self
    }

    #[must_use]
    pub fn with_auth_storage(mut self, auth: AuthStorage) -> Self {
        self.set_auth_storage(auth);
        self
    }

    pub fn set_model_registry(&mut self, registry: ModelRegistry) {
        let provider = self.agent.provider();
        let entry = registry.find(provider.name(), provider.model_id());
        let keyword_max = entry
            .as_ref()
            .map_or(self.agent.keyword_max_thinking_level, |entry| {
                entry.clamp_thinking_level(crate::model::ThinkingLevel::Max)
            });
        self.agent.set_keyword_max_thinking_level(keyword_max);
        self.agent.set_tool_call_dialect(entry.as_ref().map_or_else(
            || self.agent.tool_call_dialect(),
            ModelEntry::tool_call_dialect,
        ));
        self.set_extension_ai_models(pi_ai_model_registry_values(&registry));
        // Keep the extension ctx catalog in sync when the registry is
        // replaced after extension boot (e.g. after merging extension
        // providers in main). No-op before boot; boot seeds it (gh #167).
        if let Some(region) = &self.extensions {
            region
                .manager()
                .set_extension_models(pi_ai_model_registry_values(&registry));
        }
        self.model_registry = Some(registry);
        self.refresh_extension_compaction_models();
    }

    pub fn set_auth_storage(&mut self, auth: AuthStorage) {
        self.auth_storage = Some(auth);
        self.refresh_extension_compaction_models();
    }

    pub(crate) fn scope_auth_storage(&self, auth: &mut AuthStorage) {
        if let Some(previous) = &self.auth_storage {
            auth.inherit_command_working_directory(previous);
        }
    }

    /// Adopt credentials changed outside this session (`/login`, `/logout`)
    /// and re-resolve the running model's key with the usual precedence:
    /// CLI override, then stored credential, then the catalog entry's key.
    pub(crate) fn adopt_auth_storage(&mut self, mut auth: AuthStorage) {
        self.scope_auth_storage(&mut auth);
        self.set_auth_storage(auth);
        let entry = self.current_model_entry();
        let key = entry.as_ref().map_or_else(
            || {
                let provider = self.agent.provider();
                normalize_api_key_opt(self.api_key_override.clone()).or_else(|| {
                    self.auth_storage.as_ref().and_then(|auth| {
                        normalize_api_key_opt(auth.resolve_api_key(provider.name(), None))
                    })
                })
            },
            |entry| self.resolve_stream_api_key_for_model(entry),
        );
        self.agent.stream_options_mut().api_key = key;
        self.refresh_extension_completion_host_state();
    }

    pub(crate) const fn model_registry(&self) -> Option<&ModelRegistry> {
        self.model_registry.as_ref()
    }

    #[must_use]
    pub fn with_api_key_override(mut self, api_key: Option<String>) -> Self {
        self.set_api_key_override(api_key);
        self
    }

    pub fn set_api_key_override(&mut self, api_key: Option<String>) {
        self.api_key_override = normalize_api_key_opt(api_key);
    }

    pub fn refresh_extension_completion_host_state(&self) {
        let Ok(mut state) = self.extension_ai_completion.lock() else {
            tracing::error!("extension completion host state mutex poisoned; keeping stale state");
            return;
        };
        state.provider = self.agent.provider();
        state.stream_options = self.agent.stream_options().clone();
    }

    fn refresh_extension_compaction_models(&self) {
        // Only extensions read this state (`ctx.compact` with a `model`), and
        // building it deep-copies the whole model registry and auth store.
        // Startup sets both, so a session without extensions paid for two
        // copies it could never use, several milliseconds of every start.
        // Installing extensions calls this again, so they still get it.
        if self.extensions.is_none() {
            return;
        }
        let models = ExtensionCompactionModelState {
            registry: self.model_registry.clone().map(Arc::new),
            auth: self.auth_storage.clone().map(Arc::new),
            js_provider_ids: self.extensions.as_ref().map_or_else(Vec::new, |region| {
                region
                    .manager()
                    .extension_providers()
                    .iter()
                    .filter(|spec| {
                        spec.get("hasStreamSimple")
                            .and_then(Value::as_bool)
                            .or_else(|| spec.get("streamSimple").and_then(Value::as_bool))
                            .unwrap_or(false)
                    })
                    .filter_map(|spec| spec.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            }),
        };
        let Ok(mut state) = self.extension_ai_completion.lock() else {
            tracing::error!(
                "extension completion host state mutex poisoned; keeping stale compaction registry"
            );
            return;
        };
        state.compaction_models = models;
    }

    fn set_extension_ai_models(&self, models: Vec<Value>) {
        let Ok(mut state) = self.extension_ai_completion.lock() else {
            tracing::error!(
                "extension completion host state mutex poisoned; keeping stale model catalog"
            );
            return;
        };
        state.models = models;
    }

    pub fn set_semantic_context_bundle(
        &mut self,
        injection: Option<SemanticContextBundleInjection>,
    ) {
        self.semantic_context_bundle = injection;
    }

    pub const fn semantic_context_bundle(&self) -> Option<&SemanticContextBundleInjection> {
        self.semantic_context_bundle.as_ref()
    }

    pub fn set_queue_modes(&mut self, steering_mode: QueueMode, follow_up_mode: QueueMode) {
        self.agent.set_queue_modes(steering_mode, follow_up_mode);

        if let Some(queue_modes) = &self.extension_queue_modes
            && let Ok(mut state) = queue_modes.lock()
        {
            state.set_modes(steering_mode, follow_up_mode);
        }

        if let Some(injected_queue) = &self.extension_injected_queue
            && let Ok(mut queue) = injected_queue.lock()
        {
            queue.set_modes(steering_mode, follow_up_mode);
        }
    }

    pub const fn set_compaction_context_window(&mut self, context_window_tokens: u32) {
        self.compaction_settings.context_window_tokens = context_window_tokens;
    }

    /// The resolved compaction settings this session was constructed with.
    pub const fn compaction_settings(&self) -> &ResolvedCompactionSettings {
        &self.compaction_settings
    }

    /// Enable or pause automatic compaction without changing the model's
    /// context window or the configured summary/token budgets. Disabling it
    /// also invalidates a summary already running in the background.
    pub fn set_auto_compaction(&mut self, enabled: bool) {
        if !enabled {
            self.invalidate_background_compaction();
        }
        self.compaction_settings.enabled = enabled;
    }

    pub(crate) fn provider_admission_gate(&self) -> ProviderAdmissionGate {
        self.provider_admission.clone()
    }

    pub(crate) fn session_action_admission_gate(&self) -> SessionActionAdmissionGate {
        self.session_action_admission.clone()
    }

    pub(crate) fn ensure_provider_reentry_allowed(&self) -> Result<()> {
        self.provider_admission.ensure_allowed()
    }

    fn quarantine_provider_reentry(&self, reason: String) {
        self.provider_admission.block(reason);
    }

    fn clear_provider_reentry_quarantine(&self) {
        self.provider_admission.clear();
    }

    fn prepare_model_selection(
        &self,
        provider_id: &str,
        model_id: &str,
        requested_thinking: crate::model::ThinkingLevel,
    ) -> Result<PreparedModelSelection> {
        let active_provider = self.agent.provider();
        let requested_matches_active =
            active_provider.name().eq(provider_id) && active_provider.model_id().eq(model_id);
        let entry = self
            .model_registry
            .as_ref()
            .and_then(|registry| registry.find(provider_id, model_id));

        let Some(entry) = entry else {
            if !requested_matches_active {
                return Err(Error::validation(format!(
                    "Unable to switch provider/model to {provider_id}/{model_id}"
                )));
            }
            return Ok(PreparedModelSelection {
                entry: None,
                provider: None,
                resolved_key: self.agent.stream_options().api_key.clone(),
                provider_id: active_provider.name().to_string(),
                model_id: active_provider.model_id().to_string(),
                thinking_level: requested_thinking,
            });
        };

        let canonical_provider_id = entry.model.provider.clone();
        let canonical_model_id = entry.model.id.clone();
        let already_active = active_provider.name().eq(&canonical_provider_id)
            && active_provider.model_id().eq(&canonical_model_id);
        let requires_credential = model_requires_configured_credential(&entry);
        let resolved_key = self.resolve_stream_api_key_for_model(&entry).or_else(|| {
            (already_active && requires_credential)
                .then(|| self.agent.stream_options().api_key.clone())
                .flatten()
        });
        if requires_credential && resolved_key.is_none() {
            return Err(Error::auth(format!(
                "Missing credentials for {provider_id}/{model_id}"
            )));
        }
        let provider = if already_active {
            None
        } else {
            Some(
                crate::providers::create_provider_with_auth(
                    &entry,
                    self.extensions.as_ref().map(ExtensionRegion::manager),
                    self.auth_storage.as_ref(),
                )
                .map_err(|e| {
                    Error::validation(format!(
                        "Unable to switch provider/model to {provider_id}/{model_id}: {e}"
                    ))
                })?,
            )
        };
        let thinking_level = entry.clamp_thinking_level(requested_thinking);

        Ok(PreparedModelSelection {
            entry: Some(entry),
            provider,
            resolved_key,
            provider_id: canonical_provider_id,
            model_id: canonical_model_id,
            thinking_level,
        })
    }

    fn install_prepared_model_selection(&mut self, prepared: PreparedModelSelection) {
        let active_provider = self.agent.provider();
        if active_provider.name() != prepared.provider_id
            || active_provider.model_id() != prepared.model_id
        {
            self.invalidate_background_compaction();
        }
        let PreparedModelSelection {
            entry,
            provider,
            resolved_key,
            provider_id,
            model_id,
            thinking_level,
        } = prepared;
        if let Some(provider) = provider {
            tracing::info!("Updating agent provider to {provider_id}/{model_id}");
            self.agent.set_provider(provider);
        }

        if let Some(entry) = entry {
            self.agent.set_keyword_max_thinking_level(
                entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
            );
            self.agent.set_tool_call_dialect(entry.tool_call_dialect());
            self.agent.set_model_accepts_images(
                entry
                    .model
                    .input
                    .contains(&crate::provider::InputType::Image),
            );
            {
                let stream_options = self.agent.stream_options_mut();
                stream_options.api_key = resolved_key;
                stream_options.headers.clone_from(&entry.headers);
                stream_options.max_tokens = Some(entry.model.max_tokens);
                stream_options.thinking_level = Some(thinking_level);
            }
            self.set_compaction_context_window(if entry.model.context_window == 0 {
                ResolvedCompactionSettings::default().context_window_tokens
            } else {
                entry.model.context_window
            });
        } else {
            self.agent.stream_options_mut().thinking_level = Some(thinking_level);
        }

        if let Some(region) = &self.extensions {
            region
                .manager()
                .set_current_model(Some(provider_id), Some(model_id));
        }
        self.refresh_extension_completion_host_state();
    }

    pub async fn set_provider_model(&mut self, provider_id: &str, model_id: &str) -> Result<()> {
        self.ensure_provider_reentry_allowed()?;
        let current_thinking = self
            .agent
            .stream_options()
            .thinking_level
            .unwrap_or_default();
        let prepared = self.prepare_model_selection(provider_id, model_id, current_thinking)?;
        let target_provider_id = prepared.provider_id.clone();
        let target_model_id = prepared.model_id.clone();
        let next_thinking = prepared.thinking_level;
        let active_provider = self.agent.provider();
        let runtime_model_changed = active_provider.name() != target_provider_id
            || active_provider.model_id() != target_model_id;

        let cx = crate::agent_cx::AgentCx::for_request();
        let save_enabled = self.save_enabled;
        let session_store = Arc::clone(&self.session);
        let mut session = OwnedMutexGuard::lock(session_store, cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        let mut candidate = session.clone();
        let previous_model = candidate.effective_model_for_current_path();
        let previous_thinking = candidate
            .effective_thinking_level_for_current_path()
            .as_deref()
            .and_then(|value| value.parse::<crate::model::ThinkingLevel>().ok());
        if previous_model
            .as_ref()
            .map(|(provider, model_id)| (provider.as_str(), model_id.as_str()))
            != Some((target_provider_id.as_str(), target_model_id.as_str()))
            || candidate
                .active_failover_provenance_for_current_path()
                .is_some()
        {
            // Explicitly choosing the active fallback makes it the user's
            // primary. Persist that intent even when its identity is unchanged,
            // or reopening the session revives the old automatic restoration.
            candidate.append_model_change(target_provider_id.clone(), target_model_id.clone());
        }
        candidate.set_model_header(
            Some(target_provider_id),
            Some(target_model_id),
            Some(next_thinking.to_string()),
        );
        if !previous_thinking.is_some_and(|previous| previous.eq(&next_thinking)) {
            candidate.append_thinking_level_change(next_thinking.to_string());
        }
        let _provider_transition = self
            .provider_admission
            .begin_transition(
                "model selection persistence was interrupted before live installation completed"
                    .to_string(),
                cx.cx(),
            )
            .await?;
        if save_enabled
            && let Err(first_err) = candidate.save().await
            && let Err(retry_err) = candidate.save().await
        {
            let reason = format!(
                "model selection persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
            );
            self.quarantine_provider_reentry(reason.clone());
            return Err(Error::session_persistence(reason));
        }
        // Only a durable switch retires in-flight background compaction; a
        // failed save leaves the old model live, so its compaction stays valid.
        if runtime_model_changed {
            self.invalidate_background_compaction();
        }
        *session = candidate;
        drop(session);
        self.install_prepared_model_selection(prepared);
        self.clear_provider_reentry_quarantine();
        Ok(())
    }

    /// Update the thinking/reasoning level for this session at runtime.
    ///
    /// Clamps the requested level to what the active model supports (e.g. a
    /// non-reasoning model is forced to `Off`), records a thinking-level change
    /// in session history when it actually changes, and persists the session.
    /// Mirrors [`crate::sdk::AgentSessionHandle::set_thinking_level`] but is
    /// callable directly on an [`AgentSession`] (e.g. from the ACP transport,
    /// which holds an `AgentSession` rather than an SDK handle).
    pub async fn set_thinking_level(&mut self, level: crate::model::ThinkingLevel) -> Result<()> {
        self.ensure_provider_reentry_allowed()?;
        let cx = crate::agent_cx::AgentCx::for_request();
        let save_enabled = self.save_enabled;
        let session_store = Arc::clone(&self.session);
        let mut session = OwnedMutexGuard::lock(session_store, cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        let mut candidate = session.clone();
        let (provider_id, model_id) =
            candidate
                .effective_model_for_current_path()
                .unwrap_or_else(|| {
                    let provider = self.agent.provider();
                    (provider.name().to_string(), provider.model_id().to_string())
                });
        let effective_level = self.clamp_thinking_level_for_model(&provider_id, &model_id, level);
        let level_string = effective_level.to_string();
        let changed = candidate
            .effective_thinking_level_for_current_path()
            .as_deref()
            != Some(level_string.as_str());
        // An unchanged level is not saved below, so it must not mutate the
        // header either: the installed session would diverge from disk.
        if changed {
            candidate.set_model_header(None, None, Some(level_string.clone()));
            candidate.append_thinking_level_change(level_string);
        }
        let _provider_transition = if changed {
            Some(
                self.provider_admission
                    .begin_transition(
                        "thinking-level persistence was interrupted before live installation completed"
                            .to_string(),
                        cx.cx(),
                    )
                    .await?,
            )
        } else {
            None
        };
        if save_enabled
            && changed
            && let Err(first_err) = candidate.save().await
            && let Err(retry_err) = candidate.save().await
        {
            let reason = format!(
                "thinking-level persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
            );
            self.quarantine_provider_reentry(reason.clone());
            return Err(Error::session_persistence(reason));
        }
        *session = candidate;
        self.agent.stream_options_mut().thinking_level = Some(effective_level);
        self.refresh_extension_completion_host_state();
        if changed {
            self.clear_provider_reentry_quarantine();
        }
        Ok(())
    }

    /// The catalog entry for the model this session is currently running.
    ///
    /// `None` when no registry was installed or the running model is not in
    /// it — an extension-registered or ad-hoc model, say. Callers that need a
    /// model's declared capabilities (thinking levels, context window) should
    /// resolve through here rather than re-deriving them, so a session that
    /// cannot be resolved degrades the same way everywhere.
    pub fn current_model_entry(&self) -> Option<ModelEntry> {
        let provider = self.agent.provider();
        self.model_registry
            .as_ref()
            .and_then(|registry| registry.find(provider.name(), provider.model_id()))
    }

    pub(crate) fn clamp_thinking_level_for_model(
        &self,
        provider_id: &str,
        model_id: &str,
        level: crate::model::ThinkingLevel,
    ) -> crate::model::ThinkingLevel {
        self.model_registry
            .as_ref()
            .and_then(|registry| registry.find(provider_id, model_id))
            .map_or(level, |entry| entry.clamp_thinking_level(level))
    }

    fn resolve_stream_api_key_for_model(&self, entry: &ModelEntry) -> Option<String> {
        let normalize = |key_opt: Option<String>| {
            key_opt.and_then(|key| {
                let trimmed = key.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            })
        };

        normalize(self.api_key_override.clone())
            .or_else(|| {
                self.auth_storage
                    .as_ref()
                    .and_then(|auth| normalize(auth.resolve_api_key(&entry.model.provider, None)))
            })
            .or_else(|| normalize(entry.api_key.clone()))
    }

    pub(crate) async fn sync_runtime_selection_from_session_header(&mut self) -> Result<()> {
        self.ensure_provider_reentry_allowed()?;
        let cx = crate::agent_cx::AgentCx::for_request();
        let session_store = Arc::clone(&self.session);
        let mut session = OwnedMutexGuard::lock(session_store, cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        let mut candidate = session.clone();
        let session_model = candidate.effective_model_for_current_path();
        let session_thinking = candidate.effective_thinking_level_for_current_path();
        let current_thinking = self
            .agent
            .stream_options()
            .thinking_level
            .unwrap_or_default();
        let parsed_session_thinking = session_thinking.as_deref().and_then(|raw| {
            raw.parse::<crate::model::ThinkingLevel>().map_or_else(
                |_| {
                    tracing::warn!("Ignoring invalid session thinking level: {raw}");
                    None
                },
                Some,
            )
        });
        let requested = parsed_session_thinking.unwrap_or(current_thinking);
        let (requested_provider_id, requested_model_id) = session_model.unwrap_or_else(|| {
            let provider = self.agent.provider();
            (provider.name().to_string(), provider.model_id().to_string())
        });
        let prepared =
            self.prepare_model_selection(&requested_provider_id, &requested_model_id, requested)?;
        let canonical_provider_id = prepared.provider_id.clone();
        let canonical_model_id = prepared.model_id.clone();
        let effective = prepared.thinking_level;
        let effective_string = effective.to_string();
        let active_provider = self.agent.provider();
        let runtime_model_changed = active_provider.name() != canonical_provider_id
            || active_provider.model_id() != canonical_model_id;

        let previous_model = candidate.effective_model_for_current_path();
        let previous_thinking = candidate
            .effective_thinking_level_for_current_path()
            .as_deref()
            .and_then(|value| value.parse::<crate::model::ThinkingLevel>().ok());
        let model_changed = previous_model
            .as_ref()
            .map(|(provider, model_id)| (provider.as_str(), model_id.as_str()))
            != Some((canonical_provider_id.as_str(), canonical_model_id.as_str()));
        let thinking_changed = !previous_thinking.is_some_and(|level| level.eq(&effective));
        let header_changed = candidate.header.provider.as_deref()
            != Some(canonical_provider_id.as_str())
            || candidate.header.model_id.as_deref() != Some(canonical_model_id.as_str())
            || candidate.header.thinking_level.as_deref() != Some(effective_string.as_str());

        if model_changed {
            candidate
                .append_model_change(canonical_provider_id.clone(), canonical_model_id.clone());
        }
        candidate.set_model_header(
            Some(canonical_provider_id),
            Some(canonical_model_id),
            Some(effective_string.clone()),
        );
        if thinking_changed {
            candidate.append_thinking_level_change(effective_string);
        }

        let persist_needed = model_changed || thinking_changed || header_changed;
        let save_enabled = self.save_enabled;
        if runtime_model_changed {
            self.invalidate_background_compaction();
        }
        let _provider_transition = self
            .provider_admission
            .begin_transition(
                "Session-header synchronization persistence was interrupted before live installation completed"
                    .to_string(),
                cx.cx(),
            )
            .await?;
        if save_enabled
            && persist_needed
            && let Err(first_err) = candidate.save().await
            && let Err(retry_err) = candidate.save().await
        {
            let reason = format!(
                "Session-header synchronization persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
            );
            self.quarantine_provider_reentry(reason.clone());
            return Err(Error::session_persistence(reason));
        }

        *session = candidate;
        drop(session);
        self.install_prepared_model_selection(prepared);
        self.clear_provider_reentry_quarantine();
        Ok(())
    }

    pub const fn save_enabled(&self) -> bool {
        self.save_enabled
    }

    pub(crate) fn invalidate_background_compaction(&mut self) {
        self.compaction_worker.invalidate_for_context_switch();
        self.extensions_is_compacting
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a background compaction task is currently pending.
    #[must_use]
    pub const fn has_pending_background_compaction(&self) -> bool {
        self.compaction_worker.has_pending()
    }

    /// Inject a parked background compaction task for testing context-switch invalidation.
    pub fn park_pending_compaction_for_test(
        &mut self,
        runtime_handle: &RuntimeHandle,
        aborted: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) {
        self.compaction_worker
            .park_pending_for_test(runtime_handle, aborted);
        self.extensions_is_compacting
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Strip the failed request's incomplete output so a retry RESUMES the turn
    /// instead of replaying it.
    ///
    /// A transient drop leaves a partial or error assistant message on the
    /// path. Reverting only that keeps the user prompt and every completed tool
    /// cycle, so the retry re-issues one provider request rather than re-running
    /// tools and re-billing work already done (pi_agent_rust#125).
    ///
    /// The whole transition is built on a private `Session` candidate: the live
    /// transcript and the agent's message list are untouched unless the revert
    /// and its persistence both succeed. A save that stays indeterminate after
    /// one idempotent retry is reported as a session-persistence failure, which
    /// every surface treats as terminal (bd-8188r) — silently continuing there
    /// could repeat side effects against a record that no longer describes them.
    ///
    /// `require_incomplete_tail` is for the caller that has already seen a
    /// COMPLETED error response: there must be an incomplete tail to revert, and
    /// its absence means the caller's model of the turn is wrong rather than
    /// that there is nothing to do.
    ///
    /// Print mode and the RPC server each had their own copy of this, down to
    /// the same error prose (bd-u2qv4). `admission` is what was genuinely
    /// RPC's: the gate is blocked for the window between persistence and live
    /// installation, so a crash in between leaves provider re-entry quarantined
    /// rather than silently resumed. Surfaces without such a gate pass `None`.
    /// It is taken here rather than wrapped around the call so the lock order —
    /// inner session first, admission permit second — stays exactly as it was.
    pub async fn restore_retry_tail(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        require_incomplete_tail: bool,
    ) -> Result<()> {
        self.restore_retry_tail_with_admission(cx, require_incomplete_tail, None)
            .await
    }

    /// [`Self::restore_retry_tail`] with RPC's provider-admission gate. The gate
    /// type is internal, so this stays crate-visible while the plain form above
    /// is what an embedder driving its own retry loop calls.
    pub(crate) async fn restore_retry_tail_with_admission(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        require_incomplete_tail: bool,
        admission: Option<&ProviderAdmissionGate>,
    ) -> Result<()> {
        let session_store = Arc::clone(&self.session);
        let mut inner = OwnedMutexGuard::lock(session_store, cx)
            .await
            .map_err(|err| {
                Error::session(format!("retry restoration session lock failed: {err}"))
            })?;
        let mut candidate = inner.clone();
        let reverted = candidate.revert_incomplete_response();
        if require_incomplete_tail && !reverted {
            return Err(Error::session(
                "retry restoration invariant failed: the completed error response had no incomplete assistant tail",
            ));
        }
        if !reverted {
            return Ok(());
        }

        let restored_messages = candidate.to_messages_for_current_path();
        let save_enabled = self.save_enabled();
        let _provider_transition = match admission {
            Some(gate) => Some(
                gate.begin_transition(
                    "retry restoration persistence was interrupted before live installation completed"
                        .to_string(),
                    cx,
                )
                .await?,
            ),
            None => None,
        };
        if save_enabled
            && let Err(first_err) = candidate.save().await
            && let Err(retry_err) = candidate.save().await
        {
            let reason = format!(
                "retry restoration persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
            );
            if let Some(gate) = admission {
                gate.block(reason.clone());
            }
            return Err(Error::session_persistence(reason));
        }

        // The message list is about to change underneath any background
        // compaction computed against the old one. RPC has always invalidated
        // here; print mode did not, so a compaction prepared before the revert
        // could be applied after it. Discarding prepared work is the safe
        // direction — the worker recomputes — so the shared path does it.
        self.invalidate_background_compaction();
        *inner = candidate;
        self.agent.replace_messages(restored_messages);
        if let Some(gate) = admission {
            gate.clear();
        }
        Ok(())
    }

    /// Everything a fallback-chain swap does to the session and the live agent.
    ///
    /// Print mode and the RPC server each carried this whole transition — revert
    /// the failed tail, record the swap in the transcript, persist, then install
    /// the new provider and its options — and the two install blocks were
    /// identical line for line (bd-u2qv4). A duplicated persisted-mutation path
    /// is the worst kind to let drift: a fallback running with the wrong tool
    /// dialect, thinking clamp or context window is a silent behaviour change
    /// nobody would attribute to a failover.
    ///
    /// The whole transition is built on a private `Session` candidate, so the
    /// live transcript, provider and options are untouched unless the revert and
    /// its persistence both succeed. A save that stays indeterminate after one
    /// idempotent retry is a session-persistence failure, which every surface
    /// treats as terminal (bd-8188r).
    ///
    /// Returns the thinking level actually installed, which callers record for
    /// their own restoration bookkeeping.
    pub async fn commit_failover(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        request: &FailoverSwapRequest<'_>,
    ) -> Result<crate::model::ThinkingLevel> {
        self.commit_failover_swap(cx, request, None).await
    }

    /// Walk a fallback chain and install the first entry that is actually
    /// usable, or report that the chain held nothing.
    ///
    /// The walk is bounded by the chain, never by `max_failovers_per_turn`: the
    /// caller counts committed swaps against that cap. Bounding the cursor by
    /// the cap let malformed, uncredentialed, unconstructible, current or
    /// duplicate entries consume the budget and hide a later valid entry
    /// (bd-oqo03.1). An entry with no usable credential is skipped rather than
    /// installed — failing over into an auth error is strictly worse than the
    /// quota error that started this.
    ///
    /// Print mode and the RPC server each had this loop, and it is what a third
    /// surface would otherwise copy: the interactive stacks, where a configured
    /// chain is currently inert (bd-u2qv4). Classification, chain resolution
    /// and event emission stay with the caller, because those genuinely differ.
    pub async fn try_failover(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        attempt: &FailoverSwapAttempt<'_>,
    ) -> Result<FailoverSwapOutcome> {
        self.try_failover_swap(cx, attempt, None).await
    }

    /// Reinstall the primary captured at the first swap, once its cooldown has
    /// elapsed.
    ///
    /// The counterpart to [`Self::try_failover`]: a failover is only half a
    /// policy if nothing ever goes back. Without this a single 429 pins a
    /// long-lived session to a fallback model for as long as it runs, which on
    /// an interactive surface can be hours.
    ///
    /// Returns `Ok(None)` when nothing was installed — the cooldown has not
    /// elapsed, there is no fallback live, the primary no longer resolves, it
    /// has no usable credential, or (in lenient mode) the caller's record of
    /// the live model is stale. Every one of those leaves the working fallback
    /// installed, which is the right outcome: restoring into an auth error or a
    /// half-written transition is strictly worse than staying on a model that
    /// works.
    ///
    /// Print mode and the RPC server each had their own copy of this, and the
    /// interactive stacks had none at all (bd-u2qv4, bd-gm481).
    pub async fn restore_primary(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        request: &PrimaryRestoreRequest<'_>,
    ) -> Result<Option<RestoredPrimary>> {
        self.restore_primary_swap(cx, request, None).await
    }

    /// [`Self::restore_primary`] with RPC's provider-admission gate. The gate
    /// type is internal, so this stays crate-visible while the plain form above
    /// is what print mode and any embedder call.
    pub(crate) async fn restore_primary_swap(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        request: &PrimaryRestoreRequest<'_>,
        admission: Option<&ProviderAdmissionGate>,
    ) -> Result<Option<RestoredPrimary>> {
        self.restore_primary_swap_admitted(cx, request, admission.map(ProviderSwapAdmission::Gate))
            .await
    }

    pub(crate) async fn restore_primary_with_authority(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        request: &PrimaryRestoreRequest<'_>,
        authority: &ProviderTransitionAuthority,
    ) -> Result<Option<RestoredPrimary>> {
        self.restore_primary_swap_admitted(
            cx,
            request,
            Some(ProviderSwapAdmission::Held(authority)),
        )
        .await
    }

    async fn restore_primary_swap_admitted(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        request: &PrimaryRestoreRequest<'_>,
        admission: Option<ProviderSwapAdmission<'_>>,
    ) -> Result<Option<RestoredPrimary>> {
        let gate = admission.map(ProviderSwapAdmission::gate);
        let (active_provider, active_model) = request.active;
        let catalog = self
            .model_registry
            .as_ref()
            .map_or(request.available_models, ModelRegistry::models);

        // The runtime must still be on the fallback the caller recorded.
        let runtime_provider = self.agent.provider();
        if !crate::models::model_identities_match(
            catalog,
            runtime_provider.name(),
            runtime_provider.model_id(),
            active_provider,
            active_model,
        ) {
            return Self::refuse_restore(
                request,
                gate,
                format!(
                    "primary restore invariant failed: runtime {}/{} does not match recorded fallback {active_provider}/{active_model}",
                    runtime_provider.name(),
                    runtime_provider.model_id()
                ),
            );
        }

        let session_store = Arc::clone(&self.session);
        let mut inner = OwnedMutexGuard::lock(session_store, cx)
            .await
            .map_err(|err| Error::session(format!("primary restore inner lock failed: {err}")))?;
        let session_matches_active = inner.effective_model_for_current_path().is_some_and(
            |(session_provider, session_model)| {
                crate::models::model_identities_match(
                    catalog,
                    &session_provider,
                    &session_model,
                    active_provider,
                    active_model,
                )
            },
        );
        if !session_matches_active {
            return Self::refuse_restore(
                request,
                gate,
                format!(
                    "primary restore invariant failed: Session path does not match recorded fallback {active_provider}/{active_model}"
                ),
            );
        }

        // Checked here, after the invariants, so a session whose runtime has
        // drifted is still reported rather than hidden behind a live cooldown.
        if !request.cooldown_elapsed {
            return Ok(None);
        }

        let Some((entry, key)) = self.resolve_restore_target(request)? else {
            return Ok(None);
        };
        let provider_impl = match crate::providers::create_provider_with_auth(
            &entry,
            self.extensions.as_ref().map(ExtensionRegion::manager),
            Some(request.auth),
        ) {
            Ok(provider_impl) => provider_impl,
            Err(err) if request.strict_invariants => return Err(err),
            Err(_) => return Ok(None),
        };

        let to_provider = entry.model.provider.clone();
        let to_model = entry.model.id.clone();
        let (mut candidate, target_thinking) =
            Self::prepare_restore_candidate(&inner, &entry, request.primary);

        let save_enabled = self.save_enabled();
        if request.invalidate_background_compaction {
            self.invalidate_background_compaction();
        }
        let _provider_transition = match admission {
            Some(admission) => {
                admission
                    .begin_transition(
                        "primary restore persistence was interrupted before live installation completed"
                            .to_string(),
                        cx,
                    )
                    .await?
            }
            None => None,
        };
        if save_enabled
            && let Err(first_err) = candidate.save().await
            && let Err(retry_err) = candidate.save().await
        {
            // Leave the fallback installed. A half-written restoration is worse
            // than a working session on the wrong model.
            let reason = format!(
                "primary restore persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
            );
            if let Some(gate) = gate {
                gate.block(reason.clone());
            }
            if request.strict_invariants {
                return Err(Error::session_persistence(reason));
            }
            return Ok(None);
        }

        // No fallible operation remains after installing the candidate.
        *inner = candidate;
        self.install_restored_entry(&entry, provider_impl, key, target_thinking);
        if let Some(gate) = gate {
            gate.clear();
        }
        drop(inner);
        Ok(Some(RestoredPrimary {
            provider: to_provider,
            model: to_model,
        }))
    }

    /// Resolve the primary to a model entry and a credential, or decline.
    ///
    /// `Ok(None)` is the lenient decline; the same conditions are hard errors
    /// under `strict_invariants`, with the error kinds RPC has always returned
    /// for them.
    fn resolve_restore_target(
        &self,
        request: &PrimaryRestoreRequest<'_>,
    ) -> Result<Option<(crate::models::ModelEntry, Option<String>)>> {
        let primary = request.primary;
        let catalog = self
            .model_registry
            .as_ref()
            .map_or(request.available_models, ModelRegistry::models);
        let Some(entry) =
            crate::models::resolve_model_entry(catalog, &primary.provider, &primary.model_id)
        else {
            if request.strict_invariants {
                return Err(Error::validation(format!(
                    "Unable to restore primary provider/model {}/{}",
                    primary.provider, primary.model_id
                )));
            }
            return Ok(None);
        };

        let registered =
            crate::models::find_model_entry(catalog, &primary.provider, &primary.model_id)
                .is_some();
        let admitted = request.available_models.iter().any(|candidate| {
            candidate
                .model
                .provider
                .trim()
                .eq_ignore_ascii_case(entry.model.provider.trim())
                && candidate.model.id.eq_ignore_ascii_case(&entry.model.id)
        });
        let explicit_key_recovers_unready = request
            .cli_api_key
            .is_some_and(|key| !key.trim().is_empty())
            && !crate::models::model_entry_is_ready(&entry);
        if registered && !admitted && !explicit_key_recovers_unready {
            // Availability and disabled-provider filtering happen after the
            // full catalog establishes identity. A ready alias's row cannot
            // admit a separately configured primary that was filtered out.
            if request.strict_invariants {
                return Err(Error::validation(format!(
                    "Unable to restore primary provider/model {}/{}: the requested destination is not in the admitted model catalog",
                    primary.provider, primary.model_id
                )));
            }
            return Ok(None);
        }

        let key = crate::models::resolve_model_key(request.cli_api_key, request.auth, &entry);
        if crate::models::model_requires_configured_credential(&entry) && key.is_none() {
            // Restoring into an auth error would be strictly worse than staying
            // on a working fallback.
            if request.strict_invariants {
                return Err(Error::auth(format!(
                    "Missing credentials for primary provider/model {}/{}",
                    primary.provider, primary.model_id
                )));
            }
            return Ok(None);
        }
        Ok(Some((entry, key)))
    }

    /// Stage the restoration on a private `Session` candidate, so the live
    /// transcript is untouched unless its persistence succeeds.
    fn prepare_restore_candidate(
        inner: &Session,
        entry: &crate::models::ModelEntry,
        primary: &crate::failover::FailoverPrimary,
    ) -> (Session, crate::model::ThinkingLevel) {
        let to_provider = entry.model.provider.clone();
        let to_model = entry.model.id.clone();
        let target_thinking = entry.clamp_thinking_level(primary.requested_thinking_level);
        let target_thinking_text = target_thinking.to_string();
        let mut candidate = inner.clone();
        let thinking_changed = candidate
            .effective_thinking_level_for_current_path()
            .as_deref()
            != Some(target_thinking_text.as_str());
        candidate.set_model_header(
            Some(to_provider.clone()),
            Some(to_model.clone()),
            Some(target_thinking_text.clone()),
        );
        candidate.append_model_change_with_role(
            to_provider,
            to_model,
            Some("primary_restore".to_string()),
        );
        if thinking_changed {
            candidate.append_thinking_level_change(target_thinking_text);
        }
        (candidate, target_thinking)
    }

    /// The infallible half of the transition: no operation here can fail, so
    /// the live agent can never be left describing a model it is not using.
    fn install_restored_entry(
        &mut self,
        entry: &crate::models::ModelEntry,
        provider_impl: Arc<dyn Provider>,
        key: Option<String>,
        target_thinking: crate::model::ThinkingLevel,
    ) {
        self.agent.set_provider(provider_impl);
        self.agent.set_keyword_max_thinking_level(
            entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
        );
        self.agent.set_tool_call_dialect(entry.tool_call_dialect());
        self.agent.set_model_accepts_images(
            entry
                .model
                .input
                .contains(&crate::provider::InputType::Image),
        );
        {
            let stream_options = self.agent.stream_options_mut();
            stream_options.api_key = key;
            stream_options.headers.clone_from(&entry.headers);
            stream_options.max_tokens = Some(entry.model.max_tokens);
            stream_options.thinking_level = Some(target_thinking);
        }
        self.set_compaction_context_window(context_window_tokens_for_entry(entry));
        self.refresh_extension_completion_host_state();
        if let Some(region) = &self.extensions {
            region.manager().set_current_model(
                Some(entry.model.provider.clone()),
                Some(entry.model.id.clone()),
            );
        }
    }

    /// A refused restoration: a hard, admission-blocking failure where the
    /// caller keeps long-lived state that is now known to be wrong, and a plain
    /// decline where it does not.
    fn refuse_restore(
        request: &PrimaryRestoreRequest<'_>,
        admission: Option<&ProviderAdmissionGate>,
        reason: String,
    ) -> Result<Option<RestoredPrimary>> {
        if request.strict_invariants {
            if let Some(gate) = admission {
                gate.block(reason.clone());
            }
            return Err(Error::session_persistence(reason));
        }
        Ok(None)
    }

    /// [`Self::try_failover`] with RPC's provider-admission gate. The gate type
    /// is internal, so this stays crate-visible while the plain form above is
    /// what print mode and any embedder call.
    pub(crate) async fn try_failover_swap(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        attempt: &FailoverSwapAttempt<'_>,
        admission: Option<&ProviderAdmissionGate>,
    ) -> Result<FailoverSwapOutcome> {
        self.try_failover_swap_admitted(cx, attempt, admission.map(ProviderSwapAdmission::Gate))
            .await
    }

    pub(crate) async fn try_failover_with_authority(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        attempt: &FailoverSwapAttempt<'_>,
        authority: &ProviderTransitionAuthority,
    ) -> Result<FailoverSwapOutcome> {
        self.try_failover_swap_admitted(cx, attempt, Some(ProviderSwapAdmission::Held(authority)))
            .await
    }

    async fn try_failover_swap_admitted(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        attempt: &FailoverSwapAttempt<'_>,
        admission: Option<ProviderSwapAdmission<'_>>,
    ) -> Result<FailoverSwapOutcome> {
        let (from_provider, from_model) = {
            let provider = self.agent.provider();
            (provider.name().to_string(), provider.model_id().to_string())
        };
        let mut walk = crate::failover::FailoverWalk::new(
            attempt.chain,
            attempt.start_position,
            &from_provider,
            &from_model,
        );
        loop {
            let catalog = self
                .model_registry
                .as_ref()
                .map_or(attempt.available_models, ModelRegistry::models);
            let Some((entry_index, spec)) = walk.next_spec(catalog) else {
                break;
            };
            let next_position = walk.position();
            let Some(entry) = crate::failover::resolve_chain_spec(spec, catalog) else {
                continue;
            };
            let registered =
                crate::models::find_model_entry(catalog, &entry.model.provider, &entry.model.id)
                    .is_some();
            let admitted = attempt.available_models.iter().any(|candidate| {
                candidate
                    .model
                    .provider
                    .trim()
                    .eq_ignore_ascii_case(entry.model.provider.trim())
                    && candidate.model.id.eq_ignore_ascii_case(&entry.model.id)
            });
            let explicit_key_recovers_unready = attempt
                .cli_api_key
                .is_some_and(|key| !key.trim().is_empty())
                && !crate::models::model_entry_is_ready(&entry);
            if registered && !admitted && !explicit_key_recovers_unready {
                // Filtering cannot erase a declared provider's identity. Keep
                // ready excluded routes out, while allowing an explicit key
                // to make an otherwise unready configured route usable.
                continue;
            }
            let api_key =
                crate::models::resolve_model_key(attempt.cli_api_key, attempt.auth, &entry);
            if crate::models::model_requires_configured_credential(&entry) && api_key.is_none() {
                continue;
            }
            let Ok(provider) = crate::providers::create_provider_with_auth(
                &entry,
                self.extensions.as_ref().map(ExtensionRegion::manager),
                Some(attempt.auth),
            ) else {
                continue;
            };

            let to_provider = entry.model.provider.clone();
            let to_model = entry.model.id.clone();
            let request = FailoverSwapRequest {
                entry: &entry,
                api_key,
                provider,
                from_provider: &from_provider,
                from_model: &from_model,
                class: attempt.class,
                chain_position: next_position,
                thinking_level_to_clamp: attempt.thinking_level_to_clamp,
                require_incomplete_tail: attempt.require_incomplete_tail,
                primary: attempt.primary,
                cooldown_secs: attempt.cooldown_secs,
                lifecycle_id: attempt.lifecycle_id,
            };
            let (_, source) = self
                .commit_failover_swap_admitted(cx, &request, admission)
                .await?;
            return Ok(FailoverSwapOutcome {
                next_position,
                committed: Some(CommittedFailover {
                    entry_index,
                    from_provider,
                    from_model,
                    to_provider,
                    to_model,
                    source,
                }),
            });
        }
        Ok(FailoverSwapOutcome {
            next_position: walk.position(),
            committed: None,
        })
    }

    /// The private `Session` candidate a swap would install, plus the message
    /// list and thinking level that go with it.
    ///
    /// Nothing here touches live state: build it, and only if the whole
    /// transition succeeds does the caller install it.
    fn prepare_failover_candidate(
        inner: &Session,
        request: &FailoverSwapRequest<'_>,
    ) -> Result<(Session, Vec<Message>, crate::model::ThinkingLevel)> {
        let mut candidate = inner.clone();
        let reverted = candidate.revert_incomplete_response();
        if request.require_incomplete_tail && !reverted {
            return Err(Error::session(
                "failover restoration invariant failed: the completed error response had no incomplete assistant tail",
            ));
        }
        let restored_messages = candidate.to_messages_for_current_path();

        let entry = request.entry;
        let to_provider = entry.model.provider.clone();
        let to_model = entry.model.id.clone();
        let target_thinking = entry.clamp_thinking_level(request.thinking_level_to_clamp);
        let target_thinking_text = target_thinking.to_string();
        let thinking_changed = candidate
            .effective_thinking_level_for_current_path()
            .as_deref()
            != Some(target_thinking_text.as_str());
        candidate.set_model_header(
            Some(to_provider.clone()),
            Some(to_model.clone()),
            Some(target_thinking_text.clone()),
        );
        candidate.append_custom_entry(
            "failover".to_string(),
            Some(serde_json::json!({
                "from": format!("{}/{}", request.from_provider, request.from_model),
                "to": format!("{to_provider}/{to_model}"),
                "class": format!("{:?}", request.class).to_ascii_lowercase(),
                "attempt": request.chain_position,
            })),
        );

        let primary_provider = request
            .primary
            .as_ref()
            .map_or_else(|| request.from_provider.to_string(), |p| p.provider.clone());
        let primary_model_id = request
            .primary
            .as_ref()
            .map_or_else(|| request.from_model.to_string(), |p| p.model_id.clone());
        let primary_thinking_level = request
            .primary
            .as_ref()
            .map(|p| p.requested_thinking_level.to_string())
            .or_else(|| Some(request.thinking_level_to_clamp.to_string()));
        let cooldown_secs = request.cooldown_secs.unwrap_or(0);
        let now = chrono::Utc::now();
        let cooldown_i64 = i64::try_from(cooldown_secs).unwrap_or(i64::MAX);
        let deadline = now + chrono::Duration::seconds(cooldown_i64);
        let cooldown_deadline = Some(deadline.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        let lifecycle_id = request.lifecycle_id.map(String::from).or_else(|| {
            inner
                .active_failover_provenance_for_current_path()
                .and_then(|p| p.lifecycle_id.clone())
                .or_else(|| Some(uuid::Uuid::new_v4().to_string()))
        });

        let failover_meta = crate::session::ModelChangeFailover {
            primary_provider,
            primary_model_id,
            primary_thinking_level,
            fallback_provider: to_provider.clone(),
            fallback_model_id: to_model.clone(),
            chain_position: Some(request.chain_position),
            cooldown_deadline,
            cooldown_secs: Some(cooldown_secs),
            lifecycle_id,
        };

        candidate.append_model_change_with_role_and_failover(
            to_provider,
            to_model,
            Some("failover".to_string()),
            Some(failover_meta),
        );
        if thinking_changed {
            candidate.append_thinking_level_change(target_thinking_text);
        }
        Ok((candidate, restored_messages, target_thinking))
    }

    /// [`Self::commit_failover`] with RPC's provider-admission gate. The gate
    /// type is internal, so this stays crate-visible while the plain form above
    /// is what print mode and any embedder call.
    pub(crate) async fn commit_failover_swap(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        request: &FailoverSwapRequest<'_>,
        admission: Option<&ProviderAdmissionGate>,
    ) -> Result<crate::model::ThinkingLevel> {
        self.commit_failover_swap_admitted(cx, request, admission.map(ProviderSwapAdmission::Gate))
            .await
            .map(|(thinking_level, _)| thinking_level)
    }

    async fn commit_failover_swap_admitted(
        &mut self,
        cx: &crate::agent_cx::AgentCx,
        request: &FailoverSwapRequest<'_>,
        admission: Option<ProviderSwapAdmission<'_>>,
    ) -> Result<(
        crate::model::ThinkingLevel,
        Option<crate::failover::FailoverSource>,
    )> {
        let gate = admission.map(ProviderSwapAdmission::gate);
        let session_store = Arc::clone(&self.session);
        let mut inner = OwnedMutexGuard::lock(session_store, cx)
            .await
            .map_err(|err| Error::session(format!("failover session lock failed: {err}")))?;
        let (mut candidate, restored_messages, target_thinking) =
            Self::prepare_failover_candidate(&inner, request)?;
        let entry = request.entry;
        let to_provider = entry.model.provider.clone();
        let to_model = entry.model.id.clone();

        let save_enabled = self.save_enabled();
        self.invalidate_background_compaction();
        let _provider_transition = match admission {
            Some(admission) => {
                admission
                    .begin_transition(
                        "failover Session persistence was interrupted before live installation completed"
                            .to_string(),
                        cx,
                    )
                    .await?
            }
            None => None,
        };
        if save_enabled
            && let Err(first_err) = candidate.save().await
            && let Err(retry_err) = candidate.save().await
        {
            let reason = format!(
                "failover Session persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
            );
            if let Some(gate) = gate {
                gate.block(reason.clone());
            }
            return Err(Error::session_persistence(reason));
        }

        // Capture provenance before releasing this lock. Reading it back
        // afterwards would add a fallible step after a committed transition.
        let source = crate::failover::FailoverSource::from_session(&candidate);
        // No fallible operation remains after installing the candidate.
        *inner = candidate;
        self.agent.replace_messages(restored_messages);
        self.agent.set_provider(Arc::clone(&request.provider));
        self.agent.set_keyword_max_thinking_level(
            entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
        );
        self.agent.set_tool_call_dialect(entry.tool_call_dialect());
        self.agent.set_model_accepts_images(
            entry
                .model
                .input
                .contains(&crate::provider::InputType::Image),
        );
        {
            let stream_options = self.agent.stream_options_mut();
            stream_options.api_key.clone_from(&request.api_key);
            stream_options.headers.clone_from(&entry.headers);
            stream_options.max_tokens = Some(entry.model.max_tokens);
            stream_options.thinking_level = Some(target_thinking);
        }
        self.set_compaction_context_window(context_window_tokens_for_entry(entry));
        self.refresh_extension_completion_host_state();
        if let Some(region) = &self.extensions {
            region
                .manager()
                .set_current_model(Some(to_provider), Some(to_model));
        }
        if let Some(gate) = gate {
            gate.clear();
        }
        Ok((target_thinking, source))
    }

    async fn current_compaction_origin(&self) -> Result<CompactionOrigin> {
        let provider = self.agent.provider();
        let cx = crate::agent_cx::AgentCx::for_request();
        let session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        Ok(CompactionOrigin {
            session_id: session.header.id.clone(),
            provider_id: provider.name().to_string(),
            model_id: provider.model_id().to_string(),
            snapshot_leaf_id: session.leaf_id().map(str::to_string),
        })
    }

    async fn compaction_origin_matches_current(
        &self,
        origin: &CompactionOrigin,
    ) -> Result<(bool, CompactionOrigin)> {
        let provider = self.agent.provider();
        let cx = crate::agent_cx::AgentCx::for_request();
        let session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        let current = CompactionOrigin {
            session_id: session.header.id.clone(),
            provider_id: provider.name().to_string(),
            model_id: provider.model_id().to_string(),
            snapshot_leaf_id: session.leaf_id().map(str::to_string),
        };
        let snapshot_is_ancestor = origin.snapshot_leaf_id.as_ref().map_or_else(
            || session.leaf_id().is_none(),
            |snapshot_leaf_id| {
                session.entries_for_current_path().iter().any(|entry| {
                    entry
                        .base_id()
                        .is_some_and(|entry_id| entry_id == snapshot_leaf_id)
                })
            },
        );
        let matches = origin.session_id == current.session_id
            && origin.provider_id == current.provider_id
            && origin.model_id == current.model_id
            && snapshot_is_ancestor;
        Ok((matches, current))
    }

    /// Force-run compaction synchronously (used by `/compact` slash command).
    pub async fn compact_now(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<()> {
        self.ensure_provider_reentry_allowed()?;
        self.compact_synchronous(Arc::new(on_event), false).await
    }

    /// OMP `/shake` (bd-cv653.3.18): compact by dropping bulky tool output
    /// from the older span instead of asking the model for a summary.
    /// Instant, and no provider request is made.
    pub async fn shake_now(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<()> {
        self.compact_synchronous(Arc::new(on_event), true).await
    }

    pub async fn execute_extension_command(
        &mut self,
        command_name: &str,
        args: &str,
        timeout_ms: u64,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<Value> {
        self.execute_extension_command_with_abort(command_name, args, timeout_ms, None, on_event)
            .await
    }

    pub(crate) fn has_pending_extension_idle_actions(&self) -> bool {
        self.extensions_pending_idle_actions
            .lock()
            .map_or(true, |actions| !actions.is_empty())
    }

    pub async fn execute_extension_command_with_abort(
        &mut self,
        command_name: &str,
        args: &str,
        timeout_ms: u64,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<Value> {
        self.ensure_provider_reentry_allowed()?;
        let manager = self
            .extensions
            .as_ref()
            .map(ExtensionRegion::manager)
            .ok_or_else(|| Error::extension("Extensions are disabled"))?
            .clone();
        let on_event: AgentEventHandler = Arc::new(on_event);

        self.run_pending_idle_actions_with_abort(abort.clone(), Arc::clone(&on_event))
            .await?;

        let command_result = manager
            .execute_command(command_name, args, timeout_ms)
            .await;
        let replay_result = self
            .run_pending_idle_actions_with_abort(abort, Arc::clone(&on_event))
            .await;

        match command_result {
            Ok(value) => {
                replay_result?;
                Ok(value)
            }
            Err(err) => {
                if let Err(replay_err) = replay_result {
                    tracing::warn!(
                        "extension command follow-up replay failed after command error: {replay_err}"
                    );
                }
                Err(err)
            }
        }
    }

    /// Two-phase non-blocking compaction.
    ///
    /// **Phase 1** — apply a completed background compaction result (if any).
    /// **Phase 2** — if quotas allow and the session needs compaction, start a
    /// new background compaction task.
    #[allow(clippy::too_many_lines)]
    async fn maybe_compact(&mut self, on_event: AgentEventHandler) -> Result<()> {
        if !self.compaction_settings.enabled {
            return Ok(());
        }

        // Phase 1: apply completed background result.
        if let Some((origin, outcome)) = self.compaction_worker.try_recv_bound().await {
            // Preserve legacy lifecycle semantics: isCompacting remains true
            // while the terminal event is delivered, then clears on scope exit.
            let _terminal_compacting_guard =
                AtomicBoolGuard::activate(&self.extensions_is_compacting);
            let origin_description = origin.as_ref().map_or_else(
                || "missing origin metadata".to_string(),
                |origin| {
                    format!(
                        "session {}/{}/{} at leaf {}",
                        origin.session_id,
                        origin.provider_id,
                        origin.model_id,
                        origin.snapshot_leaf_id.as_deref().unwrap_or("<root>")
                    )
                },
            );
            let Some(origin) = origin else {
                let current_origin = self.current_compaction_origin().await?;
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: Some(format!(
                        "Discarded background compaction with {origin_description}; current context is session {}/{}/{} at leaf {}",
                        current_origin.session_id,
                        current_origin.provider_id,
                        current_origin.model_id,
                        current_origin
                            .snapshot_leaf_id
                            .as_deref()
                            .unwrap_or("<root>")
                    )),
                });
                return Ok(());
            };
            let (origin_matches, current_origin) =
                self.compaction_origin_matches_current(&origin).await?;
            if !origin_matches {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: Some(format!(
                        "Discarded stale background compaction from {origin_description}; current context is session {}/{}/{} at leaf {}",
                        current_origin.session_id,
                        current_origin.provider_id,
                        current_origin.model_id,
                        current_origin
                            .snapshot_leaf_id
                            .as_deref()
                            .unwrap_or("<root>")
                    )),
                });
                return Ok(());
            }
            match outcome {
                Ok(result) => {
                    let cx = crate::agent_cx::AgentCx::for_current_or_request();
                    let provider_admission = match self.provider_admission.acquire(cx.cx()).await {
                        Ok(provider_admission) => provider_admission,
                        Err(err) => {
                            on_event(AgentEvent::AutoCompactionEnd {
                                result: None,
                                aborted: false,
                                will_retry: false,
                                error_message: Some(err.to_string()),
                            });
                            return Err(err);
                        }
                    };
                    if let Err(err) = self.provider_admission.ensure_allowed() {
                        on_event(AgentEvent::AutoCompactionEnd {
                            result: None,
                            aborted: false,
                            will_retry: false,
                            error_message: Some(err.to_string()),
                        });
                        return Err(err);
                    }
                    self.apply_compaction_result(result, Arc::clone(&on_event), provider_admission)
                        .await?;
                    self.compaction_worker.mark_applied_success();
                }
                Err(e) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(e.to_string()),
                    });
                }
            }
        }

        // Phase 2: start new background compaction if quotas allow.
        if !self.compaction_worker.can_start() {
            // Failsafe: a quota-blocked worker must not let a catastrophically
            // oversized session grow without bound.
            return self.force_local_compaction_if_oversized(on_event).await;
        }

        let active_provider = self.agent.provider();
        let origin_provider_id = active_provider.name().to_string();
        let origin_model_id = active_provider.model_id().to_string();
        let (entries, preparation, origin) = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.ensure_entry_ids();
            let entries = session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            let prep = compaction::prepare_compaction(&entries, self.compaction_settings.clone());
            let origin = CompactionOrigin {
                session_id: session.header.id.clone(),
                provider_id: origin_provider_id,
                model_id: origin_model_id,
                snapshot_leaf_id: session.leaf_id().map(str::to_string),
            };
            (entries, prep, origin)
        };

        if let Some(prep) = preparation {
            let admission = self
                .compaction_worker
                .admission_decision(Some(&prep), &CompactionAdmissionSignals::default());
            if !admission.allowed {
                tracing::info!(
                    reason = admission.reason.as_str(),
                    tokens_before = admission.tokens_before,
                    "Background compaction admission denied"
                );
                return Ok(());
            }

            on_event(AgentEvent::AutoCompactionStart {
                reason: format!("threshold;admission={}", admission.reason.as_str()),
            });
            let compacting_guard = AtomicBoolGuard::activate(&self.extensions_is_compacting);

            let before_outcome = self.dispatch_before_compact(&prep, &entries, None).await;
            if before_outcome.cancel {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: None,
                });
                return Ok(());
            }

            let (origin_matches, current_origin) =
                self.compaction_origin_matches_current(&origin).await?;
            if !origin_matches {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: Some(format!(
                        "Compaction context changed during session_before_compact; snapshot session {}/{}/{} at leaf {}, current session {}/{}/{} at leaf {}",
                        origin.session_id,
                        origin.provider_id,
                        origin.model_id,
                        origin.snapshot_leaf_id.as_deref().unwrap_or("<root>"),
                        current_origin.session_id,
                        current_origin.provider_id,
                        current_origin.model_id,
                        current_origin
                            .snapshot_leaf_id
                            .as_deref()
                            .unwrap_or("<root>")
                    )),
                });
                return Ok(());
            }

            if let Some(compaction) = before_outcome.compaction {
                let cx = crate::agent_cx::AgentCx::for_current_or_request();
                let provider_admission = match self.provider_admission.acquire(cx.cx()).await {
                    Ok(provider_admission) => provider_admission,
                    Err(err) => {
                        on_event(AgentEvent::AutoCompactionEnd {
                            result: None,
                            aborted: false,
                            will_retry: false,
                            error_message: Some(err.to_string()),
                        });
                        return Err(err);
                    }
                };
                let apply_result = self
                    .apply_compaction_entry(
                        compaction.summary.clone(),
                        compaction.first_kept_entry_id.clone(),
                        compaction.tokens_before,
                        compaction.details.clone(),
                        true,
                        provider_admission,
                    )
                    .await;
                let tokens_after = match apply_result {
                    Ok(tokens_after) => tokens_after,
                    Err(err) => {
                        on_event(AgentEvent::AutoCompactionEnd {
                            result: None,
                            aborted: false,
                            will_retry: false,
                            error_message: Some(err.to_string()),
                        });
                        return Err(err);
                    }
                };
                let result_value = Some(Self::auto_compaction_result_payload(
                    compaction.summary,
                    compaction.first_kept_entry_id,
                    compaction.tokens_before,
                    tokens_after,
                    compaction.details,
                ));
                on_event(AgentEvent::AutoCompactionEnd {
                    result: result_value,
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                });
                return Ok(());
            }

            let provider = self.agent.provider();
            let stream_options = self.agent.stream_options().clone();

            let runtime_handle = match self.compaction_runtime_handle() {
                Ok(runtime_handle) => runtime_handle,
                Err(e) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(e.to_string()),
                    });
                    return Ok(());
                }
            };

            let cx = crate::agent_cx::AgentCx::for_current_or_request();
            let provider_permit = match self.provider_admission.acquire(cx.cx()).await {
                Ok(provider_permit) => provider_permit,
                Err(err) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(err.to_string()),
                    });
                    return Err(err);
                }
            };
            if let Err(err) = self.provider_admission.ensure_allowed() {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(err.to_string()),
                });
                return Err(err);
            }
            if let Err(err) = self.compaction_worker.start_for_origin(
                origin,
                provider_permit,
                &runtime_handle,
                prep,
                provider,
                stream_options,
                None,
                self.agent.compaction_privacy(),
            ) {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(err.to_string()),
                });
                return Err(err);
            }
            compacting_guard.keep_active();
        }

        Ok(())
    }

    /// Failsafe for quota-blocked compaction.
    ///
    /// When the background worker cannot start (attempt limit exhausted or
    /// still cooling down) but the session has grown to at least
    /// [`compaction::FORCED_LOCAL_COMPACTION_WINDOW_FACTOR`] times the context
    /// window, apply a synchronous, provider-free compaction so every future
    /// provider call is not doomed to fail on an oversized context. Never runs
    /// while a background compaction is still pending, and skips extension
    /// `before_compact` dispatch: this path exists to guarantee forward
    /// progress, so nothing may cancel it.
    async fn force_local_compaction_if_oversized(&self, on_event: AgentEventHandler) -> Result<()> {
        let decision = self
            .compaction_worker
            .admission_decision(None, &CompactionAdmissionSignals::default());
        if decision.reason == CompactionAdmissionReason::Pending {
            // An in-flight background compaction will resolve on its own;
            // never compact underneath it.
            return Ok(());
        }

        let preparation = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.ensure_entry_ids();
            let entries = session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            compaction::prepare_compaction(&entries, self.compaction_settings.clone())
        };

        let Some(prep) = preparation else {
            return Ok(());
        };
        if !compaction::requires_forced_local_compaction(prep.tokens_before, &prep.settings) {
            return Ok(());
        }

        tracing::warn!(
            blocked_reason = decision.reason.as_str(),
            tokens_before = prep.tokens_before,
            context_window_tokens = prep.settings.context_window_tokens,
            "Background compaction quota-blocked while context is far over the window; applying deterministic local compaction"
        );
        on_event(AgentEvent::AutoCompactionStart {
            reason: format!("forced_local;blocked={}", decision.reason.as_str()),
        });
        let _compacting_guard = AtomicBoolGuard::activate(&self.extensions_is_compacting);

        let result = compaction::compact_local(prep, &self.agent.compaction_privacy());
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let provider_admission = match self.provider_admission.acquire(cx.cx()).await {
            Ok(provider_admission) => provider_admission,
            Err(err) => {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(err.to_string()),
                });
                return Err(err);
            }
        };

        self.apply_compaction_result(result, Arc::clone(&on_event), provider_admission)
            .await
    }

    fn compaction_runtime_handle(&mut self) -> Result<RuntimeHandle> {
        if let Some(runtime_handle) = self.runtime_handle.clone() {
            return Ok(runtime_handle);
        }

        let runtime = RuntimeBuilder::new().build().map_err(|e| {
            Error::session(format!("Background compaction runtime init failed: {e}"))
        })?;
        let runtime_handle = runtime.handle();
        self.compaction_runtime = Some(runtime);
        self.runtime_handle = Some(runtime_handle.clone());
        Ok(runtime_handle)
    }

    fn auto_compaction_result_payload(
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        tokens_after: u64,
        details: Option<Value>,
    ) -> Value {
        let mut payload = serde_json::Map::new();
        payload.insert("summary".to_string(), Value::String(summary));
        payload.insert(
            "firstKeptEntryId".to_string(),
            Value::String(first_kept_entry_id),
        );
        payload.insert("tokensBefore".to_string(), Value::from(tokens_before));
        payload.insert("tokensAfter".to_string(), Value::from(tokens_after));
        if let Some(details) = details {
            payload.insert("details".to_string(), details);
        }
        Value::Object(payload)
    }

    /// Append the compaction entry to the session and return the estimated
    /// token count of the context the next provider request will see (the
    /// post-compaction current path). The estimate uses the char-based
    /// heuristic and ignores retained assistant `usage`, so stale
    /// pre-compaction usage never inflates it.
    async fn apply_compaction_entry(
        &self,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        details: Option<Value>,
        from_extension: bool,
        provider_admission: OwnedMutexGuard<()>,
    ) -> Result<u64> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        self.provider_admission.ensure_allowed()?;
        let mut candidate = session.clone();

        let from_hook = if from_extension { Some(true) } else { None };
        let entry_id = candidate.append_compaction(
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
            from_hook,
        );

        if self.save_enabled {
            self.provider_admission.block(
                "compaction persistence was interrupted before live installation completed"
                    .to_string(),
            );
            if let Err(first_err) = candidate.save().await
                && let Err(retry_err) = candidate.save().await
            {
                let reason = format!(
                    "compaction persistence remained indeterminate after an idempotent retry: first failure: {first_err}; retry failure: {retry_err}"
                );
                self.provider_admission.block(reason.clone());
                return Err(Error::session_persistence(reason));
            }
        }

        // Estimate the context the *next* provider request will see now that the
        // compacted history is written: the post-compaction current path via the
        // shared char-based heuristic, ignoring any retained assistant `usage`.
        let tokens_after =
            compaction::estimate_entries_context_tokens(&candidate.entries_for_current_path());

        let compaction_entry = candidate.get_entry(&entry_id).and_then(|entry| {
            if let crate::session::SessionEntry::Compaction(compaction) = entry {
                Some(compaction.clone())
            } else {
                None
            }
        });
        *session = candidate;
        if self.save_enabled {
            self.provider_admission.clear();
        }
        drop(session);
        // The Session transition is complete. Release provider admission
        // before dispatching the re-entrant post-install extension hook.
        drop(provider_admission);

        if let (Some(region), Some(compaction_entry)) = (&self.extensions, compaction_entry) {
            let payload = json!({
                "compactionEntry": compaction_entry,
                "fromExtension": from_extension,
            });
            if let Err(err) = region
                .manager()
                .dispatch_event(ExtensionEventName::SessionCompact, Some(payload))
                .await
            {
                tracing::warn!("session_compact extension hook failed (fail-open): {err}");
            }
        }

        Ok(tokens_after)
    }

    /// Apply a completed compaction result to the session.
    async fn apply_compaction_result(
        &self,
        result: compaction::CompactionResult,
        on_event: AgentEventHandler,
        provider_admission: OwnedMutexGuard<()>,
    ) -> Result<()> {
        let details = match compaction::compaction_details_to_value(&result.details) {
            Ok(details) => Some(details),
            Err(err) => {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(err.to_string()),
                });
                return Err(err);
            }
        };
        let tokens_after = match self
            .apply_compaction_entry(
                result.summary.clone(),
                result.first_kept_entry_id.clone(),
                result.tokens_before,
                details.clone(),
                false,
                provider_admission,
            )
            .await
        {
            Ok(tokens_after) => tokens_after,
            Err(err) => {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(err.to_string()),
                });
                return Err(err);
            }
        };

        let result_value = Some(Self::auto_compaction_result_payload(
            result.summary,
            result.first_kept_entry_id,
            result.tokens_before,
            tokens_after,
            details,
        ));

        on_event(AgentEvent::AutoCompactionEnd {
            result: result_value,
            aborted: false,
            will_retry: false,
            error_message: None,
        });

        Ok(())
    }

    /// Run compaction synchronously (inline), blocking until completion.
    #[allow(clippy::too_many_lines)]
    async fn compact_synchronous(
        &mut self,
        on_event: AgentEventHandler,
        shake: bool,
    ) -> Result<()> {
        if !self.compaction_settings.enabled {
            return Ok(());
        }

        let (entries, preparation) = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.ensure_entry_ids();
            let entries = session
                .entries_for_current_path()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            let prep = compaction::prepare_compaction(&entries, self.compaction_settings.clone());
            (entries, prep)
        };

        if let Some(prep) = preparation {
            on_event(AgentEvent::AutoCompactionStart {
                reason: "threshold".to_string(),
            });
            let _compacting_guard = AtomicBoolGuard::activate(&self.extensions_is_compacting);

            let before_outcome = self.dispatch_before_compact(&prep, &entries, None).await;
            if before_outcome.cancel {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: true,
                    will_retry: false,
                    error_message: None,
                });
                return Err(Error::extension("Compaction cancelled".to_string()));
            }

            if let Some(compaction) = before_outcome.compaction {
                let cx = crate::agent_cx::AgentCx::for_current_or_request();
                let provider_admission = match self.provider_admission.acquire(cx.cx()).await {
                    Ok(provider_admission) => provider_admission,
                    Err(err) => {
                        on_event(AgentEvent::AutoCompactionEnd {
                            result: None,
                            aborted: false,
                            will_retry: false,
                            error_message: Some(err.to_string()),
                        });
                        return Err(err);
                    }
                };
                let apply_result = self
                    .apply_compaction_entry(
                        compaction.summary.clone(),
                        compaction.first_kept_entry_id.clone(),
                        compaction.tokens_before,
                        compaction.details.clone(),
                        true,
                        provider_admission,
                    )
                    .await;
                let tokens_after = match apply_result {
                    Ok(tokens_after) => tokens_after,
                    Err(err) => {
                        on_event(AgentEvent::AutoCompactionEnd {
                            result: None,
                            aborted: false,
                            will_retry: false,
                            error_message: Some(err.to_string()),
                        });
                        return Err(err);
                    }
                };
                let result_value = Some(Self::auto_compaction_result_payload(
                    compaction.summary,
                    compaction.first_kept_entry_id,
                    compaction.tokens_before,
                    tokens_after,
                    compaction.details,
                ));
                on_event(AgentEvent::AutoCompactionEnd {
                    result: result_value,
                    aborted: false,
                    will_retry: false,
                    error_message: None,
                });
                return Ok(());
            }

            let provider = self.agent.provider();
            let stream_options = self.agent.stream_options().clone();

            self.invalidate_background_compaction();
            let cx = crate::agent_cx::AgentCx::for_current_or_request();
            let provider_admission = match self.provider_admission.acquire(cx.cx()).await {
                Ok(provider_admission) => provider_admission,
                Err(err) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(err.to_string()),
                    });
                    return Err(err);
                }
            };
            if let Err(err) = self.provider_admission.ensure_allowed() {
                on_event(AgentEvent::AutoCompactionEnd {
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error_message: Some(err.to_string()),
                });
                return Err(err);
            }
            let compaction_result = if shake {
                Ok(compaction::compact_shake(
                    prep,
                    &self.agent.compaction_privacy(),
                ))
            } else {
                compaction::compact(
                    prep,
                    provider,
                    &stream_options,
                    None,
                    &self.agent.compaction_privacy(),
                )
                .await
            };

            match compaction_result {
                Ok(result) => {
                    self.apply_compaction_result(result, Arc::clone(&on_event), provider_admission)
                        .await?;
                }
                Err(e) => {
                    on_event(AgentEvent::AutoCompactionEnd {
                        result: None,
                        aborted: false,
                        will_retry: false,
                        error_message: Some(e.to_string()),
                    });
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn resolve_extension_policy_for_enable(
        config: Option<&crate::config::Config>,
        policy: Option<ExtensionPolicy>,
    ) -> ExtensionPolicy {
        policy.unwrap_or_else(|| {
            config.map_or_else(
                || crate::config::Config::default().resolve_extension_policy(None),
                |cfg| cfg.resolve_extension_policy(None),
            )
        })
    }

    pub async fn enable_extensions(
        &mut self,
        enabled_tools: &[&str],
        cwd: &std::path::Path,
        config: Option<&crate::config::Config>,
        extension_entries: &[std::path::PathBuf],
    ) -> Result<()> {
        self.enable_extensions_with_policy(
            enabled_tools,
            cwd,
            config,
            extension_entries,
            None,
            None,
            None,
            ExtensionHostConfiguration::default(),
        )
        .await
    }

    /// `_enabled_tools` is kept for call-site compatibility: the extension
    /// runtime now resolves tools through the agent's own registry, whose
    /// enabled set was fixed when the agent was built (bd-4t6oz).
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn enable_extensions_with_policy(
        &mut self,
        _enabled_tools: &[&str],
        cwd: &std::path::Path,
        config: Option<&crate::config::Config>,
        extension_entries: &[std::path::PathBuf],
        policy: Option<ExtensionPolicy>,
        repair_policy: Option<RepairPolicyMode>,
        pre_warmed: Option<PreWarmedExtensionRuntime>,
        host: ExtensionHostConfiguration,
    ) -> Result<()> {
        let mut js_specs: Vec<JsExtensionLoadSpec> = Vec::new();
        let mut native_specs: Vec<NativeRustExtensionLoadSpec> = Vec::new();
        #[cfg(feature = "wasm-host")]
        let mut wasm_specs: Vec<WasmExtensionLoadSpec> = Vec::new();

        for entry in extension_entries {
            match resolve_extension_load_spec(entry)? {
                ExtensionLoadSpec::Js(spec) => js_specs.push(spec),
                ExtensionLoadSpec::NativeRust(spec) => native_specs.push(spec),
                #[cfg(feature = "wasm-host")]
                ExtensionLoadSpec::Wasm(spec) => wasm_specs.push(spec),
            }
        }

        if !js_specs.is_empty() && !native_specs.is_empty() {
            return Err(Error::validation(
                "Mixed extension runtimes are not supported in one session yet. Use either JS/TS extensions (QuickJS) or native-rust descriptors (*.native.json), but not both at once."
                    .to_string(),
            ));
        }

        #[cfg(feature = "wasm-host")]
        if js_specs.is_empty() && native_specs.is_empty() && wasm_specs.is_empty() {
            self.extensions = None;
            self.agent.extensions = None;
            self.extension_queue_modes = None;
            self.extension_injected_queue = None;
            return Ok(());
        }

        #[cfg(not(feature = "wasm-host"))]
        if js_specs.is_empty() && native_specs.is_empty() {
            self.extensions = None;
            self.agent.extensions = None;
            self.extension_queue_modes = None;
            self.extension_injected_queue = None;
            return Ok(());
        }

        let resolved_policy = Self::resolve_extension_policy_for_enable(config, policy);
        let resolved_repair_policy = repair_policy
            .or_else(|| config.map(|cfg| cfg.resolve_repair_policy(None)))
            .unwrap_or(RepairPolicyMode::AutoSafe);
        let runtime_repair_mode =
            Self::runtime_repair_mode_from_policy_mode(resolved_repair_policy);
        let memory_limit_bytes =
            (resolved_policy.max_memory_mb as usize).saturating_mul(1024 * 1024);
        let wants_js_runtime = !js_specs.is_empty();

        // Either use the pre-warmed extension runtime (booted concurrently with startup)
        // or create a fresh runtime inline.
        #[allow(unused_variables)]
        let (manager, tools) = if let Some(pre) = pre_warmed {
            let manager = pre.manager;
            let tools = pre.tools;
            let runtime = match pre.runtime {
                ExtensionRuntimeHandle::NativeRust(runtime) => {
                    if wants_js_runtime {
                        tracing::warn!(
                            event = "pi.extension_runtime.prewarm.mismatch",
                            expected = "quickjs",
                            got = "native-rust",
                            "Pre-warmed runtime mismatched requested JS mode; creating quickjs runtime"
                        );
                        Self::start_js_extension_runtime(
                            "agent_enable_extensions_prewarm_mismatch",
                            cwd,
                            tools.clone(),
                            manager.clone(),
                            resolved_policy.clone(),
                            runtime_repair_mode,
                            memory_limit_bytes,
                        )
                        .await?
                    } else {
                        tracing::info!(
                            event = "pi.extension_runtime.engine_decision",
                            stage = "agent_enable_extensions_prewarmed",
                            requested = "native-rust",
                            selected = "native-rust",
                            fallback = false,
                            "Using pre-warmed extension runtime"
                        );
                        ExtensionRuntimeHandle::NativeRust(runtime)
                    }
                }
                ExtensionRuntimeHandle::Js(runtime) => {
                    if wants_js_runtime {
                        tracing::info!(
                            event = "pi.extension_runtime.engine_decision",
                            stage = "agent_enable_extensions_prewarmed",
                            requested = "quickjs",
                            selected = "quickjs",
                            fallback = false,
                            "Using pre-warmed extension runtime"
                        );
                        ExtensionRuntimeHandle::Js(runtime)
                    } else {
                        tracing::warn!(
                            event = "pi.extension_runtime.prewarm.mismatch",
                            expected = "native-rust",
                            got = "quickjs",
                            "Pre-warmed runtime mismatched requested native mode; creating native-rust runtime"
                        );
                        Self::start_native_extension_runtime(
                            "agent_enable_extensions_prewarm_mismatch",
                            cwd,
                            tools.clone(),
                            manager.clone(),
                            resolved_policy.clone(),
                            runtime_repair_mode,
                            memory_limit_bytes,
                        )
                        .await?
                    }
                }
            };
            manager.set_runtime(runtime);
            (manager, tools)
        } else {
            let manager = ExtensionManager::new();
            manager.set_cwd(cwd.display().to_string());
            // Share the agent's undo recorder so extension `pi.tool` hostcalls
            // that write files land in the same /undo ledger as the agent's
            // own tool calls (bd-4t6oz). Workspace roots are not available on
            // this path; the classic startup threads them through the
            // pre-warmed registry instead.
            // The runtime resolves tools through the agent's own registry:
            // same undo recorder, same workspace roots, and every tool
            // mounted later (extension wrappers, MCP, plan tools) is visible
            // to `pi.tool` hostcalls (bd-4t6oz).
            let tools = self.agent.shared_tools();

            if let Some(cfg) = config {
                let resolved_risk = cfg.resolve_extension_risk_with_metadata();
                tracing::info!(
                    event = "pi.extension_runtime_risk.config",
                    source = resolved_risk.source,
                    enabled = resolved_risk.settings.enabled,
                    alpha = resolved_risk.settings.alpha,
                    window_size = resolved_risk.settings.window_size,
                    ledger_limit = resolved_risk.settings.ledger_limit,
                    fail_closed = resolved_risk.settings.fail_closed,
                    "Resolved extension runtime risk settings"
                );
                manager.set_runtime_risk_config(resolved_risk.settings);
            }

            let runtime = if wants_js_runtime {
                Self::start_js_extension_runtime(
                    "agent_enable_extensions_boot",
                    cwd,
                    tools.clone(),
                    manager.clone(),
                    resolved_policy.clone(),
                    runtime_repair_mode,
                    memory_limit_bytes,
                )
                .await?
            } else {
                Self::start_native_extension_runtime(
                    "agent_enable_extensions_boot",
                    cwd,
                    tools.clone(),
                    manager.clone(),
                    resolved_policy.clone(),
                    runtime_repair_mode,
                    memory_limit_bytes,
                )
                .await?
            };
            manager.set_runtime(runtime);
            (manager, tools)
        };
        if let Some(handler) = host.ui_handler {
            manager.set_ui_handler(handler);
        }
        manager.set_policy_prompt_persistence(host.persist_permission_decisions);
        tools
            .snapshot()
            .bind_job_session_resolver(Self::job_session_id_resolver(&self.session));

        // Session, host actions, and message fetchers are always set here
        // (after runtime boot) — the JS runtime only needs these when
        // dispatching hostcalls, which happens during extension loading.
        let (steering_mode, follow_up_mode) = self.agent.queue_modes();
        let queue_modes = Arc::new(StdMutex::new(ExtensionQueueModeState::new(
            steering_mode,
            follow_up_mode,
        )));
        manager.set_session_action_origin_source(self.session_action_admission.origin_source());
        {
            let mut state = self
                .extension_ai_completion
                .lock()
                .map_err(|_| Error::extension("Failed to initialize the live model catalog"))?;
            // Startup hooks can select models before `self.extensions` is
            // installed. Seed the same catalog that subsequent registry refreshes
            // keep current for runtime model selection and compaction.
            state.compaction_models.registry = self.model_registry.clone().map(Arc::new);
        }
        manager.set_session(Arc::new(AgentExtensionSession {
            handle: SessionHandle(self.session.clone()),
            ai_completion: Arc::clone(&self.extension_ai_completion),
            session_action_admission: self.session_action_admission.clone(),
            is_streaming: Arc::clone(&self.extensions_is_streaming),
            is_compacting: Arc::clone(&self.extensions_is_compacting),
            queue_modes: Arc::clone(&queue_modes),
            auto_compaction_enabled: self.compaction_settings.enabled,
        }));

        // gh #167 ctx parity: seed the manager with the model catalog (for
        // ctx.modelRegistry.find), the current provider/model pair (ctx.model
        // fallback + cache-generation bump on switches), and the effective
        // system prompt (ctx.getSystemPrompt before any before_agent_start).
        if let Some(registry) = &self.model_registry {
            manager.set_extension_models(pi_ai_model_registry_values(registry));
        }
        manager.set_current_model(
            Some(self.agent.provider().name().to_string()),
            Some(self.agent.provider().model_id().to_string()),
        );
        manager.set_system_prompt(self.agent.system_prompt().map(ToString::to_string));

        let injected = Arc::new(StdMutex::new(ExtensionInjectedQueue::new(
            steering_mode,
            follow_up_mode,
        )));
        let host_actions = AgentSessionHostActions {
            session: Arc::clone(&self.session),
            injected: Arc::clone(&injected),
            is_streaming: Arc::clone(&self.extensions_is_streaming),
            is_turn_active: Arc::clone(&self.extensions_turn_active),
            pending_idle_actions: Arc::clone(&self.extensions_pending_idle_actions),
            ai_completion: Arc::clone(&self.extension_ai_completion),
            provider_admission: self.provider_admission.clone(),
            session_action_admission: self.session_action_admission.clone(),
        };
        self.extension_queue_modes = Some(Arc::clone(&queue_modes));
        self.extension_injected_queue = Some(Arc::clone(&injected));
        manager.set_host_actions(Arc::new(host_actions));
        {
            let steering_queue = Arc::clone(&injected);
            let follow_up_queue = Arc::clone(&injected);
            let steering_fetcher = move || -> BoxFuture<'static, Vec<QueuedAgentMessage>> {
                let steering_queue = Arc::clone(&steering_queue);
                Box::pin(async move {
                    let Ok(mut queue) = steering_queue.lock() else {
                        return Vec::new();
                    };
                    queue
                        .pop_steering()
                        .into_iter()
                        .map(QueuedAgentMessage::generated)
                        .collect()
                })
            };
            let follow_up_fetcher = move || -> BoxFuture<'static, Vec<QueuedAgentMessage>> {
                let follow_up_queue = Arc::clone(&follow_up_queue);
                Box::pin(async move {
                    let Ok(mut queue) = follow_up_queue.lock() else {
                        return Vec::new();
                    };
                    queue
                        .pop_follow_up()
                        .into_iter()
                        .map(QueuedAgentMessage::generated)
                        .collect()
                })
            };
            self.agent.register_message_fetchers(
                Some(Arc::new(steering_fetcher)),
                Some(Arc::new(follow_up_fetcher)),
            );
        }
        if !js_specs.is_empty() {
            manager.load_js_extensions(js_specs).await?;
        }

        if !native_specs.is_empty() {
            manager.load_native_extensions(native_specs).await?;
        }

        // Drain and log auto-repair diagnostics (bd-k5q5.8.11).
        if let Some(rt) = manager.runtime() {
            let events = rt.drain_repair_events().await;
            if !events.is_empty() {
                log_repair_diagnostics(&events);
            }
        }

        #[cfg(feature = "wasm-host")]
        if !wasm_specs.is_empty() {
            let host = WasmExtensionHost::new(cwd, resolved_policy.clone())?;
            manager
                .load_wasm_extensions(&host, wasm_specs, tools.clone())
                .await?;
        }

        // Extension flag definitions are registered by the load calls above,
        // but startup hooks may read their resolved values. Apply the parsed
        // host values in that narrow interval rather than after startup.
        crate::extensions::apply_cli_flags(&manager, &host.cli_flags).await?;

        // A startup hook may select a provider registered by the extension
        // that just loaded. Validate a private registration snapshot for those
        // hooks; the caller still owns final credential refresh and selection.
        let bindings = crate::models::extension_provider_bindings(&manager.extension_providers())?;
        let mut hook_registry = self.model_registry.clone().unwrap_or_default();
        hook_registry.merge_extension_registry(&bindings, manager.extension_model_entries())?;
        manager.set_extension_models(pi_ai_model_registry_values(&hook_registry));
        {
            let mut state = self
                .extension_ai_completion
                .lock()
                .map_err(|_| Error::extension("Failed to publish the startup model catalog"))?;
            state.compaction_models.registry = Some(Arc::new(hook_registry));
        }

        // Fire the `startup` lifecycle hook once extensions are loaded.
        // Fail-open: extension errors must not prevent the agent from running.
        let session_path = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::extension(e.to_string()))?;
            session.path.as_ref().map(|p| p.display().to_string())
        };

        if let Err(err) = manager
            .dispatch_event(
                ExtensionEventName::Startup,
                Some(serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "sessionFile": session_path,
                })),
            )
            .await
        {
            tracing::warn!("startup extension hook failed (fail-open): {err}");
        }

        if let Err(err) = manager
            .dispatch_event(ExtensionEventName::SessionStart, None)
            .await
        {
            tracing::warn!("session_start extension hook failed (fail-open): {err}");
        }

        let ctx_payload = serde_json::json!({ "cwd": cwd.display().to_string() });
        let wrappers = collect_extension_tool_wrappers(&manager, ctx_payload).await?;
        self.agent.extend_tools(wrappers);
        self.agent.extensions = Some(manager.clone());
        self.extensions = Some(ExtensionRegion::new(manager));
        self.refresh_extension_compaction_models();
        Ok(())
    }

    pub async fn save_and_index(&mut self) -> Result<()> {
        if self.save_enabled {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let persist_result = flush_turn_autosave(
                &mut session,
                &self.provider_admission,
                AutosaveFlushTrigger::Periodic,
            )
            .await;
            finish_turn_persistence(&self.provider_admission, Ok(()), persist_result)?;
        }
        Ok(())
    }

    pub async fn persist_session(&mut self) -> Result<()> {
        if !self.save_enabled {
            return Ok(());
        }
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        let persist_result = flush_turn_autosave(
            &mut session,
            &self.provider_admission,
            AutosaveFlushTrigger::Periodic,
        )
        .await;
        finish_turn_persistence(&self.provider_admission, Ok(()), persist_result)
    }

    pub async fn run_text(
        &mut self,
        input: String,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_text_with_abort(input, None, on_event).await
    }

    /// Advisor hook (bd-cv653.3.3): after a completed turn, if the advisor
    /// role resolved a model, review a compact digest and inject the verdict
    /// through the native steering queue. Reserve any automatic continuation
    /// durably before returning; optional provider failures remain isolated.
    async fn maybe_advise_turn(
        &mut self,
        abort: Option<&AbortSignal>,
        turn: &AdvisorTurn,
        allow_continuation: bool,
    ) -> Result<bool> {
        if abort.is_some_and(AbortSignal::is_aborted) || turn.time_exhausted() {
            return Ok(false);
        }
        let Some(runtime) = self.advisor.as_mut() else {
            return Ok(false);
        };
        let Ok(digest) = runtime.build_digest_with_agent(&self.agent) else {
            tracing::info!(
                event = "pi.advisor.privacy_projection_refused",
                "Advisor request omitted because the configured privacy policy refused its digest"
            );
            return Ok(false);
        };
        if digest.is_trivial() {
            return Ok(false);
        }
        let turn_index = self.agent.messages().len() as u64;
        let outcome = runtime
            .review_turn_with_budget(&digest, turn_index, abort, turn.remaining_time())
            .await;
        if abort.is_some_and(AbortSignal::is_aborted) {
            return Ok(false);
        }
        if std::env::var_os("PI_DEBUG_ADVISOR").is_some() {
            eprintln!(
                "[advisor] digest tools={} trivial={} outcome={}",
                digest.tool_call_count,
                digest.is_trivial(),
                match &outcome {
                    crate::advisor::AdvisorOutcome::Inject(v) =>
                        format!("inject:{}", v.level.as_str()),
                    crate::advisor::AdvisorOutcome::Quiet => "quiet".to_string(),
                    crate::advisor::AdvisorOutcome::Failed => "failed".to_string(),
                }
            );
        }
        let verdict = match outcome {
            crate::advisor::AdvisorOutcome::Inject(verdict) => verdict,
            crate::advisor::AdvisorOutcome::Quiet => return Ok(false),
            crate::advisor::AdvisorOutcome::Failed => {
                // Surface the one-time disable notice if the watchdog fired.
                if let Some(notice) = runtime.disabled_notice.clone() {
                    let msg = crate::model::Message::User(crate::model::UserMessage {
                        content: crate::model::UserContent::Text(format!(
                            "[advisor disabled: {notice}]"
                        )),
                        timestamp: chrono::Utc::now().timestamp_millis(),
                    });
                    self.agent.queue_generated_steering(msg);
                }
                return Ok(false);
            }
        };
        let severity_allows_continuation = runtime.should_continue(verdict.level);
        let level_str = verdict.level.as_str().to_string();
        let injection = crate::advisor::format_injection(&verdict);
        let message = crate::model::Message::User(crate::model::UserMessage {
            content: crate::model::UserContent::Text(injection),
            timestamp: chrono::Utc::now().timestamp_millis(),
        });
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut inner = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
            .await
            .map_err(|error| Error::session(error.to_string()))?;
        if abort.is_some_and(AbortSignal::is_aborted) {
            return Ok(false);
        }
        // The latest authored input resets this counter. Recording each
        // reservation before provider admission also bounds retry/reopen;
        // an interrupted continuation cannot silently acquire a fresh budget.
        let continuations = inner
            .entries_for_current_path()
            .into_iter()
            .rev()
            .find_map(|entry| match entry {
                crate::session::SessionEntry::Custom(custom)
                    if custom.custom_type == ADVISOR_CONTINUATION_ENTRY =>
                {
                    Some(
                        custom
                            .data
                            .as_ref()
                            .and_then(|data| data.get("count"))
                            .and_then(Value::as_u64)
                            .unwrap_or(MAX_ADVISOR_CONTINUATIONS),
                    )
                }
                _ => None,
            })
            .unwrap_or(0);
        let continue_turn = allow_continuation
            && severity_allows_continuation
            && continuations < MAX_ADVISOR_CONTINUATIONS
            && turn.may_continue(&self.agent, abort);
        inner.append_custom_entry(
            "advisor_note".to_string(),
            Some(serde_json::json!({
                "level": level_str,
                "rationale": verdict.rationale,
                "continuationReserved": continue_turn,
            })),
        );
        if continue_turn {
            inner.append_custom_entry(
                ADVISOR_CONTINUATION_ENTRY.to_string(),
                Some(serde_json::json!({"count": continuations + 1})),
            );
        }
        self.agent.queue_generated_steering(message);
        if self.save_enabled {
            persist_session_mutations(
                &mut inner,
                &self.provider_admission,
                AutosaveFlushTrigger::Manual,
            )
            .await?;
        }
        drop(inner);
        (turn.on_event)(AgentEvent::AdvisorNote {
            level: level_str.to_ascii_uppercase(),
            rationale: verdict.rationale,
        });
        Ok(continue_turn)
    }

    /// Every execution entrypoint reaches this boundary after its transcript
    /// save. Pauses and failed turns retain their existing queue; only a
    /// durable completion can admit a new advisory provider request.
    async fn finish_turn_with_advice(
        &mut self,
        result: Result<AssistantMessage>,
        abort: Option<&AbortSignal>,
        turn: AdvisorTurn,
    ) -> Result<AssistantMessage> {
        let result = async {
            let mut message = result?;
            for continuation in 0..=MAX_ADVISOR_CONTINUATIONS {
                if message.stop_reason != StopReason::Stop
                    || time_cap_marker(&message).is_some()
                    || abort.is_some_and(AbortSignal::is_aborted)
                    || turn.time_exhausted()
                {
                    break;
                }
                let should_continue = self
                    .maybe_advise_turn(
                        abort,
                        &turn,
                        continuation < MAX_ADVISOR_CONTINUATIONS,
                    )
                    .await?;
                if !should_continue || !turn.may_continue(&self.agent, abort) {
                    break;
                }
                message = self
                    .run_continuation_attempt(
                        false,
                        abort.cloned(),
                        || true,
                        turn.event_handler(),
                        Some(&turn),
                    )
                    .await?;
            }
            Ok(message)
        }
        .await;
        turn.finish(result)
    }

    pub async fn run_text_with_abort(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let mut turn_system_prompt = None;
        self.run_text_with_abort_capturing_prompt(input, abort, &mut turn_system_prompt, on_event)
            .await
    }

    /// Capture the hook-selected base for one logical recovery turn.
    /// Semantic context and keyword directives are applied later by the agent
    /// and must not become part of the base reapplied on every attempt.
    /// Keep this snapshot only until this logical turn completes; continue it
    /// with [`Self::run_continue_with_abort_and_system_prompt`] after recovery.
    pub async fn run_text_with_abort_capturing_prompt(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        turn_system_prompt: &mut Option<String>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.ensure_provider_reentry_allowed()?;
        let _turn_active = AtomicBoolGuard::activate(&self.extensions_turn_active);
        async {
            // Consume the one-shot provenance before extension dispatch so a
            // blocked/failed input cannot leak it into the next user turn.
            let keyword_scan_override = self.agent.magic_keyword_scan_override.take();
            let outcome = self.dispatch_input_event(input, Vec::new()).await?;
            let (text, images) = match outcome {
                InputEventOutcome::Continue { text, images } => (text, images),
                InputEventOutcome::Block { reason } => {
                    let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                    return Err(Error::extension(message));
                }
            };

            let base_system_prompt = self.agent.system_prompt().map(str::to_string);
            let BeforeAgentStartOutcome {
                messages: custom_messages,
                system_prompt,
            } = self
                .dispatch_before_agent_start(
                    &text,
                    &images,
                    base_system_prompt.as_deref().unwrap_or(""),
                )
                .await;
            *turn_system_prompt = system_prompt.or(base_system_prompt);
            let prompt_scope = SessionTurnPromptGuard::new(self, turn_system_prompt.clone());
            prompt_scope.session.agent.magic_keyword_scan_override = keyword_scan_override;

            let result = if images.is_empty() {
                prompt_scope
                    .session
                    .run_agent_with_text(text, abort, on_event, custom_messages)
                    .await
            } else {
                let content = Self::build_content_blocks_for_input(&text, &images);
                prompt_scope
                    .session
                    .run_agent_with_content(content, abort, on_event, custom_messages)
                    .await
            };
            drop(prompt_scope);
            result
        }
        .await
    }

    pub async fn run_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_with_content_with_abort(content, None, on_event)
            .await
    }

    pub async fn run_with_content_with_abort(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let mut turn_system_prompt = None;
        self.run_with_content_with_abort_capturing_prompt(
            content,
            abort,
            &mut turn_system_prompt,
            on_event,
        )
        .await
    }

    /// Capture the effective turn prompt while admitting native content once.
    /// Recovery uses the captured prompt without replaying input/start hooks.
    pub async fn run_with_content_with_abort_capturing_prompt(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        turn_system_prompt: &mut Option<String>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.ensure_provider_reentry_allowed()?;
        let _turn_active = AtomicBoolGuard::activate(&self.extensions_turn_active);
        async {
            // See the text path above: provenance is one-shot even when an
            // input extension blocks before the Agent loop starts.
            let keyword_scan_override = self.agent.magic_keyword_scan_override.take();
            Self::validate_user_content_blocks(&content)?;
            let (original_text, images) = Self::split_content_blocks_for_input(&content);
            let outcome = self
                .dispatch_input_event(original_text.clone(), images)
                .await?;
            let (text, images) = match outcome {
                InputEventOutcome::Continue { text, images } => (text, images),
                InputEventOutcome::Block { reason } => {
                    let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                    return Err(Error::extension(message));
                }
            };

            let content_for_agent =
                Self::merge_input_event_content(content, &original_text, &text, &images);
            Self::validate_user_content_blocks(&content_for_agent)?;
            let base_system_prompt = self.agent.system_prompt().map(str::to_string);
            let BeforeAgentStartOutcome {
                messages: custom_messages,
                system_prompt,
            } = self
                .dispatch_before_agent_start(
                    &text,
                    &images,
                    base_system_prompt.as_deref().unwrap_or(""),
                )
                .await;
            *turn_system_prompt = system_prompt.or(base_system_prompt);
            let prompt_scope = SessionTurnPromptGuard::new(self, turn_system_prompt.clone());
            prompt_scope.session.agent.magic_keyword_scan_override = keyword_scan_override;

            let result = prompt_scope
                .session
                .run_agent_with_content(content_for_agent, abort, on_event, custom_messages)
                .await;
            drop(prompt_scope);
            result
        }
        .await
    }

    pub async fn revert_last_user_message(&mut self) -> Result<bool> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;

        let reverted = session.revert_last_user_message();
        if reverted {
            let messages = session.to_messages_for_current_path();
            self.agent.replace_messages(messages);
        }
        Ok(reverted)
    }

    /// Revert only the incomplete trailing assistant output of a failed request
    /// (the partial/error message from a transient connection drop), preserving
    /// the user prompt and every completed tool cycle. Used before a retry that
    /// *resumes* the turn (`run_continue_with_abort`) rather than replaying it
    /// from the user message (pi_agent_rust#125). Syncs the agent's in-memory
    /// transcript to the reverted session path so a subsequent resume streams
    /// from the last completed state.
    pub async fn revert_incomplete_response(&mut self) -> Result<bool> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = self
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;

        let reverted = session.revert_incomplete_response();
        if reverted {
            let messages = session.to_messages_for_current_path();
            self.agent.replace_messages(messages);
        }
        Ok(reverted)
    }

    async fn dispatch_input_event(
        &self,
        text: String,
        images: Vec<ImageContent>,
    ) -> Result<InputEventOutcome> {
        let Some(region) = &self.extensions else {
            return Ok(InputEventOutcome::Continue { text, images });
        };

        let images_value = serde_json::to_value(&images).unwrap_or(Value::Null);
        let attachments_value = images_value.clone();
        let text_clone = text.clone();
        let payload = json!({
            "text": text,
            "content": text_clone,
            "images": images_value,
            "attachments": attachments_value,
            "source": self.input_source.as_str(),
        });

        let response = region
            .manager()
            .dispatch_event_with_response(
                ExtensionEventName::Input,
                Some(payload),
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await?;

        Ok(apply_input_event_response(response, text, images))
    }

    async fn dispatch_before_agent_start(
        &self,
        prompt: &str,
        images: &[ImageContent],
        system_prompt: &str,
    ) -> BeforeAgentStartOutcome {
        let Some(region) = &self.extensions else {
            return BeforeAgentStartOutcome {
                messages: Vec::new(),
                system_prompt: None,
            };
        };

        let images_value = serde_json::to_value(images).unwrap_or(Value::Null);
        let payload = json!({
            "prompt": prompt,
            "images": images_value,
            "systemPrompt": system_prompt,
        });

        let response = region
            .manager()
            .dispatch_event_with_response(
                ExtensionEventName::BeforeAgentStart,
                Some(payload),
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await;

        match response {
            Ok(value) => apply_before_agent_start_response(value, Utc::now().timestamp_millis()),
            Err(err) => {
                tracing::warn!("before_agent_start extension hook failed (fail-open): {err}");
                BeforeAgentStartOutcome {
                    messages: Vec::new(),
                    system_prompt: None,
                }
            }
        }
    }

    async fn dispatch_before_compact(
        &self,
        preparation: &compaction::CompactionPreparation,
        branch_entries: &[crate::session::SessionEntry],
        custom_instructions: Option<&str>,
    ) -> SessionBeforeCompactOutcome {
        let Some(region) = &self.extensions else {
            return SessionBeforeCompactOutcome::default();
        };

        let prep_value = compaction::compaction_preparation_to_value(preparation);
        let branch_entries_value =
            serde_json::to_value(branch_entries).unwrap_or(Value::Array(Vec::new()));
        let mut payload = serde_json::Map::new();
        payload.insert("preparation".to_string(), prep_value);
        payload.insert("branchEntries".to_string(), branch_entries_value);
        if let Some(custom_instructions) = custom_instructions {
            payload.insert(
                "customInstructions".to_string(),
                Value::String(custom_instructions.to_string()),
            );
        }

        let response = region
            .manager()
            .dispatch_event_with_response(
                ExtensionEventName::SessionBeforeCompact,
                Some(Value::Object(payload)),
                // gh #178: a dedicated long-running budget — this hook may
                // legitimately await the host compaction bridge.
                ExtensionEventName::SessionBeforeCompact.default_timeout_ms(),
            )
            .await;

        match response {
            Ok(value) => apply_session_before_compact_response(value, preparation.tokens_before),
            Err(err) => {
                tracing::warn!("session_before_compact extension hook failed (fail-open): {err}");
                SessionBeforeCompactOutcome::default()
            }
        }
    }

    fn prepare_semantic_context_prompt(&self) -> Option<PreparedSemanticContextPrompt> {
        let injection = self.semantic_context_bundle.as_ref()?;
        if !injection.enabled {
            return None;
        }

        let provider = self.agent.provider();
        let shape = semantic_context_prompt_shape_for_provider(provider.api());
        let budget = semantic_context_prompt_budget_for_provider(provider.api(), injection);
        let revision = semantic_context_bundle_revision(&injection.bundle);
        let (prompt, stats) =
            render_semantic_context_prompt(&injection.bundle, injection, budget, &revision);
        if prompt.trim().is_empty() {
            tracing::warn!(
                event = "pi.semantic_context.prompt.skipped",
                provider = provider.name(),
                api = provider.api(),
                model = provider.model_id(),
                revision = %revision,
                max_bytes = budget.max_bytes,
                "semantic context bundle prompt skipped because prompt budget was too small"
            );
            return None;
        }

        tracing::info!(
            event = "pi.semantic_context.prompt.injected",
            provider = provider.name(),
            api = provider.api(),
            model = provider.model_id(),
            revision = %revision,
            shape = ?shape,
            prompt_bytes = prompt.len(),
            selected_items = stats.selected_items_included,
            selected_items_omitted = stats.selected_items_omitted,
            validation_commands = stats.validation_commands_included,
            truncated = stats.truncated,
            "semantic context bundle attached to agent turn"
        );

        let details = json!({
            "schema": SEMANTIC_CONTEXT_PROVENANCE_SCHEMA_V1,
            "bundleSchema": injection.bundle.schema.as_str(),
            "bundleRevision": revision.as_str(),
            "provider": {
                "name": provider.name(),
                "api": provider.api(),
                "model": provider.model_id(),
                "promptShape": shape,
            },
            "budget": {
                "requestedMaxItems": injection.max_prompt_items,
                "requestedMaxBytes": injection.max_prompt_bytes,
                "effectiveMaxItems": budget.max_items,
                "effectiveMaxBytes": budget.max_bytes,
            },
            "prompt": {
                "bytes": prompt.len(),
                "selectedItemsIncluded": stats.selected_items_included,
                "selectedItemsOmitted": stats.selected_items_omitted,
                "validationCommandsIncluded": stats.validation_commands_included,
                "validationCommandsOmitted": stats.validation_commands_omitted,
                "exclusionsIncluded": stats.exclusions_included,
                "exclusionsOmitted": stats.exclusions_omitted,
                "truncated": stats.truncated,
            },
            "bundle": {
                "selectedItems": injection.bundle.selected_items.len(),
                "excludedItems": injection.bundle.excluded_items.len(),
                "staleEvidenceSuppressions": injection.bundle.stale_evidence_suppressions.len(),
                "estimatedBytes": injection.bundle.estimated_bytes,
                "estimatedTokens": injection.bundle.estimated_tokens,
                "redactionStatus": injection.bundle.redaction_summary.overall_status,
                "inputFingerprintSha256": injection.bundle.invalidation_policy.input_fingerprint_sha256.as_str(),
                "cacheable": injection.bundle.invalidation_policy.cacheable,
                "workspaceId": injection.bundle.invalidation_policy.workspace_id.as_str(),
                "branch": injection.bundle.invalidation_policy.branch.as_deref(),
                "sessionId": injection.bundle.invalidation_policy.session_id.as_deref(),
            }
        });

        Some(PreparedSemanticContextPrompt {
            prompt,
            revision,
            shape,
            details,
        })
    }

    fn semantic_context_prompt_messages(
        prepared: &PreparedSemanticContextPrompt,
        timestamp: i64,
    ) -> Vec<Message> {
        match prepared.shape {
            SemanticContextPromptShape::CustomUserMessage => {
                vec![Message::Custom(CustomMessage {
                    content: prepared.prompt.clone(),
                    custom_type: SEMANTIC_CONTEXT_CUSTOM_TYPE.to_string(),
                    display: true,
                    details: Some(prepared.details.clone()),
                    timestamp,
                })]
            }
            SemanticContextPromptShape::SystemPromptAppend => {
                vec![Message::Custom(CustomMessage {
                    content: format!(
                        "Semantic context bundle revision {} attached to system prompt.",
                        prepared.revision
                    ),
                    custom_type: SEMANTIC_CONTEXT_CUSTOM_TYPE.to_string(),
                    display: false,
                    details: Some(prepared.details.clone()),
                    timestamp,
                })]
            }
        }
    }

    /// Re-project the last matching provenance record for the active provider.
    /// A model swap can change both its budget and whether the bundle belongs
    /// in a custom message or the system prompt. Replacing a history entry
    /// keeps the durable provenance and message count stable across retries.
    fn prepare_semantic_context_continuation(
        &self,
        history: &mut Vec<Message>,
    ) -> Option<PreparedSemanticContextPrompt> {
        let prepared = self.prepare_semantic_context_prompt()?;
        let existing = history.iter().rposition(|message| {
            matches!(message, Message::Custom(custom)
            if custom.custom_type == SEMANTIC_CONTEXT_CUSTOM_TYPE
                && custom.details.as_ref().is_some_and(|details| {
                    details.get("schema").and_then(Value::as_str)
                        == Some(SEMANTIC_CONTEXT_PROVENANCE_SCHEMA_V1)
                        && details.get("bundleRevision").and_then(Value::as_str)
                            == Some(prepared.revision.as_str())
                }))
        });
        let timestamp = existing
            .and_then(|index| match &history[index] {
                Message::Custom(custom) => Some(custom.timestamp),
                _ => None,
            })
            .unwrap_or_else(|| Utc::now().timestamp_millis());
        let projected = Self::semantic_context_prompt_messages(&prepared, timestamp)
            .into_iter()
            .next()?;
        if let Some(index) = existing {
            history[index] = projected;
        } else {
            // An explicit continuation can start without an earlier bundle
            // record. Persist its first representation with the new artifacts.
            history.push(projected);
        }
        Some(prepared)
    }

    fn semantic_context_system_prompt_for_turn(
        base_system_prompt: Option<String>,
        prepared: Option<&PreparedSemanticContextPrompt>,
    ) -> Option<String> {
        let Some(prepared) = prepared else {
            return base_system_prompt;
        };
        if !matches!(
            prepared.shape,
            SemanticContextPromptShape::SystemPromptAppend
        ) {
            return base_system_prompt;
        }

        let mut prompt = base_system_prompt.unwrap_or_default();
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(&prepared.prompt);
        Some(prompt)
    }

    pub(crate) fn validate_user_content_blocks(blocks: &[ContentBlock]) -> Result<()> {
        if blocks.is_empty() {
            return Err(Error::validation(
                "PI_INPUT_CONTENT: a structured prompt must contain at least one content block",
            ));
        }
        if let Some(index) = blocks.iter().position(|block| {
            !matches!(
                block,
                ContentBlock::Text(_) | ContentBlock::Image(_) | ContentBlock::Media(_)
            )
        }) {
            return Err(Error::validation(format!(
                "PI_INPUT_CONTENT: user content block {index} must be text, image, or media"
            )));
        }
        Ok(())
    }

    fn split_content_blocks_for_input(blocks: &[ContentBlock]) -> (String, Vec<ImageContent>) {
        let mut text = String::new();
        let mut images = Vec::new();
        for block in blocks {
            match block {
                ContentBlock::Text(text_block) if !text_block.text.trim().is_empty() => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&text_block.text);
                }
                ContentBlock::Image(image) => images.push(image.clone()),
                _ => {}
            }
        }
        (text, images)
    }

    fn build_content_blocks_for_input(text: &str, images: &[ImageContent]) -> Vec<ContentBlock> {
        let mut content = Vec::new();
        if !text.trim().is_empty() {
            content.push(ContentBlock::Text(TextContent::new(text.to_string())));
        }
        for image in images {
            content.push(ContentBlock::Image(image.clone()));
        }
        content
    }

    /// Input hooks edit text and images, not the remaining native media.
    /// Preserve the original block order when that projection is unchanged.
    /// A transformed text replaces the old text at its first slot; transformed
    /// images replace image slots in order, with new extra images at the end.
    /// Neither operation can silently discard an intervening audio/video block.
    fn merge_input_event_content(
        content: Vec<ContentBlock>,
        original_text: &str,
        text: &str,
        images: &[ImageContent],
    ) -> Vec<ContentBlock> {
        let text_changed = original_text != text;
        let images_unchanged = content
            .iter()
            .filter(|block| matches!(block, ContentBlock::Image(_)))
            .count()
            == images.len()
            && content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Image(image) => Some(image),
                    _ => None,
                })
                .zip(images)
                .all(|(before, after)| {
                    before.data == after.data && before.mime_type == after.mime_type
                });
        if !text_changed && images_unchanged {
            return content;
        }

        let mut replacement_text = (text_changed && !text.trim().is_empty())
            .then(|| ContentBlock::Text(TextContent::new(text)));
        let mut replacement_images = images.iter().cloned();
        let mut result = Vec::with_capacity(content.len());
        for block in content {
            match block {
                ContentBlock::Text(_) if text_changed => {
                    result.extend(replacement_text.take());
                }
                ContentBlock::Image(_) if !images_unchanged => {
                    result.extend(replacement_images.next().map(ContentBlock::Image));
                }
                other => result.push(other),
            }
        }
        if let Some(text) = replacement_text {
            result.insert(0, text);
        }
        if !images_unchanged {
            result.extend(replacement_images.map(ContentBlock::Image));
        }
        result
    }

    fn take_pending_idle_actions(&self) -> Vec<PendingIdleAction> {
        let Ok(mut actions) = self.extensions_pending_idle_actions.lock() else {
            return Vec::new();
        };
        actions.drain(..).collect()
    }

    async fn run_pending_idle_actions_with_abort(
        &mut self,
        abort: Option<AbortSignal>,
        on_event: AgentEventHandler,
    ) -> Result<()> {
        let actions = self.take_pending_idle_actions();
        if actions.is_empty() {
            return Ok(());
        }

        let source_scope = SessionInputSourceGuard::new(self, InputSource::Extension);
        for action in actions {
            match action {
                PendingIdleAction::CustomMessage(message) => {
                    let handler = Arc::clone(&on_event);
                    source_scope
                        .session
                        .run_custom_message_with_abort(message, abort.clone(), move |event| {
                            handler(event);
                        })
                        .await?;
                }
                PendingIdleAction::UserText(text) => {
                    let handler = Arc::clone(&on_event);
                    // Extension-authored user-shaped text is generated
                    // input. An explicit empty override suppresses the
                    // normal fallback scan of its provider-visible text.
                    source_scope
                        .session
                        .agent
                        .set_magic_keyword_scan_override(Some(String::new()));
                    source_scope
                        .session
                        .run_text_with_abort(text, abort.clone(), move |event| {
                            handler(event);
                        })
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn run_custom_message_with_abort(
        &mut self,
        message: Message,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.ensure_provider_reentry_allowed()?;
        let _turn_active = AtomicBoolGuard::activate(&self.extensions_turn_active);
        let base_system_prompt = self.agent.system_prompt().map(str::to_string);
        let BeforeAgentStartOutcome {
            messages: custom_messages,
            system_prompt,
        } = self
            .dispatch_before_agent_start("", &[], base_system_prompt.as_deref().unwrap_or(""))
            .await;
        let prompt_scope = SessionTurnPromptGuard::new(self, system_prompt.or(base_system_prompt));
        prompt_scope
            .session
            .run_agent_with_prompt_message(message, abort, on_event, custom_messages)
            .await
    }

    async fn run_agent_with_prompt_message(
        &mut self,
        prompt_message: Message,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
        custom_messages: Vec<CustomMessage>,
    ) -> Result<AssistantMessage> {
        let turn = AdvisorTurn::new(self, Arc::new(on_event));
        let on_event = turn.event_handler();
        self.sync_runtime_selection_from_session_header().await?;

        self.maybe_compact(Arc::clone(&on_event)).await?;
        let history = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.to_messages_for_current_path()
        };
        self.agent.replace_messages(history);

        let start_len = self.agent.messages().len();
        let mut prompts = Vec::with_capacity(1 + custom_messages.len());
        prompts.push(prompt_message.clone());
        prompts.extend(custom_messages.into_iter().map(Message::Custom));

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            // Extension custom messages do not start a new authored request.
            session.append_model_message(prompt_message.clone());
            if self.save_enabled {
                let persist_result = flush_turn_autosave(
                    &mut session,
                    &self.provider_admission,
                    AutosaveFlushTrigger::Manual,
                )
                .await;
                finish_turn_persistence(&self.provider_admission, Ok(()), persist_result)?;
            }
        }

        let semantic_context = self.prepare_semantic_context_prompt();
        let semantic_context_messages = semantic_context
            .as_ref()
            .map(|prepared| {
                Self::semantic_context_prompt_messages(prepared, Utc::now().timestamp_millis())
            })
            .unwrap_or_default();
        let streaming_guard = AtomicBoolGuard::activate(&self.extensions_is_streaming);
        let base_system_prompt = self.agent.system_prompt().map(str::to_string);
        self.agent
            .set_system_prompt(Self::semantic_context_system_prompt_for_turn(
                base_system_prompt.clone(),
                semantic_context.as_ref(),
            ));
        let on_event_for_run = Arc::clone(&on_event);
        prompts.extend(semantic_context_messages);
        let result = self
            .agent
            .run_with_messages_with_abort(prompts, abort.clone(), move |event| {
                on_event_for_run(event);
            })
            .await;
        drop(streaming_guard);
        self.agent.set_system_prompt(base_system_prompt);

        let run_incomplete = result.as_ref().map_or(true, |message| {
            matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
        });
        let persist_result = self
            .persist_turn_artifacts(start_len + 1, result.is_err(), run_incomplete)
            .await;

        let result = finish_turn_persistence(&self.provider_admission, result, persist_result);
        self.finish_turn_with_advice(result, abort.as_ref(), turn)
            .await
    }

    pub(crate) async fn run_agent_with_text(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
        custom_messages: Vec<CustomMessage>,
    ) -> Result<AssistantMessage> {
        let turn = AdvisorTurn::new(self, Arc::new(on_event));
        let on_event = turn.event_handler();
        self.sync_runtime_selection_from_session_header().await?;

        self.maybe_compact(Arc::clone(&on_event)).await?;
        let history = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.to_messages_for_current_path()
        };
        self.agent.replace_messages(history);

        let start_len = self.agent.messages().len();

        // Create and persist user message immediately to avoid data loss on API errors
        let user_message = Message::User(UserMessage {
            content: UserContent::Text(input),
            timestamp: Utc::now().timestamp_millis(),
        });
        let mut prompts = Vec::with_capacity(1 + custom_messages.len());
        prompts.push(user_message.clone());
        let semantic_context = self.prepare_semantic_context_prompt();
        let semantic_context_messages = semantic_context
            .as_ref()
            .map(|prepared| {
                Self::semantic_context_prompt_messages(prepared, Utc::now().timestamp_millis())
            })
            .unwrap_or_default();
        prompts.extend(semantic_context_messages);
        prompts.extend(custom_messages.into_iter().map(Message::Custom));

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            // Owned guard: `MutexGuard` is `!Send` (asupersync 0.3.9); this future
            // is reachable from `RuntimeHandle::spawn` (the ACP prompt task in
            // src/acp.rs), which requires the whole future to be `Send`.
            let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.append_model_message(user_message.clone());
            if self.advisor.is_some() && self.input_source != InputSource::Extension {
                session.append_custom_entry(
                    ADVISOR_CONTINUATION_ENTRY.to_string(),
                    Some(serde_json::json!({"count": 0})),
                );
            }
            if self.save_enabled {
                let persist_result = flush_turn_autosave(
                    &mut session,
                    &self.provider_admission,
                    AutosaveFlushTrigger::Manual,
                )
                .await;
                finish_turn_persistence(&self.provider_admission, Ok(()), persist_result)?;
            }
        }

        let streaming_guard = AtomicBoolGuard::activate(&self.extensions_is_streaming);
        let base_system_prompt = self.agent.system_prompt().map(str::to_string);
        self.agent
            .set_system_prompt(Self::semantic_context_system_prompt_for_turn(
                base_system_prompt.clone(),
                semantic_context.as_ref(),
            ));
        let on_event_for_run = Arc::clone(&on_event);
        let result = self
            .agent
            .run_with_messages_with_abort(prompts, abort.clone(), move |event| {
                on_event_for_run(event);
            })
            .await;
        drop(streaming_guard);
        self.agent.set_system_prompt(base_system_prompt);

        // Persist any NEW messages (assistant/tools) generated before the agent stopped,
        // even if it stopped due to an error, skipping the user message we already saved.
        let run_incomplete = result.as_ref().map_or(true, |message| {
            matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
        });
        let persist_result = self
            .persist_turn_artifacts(start_len + 1, result.is_err(), run_incomplete)
            .await;

        let result = finish_turn_persistence(&self.provider_admission, result, persist_result);
        self.finish_turn_with_advice(result, abort.as_ref(), turn)
            .await
    }

    pub(crate) async fn run_agent_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
        custom_messages: Vec<CustomMessage>,
    ) -> Result<AssistantMessage> {
        let turn = AdvisorTurn::new(self, Arc::new(on_event));
        let on_event = turn.event_handler();
        self.sync_runtime_selection_from_session_header().await?;

        self.maybe_compact(Arc::clone(&on_event)).await?;
        let history = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.to_messages_for_current_path()
        };
        self.agent.replace_messages(history);

        let start_len = self.agent.messages().len();

        // Create and persist user message immediately to avoid data loss on API errors
        let user_message = Message::User(UserMessage {
            content: UserContent::Blocks(content),
            timestamp: Utc::now().timestamp_millis(),
        });
        let mut prompts = Vec::with_capacity(1 + custom_messages.len());
        prompts.push(user_message.clone());
        let semantic_context = self.prepare_semantic_context_prompt();
        let semantic_context_messages = semantic_context
            .as_ref()
            .map(|prepared| {
                Self::semantic_context_prompt_messages(prepared, Utc::now().timestamp_millis())
            })
            .unwrap_or_default();
        prompts.extend(semantic_context_messages);
        prompts.extend(custom_messages.into_iter().map(Message::Custom));

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            // Owned guard: `MutexGuard` is `!Send` (asupersync 0.3.9); this future
            // is reachable from `RuntimeHandle::spawn` (the ACP prompt task in
            // src/acp.rs), which requires the whole future to be `Send`.
            let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.append_model_message(user_message.clone());
            if self.advisor.is_some() && self.input_source != InputSource::Extension {
                session.append_custom_entry(
                    ADVISOR_CONTINUATION_ENTRY.to_string(),
                    Some(serde_json::json!({"count": 0})),
                );
            }
            if self.save_enabled {
                let persist_result = flush_turn_autosave(
                    &mut session,
                    &self.provider_admission,
                    AutosaveFlushTrigger::Manual,
                )
                .await;
                finish_turn_persistence(&self.provider_admission, Ok(()), persist_result)?;
            }
        }

        let streaming_guard = AtomicBoolGuard::activate(&self.extensions_is_streaming);
        let base_system_prompt = self.agent.system_prompt().map(str::to_string);
        self.agent
            .set_system_prompt(Self::semantic_context_system_prompt_for_turn(
                base_system_prompt.clone(),
                semantic_context.as_ref(),
            ));
        let on_event_for_run = Arc::clone(&on_event);
        let result = self
            .agent
            .run_with_messages_with_abort(prompts, abort.clone(), move |event| {
                on_event_for_run(event);
            })
            .await;
        drop(streaming_guard);
        self.agent.set_system_prompt(base_system_prompt);

        // Persist any NEW messages (assistant/tools) generated before the agent stopped,
        // even if it stopped due to an error, skipping the user message we already saved.
        let run_incomplete = result.as_ref().map_or(true, |message| {
            matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
        });
        let persist_result = self
            .persist_turn_artifacts(start_len + 1, result.is_err(), run_incomplete)
            .await;

        let result = finish_turn_persistence(&self.provider_admission, result, persist_result);
        self.finish_turn_with_advice(result, abort.as_ref(), turn)
            .await
    }

    /// Resume the current turn after a transient failure WITHOUT adding a new
    /// user message: the agent loop continues from the last completed state
    /// (the user prompt plus any already-completed tool cycles that are still
    /// on the session path), so a retry re-issues only the failed provider
    /// request instead of replaying the whole turn. This is what makes
    /// auto-retry idempotent — no tool re-execution, no re-billing of prior
    /// work (pi_agent_rust#125).
    ///
    /// Callers should strip the failed request's incomplete output first via
    /// [`Self::revert_incomplete_response`] so the resume streams from a clean
    /// tail (no dangling error/partial assistant, no orphaned tool call).
    pub async fn run_continue_with_abort(
        &mut self,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_continue_with_follow_up_with_abort(false, abort, || true, on_event)
            .await
    }

    /// Reuse the first attempt's extension prompt without replaying its hooks.
    /// The caller owns this snapshot only for the duration of one logical turn.
    /// The session's ordinary prompt is restored when this attempt ends.
    pub async fn run_continue_with_abort_and_system_prompt(
        &mut self,
        abort: Option<AbortSignal>,
        turn_system_prompt: Option<&str>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_continue_with_follow_up_with_abort_and_system_prompt(
            false,
            abort,
            turn_system_prompt,
            || true,
            on_event,
        )
        .await
    }

    /// Keep a logical turn's prompt while admitting follow-up input at the
    /// existing durable continuation boundary. RPC uses this when input was
    /// queued just as an attempt finished, including after a recovered attempt.
    pub(crate) async fn run_continue_with_follow_up_with_abort_and_system_prompt(
        &mut self,
        follow_up_first: bool,
        abort: Option<AbortSignal>,
        turn_system_prompt: Option<&str>,
        on_ready: impl FnOnce() -> bool + Send + 'static,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let prompt_scope =
            SessionTurnPromptGuard::new(self, turn_system_prompt.map(str::to_string));
        prompt_scope
            .session
            .run_continue_with_follow_up_with_abort(follow_up_first, abort, on_ready, on_event)
            .await
    }

    /// Resume from persisted history and optionally drain the registered
    /// follow-up fetchers before issuing the first provider request.
    pub(crate) async fn run_continue_with_follow_up_with_abort(
        &mut self,
        follow_up_first: bool,
        abort: Option<AbortSignal>,
        on_ready: impl FnOnce() -> bool + Send + 'static,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.ensure_provider_reentry_allowed()?;
        let _turn_active = AtomicBoolGuard::activate(&self.extensions_turn_active);
        let turn = AdvisorTurn::new(self, Arc::new(on_event));
        let result = self
            .run_continuation_attempt(
                follow_up_first,
                abort.clone(),
                on_ready,
                turn.event_handler(),
                None,
            )
            .await;
        self.finish_turn_with_advice(result, abort.as_ref(), turn)
            .await
    }

    /// One native continuation and its transcript save. Explicit retry,
    /// late queued input, and advisor follow-up all use this same path.
    /// Only automatic advisor follow-up overrides the remaining time budget.
    async fn run_continuation_attempt(
        &mut self,
        follow_up_first: bool,
        abort: Option<AbortSignal>,
        on_ready: impl FnOnce() -> bool + Send + 'static,
        on_event: AgentEventHandler,
        advisor_turn: Option<&AdvisorTurn>,
    ) -> Result<AssistantMessage> {
        self.ensure_provider_reentry_allowed()?;
        self.sync_runtime_selection_from_session_header().await?;

        // Rehydrate the agent transcript from the (already reverted) session
        // path so the resume streams from the last completed state.
        let mut history = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session.to_messages_for_current_path()
        };
        let start_len = history.len();
        let semantic_context = self.prepare_semantic_context_continuation(&mut history);
        self.agent.replace_messages(history);
        let system_prompt = Self::semantic_context_system_prompt_for_turn(
            self.agent.system_prompt().map(str::to_string),
            semantic_context.as_ref(),
        );
        let prompt_scope = SessionTurnPromptGuard::new(self, system_prompt);
        let streaming_guard =
            AtomicBoolGuard::activate(&prompt_scope.session.extensions_is_streaming);
        let on_event_for_run = Arc::clone(&on_event);
        if let Some(turn) = advisor_turn {
            // Rehydration may have awaited a session lock. Sample the shared
            // budget only after that preparation, immediately before dispatch.
            // Some(ZERO) is the native stop boundary, never an unlimited run.
            prompt_scope.session.agent.config.max_time = turn.remaining_time();
        }
        let result = if follow_up_first {
            prompt_scope
                .session
                .agent
                .run_continue_with_follow_up_on_ready_with_abort(
                    abort.clone(),
                    on_ready,
                    move |event| {
                        on_event_for_run(event);
                    },
                )
                .await
        } else {
            on_ready();
            prompt_scope
                .session
                .agent
                .run_continue_with_abort(abort.clone(), move |event| {
                    on_event_for_run(event);
                })
                .await
        };
        drop(streaming_guard);
        drop(prompt_scope);

        // Persist any NEW messages generated by the resume, even on error.
        // No user message was added, so nothing to skip: persist from start_len.
        let run_incomplete = result.as_ref().map_or(true, |message| {
            matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
        });
        let persist_result = self
            .persist_turn_artifacts(start_len, result.is_err(), run_incomplete)
            .await;

        finish_turn_persistence(&self.provider_admission, result, persist_result)
    }

    /// Persist the turn transcript and both audit ledgers under one session
    /// lock. On a failed/aborted turn the incomplete assistant is deliberately
    /// appended last, after audit and queued-steering entries, so
    /// `revert_incomplete_response` can remove it without discarding the
    /// durable audit trail or replaying completed tool effects.
    async fn persist_turn_artifacts(
        &mut self,
        start_len: usize,
        run_failed: bool,
        run_incomplete: bool,
    ) -> Result<()> {
        let mut new_messages = self.agent.messages()[start_len..]
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, message)| (start_len + index, message))
            .collect::<Vec<_>>();
        let incomplete_assistant = if run_incomplete {
            new_messages
                .iter()
                .rposition(|(_, message)| is_incomplete_assistant_message(message))
                .map(|index| new_messages.remove(index).1)
        } else {
            None
        };
        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let repairs = self.agent.drain_repair_ledger()?;
            let activations = self.agent.drain_keyword_ledger();
            for (message_index, message) in new_messages {
                if run_failed && is_synthetic_empty_error_assistant(&message) {
                    continue;
                }
                let reset_advisor = self.advisor.is_some()
                    && self.agent.is_authored_queue_delivery(message_index)
                    && matches!(&message, Message::User(_));
                let entry_id = session.append_model_message(message);
                if reset_advisor {
                    append_advisor_input_reset(&mut session, &entry_id)?;
                }
            }
            self.agent.authored_queue_delivery_indices.clear();
            append_dialect_repair_telemetry(&mut session, &repairs);
            crate::magic_keywords::append_session_telemetry(&mut session, &activations);
            if let Some(message) = incomplete_assistant
                && !(run_failed && is_synthetic_empty_error_assistant(&message))
            {
                session.append_model_message(message);
            }
            if self.save_enabled {
                flush_turn_autosave(
                    &mut session,
                    &self.provider_admission,
                    AutosaveFlushTrigger::Periodic,
                )
                .await?;
            }
        }
        Ok(())
    }
}

fn is_incomplete_assistant_message(message: &Message) -> bool {
    matches!(
        message,
        Message::Assistant(assistant)
            if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted)
    )
}

fn is_synthetic_empty_error_assistant(message: &Message) -> bool {
    matches!(
        message,
        Message::Assistant(assistant)
            if assistant.content.is_empty()
                && matches!(assistant.stop_reason, StopReason::Error)
                && assistant.error_message.is_some()
    )
}

fn semantic_context_prompt_shape_for_provider(api: &str) -> SemanticContextPromptShape {
    match api {
        "bedrock-converse-stream" | "gitlab-chat" => SemanticContextPromptShape::SystemPromptAppend,
        _ => SemanticContextPromptShape::CustomUserMessage,
    }
}

fn semantic_context_prompt_budget_for_provider(
    api: &str,
    injection: &SemanticContextBundleInjection,
) -> SemanticContextPromptBudget {
    let provider_max_bytes = match api {
        "gitlab-chat" => 8 * 1024,
        "bedrock-converse-stream" | "google-gemini" | "google-vertex" => 12 * 1024,
        "openai-responses" | "openai-completions" | "azure-openai" => 24 * 1024,
        "anthropic" => 32 * 1024,
        _ => DEFAULT_SEMANTIC_CONTEXT_PROMPT_MAX_BYTES,
    };
    let provider_max_items = match api {
        "gitlab-chat" => 8,
        "bedrock-converse-stream" | "google-gemini" | "google-vertex" => 12,
        _ => DEFAULT_SEMANTIC_CONTEXT_PROMPT_MAX_ITEMS,
    };

    SemanticContextPromptBudget {
        max_items: injection
            .max_prompt_items
            .min(injection.bundle.budget.max_items)
            .min(provider_max_items),
        max_bytes: injection
            .max_prompt_bytes
            .min(injection.bundle.budget.max_bytes)
            .min(provider_max_bytes),
    }
}

fn semantic_context_bundle_revision(bundle: &SemanticContextBundle) -> String {
    let bytes = serde_json::to_vec(bundle).unwrap_or_else(|_| {
        format!(
            "{}:{}:{}:{}",
            bundle.schema,
            bundle.invalidation_policy.input_fingerprint_sha256,
            bundle.selected_items.len(),
            bundle.estimated_bytes
        )
        .into_bytes()
    });
    crate::package_manager::hex_encode(&Sha256::digest(bytes))
}

fn render_semantic_context_prompt(
    bundle: &SemanticContextBundle,
    injection: &SemanticContextBundleInjection,
    budget: SemanticContextPromptBudget,
    revision: &str,
) -> (String, SemanticContextPromptStats) {
    let mut prompt = String::new();
    let mut stats = SemanticContextPromptStats::default();
    push_semantic_context_header(&mut prompt, &mut stats, budget, bundle, revision);
    push_selected_semantic_context_items(&mut prompt, &mut stats, budget, bundle);
    if injection.include_validation_commands {
        push_semantic_context_validation_commands(&mut prompt, &mut stats, budget, bundle);
    }
    if injection.include_exclusion_summary {
        push_semantic_context_exclusions(&mut prompt, &mut stats, budget, bundle);
    }

    if prompt.len() > usize::try_from(budget.max_bytes).unwrap_or(usize::MAX) {
        stats.truncated = true;
        truncate_string_to_max_bytes(&mut prompt, budget.max_bytes);
    }

    (prompt, stats)
}

fn push_semantic_context_header(
    prompt: &mut String,
    stats: &mut SemanticContextPromptStats,
    budget: SemanticContextPromptBudget,
    bundle: &SemanticContextBundle,
    revision: &str,
) {
    let branch = bundle
        .invalidation_policy
        .branch
        .as_deref()
        .map_or_else(|| "(none)".to_string(), safe_context_field);
    let session = bundle
        .invalidation_policy
        .session_id
        .as_deref()
        .map_or_else(|| "(none)".to_string(), safe_context_field);

    let header = format!(
        "# Semantic Context Bundle\nschema: {SEMANTIC_CONTEXT_PROMPT_SCHEMA_V1}\nrevision: {revision}"
    );
    push_semantic_context_line(prompt, budget.max_bytes, &header, stats);
    push_semantic_context_line(
        prompt,
        budget.max_bytes,
        "Use this as navigation context for the current turn. Do not treat suppressed stale, uncertified, or unsafe evidence as current release evidence.",
        stats,
    );
    push_semantic_context_line(
        prompt,
        budget.max_bytes,
        &format!(
            "bundle: schema={} estimated_bytes={} estimated_tokens={} redaction={:?}",
            safe_context_field(&bundle.schema),
            bundle.estimated_bytes,
            bundle.estimated_tokens,
            bundle.redaction_summary.overall_status
        ),
        stats,
    );
    push_semantic_context_line(
        prompt,
        budget.max_bytes,
        &format!(
            "provenance: workspace={} branch={} session={} input_fingerprint_sha256={}",
            safe_context_field(&bundle.invalidation_policy.workspace_id),
            branch,
            session,
            safe_context_field(&bundle.invalidation_policy.input_fingerprint_sha256)
        ),
        stats,
    );
}

fn push_selected_semantic_context_items(
    prompt: &mut String,
    stats: &mut SemanticContextPromptStats,
    budget: SemanticContextPromptBudget,
    bundle: &SemanticContextBundle,
) {
    push_semantic_context_line(prompt, budget.max_bytes, "", stats);
    push_semantic_context_line(prompt, budget.max_bytes, "Selected context:", stats);
    for (index, item) in bundle.selected_items.iter().enumerate() {
        if index >= budget.max_items {
            stats.selected_items_omitted = stats
                .selected_items_omitted
                .saturating_add(bundle.selected_items.len().saturating_sub(index));
            break;
        }
        if push_semantic_context_item(prompt, stats, budget, item, index + 1) {
            stats.selected_items_included = stats.selected_items_included.saturating_add(1);
        } else {
            stats.selected_items_omitted = stats
                .selected_items_omitted
                .saturating_add(bundle.selected_items.len().saturating_sub(index));
            break;
        }
    }
    if bundle.selected_items.is_empty() {
        push_semantic_context_line(prompt, budget.max_bytes, "- (none)", stats);
    }
}

fn push_semantic_context_validation_commands(
    prompt: &mut String,
    stats: &mut SemanticContextPromptStats,
    budget: SemanticContextPromptBudget,
    bundle: &SemanticContextBundle,
) {
    push_semantic_context_line(prompt, budget.max_bytes, "", stats);
    push_semantic_context_line(
        prompt,
        budget.max_bytes,
        "Suggested validation commands:",
        stats,
    );
    if bundle.suggested_validation_commands.is_empty() {
        push_semantic_context_line(prompt, budget.max_bytes, "- (none)", stats);
        return;
    }

    for (index, command) in bundle.suggested_validation_commands.iter().enumerate() {
        let line = format!("- {}", safe_context_field(command));
        if push_semantic_context_line(prompt, budget.max_bytes, &line, stats) {
            stats.validation_commands_included =
                stats.validation_commands_included.saturating_add(1);
        } else {
            stats.validation_commands_omitted = bundle
                .suggested_validation_commands
                .len()
                .saturating_sub(index);
            break;
        }
    }
}

fn push_semantic_context_exclusions(
    prompt: &mut String,
    stats: &mut SemanticContextPromptStats,
    budget: SemanticContextPromptBudget,
    bundle: &SemanticContextBundle,
) {
    push_semantic_context_line(prompt, budget.max_bytes, "", stats);
    push_semantic_context_line(
        prompt,
        budget.max_bytes,
        "Suppressed or excluded context:",
        stats,
    );
    let mut seen = std::collections::BTreeSet::new();
    let unique_exclusions = bundle
        .stale_evidence_suppressions
        .iter()
        .chain(bundle.excluded_items.iter())
        .filter(|item| {
            seen.insert((
                item.node_type,
                item.source_path.as_str(),
                item.title.as_str(),
                item.reason.as_str(),
            ))
        })
        .collect::<Vec<_>>();

    if unique_exclusions.is_empty() {
        push_semantic_context_line(prompt, budget.max_bytes, "- (none)", stats);
        return;
    }

    let mut included = 0_usize;
    for item in unique_exclusions.iter().take(8) {
        let line = format!(
            "- {:?} {} :: {} reason={}",
            item.node_type,
            safe_context_field(&item.source_path),
            safe_context_field(&item.title),
            safe_context_field(&item.reason)
        );
        if push_semantic_context_line(prompt, budget.max_bytes, &line, stats) {
            included = included.saturating_add(1);
        } else {
            break;
        }
    }
    stats.exclusions_included = stats.exclusions_included.saturating_add(included);
    stats.exclusions_omitted = stats
        .exclusions_omitted
        .saturating_add(unique_exclusions.len().saturating_sub(included));
}

fn push_semantic_context_item(
    prompt: &mut String,
    stats: &mut SemanticContextPromptStats,
    budget: SemanticContextPromptBudget,
    item: &ContextBundleItem,
    ordinal: usize,
) -> bool {
    let freshness = item.freshness_status.map_or_else(
        || "not_applicable".to_string(),
        |status| format!("{status:?}"),
    );
    let line = format!(
        "{ordinal}. {:?} {} :: {}",
        item.node_type,
        safe_context_field(&item.source_path),
        safe_context_field(&item.title)
    );
    let detail = format!(
        "   reason={} score={} tokens={} freshness={} redaction={:?}",
        safe_context_field(&item.reason),
        item.score,
        item.estimated_tokens,
        freshness,
        item.redaction_status
    );
    push_semantic_context_line(prompt, budget.max_bytes, &line, stats)
        && push_semantic_context_line(prompt, budget.max_bytes, &detail, stats)
}

fn push_semantic_context_line(
    prompt: &mut String,
    max_bytes: u64,
    line: &str,
    stats: &mut SemanticContextPromptStats,
) -> bool {
    let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let required = line.len().saturating_add(1);
    if prompt.len().saturating_add(required) > max_bytes {
        stats.truncated = true;
        return false;
    }
    prompt.push_str(line);
    prompt.push('\n');
    true
}

fn truncate_string_to_max_bytes(value: &mut String, max_bytes: u64) {
    let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.truncate(end);
}

fn safe_context_field(value: &str) -> String {
    let mut output = String::with_capacity(value.len().min(512));
    for ch in value.chars() {
        if matches!(ch, '\n' | '\r' | '\t') {
            output.push(' ');
        } else if ch.is_control() {
            output.push('?');
        } else {
            output.push(ch);
        }
        if output.len() >= 512 {
            output.push_str("...");
            break;
        }
    }
    output
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Log a summary of auto-repair events that fired during extension loading.
///
/// Default: one-line summary.  Set `PI_AUTO_REPAIR_VERBOSE=1` for per-extension
/// detail.  Structured tracing events are always emitted regardless of verbosity.
fn log_repair_diagnostics(events: &[crate::extensions_js::ExtensionRepairEvent]) {
    use std::collections::BTreeMap;

    // Always emit structured tracing events for each repair.
    for ev in events {
        tracing::info!(
            event = "extension.auto_repair",
            extension_id = %ev.extension_id,
            pattern = %ev.pattern,
            success = ev.success,
            original_error = %ev.original_error,
            repair_action = %ev.repair_action,
        );
    }

    // Group by pattern for the summary line.
    let mut by_pattern: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for ev in events {
        by_pattern
            .entry(ev.pattern.to_string())
            .or_default()
            .push(&ev.extension_id);
    }

    let verbose = std::env::var("PI_AUTO_REPAIR_VERBOSE")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    if verbose {
        warn!(
            "[auto-repair] {} extension{} auto-repaired:",
            events.len(),
            if events.len() == 1 { "" } else { "s" }
        );
        for ev in events {
            warn!(
                "  {}: {} ({})",
                ev.pattern, ev.extension_id, ev.repair_action
            );
        }
    } else {
        // Compact one-line summary.
        let patterns: Vec<String> = by_pattern
            .iter()
            .map(|(pat, ids)| format!("{pat}:{}", ids.len()))
            .collect();
        tracing::info!(
            event = "extension.auto_repair.summary",
            count = events.len(),
            patterns = %patterns.join(", "),
            "auto-repaired {} extension(s)",
            events.len(),
        );
    }
}

const BLOCK_IMAGES_PLACEHOLDER: &str = "Image reading is disabled.";
const UNSUPPORTED_IMAGES_PLACEHOLDER: &str =
    "[Image omitted: the active model does not support image input.]";

#[derive(Debug, Default, Clone, Copy)]
struct ImageFilterStats {
    removed_images: usize,
    affected_messages: usize,
}

fn apply_image_input_policy(
    messages: &mut [Message],
    block_images: bool,
    model_accepts_images: bool,
) {
    let placeholder = if block_images {
        BLOCK_IMAGES_PLACEHOLDER
    } else if !model_accepts_images {
        UNSUPPORTED_IMAGES_PLACEHOLDER
    } else {
        return;
    };

    if !model_accepts_images {
        // Preserve snapcompact's named degradation and its text-only summary
        // shape before replacing ordinary images with omission explanations.
        crate::compaction_snap::strip_snapcompact_images(messages, false);
    }
    let stats = filter_images_for_provider(messages, placeholder);
    if stats.removed_images > 0 {
        tracing::debug!(
            filtered_images = stats.removed_images,
            affected_messages = stats.affected_messages,
            block_images,
            model_accepts_images,
            "Filtered image content from outbound provider context"
        );
    }
}

fn message_has_images(message: &Message) -> bool {
    let blocks = match message {
        Message::User(UserMessage {
            content: UserContent::Blocks(blocks),
            ..
        }) => blocks.as_slice(),
        Message::Assistant(assistant) => assistant.content.as_slice(),
        Message::ToolResult(result) => result.content.as_slice(),
        Message::User(_) | Message::Custom(_) => return false,
    };
    blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
}

fn filter_images_for_provider(messages: &mut [Message], placeholder: &str) -> ImageFilterStats {
    let mut stats = ImageFilterStats::default();
    for message in messages {
        let removed = filter_images_from_message(message, placeholder);
        if removed > 0 {
            stats.removed_images += removed;
            stats.affected_messages += 1;
        }
    }
    stats
}

fn filter_images_from_message(message: &mut Message, placeholder: &str) -> usize {
    // Most text-only sessions contain no images. Keep their shared assistant
    // and tool-result payloads intact instead of deep-cloning every Arc.
    if !message_has_images(message) {
        return 0;
    }
    match message {
        Message::User(user) => match &mut user.content {
            UserContent::Text(_) => 0,
            UserContent::Blocks(blocks) => filter_image_blocks(blocks, placeholder),
        },
        Message::Assistant(assistant) => {
            let assistant = Arc::make_mut(assistant);
            filter_image_blocks(&mut assistant.content, placeholder)
        }
        Message::ToolResult(tool_result) => {
            filter_image_blocks(&mut Arc::make_mut(tool_result).content, placeholder)
        }
        Message::Custom(_) => 0,
    }
}

fn filter_image_blocks(blocks: &mut Vec<ContentBlock>, placeholder: &str) -> usize {
    let mut removed = 0usize;
    let mut filtered = Vec::with_capacity(blocks.len());

    for block in blocks.drain(..) {
        match block {
            ContentBlock::Image(_) => {
                removed += 1;
                let previous_is_placeholder =
                    filtered
                        .last()
                        .is_some_and(|prev| matches!(prev, ContentBlock::Text(TextContent { text, .. }) if text.as_str().eq(placeholder)));
                if !previous_is_placeholder {
                    filtered.push(ContentBlock::Text(TextContent::new(placeholder)));
                }
            }
            other => filtered.push(other),
        }
    }

    *blocks = filtered;
    removed
}

/// Error text stamped on an assistant message whose stream was cut off by the
/// provider's token limit while a tool call was still being assembled (#148).
const TRUNCATED_TOOL_CALL_ERROR: &str = "Model output truncated before tool call completed (provider stop reason: length); \
     the incomplete tool call was not executed";

/// Validate every call before any peer can reach approval, hooks, or execution.
/// Streaming ToolCallEnd events are content updates, not execution authority.
fn terminal_tool_batch_error(message: &AssistantMessage) -> Option<String> {
    if matches!(
        message.stop_reason,
        StopReason::Error | StopReason::Aborted | StopReason::PauseTurn | StopReason::Refusal
    ) {
        // Failed or refused turns cannot execute. Paused turns must retain
        // their signed provider payload verbatim for continuation.
        return None;
    }

    let mut ids = std::collections::HashSet::new();
    for (index, block) in message.content.iter().enumerate() {
        let ContentBlock::ToolCall(call) = block else {
            continue;
        };
        let detail = if call.id.trim().is_empty() {
            "tool call ID must not be blank"
        } else if !ids.insert(call.id.as_str()) {
            "tool call IDs must be unique within the response"
        } else if call.name.trim().is_empty() {
            "tool name must not be blank"
        } else if !call.arguments.is_object() {
            "tool arguments must be a JSON object"
        } else {
            continue;
        };
        // IDs, names, and arguments come from the provider and may contain
        // secrets. The diagnostic identifies only the field and block index.
        return Some(format!(
            "PI_TOOL_CALL_PROTOCOL: invalid tool call at content index {index}: {detail}; local tool batch withheld"
        ));
    }
    None
}

/// Whether a tool call survived streaming intact enough to execute.
///
/// Two ways a truncated call fails this: an empty `name` (the placeholder
/// `stream_assistant_response` seeds when deltas arrive before the call is
/// identified), or `arguments` left as JSON null. Providers set null exactly
/// when the accumulated argument fragment does not parse — see
/// `openai::finalize_tool_call_arguments` and
/// `anthropic::handle_content_block_stop`, which both log a parse warning and
/// fall back to `Value::Null`. A complete no-argument call carries `{}`, not
/// null, so this does not misjudge argument-less tools.
fn is_complete_tool_call(tool_call: &ToolCall) -> bool {
    !tool_call.name.trim().is_empty() && !tool_call.arguments.is_null()
}

/// Whether the provider's token limit left an incomplete or missing tool batch.
///
/// Count distinct indices from `ToolCall{Start,Delta,End}` events so repeated
/// updates do not invent extra calls. A terminal snapshot can omit an unfinished
/// call; even if a peer completed, executing it would apply only part of the
/// batch. Done-only providers have no streamed indices and remain supported.
///
/// Otherwise retain #148's incomplete-only detection. Malformed peers still
/// present in the snapshot are refused by `terminal_tool_batch_error`.
fn is_truncated_before_tool_call(
    message: &AssistantMessage,
    streamed_tool_call_count: usize,
) -> bool {
    if message.stop_reason != StopReason::Length {
        return false;
    }

    let mut final_tool_call_count = 0;
    let mut has_complete_tool_call = false;
    for block in &message.content {
        if let ContentBlock::ToolCall(tool_call) = block {
            final_tool_call_count += 1;
            has_complete_tool_call |= is_complete_tool_call(tool_call);
        }
    }

    final_tool_call_count < streamed_tool_call_count
        || (!has_complete_tool_call && (streamed_tool_call_count > 0 || final_tool_call_count > 0))
}

/// Extract tool calls from content blocks.
/// Concatenated visible text of an assistant message, for stop
/// classification (bd-cv653.3.15).
fn assistant_text_content(content: &[ContentBlock]) -> String {
    let mut text = String::new();
    for block in content {
        if let ContentBlock::Text(part) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&part.text);
        }
    }
    text
}

fn extract_tool_calls(content: &[ContentBlock]) -> Vec<ToolCall> {
    content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::ToolCall(tc) = block {
                Some(tc.clone())
            } else {
                None
            }
        })
        .collect()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
include!("agent_tests.rs");
