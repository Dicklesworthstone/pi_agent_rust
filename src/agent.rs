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
    CompactionAdmissionReason, CompactionAdmissionSignals, CompactionQuota, CompactionWorkerState,
};
use crate::error::{Error, Result};
use crate::extension_events::{
    BeforeAgentStartOutcome, InputEventOutcome, SessionBeforeCompactOutcome,
    apply_before_agent_start_response, apply_input_event_response,
    apply_session_before_compact_response,
};
use crate::extension_tools::collect_extension_tool_wrappers;
use crate::extensions::{
    EXTENSION_EVENT_TIMEOUT_MS, ExtensionAiCompletionRequest, ExtensionDeliverAs,
    ExtensionEventName, ExtensionHostActions, ExtensionLoadSpec, ExtensionManager, ExtensionPolicy,
    ExtensionRegion, ExtensionRuntimeHandle, ExtensionSendMessage, ExtensionSendUserMessage,
    JsExtensionLoadSpec, JsExtensionRuntimeHandle, NativeRustExtensionLoadSpec,
    NativeRustExtensionRuntimeHandle, RepairPolicyMode, resolve_extension_load_spec,
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
use std::sync::Arc;
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

/// Configuration for the agent.
#[derive(Clone)]
pub struct AgentConfig {
    /// System prompt to use for all requests.
    pub system_prompt: Option<String>,

    /// Maximum tool call iterations before stopping.
    pub max_tool_iterations: usize,

    /// Default stream options.
    pub stream_options: StreamOptions,

    /// Whether the active model accepts image inputs (bd-cv653.7.6).
    /// When false, snapcompact compaction frames are stripped from the
    /// outbound context with a logged degradation reason.
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
/// state only and is deliberately not serialized into [`Message`].
#[derive(Debug, Clone)]
pub struct QueuedAgentMessage {
    message: Message,
    keyword_scan_source: Option<String>,
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
        }
    }

    /// Wrap a generated/internal message. Its provider-visible text is never
    /// eligible for magic-keyword activation.
    #[must_use]
    pub const fn generated(message: Message) -> Self {
        Self {
            message,
            keyword_scan_source: None,
        }
    }

    #[must_use]
    pub const fn message(&self) -> &Message {
        &self.message
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

    fn next_entry(&mut self, delivery: QueuedAgentMessage) -> SequencedQueuedMessage {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        SequencedQueuedMessage {
            seq,
            enqueued_at: Utc::now().timestamp_millis(),
            delivery,
        }
    }

    fn push(&mut self, kind: QueueKind, delivery: QueuedAgentMessage) -> u64 {
        let entry = self.next_entry(delivery);
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
                if self.follow_up.len() >= MAX_FOLLOW_UP_QUEUE_SIZE {
                    tracing::warn!(
                        "Follow-up queue full ({} messages), dropping oldest message",
                        MAX_FOLLOW_UP_QUEUE_SIZE
                    );
                    self.follow_up.pop_front();
                }
                self.follow_up.push_back(entry);
            }
        }
        seq
    }

    fn push_steering(&mut self, delivery: QueuedAgentMessage) -> u64 {
        self.push(QueueKind::Steering, delivery)
    }

    fn push_follow_up(&mut self, delivery: QueuedAgentMessage) -> u64 {
        self.push(QueueKind::FollowUp, delivery)
    }

    fn push_follow_up_lossless(&mut self, delivery: QueuedAgentMessage) -> u64 {
        let entry = self.next_entry(delivery);
        let seq = entry.seq;
        self.follow_up.push_back(entry);
        seq
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
}

// ============================================================================
// Agent Event
// ============================================================================

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
        attempt: u32,
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
    /// Advisor verdict delivered into the session (bd-cv653.3.3).
    AdvisorNote { level: String, rationale: String },
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
        if self.is_aborted() {
            return;
        }

        loop {
            self.inner.notify.notified().await;
            if self.is_aborted() {
                return;
            }
        }
    }
}

/// The agent runtime that orchestrates LLM calls and tool execution.
pub struct Agent {
    /// The LLM provider.
    provider: Arc<dyn Provider>,

    /// Tool registry.
    tools: ToolRegistry,

    /// Agent configuration.
    config: AgentConfig,

    /// Optional extension manager for tool/event hooks.
    extensions: Option<ExtensionManager>,

    /// Message history.
    messages: Vec<Message>,

    /// Fetchers for queued steering messages (interrupts).
    steering_fetchers: Vec<MessageFetcher>,

    /// Owning surface's bounded follow-up source used for resumed handoffs.
    initial_follow_up_fetcher: Option<MessageFetcher>,

    /// Fetchers for queued follow-up messages (idle).
    follow_up_fetchers: Vec<MessageFetcher>,

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
        let keyword_max_thinking_level = config
            .stream_options
            .thinking_level
            .unwrap_or(crate::model::ThinkingLevel::Off);
        Self {
            provider,
            tools,
            config,
            extensions: None,
            messages: Vec::new(),
            steering_fetchers: Vec::new(),
            initial_follow_up_fetcher: None,
            follow_up_fetchers: Vec::new(),
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
        if self.tools.tools().is_empty() {
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
                            self.tools.get(name).is_some()
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

    /// The shared tool approval state (bd-cv653.3.19).
    #[must_use]
    pub fn approval_state(&self) -> Option<crate::approval::ApprovalState> {
        self.config.approval_state.clone()
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
    }

    /// Truncate the active message history to `len` (rewind/retry keep the
    /// full span in the session tree — bd-cv653.3.7).
    pub fn truncate_messages(&mut self, len: usize) {
        self.messages.truncate(len);
    }

    /// Add a message to the history.
    pub fn add_message(&mut self, message: Message) {
        if self.messages.len() >= MAX_AGENT_MESSAGES {
            tracing::warn!(
                "Agent message history full ({} messages), dropping oldest message",
                MAX_AGENT_MESSAGES
            );
            self.messages.remove(0);
        }
        self.messages.push(message);
    }

    /// Replace the message history.
    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Replace the provider implementation (used for model/provider switching).
    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.provider = provider;
        // A bare provider object does not carry registry capability metadata.
        // Reset fail-closed so a new or external call site cannot accidentally
        // carry a high thinking cap from the previous model. Registry-aware
        // switch paths immediately install the target model's clamped cap.
        self.keyword_max_thinking_level = crate::model::ThinkingLevel::Off;
        self.tool_call_dialect = crate::dialects::Dialect::Native;
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
            self.steering_fetchers.push(fetcher);
        }
        if let Some(fetcher) = follow_up {
            self.follow_up_fetchers.push(fetcher);
        }
    }

    /// Register the owning surface's bounded follow-up source for a resumed
    /// handoff. Unlike additive follow-up sources, this source is fetched on
    /// its own before the first provider request.
    pub(crate) fn register_initial_follow_up_fetcher(&mut self, fetcher: MessageFetcher) {
        self.initial_follow_up_fetcher = Some(fetcher);
    }

    /// Extend the tool registry with additional tools (e.g. extension-registered tools).
    pub fn extend_tools<I>(&mut self, tools: I)
    where
        I: IntoIterator<Item = Box<dyn Tool>>,
    {
        self.tools.extend(tools);
        self.cached_tool_defs = None; // Invalidate cache when tools change
    }

    /// Whether a tool implementation is enabled for this agent session.
    ///
    /// Interactive host commands use this to inherit opt-in tool gates
    /// without exposing the registry itself.
    #[must_use]
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.get(name).is_some()
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

    /// Count queued messages (steering + follow-up).
    #[must_use]
    pub fn queued_message_count(&self) -> usize {
        self.message_queue.pending_count()
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    /// The session undo recorder attached to the tool registry, if any
    /// (bd-cv653.3.13).
    #[must_use]
    pub fn mutation_recorder(&self) -> Option<Arc<crate::undo::FileMutationRecorder>> {
        self.tools.mutation_recorder()
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

    /// Build context for a completion request.
    fn build_context(&mut self) -> Context<'_> {
        let messages: Cow<'_, [Message]> = if self.config.block_images {
            let mut msgs = self.messages.clone();
            // Filter out hidden custom messages.
            msgs.retain(|m| match m {
                Message::Custom(c) => c.display,
                _ => true,
            });
            let stats = filter_images_for_provider(&mut msgs);
            if stats.removed_images > 0 {
                tracing::debug!(
                    filtered_images = stats.removed_images,
                    affected_messages = stats.affected_messages,
                    "Filtered image content from outbound provider context (images.block_images=true)"
                );
            }
            Cow::Owned(msgs)
        } else {
            // Check if we need to filter hidden custom messages to avoid cloning if not needed.
            let has_hidden = self.messages.iter().any(|m| match m {
                Message::Custom(c) => !c.display,
                _ => false,
            });

            if has_hidden {
                let mut msgs = self.messages.clone();
                msgs.retain(|m| match m {
                    Message::Custom(c) => c.display,
                    _ => true,
                });
                Cow::Owned(msgs)
            } else {
                Cow::Borrowed(self.messages.as_slice())
            }
        };

        // Snapcompact vision gating (bd-cv653.7.6): text-only models never see
        // rasterized compaction frames; the helper logs the degradation with a
        // stable reason code and never touches user-pasted images.
        let messages = if self.config.model_accepts_images {
            messages
        } else {
            let mut owned = messages.into_owned();
            let _stats = crate::compaction_snap::strip_snapcompact_images(&mut owned, false);
            std::borrow::Cow::Owned(owned)
        };

        // Borrow cached tool defs if available; otherwise build + cache + borrow.
        // Load modes (bd-cv653.1.6): discoverable-tier tools are excluded
        // until promoted; the generation counter invalidates on promotion.
        let generation = self
            .tool_defs_generation
            .load(std::sync::atomic::Ordering::SeqCst);
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
            let defs: Vec<ToolDef> = self
                .tools
                .tools()
                .iter()
                .filter(|t| !self.tools.is_discoverable(t.name()) || promoted.contains(t.name()))
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

    /// Outbound secrets transform (bd-cv653.7.9): obfuscate credential
    /// shapes in the context before the provider sees it (or refuse the
    /// send in block mode). Off mode is byte-identical.
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
        let mut total = 0usize;
        let mut labels: Vec<String> = Vec::new();

        if let Some(prompt) = context.system_prompt.as_deref() {
            let out = Self::secrets_transform_text(
                prompt,
                &mut self.secrets_vault,
                mode,
                &extra,
                &mut total,
                &mut labels,
            )?;
            context.system_prompt = Some(std::borrow::Cow::Owned(out));
        }
        for message in context.messages.to_mut().iter_mut() {
            match message {
                Message::User(user) => {
                    Self::secrets_transform_user_content(
                        &mut user.content,
                        &mut self.secrets_vault,
                        mode,
                        &extra,
                        &mut total,
                        &mut labels,
                    )?;
                }
                Message::Assistant(assistant) => {
                    let assistant_mut = Arc::make_mut(assistant);
                    for block in &mut assistant_mut.content {
                        match block {
                            ContentBlock::Text(t) => {
                                t.text = Self::secrets_transform_text(
                                    &t.text,
                                    &mut self.secrets_vault,
                                    mode,
                                    &extra,
                                    &mut total,
                                    &mut labels,
                                )?;
                            }
                            ContentBlock::Thinking(t) => {
                                t.thinking = Self::secrets_transform_text(
                                    &t.thinking,
                                    &mut self.secrets_vault,
                                    mode,
                                    &extra,
                                    &mut total,
                                    &mut labels,
                                )?;
                            }
                            _ => {}
                        }
                    }
                }
                Message::ToolResult(result) => {
                    let result_mut = Arc::make_mut(result);
                    for block in &mut result_mut.content {
                        if let ContentBlock::Text(t) = block {
                            t.text = Self::secrets_transform_text(
                                &t.text,
                                &mut self.secrets_vault,
                                mode,
                                &extra,
                                &mut total,
                                &mut labels,
                            )?;
                        }
                    }
                }
                Message::Custom(_) => {}
            }
        }
        if total > 0 {
            tracing::info!(
                event = "pi.secrets.outbound",
                detections = total,
                rules = ?labels,
                "secrets obfuscated in outbound context (redacted)"
            );
        }
        Ok(context)
    }

    fn secrets_transform_text(
        text: &str,
        vault: &mut crate::secrets::SecretVault,
        mode: crate::secrets::SecretsMode,
        extra: &[regex::Regex],
        total: &mut usize,
        labels: &mut Vec<String>,
    ) -> Result<String> {
        if mode == crate::secrets::SecretsMode::Block {
            crate::secrets::gate_outbound(text, mode, extra)?;
        }
        let (out, audit) = crate::secrets::obfuscate(text, vault, extra);
        *total += audit.detections;
        for rule in audit.rules {
            if !labels.contains(&rule) {
                labels.push(rule);
            }
        }
        Ok(out)
    }

    /// Outbound secret hygiene for a user message. Attachment-carrying
    /// messages (`Blocks`: text + images) must get the same treatment as
    /// plain text — that is the shape the interactive app sends whenever an
    /// attachment exists.
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
                for block in blocks.iter_mut() {
                    if let ContentBlock::Text(t) = block {
                        t.text = Self::secrets_transform_text(
                            &t.text, vault, mode, extra, total, labels,
                        )?;
                    }
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
        let saved_thinking = self.config.stream_options.thinking_level;
        let saved_system_prompt = self.config.system_prompt.clone();
        let result = self
            .run_loop_inner(
                prompts,
                initial_follow_up,
                initial_queue_ready,
                on_event,
                abort,
            )
            .await;
        self.config.stream_options.thinking_level = saved_thinking;
        self.config.system_prompt = saved_system_prompt;
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
    /// during the current run. The caller owns the turn-wide de-duplication
    /// set so initial prompts, steering, and follow-ups share one activation
    /// boundary and cannot inject the same directive repeatedly.
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

        if prompts.is_empty() && !self.retry_keyword_activations.is_empty() {
            let retry_hits = self.retry_keyword_activations.clone();
            turn_keyword_words.extend(retry_hits.iter().map(|hit| hit.word.clone()));
            self.apply_magic_keyword_effects(&retry_hits);
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
        let mut follow_up_staged = false;
        let mut required_initial_follow_up_pending = initial_follow_up;
        let mut pending_messages = if initial_follow_up {
            self.fetch_initial_follow_up_messages().await;
            follow_up_staged = self.message_queue.follow_up_batch_len() > 0;
            Vec::new()
        } else {
            self.drain_steering_messages().await
        };
        if initial_follow_up && !follow_up_staged {
            return Err(Error::session(
                "accepted follow-up was unavailable at continuation boundary",
            ));
        }
        let mut turn_recovery =
            crate::turn_recovery::TurnRecoveryState::new(self.config.turn_recovery);

        // Wall-clock run cap (bd-cv653.3.7, --max-time): checked at turn
        // boundaries — never mid-tool-call. On expiry the run stops with a
        // marker instead of starting another turn.
        let run_started = std::time::Instant::now();
        let max_time = self.config.max_time;

        loop {
            let mut has_more_tool_calls = true;
            let mut steering_after_tools: Option<Vec<QueuedAgentMessage>> = None;

            while has_more_tool_calls || !pending_messages.is_empty() {
                if !required_initial_follow_up_pending
                    && let Some(cap) = max_time
                    && run_started.elapsed() >= cap
                {
                    let marker = format!(
                        "time cap reached after {}s (--max-time); stopping at the turn boundary",
                        cap.as_secs()
                    );
                    tracing::info!("{marker}");
                    let marker_message = Message::User(UserMessage {
                        content: UserContent::Text(format!("[TIME CAP] {marker}")),
                        timestamp: Utc::now().timestamp_millis(),
                    });
                    on_event(AgentEvent::MessageStart {
                        message: marker_message.clone(),
                    });
                    on_event(AgentEvent::MessageEnd {
                        message: marker_message,
                    });
                    return Ok(AssistantMessage {
                        content: vec![crate::model::ContentBlock::Text(TextContent::new(format!(
                            "[time cap reached] {marker}"
                        )))],
                        ..AssistantMessage::default()
                    });
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

                let delivering_follow_up = follow_up_staged;
                let delivering_required_initial_follow_up =
                    delivering_follow_up && required_initial_follow_up_pending;
                if delivering_follow_up {
                    pending_messages = self.message_queue.pop_follow_up();
                    follow_up_staged = false;
                    required_initial_follow_up_pending = false;
                }

                for delivery in std::mem::take(&mut pending_messages) {
                    self.apply_magic_keywords_for_delivery(&delivery, &mut turn_keyword_words);
                    let message = delivery.into_message();
                    // Advisor notes get a dedicated event on delivery
                    // (bd-cv653.3.3) so RPC/ACP surfaces can render them.
                    if let Message::User(user) = &message
                        && let crate::model::UserContent::Text(text) = &user.content
                        && let Some(rest) = text.strip_prefix("[ADVISOR:")
                    {
                        let level = rest.split(']').next().unwrap_or("NOTE").to_string();
                        let rationale = rest
                            .split_once(']')
                            .map(|(_, tail)| tail.trim().to_string())
                            .unwrap_or_default();
                        on_event(AgentEvent::AdvisorNote { level, rationale });
                    }
                    self.messages.push(message.clone());
                    on_event(AgentEvent::MessageStart {
                        message: message.clone(),
                    });
                    on_event(AgentEvent::MessageEnd {
                        message: message.clone(),
                    });
                    new_messages.push(message);
                }

                if delivering_required_initial_follow_up
                    && let Some(on_ready) = initial_queue_ready.take()
                    && !on_ready()
                {
                    return Err(Error::session(
                        "required follow-up source was unavailable at continuation boundary",
                    ));
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
                    Ok(msg) => self.maybe_repair_dialect_tool_calls(msg),
                    Err(err) => {
                        let err_string = err.to_string();
                        let steering_to_add = self.drain_steering_messages().await;
                        for delivery in steering_to_add {
                            self.apply_magic_keywords_for_delivery(
                                &delivery,
                                &mut turn_keyword_words,
                            );
                            let message = delivery.into_message();
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
                        let message = delivery.into_message();
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
                let execute_local_tools = !pause_turn && !tool_calls.is_empty();
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
                            let message = delivery.into_message();
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
                                let message = delivery.into_message();
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
                if pending_messages.is_empty() && !has_more_tool_calls {
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
        for fetcher in &self.steering_fetchers {
            let fetched = self.fetch_messages(Some(fetcher)).await;
            for message in fetched {
                self.message_queue.push_steering(message);
            }
        }
        self.message_queue.pop_steering()
    }

    async fn stage_follow_up_messages(&mut self) -> bool {
        if self.message_queue.follow_up_batch_len() > 0 {
            return true;
        }

        let owning_surface = self
            .fetch_messages(self.initial_follow_up_fetcher.as_ref())
            .await;
        if !owning_surface.is_empty() {
            for message in owning_surface {
                // The owning surface already applied its queue mode and
                // admission bound. Preserve its complete accepted batch until
                // the synchronous delivery point after TurnStart.
                self.message_queue.push_follow_up_lossless(message);
            }
            return true;
        }

        self.fetch_additive_follow_up_messages().await;
        self.message_queue.follow_up_batch_len() > 0
    }

    async fn fetch_additive_follow_up_messages(&mut self) {
        for fetcher in &self.follow_up_fetchers {
            let fetched = self.fetch_messages(Some(fetcher)).await;
            for message in fetched {
                self.message_queue.push_follow_up(message);
            }
        }
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
        let messages = self
            .dispatch_context_event(&base_messages)
            .await
            .unwrap_or(base_messages);
        let context = Context::owned(system_prompt, messages, tools);
        // Secrets vault (bd-cv653.7.9): credential shapes in the outbound
        // context become stable placeholders (obfuscate) or a named refusal
        // (block) before any provider sees them. The vault is in-memory and
        // dies with the session.
        let context = self.apply_secrets_outbound(context)?;
        let mut stream = provider.stream(&context, &stream_options).await?;

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
        // #148: whether this streaming response ever began assembling a tool
        // call. Needed to tell "cut off mid tool call" apart from "cut off in
        // the middle of plain prose", which is an ordinary `Length` stop.
        let mut tool_call_started = false;

        'stream: loop {
            if checkpoint_cx.checkpoint().is_err() {
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
                    tool_call_started = true;
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
                    tool_call_started = true;
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
                    tool_call_started = true;
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
                StreamEvent::Done { mut message, .. } => {
                    // #148: a turn cut short by the provider's token limit while a
                    // tool call was still being assembled must not finalize as an
                    // ordinary stop. The half-built call is unusable, so the run
                    // loop would see no executable tool call and quietly end the
                    // turn. Re-stamping it as `Error` routes it through the
                    // existing error handling, which surfaces `error_message` to
                    // the caller via `AgentEnd`.
                    if is_truncated_before_tool_call(&message, tool_call_started) {
                        message.stop_reason = StopReason::Error;
                        message.error_message = Some(TRUNCATED_TOOL_CALL_ERROR.to_string());
                    }
                    return Ok(self.finalize_assistant_message(message, &on_event, added_partial));
                }
                StreamEvent::Error { error, .. } => {
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
            let truncated_tool_call = is_truncated_before_tool_call(&final_msg, tool_call_started);
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
        message: AssistantMessage,
        on_event: &Arc<dyn Fn(AgentEvent) + Send + Sync>,
        added_partial: bool,
    ) -> AssistantMessage {
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
            let all_fut = stream::iter(futures)
                .buffer_unordered(parallelism)
                .collect::<Vec<_>>()
                .fuse();
            let abort_fut = signal.wait().fuse();
            futures::pin_mut!(all_fut, abort_fut);

            match select(all_fut, abort_fut).await {
                Either::Left((batch_results, _)) => batch_results,
                Either::Right(_) => Vec::new(), // Aborted
            }
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
        let tool_call = self.restore_secrets_inbound(tool_call);

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
    pub fn restore_secrets_inbound(&self, mut tool_call: ToolCall) -> ToolCall {
        if crate::secrets::SecretsMode::from_setting(
            self.config.secrets.as_ref().and_then(|s| s.mode.as_deref()),
        ) == crate::secrets::SecretsMode::Off
        {
            return tool_call;
        }
        tool_call.arguments = Self::restore_json_value(&self.secrets_vault, tool_call.arguments);
        tool_call
    }

    fn restore_json_value(
        vault: &crate::secrets::SecretVault,
        value: serde_json::Value,
    ) -> serde_json::Value {
        match value {
            serde_json::Value::String(text) => serde_json::Value::String(vault.restore(&text)),
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items
                    .into_iter()
                    .map(|item| Self::restore_json_value(vault, item))
                    .collect(),
            ),
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.into_iter()
                    .map(|(key, item)| (vault.restore(&key), Self::restore_json_value(vault, item)))
                    .collect(),
            ),
            other => other,
        }
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
        if let Some(details) = &mut output.details
            && let Some(text) = serde_json::to_string(details).ok()
        {
            // Mask maps real secret values back to placeholders; only
            // re-parse when something actually changed.
            let masked = self.secrets_vault.mask(&text);
            if masked != text
                && let Ok(value) = serde_json::from_str(&masked)
            {
                *details = value;
            }
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
        Self::secrets_transform_text(
            text,
            &mut self.secrets_vault,
            mode,
            &extra,
            &mut total,
            &mut labels,
        )
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
                if !self.tools.is_discoverable(name) {
                    return Some(Self::xdev_text_output(
                        &if self.tools.get(name).is_some() {
                            format!("{name:?} is already in the live schema — call it directly.")
                        } else {
                            format!("No discoverable tool named {name:?}; use xdev list.")
                        },
                        true,
                    ));
                }
                let inner_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
                let Some(inner) = self.tools.get(name) else {
                    return Some(Self::xdev_text_output(
                        &format!("Tool {name:?} not registered"),
                        true,
                    ));
                };
                let mut output = inner
                    .execute(&tool_call.id, inner_args, None)
                    .await
                    .unwrap_or_else(|err| Self::xdev_text_output(&err.to_string(), true));
                output.details = Some(json!({
                    "dispatchedVia": "xdev",
                    "tool": name,
                }));
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
                if !self.tools.is_discoverable(name) {
                    return Some(Self::xdev_text_output(
                        &if self.tools.get(name).is_some() {
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

    /// Effective side effects for a tool call (bd-cv653.3.5): for `xdev run`
    /// dispatches, the INNER tool's effects decide (read-only runs stay
    /// allowed in plan mode); unknown names default to read-only here — the
    /// not-found path produces its own error downstream.
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
            return self
                .tools
                .get(inner)
                .map_or_else(crate::tools::ToolEffects::read, crate::tools::Tool::effects);
        }
        self.tools
            .get(&tool_call.name)
            .map_or_else(crate::tools::ToolEffects::read, crate::tools::Tool::effects)
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

        // Find the tool
        let Some(tool) = self.tools.get(&tool_call.name) else {
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

        match tool
            .execute(
                &tool_call.id,
                tool_call.arguments.clone(),
                Some(Box::new(update_callback)),
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
    /// The tool registry passed to the runtime during boot.
    pub tools: Arc<ToolRegistry>,
}

/// RAII guard that resets an `AtomicBool` to `false` on drop, ensuring the
/// flag is cleared even if the enclosing async task is cancelled.
struct AtomicBoolGuard(Arc<AtomicBool>);

impl AtomicBoolGuard {
    fn activate(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(Arc::clone(flag))
    }
}

impl Drop for AtomicBoolGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
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
}

#[derive(Clone)]
struct ExtensionAiCompletionHostState {
    provider: Arc<dyn Provider>,
    stream_options: StreamOptions,
    models: Vec<Value>,
}

impl AgentSessionHostActions {
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
    async fn send_message(&self, message: ExtensionSendMessage) -> Result<()> {
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

    async fn send_user_message(&self, message: ExtensionSendUserMessage) -> Result<()> {
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

    async fn compact_session(&self, preparation: Value) -> Result<Value> {
        // gh #167 / bd-i28yz: bridge ctx.compact() / pi-coding-agent
        // compact() to the native compaction engine. The preparation JSON is
        // untrusted extension input; the strict deserializer rejects
        // malformed shapes instead of defaulting them.
        let preparation = crate::compaction::compaction_preparation_from_value(&preparation)?;

        // Always compact with the SESSION's own provider + resolved API key
        // (the same state the agent's auto-compaction path reads via
        // `agent.provider()` / `agent.stream_options().api_key`, mirrored
        // into the ai-completion host state and refreshed on model change).
        // `custom_instructions` is None to match the auto-compaction path.
        //
        // Lock order / re-entrancy: this briefly locks only the
        // ai-completion `StdMutex` (exactly like `complete_ai` /
        // `list_ai_models`) and never touches the session mutex, while
        // `dispatch_before_compact` holds neither when awaiting extension
        // handlers -- so an extension calling compact() from inside
        // `session_before_compact` cannot deadlock.
        let (provider, api_key) = {
            let state = self.ai_completion.lock().map_err(|_| {
                Error::extension("extension completion host state mutex poisoned".to_string())
            })?;
            (
                Arc::clone(&state.provider),
                state.stream_options.api_key.clone().unwrap_or_default(),
            )
        };

        let result = crate::compaction::compact(preparation, provider, &api_key, None).await?;
        serde_json::to_value(&result).map_err(|err| {
            Error::extension(format!("serialize extension compaction result: {err}"))
        })
    }

    async fn compact_session_native(&self, preparation: Value, request: Value) -> Result<Value> {
        // gh #167: host-mediated native-Responses compaction. The extension
        // composes the compact request (so replay chains of previously
        // stored windows keep working) but the host owns endpoint + auth:
        // the request is reduced to an allowlist, credential-shaped keys are
        // rejected loudly, the model is pinned to the session's own, and the
        // POST happens inside the provider with the session's credentials.
        // Every failure is an `Err` so the calling extension fails open to
        // pi's default compaction (never a fabricated result).
        let preparation = crate::compaction::compaction_preparation_from_value(&preparation)?;

        let (provider, stream_options) = {
            let state = self.ai_completion.lock().map_err(|_| {
                Error::extension("extension completion host state mutex poisoned".to_string())
            })?;
            (Arc::clone(&state.provider), state.stream_options.clone())
        };

        if provider.api() != "openai-responses" {
            return Err(Error::validation(format!(
                "native compaction requires the session's provider to use the openai-responses API (current: {})",
                provider.api()
            )));
        }

        let sanitized = sanitize_native_compact_request(&request, provider.model_id())?;
        let response = provider.compact_native(&sanitized, &stream_options).await?;

        let (read_files, modified_files) =
            crate::compaction::compute_file_lists(&preparation.file_ops);
        shape_native_compact_result(
            &response,
            &preparation.first_kept_entry_id,
            preparation.tokens_before,
            read_files,
            modified_files,
        )
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
    use std::time::Duration;

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

            let tool = agent_session
                .agent
                .tools
                .get("hello_tool")
                .expect("hello_tool registered");

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

            let tool = agent_session
                .agent
                .tools
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

            let tool = agent_session
                .agent
                .tools
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
                })),
            };

            let hold_cx = crate::agent_cx::AgentCx::for_request();
            let held_guard = session.lock(hold_cx.cx()).await.expect("lock session");

            let ambient_cx = asupersync::Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = asupersync::Cx::set_current(Some(ambient_cx));
            let inner = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_millis(100),
                actions.send_message(ExtensionSendMessage {
                    extension_id: Some("ext".to_string()),
                    custom_type: "note".to_string(),
                    content: "blocked".to_string(),
                    display: false,
                    details: None,
                    deliver_as: Some(ExtensionDeliverAs::NextTurn),
                    trigger_turn: false,
                }),
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

    #[derive(Debug, Default)]
    struct PiAiCapturedProviderContext {
        system_prompt: Option<String>,
        messages: Vec<Message>,
    }

    #[derive(Debug)]
    struct PiAiCaptureProvider {
        calls: Arc<StdMutex<Vec<PiAiCapturedProviderContext>>>,
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
            _options: &StreamOptions,
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
                })),
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
        });
    }

    /// gh #167 / bd-i28yz: the compact_session host action deserializes a
    /// valid preparation, compacts with the session's own provider, and
    /// returns the {summary, firstKeptEntryId, tokensBefore, details} shape
    /// consumed by the session_before_compact replacement contract; malformed
    /// preparation is rejected with a validation error, never a fabricated
    /// summary.
    #[test]
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
                    stream_options: StreamOptions::default(),
                    models: Vec::new(),
                })),
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
                .compact_session(preparation)
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

            // Malformed preparation is rejected with a validation error.
            let err = actions
                .compact_session(json!({ "firstKeptEntryId": "" }))
                .await
                .expect_err("empty firstKeptEntryId must be rejected");
            assert!(
                err.to_string()
                    .contains("`firstKeptEntryId` must be a non-empty string"),
                "unexpected error: {err}"
            );

            let err = actions
                .compact_session(json!("not-an-object"))
                .await
                .expect_err("non-object preparation must be rejected");
            assert!(
                err.to_string()
                    .contains("compaction preparation must be a JSON object"),
                "unexpected error: {err}"
            );
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
                })),
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
    }

    impl PhasedProvider {
        const fn new(pending_calls: usize) -> Self {
            Self {
                pending_calls,
                calls: AtomicUsize::new(0),
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
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
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
    struct ToolCallProvider;

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
            let message = AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "call-1".to_string(),
                    name: "hanging_tool".to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                })],
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
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .expect("stream start");
                cancel_cx.set_cancel_requested(true);
            });

            let run = agent_session.run_text_with_abort("hello".to_string(), None, move |event| {
                if matches!(
                    event,
                    AgentEvent::MessageStart {
                        message: Message::Assistant(_)
                    }
                ) {
                    let _ = started_tx.send(());
                }
            });
            futures::pin_mut!(run);

            let message = asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(1),
                run,
            )
            .await
            .expect("ambient cancellation should finish before timeout")
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
            let provider = Arc::new(ToolCallProvider);
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
    is_streaming: Arc<AtomicBool>,
    is_compacting: Arc<AtomicBool>,
    queue_modes: Arc<StdMutex<ExtensionQueueModeState>>,
    auto_compaction_enabled: bool,
}

impl AgentExtensionSession {
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

    async fn set_name(&self, name: String) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::set_name(&self.handle, name).await
    }

    async fn append_message(
        &self,
        message: crate::session::SessionMessage,
    ) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::append_message(
            &self.handle,
            message,
        )
        .await
    }

    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<Value>,
    ) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::append_custom_entry(
            &self.handle,
            custom_type,
            data,
        )
        .await
    }

    async fn set_model(&self, provider: String, model_id: String) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::set_model(
            &self.handle,
            provider,
            model_id,
        )
        .await
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        <SessionHandle as crate::extensions::ExtensionSession>::get_model(&self.handle).await
    }

    async fn set_thinking_level(&self, level: String) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::set_thinking_level(
            &self.handle,
            level,
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
    ) -> crate::error::Result<()> {
        <SessionHandle as crate::extensions::ExtensionSession>::set_label(
            &self.handle,
            target_id,
            label,
        )
        .await
    }
}

impl AgentSession {
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
        tools: Arc<ToolRegistry>,
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
        _tools: Arc<ToolRegistry>,
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
        let extension_ai_completion = Arc::new(StdMutex::new(ExtensionAiCompletionHostState {
            provider: agent.provider(),
            stream_options: agent.stream_options().clone(),
            models: Vec::new(),
        }));

        Self {
            agent,
            session,
            save_enabled,
            input_source: InputSource::Interactive,
            extensions: None,
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
        }
    }

    pub const fn set_input_source(&mut self, source: InputSource) {
        self.input_source = source;
    }

    #[must_use]
    pub fn with_runtime_handle(mut self, runtime_handle: RuntimeHandle) -> Self {
        self.compaction_runtime = None;
        self.runtime_handle = Some(runtime_handle);
        self
    }

    #[must_use]
    pub fn with_model_registry(mut self, registry: ModelRegistry) -> Self {
        self.set_model_registry(registry);
        self
    }

    #[must_use]
    pub fn with_auth_storage(mut self, auth: AuthStorage) -> Self {
        self.auth_storage = Some(auth);
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
    }

    pub fn set_auth_storage(&mut self, auth: AuthStorage) {
        self.auth_storage = Some(auth);
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

    pub async fn set_provider_model(&mut self, provider_id: &str, model_id: &str) -> Result<()> {
        let already_active = {
            let provider = self.agent.provider();
            provider.name().eq(provider_id) && provider.model_id().eq(model_id)
        };
        let current_thinking = self
            .agent
            .stream_options()
            .thinking_level
            .unwrap_or_default();

        let target_entry = self
            .model_registry
            .as_ref()
            .and_then(|registry| registry.find(provider_id, model_id));
        let next_thinking = if let Some(target_entry) = target_entry {
            let resolved_key = self.resolve_stream_api_key_for_model(&target_entry);
            if !already_active
                && model_requires_configured_credential(&target_entry)
                && resolved_key.is_none()
            {
                return Err(Error::auth(format!(
                    "Missing credentials for {provider_id}/{model_id}"
                )));
            }
            self.clamp_thinking_level_for_model(provider_id, model_id, current_thinking)
        } else if already_active {
            current_thinking
        } else {
            return Err(Error::validation(format!(
                "Unable to switch provider/model to {provider_id}/{model_id}"
            )));
        };

        if !already_active {
            self.apply_session_model_selection(provider_id, model_id)?;
        }
        self.agent.stream_options_mut().thinking_level = Some(next_thinking);
        let keyword_max = self.keyword_max_thinking_level_for_model(provider_id, model_id);
        self.agent.set_keyword_max_thinking_level(keyword_max);
        let dialect = self.tool_call_dialect_for_model(provider_id, model_id);
        self.agent.set_tool_call_dialect(dialect);
        self.refresh_extension_completion_host_state();

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let previous_model = session.effective_model_for_current_path();
            let previous_thinking = session
                .effective_thinking_level_for_current_path()
                .as_deref()
                .and_then(|value| value.parse::<crate::model::ThinkingLevel>().ok());
            if previous_model
                .as_ref()
                .map(|(provider, model_id)| (provider.as_str(), model_id.as_str()))
                != Some((provider_id, model_id))
            {
                session.append_model_change(provider_id.to_string(), model_id.to_string());
            }
            session.set_model_header(
                Some(provider_id.to_string()),
                Some(model_id.to_string()),
                Some(next_thinking.to_string()),
            );
            if !previous_thinking.is_some_and(|previous| previous.eq(&next_thinking)) {
                session.append_thinking_level_change(next_thinking.to_string());
            }
        }

        self.persist_session().await
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
        let cx = crate::agent_cx::AgentCx::for_request();
        let (effective_level, changed) = {
            let mut guard = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let (provider_id, model_id) =
                guard.effective_model_for_current_path().unwrap_or_else(|| {
                    let provider = self.agent.provider();
                    (provider.name().to_string(), provider.model_id().to_string())
                });
            let effective_level =
                self.clamp_thinking_level_for_model(&provider_id, &model_id, level);
            let level_string = effective_level.to_string();
            let changed = guard.effective_thinking_level_for_current_path().as_deref()
                != Some(level_string.as_str());
            guard.set_model_header(None, None, Some(level_string.clone()));
            if changed {
                guard.append_thinking_level_change(level_string);
            }
            (effective_level, changed)
        };
        self.agent.stream_options_mut().thinking_level = Some(effective_level);
        self.refresh_extension_completion_host_state();
        if changed {
            self.persist_session().await
        } else {
            Ok(())
        }
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

    fn keyword_max_thinking_level_for_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> crate::model::ThinkingLevel {
        self.model_registry
            .as_ref()
            .and_then(|registry| registry.find(provider_id, model_id))
            .map_or(self.agent.keyword_max_thinking_level, |entry| {
                entry.clamp_thinking_level(crate::model::ThinkingLevel::Max)
            })
    }

    fn tool_call_dialect_for_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> crate::dialects::Dialect {
        self.model_registry
            .as_ref()
            .and_then(|registry| registry.find(provider_id, model_id))
            .map_or_else(
                || self.agent.tool_call_dialect(),
                |entry| entry.tool_call_dialect(),
            )
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
        let session_state = {
            let cx = crate::agent_cx::AgentCx::for_request();
            let session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            (
                session.effective_model_for_current_path(),
                session.effective_thinking_level_for_current_path(),
            )
        };

        let (session_model, session_thinking) = session_state;
        let current_thinking = self
            .agent
            .stream_options()
            .thinking_level
            .unwrap_or_default();

        if let Some((provider_id, model_id)) = session_model.as_ref() {
            self.apply_session_model_selection(provider_id, model_id)?;
        }

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

        let (active_provider_id, active_model_id) = session_model.clone().unwrap_or_else(|| {
            let provider = self.agent.provider();
            (provider.name().to_string(), provider.model_id().to_string())
        });
        let effective =
            self.clamp_thinking_level_for_model(&active_provider_id, &active_model_id, requested);
        let keyword_max =
            self.keyword_max_thinking_level_for_model(&active_provider_id, &active_model_id);
        let dialect = self.tool_call_dialect_for_model(&active_provider_id, &active_model_id);

        self.agent.stream_options_mut().thinking_level = Some(effective);
        self.agent.set_keyword_max_thinking_level(keyword_max);
        self.agent.set_tool_call_dialect(dialect);
        self.refresh_extension_completion_host_state();

        let thinking_changed = !effective.eq(&current_thinking);
        let persist_needed = if session_thinking.is_some() {
            !parsed_session_thinking.is_some_and(|parsed| parsed.eq(&effective))
        } else {
            thinking_changed
        };
        if !persist_needed {
            return Ok(());
        }

        {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = self
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let previous_thinking = session
                .header
                .thinking_level
                .as_deref()
                .and_then(|value| value.parse::<crate::model::ThinkingLevel>().ok());
            session.set_model_header(None, None, Some(effective.to_string()));
            if thinking_changed
                && !previous_thinking.is_some_and(|previous| previous.eq(&effective))
            {
                session.append_thinking_level_change(effective.to_string());
            }
        }

        self.persist_session().await
    }

    fn apply_session_model_selection(&mut self, provider_id: &str, model_id: &str) -> Result<()> {
        if self.agent.provider().name().eq(provider_id)
            && self.agent.provider().model_id().eq(model_id)
        {
            return Ok(());
        }

        let Some(registry) = &self.model_registry else {
            return Err(Error::validation(format!(
                "Unable to switch provider/model to {provider_id}/{model_id}"
            )));
        };

        let Some(entry) = registry.find(provider_id, model_id) else {
            return Err(Error::validation(format!(
                "Unable to switch provider/model to {provider_id}/{model_id}"
            )));
        };

        let resolved_key = self.resolve_stream_api_key_for_model(&entry);
        if model_requires_configured_credential(&entry) && resolved_key.is_none() {
            return Err(Error::auth(format!(
                "Missing credentials for {provider_id}/{model_id}"
            )));
        }

        match crate::providers::create_provider(
            &entry,
            self.extensions.as_ref().map(ExtensionRegion::manager),
        ) {
            Ok(provider) => {
                tracing::info!("Updating agent provider to {provider_id}/{model_id}");
                self.agent.set_provider(provider);

                let stream_options = self.agent.stream_options_mut();
                stream_options.api_key.clone_from(&resolved_key);
                stream_options.headers.clone_from(&entry.headers);
                // Track the new model's configured output cap so a runtime
                // model switch (e.g. RPC `set_model`) honors its registry
                // `maxTokens` instead of carrying over the previous model's
                // limit or falling back to the provider default.
                stream_options.max_tokens = Some(entry.model.max_tokens);
                self.refresh_extension_completion_host_state();
                // gh #167: bump the extension ctx generation so handlers see
                // the fresh ctx.model instead of a cached payload built for
                // the previous model.
                if let Some(region) = &self.extensions {
                    region.manager().set_current_model(
                        Some(provider_id.to_string()),
                        Some(model_id.to_string()),
                    );
                }
                Ok(())
            }
            Err(e) => Err(Error::validation(format!(
                "Unable to switch provider/model to {provider_id}/{model_id}: {e}"
            ))),
        }
    }

    pub const fn save_enabled(&self) -> bool {
        self.save_enabled
    }

    /// Force-run compaction synchronously (used by `/compact` slash command).
    pub async fn compact_now(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<()> {
        self.compact_synchronous(Arc::new(on_event)).await
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

    pub async fn execute_extension_command_with_abort(
        &mut self,
        command_name: &str,
        args: &str,
        timeout_ms: u64,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<Value> {
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
        if let Some(outcome) = self.compaction_worker.try_recv().await {
            self.extensions_is_compacting
                .store(false, std::sync::atomic::Ordering::SeqCst);
            match outcome {
                Ok(result) => {
                    self.apply_compaction_result(result, Arc::clone(&on_event))
                        .await?;
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

            if let Some(compaction) = before_outcome.compaction {
                self.extensions_is_compacting
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let apply_result = self
                    .apply_compaction_entry(
                        compaction.summary.clone(),
                        compaction.first_kept_entry_id.clone(),
                        compaction.tokens_before,
                        compaction.details.clone(),
                        true,
                    )
                    .await;
                self.extensions_is_compacting
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                let tokens_after = apply_result?;
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
            let credential = self
                .agent
                .stream_options()
                .api_key
                .clone()
                .unwrap_or_default();

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

            self.compaction_worker
                .start(&runtime_handle, prep, provider, credential, None);
            self.extensions_is_compacting
                .store(true, std::sync::atomic::Ordering::SeqCst);
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

        let result = compaction::compact_local(prep);
        self.extensions_is_compacting
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let apply_result = self
            .apply_compaction_result(result, Arc::clone(&on_event))
            .await;
        self.extensions_is_compacting
            .store(false, std::sync::atomic::Ordering::SeqCst);
        apply_result
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
    ) -> Result<u64> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;

        let from_hook = if from_extension { Some(true) } else { None };
        let entry_id = session.append_compaction(
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
            from_hook,
        );

        if self.save_enabled {
            session
                .flush_autosave(AutosaveFlushTrigger::Periodic)
                .await?;
        }

        // Estimate the context the *next* provider request will see now that the
        // compacted history is written: the post-compaction current path via the
        // shared char-based heuristic, ignoring any retained assistant `usage`.
        let tokens_after =
            compaction::estimate_entries_context_tokens(&session.entries_for_current_path());

        let compaction_entry = session.get_entry(&entry_id).and_then(|entry| {
            if let crate::session::SessionEntry::Compaction(compaction) = entry {
                Some(compaction.clone())
            } else {
                None
            }
        });
        drop(session);

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
    ) -> Result<()> {
        let details = Some(compaction::compaction_details_to_value(&result.details)?);
        let tokens_after = self
            .apply_compaction_entry(
                result.summary.clone(),
                result.first_kept_entry_id.clone(),
                result.tokens_before,
                details.clone(),
                false,
            )
            .await?;

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
    async fn compact_synchronous(&self, on_event: AgentEventHandler) -> Result<()> {
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
                self.extensions_is_compacting
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let apply_result = self
                    .apply_compaction_entry(
                        compaction.summary.clone(),
                        compaction.first_kept_entry_id.clone(),
                        compaction.tokens_before,
                        compaction.details.clone(),
                        true,
                    )
                    .await;
                self.extensions_is_compacting
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                let tokens_after = apply_result?;
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
            self.extensions_is_compacting
                .store(true, std::sync::atomic::Ordering::SeqCst);

            let provider = self.agent.provider();
            let credential = self
                .agent
                .stream_options()
                .api_key
                .clone()
                .unwrap_or_default();

            let compaction_result = compaction::compact(prep, provider, &credential, None).await;
            self.extensions_is_compacting
                .store(false, std::sync::atomic::Ordering::SeqCst);

            match compaction_result {
                Ok(result) => {
                    self.apply_compaction_result(result, Arc::clone(&on_event))
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
        )
        .await
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn enable_extensions_with_policy(
        &mut self,
        enabled_tools: &[&str],
        cwd: &std::path::Path,
        config: Option<&crate::config::Config>,
        extension_entries: &[std::path::PathBuf],
        policy: Option<ExtensionPolicy>,
        repair_policy: Option<RepairPolicyMode>,
        pre_warmed: Option<PreWarmedExtensionRuntime>,
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
                            Arc::clone(&tools),
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
                            Arc::clone(&tools),
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
            let tools = Arc::new(ToolRegistry::new(enabled_tools, cwd, config));

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
                    Arc::clone(&tools),
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
                    Arc::clone(&tools),
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

        // Session, host actions, and message fetchers are always set here
        // (after runtime boot) — the JS runtime only needs these when
        // dispatching hostcalls, which happens during extension loading.
        let (steering_mode, follow_up_mode) = self.agent.queue_modes();
        let queue_modes = Arc::new(StdMutex::new(ExtensionQueueModeState::new(
            steering_mode,
            follow_up_mode,
        )));
        manager.set_session(Arc::new(AgentExtensionSession {
            handle: SessionHandle(self.session.clone()),
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
        // Background job completion notices (bd-cv653.3.10): additive
        // follow-up source drained from the jobs registry.
        self.agent
            .register_message_fetchers(None, Some(crate::jobs::follow_up_fetcher()));

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
                .load_wasm_extensions(&host, wasm_specs, Arc::clone(&tools))
                .await?;
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
        Ok(())
    }

    pub async fn save_and_index(&mut self) -> Result<()> {
        if self.save_enabled {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = OwnedMutexGuard::lock(Arc::clone(&self.session), cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            session
                .flush_autosave(AutosaveFlushTrigger::Periodic)
                .await?;
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
        session
            .flush_autosave(AutosaveFlushTrigger::Periodic)
            .await?;
        Ok(())
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
    /// into the next turn via the steering queue. Zero-overhead gate: no
    /// advisor configured → no digest built. Failures are isolated inside
    /// the runtime and never fail the run.
    async fn maybe_advise_turn(&mut self) {
        if self.advisor.is_none() {
            return;
        }
        let digest = crate::advisor::build_digest(self.agent.messages());
        if digest.is_trivial() {
            return;
        }
        let turn_index = self.agent.messages().len() as u64;
        let Some(runtime) = self.advisor.as_mut() else {
            return;
        };
        let outcome = runtime.review_turn(&digest, turn_index).await;
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
            crate::advisor::AdvisorOutcome::Quiet => return,
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
                return;
            }
        };
        let level_str = verdict.level.as_str().to_string();
        let injection = crate::advisor::format_injection(&verdict);
        let message = crate::model::Message::User(crate::model::UserMessage {
            content: crate::model::UserContent::Text(injection.clone()),
            timestamp: chrono::Utc::now().timestamp_millis(),
        });
        self.agent.queue_generated_steering(message);
        // Session audit entry (replayable advisor trail).
        let cx = pi::agent_cx::AgentCx::for_request();
        if let Ok(mut inner) = self.session.lock(cx.cx()).await {
            inner.append_custom_entry(
                "advisor_note".to_string(),
                Some(serde_json::json!({
                    "level": level_str,
                    "rationale": verdict.rationale,
                })),
            );
        }
    }

    pub async fn run_text_with_abort(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.extensions_turn_active.store(true, Ordering::SeqCst);
        let result = async {
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
            if let Some(prompt) = system_prompt {
                self.agent.set_system_prompt(Some(prompt));
            } else {
                self.agent.set_system_prompt(base_system_prompt.clone());
            }
            self.agent.magic_keyword_scan_override = keyword_scan_override;

            let result = if images.is_empty() {
                self.run_agent_with_text(text, abort, on_event, custom_messages)
                    .await
            } else {
                let content = Self::build_content_blocks_for_input(&text, &images);
                self.run_agent_with_content(content, abort, on_event, custom_messages)
                    .await
            };
            // `run_loop_inner` normally consumes this. Clear it here as the
            // fail-closed fallback when setup/synchronization returns early.
            let _ = self.agent.magic_keyword_scan_override.take();

            self.agent.set_system_prompt(base_system_prompt);
            match result {
                Ok(message) => {
                    if !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                        self.maybe_advise_turn().await;
                    }
                    Ok(message)
                }
                Err(err) => Err(err),
            }
        }
        .await;
        self.extensions_turn_active.store(false, Ordering::SeqCst);
        result
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
        self.extensions_turn_active.store(true, Ordering::SeqCst);
        let result = async {
            // See the text path above: provenance is one-shot even when an
            // input extension blocks before the Agent loop starts.
            let keyword_scan_override = self.agent.magic_keyword_scan_override.take();
            let (text, images) = Self::split_content_blocks_for_input(&content);
            let outcome = self.dispatch_input_event(text, images).await?;
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
            if let Some(prompt) = system_prompt {
                self.agent.set_system_prompt(Some(prompt));
            } else {
                self.agent.set_system_prompt(base_system_prompt.clone());
            }
            self.agent.magic_keyword_scan_override = keyword_scan_override;

            let content_for_agent = Self::build_content_blocks_for_input(&text, &images);
            let result = self
                .run_agent_with_content(content_for_agent, abort, on_event, custom_messages)
                .await;
            let _ = self.agent.magic_keyword_scan_override.take();

            self.agent.set_system_prompt(base_system_prompt);
            match result {
                Ok(message) => {
                    if !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                        self.maybe_advise_turn().await;
                    }
                    Ok(message)
                }
                Err(err) => Err(err),
            }
        }
        .await;
        self.extensions_turn_active.store(false, Ordering::SeqCst);
        result
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

        let previous_source = self.input_source;
        self.input_source = InputSource::Extension;
        let result = async {
            for action in actions {
                match action {
                    PendingIdleAction::CustomMessage(message) => {
                        let handler = Arc::clone(&on_event);
                        self.run_custom_message_with_abort(message, abort.clone(), move |event| {
                            handler(event);
                        })
                        .await?;
                    }
                    PendingIdleAction::UserText(text) => {
                        let handler = Arc::clone(&on_event);
                        // Extension-authored user-shaped text is generated
                        // input. An explicit empty override suppresses the
                        // normal fallback scan of its provider-visible text.
                        self.agent
                            .set_magic_keyword_scan_override(Some(String::new()));
                        self.run_text_with_abort(text, abort.clone(), move |event| {
                            handler(event);
                        })
                        .await?;
                    }
                }
            }
            Ok(())
        }
        .await;
        self.input_source = previous_source;
        result
    }

    async fn run_custom_message_with_abort(
        &mut self,
        message: Message,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.extensions_turn_active.store(true, Ordering::SeqCst);
        let result = async {
            let base_system_prompt = self.agent.system_prompt().map(str::to_string);
            let BeforeAgentStartOutcome {
                messages: custom_messages,
                system_prompt,
            } = self
                .dispatch_before_agent_start("", &[], base_system_prompt.as_deref().unwrap_or(""))
                .await;
            if let Some(prompt) = system_prompt {
                self.agent.set_system_prompt(Some(prompt));
            } else {
                self.agent.set_system_prompt(base_system_prompt.clone());
            }

            let result = self
                .run_agent_with_prompt_message(message, abort, on_event, custom_messages)
                .await;

            self.agent.set_system_prompt(base_system_prompt);
            result
        }
        .await;
        self.extensions_turn_active.store(false, Ordering::SeqCst);
        result
    }

    async fn run_agent_with_prompt_message(
        &mut self,
        prompt_message: Message,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
        custom_messages: Vec<CustomMessage>,
    ) -> Result<AssistantMessage> {
        let on_event: AgentEventHandler = Arc::new(on_event);
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
            session.append_model_message(prompt_message.clone());
            if self.save_enabled {
                session.flush_autosave(AutosaveFlushTrigger::Manual).await?;
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
            .run_with_messages_with_abort(prompts, abort, move |event| {
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

        let result = result?;
        persist_result?;
        Ok(result)
    }

    pub(crate) async fn run_agent_with_text(
        &mut self,
        input: String,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
        custom_messages: Vec<CustomMessage>,
    ) -> Result<AssistantMessage> {
        let on_event: AgentEventHandler = Arc::new(on_event);
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
            if self.save_enabled {
                session.flush_autosave(AutosaveFlushTrigger::Manual).await?;
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
            .run_with_messages_with_abort(prompts, abort, move |event| {
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

        let result = result?;
        persist_result?;
        Ok(result)
    }

    pub(crate) async fn run_agent_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        abort: Option<AbortSignal>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
        custom_messages: Vec<CustomMessage>,
    ) -> Result<AssistantMessage> {
        let on_event: AgentEventHandler = Arc::new(on_event);
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
            if self.save_enabled {
                session.flush_autosave(AutosaveFlushTrigger::Manual).await?;
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
            .run_with_messages_with_abort(prompts, abort, move |event| {
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

        let result = result?;
        persist_result?;
        Ok(result)
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

    /// Resume from persisted history and optionally drain the registered
    /// follow-up fetchers before issuing the first provider request.
    pub(crate) async fn run_continue_with_follow_up_with_abort(
        &mut self,
        follow_up_first: bool,
        abort: Option<AbortSignal>,
        on_ready: impl FnOnce() -> bool + Send + 'static,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let on_event: AgentEventHandler = Arc::new(on_event);
        self.sync_runtime_selection_from_session_header().await?;

        // Rehydrate the agent transcript from the (already reverted) session
        // path so the resume streams from the last completed state.
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

        let streaming_guard = AtomicBoolGuard::activate(&self.extensions_is_streaming);
        let on_event_for_run = Arc::clone(&on_event);
        let result = if follow_up_first {
            self.agent
                .run_continue_with_follow_up_on_ready_with_abort(abort, on_ready, move |event| {
                    on_event_for_run(event);
                })
                .await
        } else {
            on_ready();
            self.agent
                .run_continue_with_abort(abort, move |event| {
                    on_event_for_run(event);
                })
                .await
        };
        drop(streaming_guard);

        // Persist any NEW messages generated by the resume, even on error.
        // No user message was added, so nothing to skip: persist from start_len.
        let run_incomplete = result.as_ref().map_or(true, |message| {
            matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
        });
        let persist_result = self
            .persist_turn_artifacts(start_len, result.is_err(), run_incomplete)
            .await;

        let result = result?;
        persist_result?;
        Ok(result)
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
        let mut new_messages = self.agent.messages()[start_len..].to_vec();
        let incomplete_assistant = if run_incomplete {
            new_messages
                .iter()
                .rposition(is_incomplete_assistant_message)
                .map(|index| new_messages.remove(index))
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
            for message in new_messages {
                if run_failed && is_synthetic_empty_error_assistant(&message) {
                    continue;
                }
                session.append_model_message(message);
            }
            append_dialect_repair_telemetry(&mut session, &repairs);
            crate::magic_keywords::append_session_telemetry(&mut session, &activations);
            if let Some(message) = incomplete_assistant
                && !(run_failed && is_synthetic_empty_error_assistant(&message))
            {
                session.append_model_message(message);
            }
            if self.save_enabled {
                session
                    .flush_autosave(AutosaveFlushTrigger::Periodic)
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

#[derive(Debug, Default, Clone, Copy)]
struct ImageFilterStats {
    removed_images: usize,
    affected_messages: usize,
}

fn filter_images_for_provider(messages: &mut [Message]) -> ImageFilterStats {
    let mut stats = ImageFilterStats::default();
    for message in messages {
        let removed = filter_images_from_message(message);
        if removed > 0 {
            stats.removed_images += removed;
            stats.affected_messages += 1;
        }
    }
    stats
}

fn filter_images_from_message(message: &mut Message) -> usize {
    match message {
        Message::User(user) => match &mut user.content {
            UserContent::Text(_) => 0,
            UserContent::Blocks(blocks) => filter_image_blocks(blocks),
        },
        Message::Assistant(assistant) => {
            let assistant = Arc::make_mut(assistant);
            filter_image_blocks(&mut assistant.content)
        }
        Message::ToolResult(tool_result) => {
            filter_image_blocks(&mut Arc::make_mut(tool_result).content)
        }
        Message::Custom(_) => 0,
    }
}

fn filter_image_blocks(blocks: &mut Vec<ContentBlock>) -> usize {
    let mut removed = 0usize;
    let mut filtered = Vec::with_capacity(blocks.len());

    for block in blocks.drain(..) {
        match block {
            ContentBlock::Image(_) => {
                removed += 1;
                let previous_is_placeholder =
                    filtered
                        .last()
                        .is_some_and(|prev| matches!(prev, ContentBlock::Text(TextContent { text, .. }) if text.as_str().eq(BLOCK_IMAGES_PLACEHOLDER)));
                if !previous_is_placeholder {
                    filtered.push(ContentBlock::Text(TextContent::new(
                        BLOCK_IMAGES_PLACEHOLDER,
                    )));
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

/// Whether `message` represents a response the provider truncated at its token
/// limit before any tool call finished (#148).
///
/// `tool_call_started` says a `ToolCall{Start,Delta,End}` event arrived on this
/// stream; without it, a `Length` stop in the middle of plain prose (no tool
/// call involved) would be misreported as a failure. It also covers the case
/// where truncation drops the half-built call from the final message entirely,
/// leaving nothing in `content` to inspect.
///
/// Returns `false` when at least one usable tool call survived — that turn still
/// has work the run loop can execute, so it keeps its provider stop reason.
fn is_truncated_before_tool_call(message: &AssistantMessage, tool_call_started: bool) -> bool {
    if message.stop_reason != StopReason::Length {
        return false;
    }

    let mut saw_incomplete = false;
    for block in &message.content {
        if let ContentBlock::ToolCall(tool_call) = block {
            if is_complete_tool_call(tool_call) {
                return false;
            }
            saw_incomplete = true;
        }
    }

    tool_call_started || saw_incomplete
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
mod tests {
    use super::*;
    use crate::auth::AuthCredential;
    use crate::provider::{InputType, Model, ModelCost};
    use asupersync::runtime::RuntimeBuilder;
    use async_trait::async_trait;
    use futures::Stream;
    use std::collections::BTreeSet;
    use std::collections::HashMap;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::{Arc as StdArc, Mutex as StdTestMutex};

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn queued_user_message(text: &str) -> QueuedAgentMessage {
        QueuedAgentMessage::from_authored_message(user_message(text))
    }

    fn assert_user_text(message: &Message, expected: &str) {
        assert!(
            matches!(
                message,
                Message::User(UserMessage {
                    content: UserContent::Text(_),
                    ..
                })
            ),
            "expected user text message, got {message:?}"
        );
        if let Message::User(UserMessage {
            content: UserContent::Text(text),
            ..
        }) = message
        {
            assert_eq!(text, expected);
        }
    }

    fn sample_image_block() -> ContentBlock {
        ContentBlock::Image(ImageContent {
            data: "aGVsbG8=".to_string(),
            mime_type: "image/png".to_string(),
        })
    }

    fn image_count_in_message(message: &Message) -> usize {
        let count_images = |blocks: &[ContentBlock]| {
            blocks
                .iter()
                .filter(|block| matches!(block, ContentBlock::Image(_)))
                .count()
        };
        match message {
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) => count_images(blocks),
            Message::Assistant(msg) => count_images(&msg.content),
            Message::ToolResult(tool_result) => count_images(&tool_result.content),
            Message::User(UserMessage {
                content: UserContent::Text(_),
                ..
            })
            | Message::Custom(_) => 0,
        }
    }

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

    #[derive(Debug)]
    struct SilentProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for SilentProvider {
        fn name(&self) -> &str {
            "silent-provider"
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
    struct DeltaOnlyProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for DeltaOnlyProvider {
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
            let final_message = assistant_message("hello");
            let events = vec![
                Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "hello".to_string(),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[derive(Debug, Clone, Default)]
    struct CapturedProviderContext {
        system_prompt: Option<String>,
        messages: Vec<Message>,
        thinking_level: Option<crate::model::ThinkingLevel>,
    }

    #[derive(Debug)]
    struct CapturingProvider {
        api: &'static str,
        calls: StdArc<StdTestMutex<Vec<CapturedProviderContext>>>,
    }

    impl CapturingProvider {
        fn new(api: &'static str) -> Self {
            Self {
                api,
                calls: StdArc::new(StdTestMutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> StdArc<StdTestMutex<Vec<CapturedProviderContext>>> {
            StdArc::clone(&self.calls)
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for CapturingProvider {
        fn name(&self) -> &str {
            "capturing-provider"
        }

        fn api(&self) -> &str {
            self.api
        }

        fn model_id(&self) -> &str {
            "capture-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.calls
                .lock()
                .expect("capture context lock")
                .push(CapturedProviderContext {
                    system_prompt: context.system_prompt.as_ref().map(ToString::to_string),
                    messages: context.messages.iter().cloned().collect(),
                    thinking_level: options.thinking_level,
                });
            let final_message = assistant_message("captured");
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: final_message,
                },
            )])))
        }
    }

    fn sample_semantic_context_bundle() -> SemanticContextBundle {
        use crate::semantic_workspace_graph::{
            ContextBundleBudget, ContextBundleExclusion, ContextBundleInvalidationPolicy,
            ContextRedactionSummary, EvidenceFreshnessStatus, RedactionStatus, SemanticNodeType,
        };

        SemanticContextBundle {
            schema: crate::semantic_workspace_graph::SEMANTIC_CONTEXT_BUNDLE_SCHEMA.to_string(),
            budget: ContextBundleBudget {
                max_items: 8,
                max_bytes: 4096,
            },
            selected_items: vec![
                ContextBundleItem {
                    node_id: "node-session".to_string(),
                    node_type: SemanticNodeType::CodeSymbol,
                    source_path: "src/agent.rs".to_string(),
                    title: "AgentSession::run_agent_with_text".to_string(),
                    reason: "query_match,related_to_bead_or_changed_path".to_string(),
                    score: 420,
                    estimated_bytes: 700,
                    estimated_tokens: 175,
                    freshness_status: None,
                    redaction_status: RedactionStatus::None,
                },
                ContextBundleItem {
                    node_id: "node-test".to_string(),
                    node_type: SemanticNodeType::TestCase,
                    source_path: "tests/agent_loop_reliability.rs".to_string(),
                    title: "semantic context session coverage".to_string(),
                    reason: "validation_context".to_string(),
                    score: 300,
                    estimated_bytes: 400,
                    estimated_tokens: 100,
                    freshness_status: Some(EvidenceFreshnessStatus::Current),
                    redaction_status: RedactionStatus::Redacted,
                },
            ],
            excluded_items: vec![ContextBundleExclusion {
                node_id: "stale-doc".to_string(),
                node_type: SemanticNodeType::DocSection,
                source_path: "README.md".to_string(),
                title: "obsolete drop-in claim".to_string(),
                reason: "suppressed_stale_or_unsafe_evidence".to_string(),
                score: 250,
                estimated_bytes: 300,
                freshness_status: Some(EvidenceFreshnessStatus::Uncertified),
                redaction_status: RedactionStatus::SensitiveOmitted,
            }],
            stale_evidence_suppressions: Vec::new(),
            redaction_summary: ContextRedactionSummary {
                policy_version: "test-policy".to_string(),
                overall_status: RedactionStatus::Redacted,
                selected_redacted_nodes: 1,
                selected_sensitive_omissions: 0,
                suppressed_unsafe_nodes: 0,
                redacted_metadata_keys: BTreeSet::from(["api_key".to_string()]),
                sensitive_path_kinds: BTreeSet::new(),
            },
            invalidation_policy: ContextBundleInvalidationPolicy {
                policy_version: "test-policy".to_string(),
                workspace_id: "workspace:test".to_string(),
                branch: Some("main".to_string()),
                session_id: Some("session-123".to_string()),
                input_fingerprint_sha256: "abc123".repeat(10),
                cache_ttl_seconds: 900,
                generated_at_utc: Some("2026-05-13T00:00:00Z".to_string()),
                expires_at_utc: Some("2026-05-13T00:15:00Z".to_string()),
                invalidates_on: vec!["input_fingerprint_change".to_string()],
                cacheable: true,
            },
            path_normalization: Vec::new(),
            suggested_validation_commands: vec![
                "cargo test agent_semantic_context".to_string(),
                "cargo check --all-targets".to_string(),
            ],
            estimated_bytes: 1100,
            estimated_tokens: 275,
        }
    }

    #[test]
    fn semantic_context_exclusion_prompt_deduplicates_before_row_cap() {
        let mut bundle = sample_semantic_context_bundle();
        let duplicate = bundle.excluded_items[0].clone();
        bundle.stale_evidence_suppressions.push(duplicate);

        for index in 0..8 {
            let mut exclusion = bundle.excluded_items[0].clone();
            exclusion.node_id = format!("excluded-{index}");
            exclusion.source_path = format!("docs/evidence/excluded-{index}.json");
            exclusion.title = format!("excluded evidence {index}");
            bundle.excluded_items.push(exclusion);
        }

        let mut prompt = String::new();
        let mut stats = SemanticContextPromptStats::default();
        push_semantic_context_exclusions(
            &mut prompt,
            &mut stats,
            SemanticContextPromptBudget {
                max_items: 16,
                max_bytes: 64 * 1024,
            },
            &bundle,
        );

        assert_eq!(
            prompt
                .matches(
                    "README.md :: obsolete drop-in claim reason=suppressed_stale_or_unsafe_evidence",
                )
                .count(),
            1,
            "the same exclusion from both bundle collections must render once"
        );
        assert_eq!(stats.exclusions_included, 8);
        assert_eq!(
            stats.exclusions_omitted, 1,
            "the row cap must count omitted unique exclusions, not raw duplicate records"
        );
    }

    #[test]
    fn delta_without_start_does_not_mutate_previous_message() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(DeltaOnlyProvider);
            let tools = ToolRegistry::from_tools(Vec::new());
            let mut agent = Agent::new(provider, tools, AgentConfig::default());

            agent.add_message(Message::Assistant(Arc::new(assistant_message("prev"))));

            agent
                .run_with_message_with_abort(user_message("hi"), None, |_| {})
                .await
                .expect("run");

            let assistant_texts = agent
                .messages()
                .iter()
                .filter_map(|message| match message {
                    Message::Assistant(msg)
                        if matches!(msg.content.as_slice(), [ContentBlock::Text(_)]) =>
                    {
                        if let [ContentBlock::Text(text)] = msg.content.as_slice() {
                            Some(text.text.clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();

            assert_eq!(
                assistant_texts.as_slice(),
                ["prev".to_string(), "hello".to_string()]
            );
        });
    }

    /// #148: first turn is cut off by the token cap while a tool call is still
    /// streaming, exactly as OpenAI (`finish_reason: "length"`) and Anthropic
    /// (`stop_reason: "max_tokens"`) report it — the tool call keeps its name
    /// but its argument fragment fails to parse, so the provider stores
    /// `arguments: null`. A second turn is scripted so the pre-fix behaviour
    /// (continue as if nothing went wrong) is observable rather than hanging.
    #[derive(Debug)]
    struct TruncatedToolCallProvider {
        stream_calls: StdArc<std::sync::atomic::AtomicUsize>,
    }

    impl TruncatedToolCallProvider {
        fn new() -> Self {
            Self {
                stream_calls: StdArc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        /// Shared handle to the stream-call counter, taken before the provider
        /// is moved into the agent.
        fn calls(&self) -> StdArc<std::sync::atomic::AtomicUsize> {
            StdArc::clone(&self.stream_calls)
        }
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for TruncatedToolCallProvider {
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
            if call_index > 0 {
                let events = vec![Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: assistant_message("recovered"),
                })];
                return Ok(Box::pin(futures::stream::iter(events)));
            }

            let mut truncated = assistant_message("Let me read that file");
            truncated.stop_reason = StopReason::Length;
            truncated.content.push(ContentBlock::ToolCall(ToolCall {
                id: "call-1".to_string(),
                name: "read_file".to_string(),
                // `{"path": "/etc/ho` does not parse; both providers fall back
                // to null after logging a parse warning.
                arguments: Value::Null,
                thought_signature: None,
            }));

            let events = vec![
                Ok(StreamEvent::Start {
                    partial: assistant_message(""),
                }),
                Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "Let me read that file".to_string(),
                }),
                Ok(StreamEvent::ToolCallStart {
                    content_index: 1,
                    id: "call-1".to_string(),
                    name: "read_file".to_string(),
                }),
                Ok(StreamEvent::ToolCallDelta {
                    content_index: 1,
                    delta: "{\"path\": \"/etc/ho".to_string(),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Length,
                    message: truncated,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// #148 negative case: the same token cap, but the model was writing prose.
    /// No tool call was ever started, so this is an ordinary `Length` stop and
    /// must not be rewritten into an error.
    #[derive(Debug)]
    struct TruncatedTextProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for TruncatedTextProvider {
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
            let mut truncated = assistant_message("The answer is fourty-tw");
            truncated.stop_reason = StopReason::Length;

            let events = vec![
                Ok(StreamEvent::Start {
                    partial: assistant_message(""),
                }),
                Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "The answer is fourty-tw".to_string(),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Length,
                    message: truncated,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn truncated_tool_call_finalizes_as_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = TruncatedToolCallProvider::new();
            let stream_calls = provider.calls();
            let tools = ToolRegistry::from_tools(Vec::new());
            let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

            let result = agent
                .run_with_message_with_abort(user_message("read /etc/hosts"), None, |_| {})
                .await
                .expect("run");

            assert_eq!(
                result.stop_reason,
                StopReason::Error,
                "truncated tool call must not finalize as a normal turn"
            );
            let error_message = result
                .error_message
                .as_deref()
                .expect("truncated turn carries an error message");
            assert!(
                error_message.contains("truncated before tool call completed"),
                "unexpected error message: {error_message}"
            );

            // The turn must stop here: continuing would either execute a tool
            // call whose arguments were cut off, or silently drop it.
            assert_eq!(
                stream_calls.load(Ordering::SeqCst),
                1,
                "agent kept running past the truncated tool call"
            );

            // The partial text is retained so the caller can show what arrived.
            let last = agent
                .messages()
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::Assistant(assistant) => Some(Arc::clone(assistant)),
                    _ => None,
                })
                .expect("assistant message in history");
            assert_eq!(last.stop_reason, StopReason::Error);
            assert!(
                last.content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text(text)
                        if text.text == "Let me read that file")),
                "partial text was dropped: {:?}",
                last.content
            );
        });
    }

    #[test]
    fn truncated_text_without_tool_call_stays_a_length_stop() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(TruncatedTextProvider);
            let tools = ToolRegistry::from_tools(Vec::new());
            let mut agent = Agent::new(provider, tools, AgentConfig::default());

            let result = agent
                .run_with_message_with_abort(user_message("what is the answer"), None, |_| {})
                .await
                .expect("run");

            assert_eq!(
                result.stop_reason,
                StopReason::Length,
                "a plain length stop must not be rewritten into an error"
            );
            assert_eq!(result.error_message, None);
            assert!(
                result
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text(text)
                        if text.text == "The answer is fourty-tw")),
                "partial text was dropped: {:?}",
                result.content
            );
        });
    }

    #[test]
    fn semantic_context_bundle_injection_is_disabled_by_default() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            agent_session
                .run_text("hello".to_string(), |_| {})
                .await
                .expect("run with default context settings");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(recorded_calls.len(), 1);
            assert_eq!(recorded_calls[0].messages.len(), 1);
            assert_user_text(&recorded_calls[0].messages[0], "hello");
            assert!(recorded_calls[0].system_prompt.is_none());
        });
    }

    #[test]
    fn queued_steering_and_follow_up_messages_apply_magic_keywords_on_delivery() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
            agent.queue_steering(user_message("ultrathink before continuing"));
            agent.queue_follow_up(user_message("orchestrate the follow-up"));

            agent
                .run_with_message_with_abort(user_message("start normally"), None, |_| {})
                .await
                .expect("queued messages complete");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(
                recorded_calls.len(),
                2,
                "follow-up must trigger a second provider turn"
            );
            assert_eq!(
                recorded_calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::High),
                "queued steering ultrathink must affect its first outbound request"
            );
            assert!(
                recorded_calls[0]
                    .system_prompt
                    .as_deref()
                    .is_none_or(|prompt| !prompt.contains("invoked `orchestrate`")),
                "the later follow-up directive must not leak into the first request"
            );
            assert!(
                recorded_calls[1]
                    .system_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt.contains("invoked `orchestrate`")),
                "queued follow-up orchestrate must affect its outbound request"
            );

            let activations = agent.drain_keyword_ledger();
            assert_eq!(activations.len(), 2);
            assert_eq!(activations[0].word, "ultrathink");
            assert_eq!(activations[1].word, "orchestrate");
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn continuation_follow_up_first_precedes_provider_and_later_steering() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
            agent.set_queue_modes(QueueMode::All, QueueMode::All);

            let queued_steering = Arc::new(StdTestMutex::new(Some(
                QueuedAgentMessage::from_authored_message(user_message("steer after handoff")),
            )));
            let steering_fetcher_state = Arc::clone(&queued_steering);
            let steering_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let steering_fetcher_state = Arc::clone(&steering_fetcher_state);
                    Box::pin(async move {
                        steering_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .into_iter()
                            .collect()
                    })
                };

            let first_visible = "expanded payload without a magic word";
            let second_visible = "generated ultrathink payload";
            let queued_follow_up = Arc::new(StdTestMutex::new(Some(vec![
                QueuedAgentMessage::authored(
                    user_message(first_visible),
                    "please orchestrate this follow-up",
                ),
                QueuedAgentMessage::generated(user_message(second_visible)),
            ])));
            let follow_up_fetcher_state = Arc::clone(&queued_follow_up);
            let follow_up_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let follow_up_fetcher_state = Arc::clone(&follow_up_fetcher_state);
                    Box::pin(async move {
                        follow_up_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .unwrap_or_default()
                    })
                };
            agent.register_message_fetchers(Some(Arc::new(steering_fetcher)), None);
            agent.register_initial_follow_up_fetcher(Arc::new(follow_up_fetcher));
            agent
                .run_continue_with_follow_up_with_abort(None, |_| {})
                .await
                .expect("follow-up-first continuation");

            let recorded_calls = {
                let guard = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            assert_eq!(
                recorded_calls.len(),
                2,
                "initial follow-up and later steering must each drive one provider request"
            );
            assert_eq!(recorded_calls[0].messages.len(), 2);
            assert_user_text(&recorded_calls[0].messages[0], first_visible);
            assert_user_text(&recorded_calls[0].messages[1], second_visible);
            assert!(
                recorded_calls[0].messages.iter().all(|message| {
                    !matches!(
                        message,
                        Message::User(UserMessage { content: UserContent::Text(text), .. })
                            if text == "steer after handoff"
                    )
                }),
                "steering admitted for the resumed turn must not precede its initial follow-up"
            );
            assert!(recorded_calls[1].messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User(UserMessage { content: UserContent::Text(text), .. })
                        if text == "steer after handoff"
                )
            }));
            assert_eq!(
                recorded_calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::Low),
                "generated ultrathink bytes must remain inert"
            );
            assert!(
                recorded_calls[0]
                    .system_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt.contains("invoked `orchestrate`")),
                "the exact authored source must activate its directive"
            );
        });
    }

    #[test]
    fn continuation_follow_up_first_preserves_full_admitted_batch() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent.set_queue_modes(QueueMode::All, QueueMode::All);

            let expected = (0..128)
                .map(|index| format!("admitted follow-up {index}"))
                .collect::<Vec<_>>();
            let queued_follow_up = Arc::new(StdTestMutex::new(Some(
                expected
                    .iter()
                    .map(|text| {
                        QueuedAgentMessage::from_authored_message(user_message(text.as_str()))
                    })
                    .collect::<Vec<_>>(),
            )));
            let follow_up_fetcher_state = Arc::clone(&queued_follow_up);
            let follow_up_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let follow_up_fetcher_state = Arc::clone(&follow_up_fetcher_state);
                    Box::pin(async move {
                        follow_up_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .unwrap_or_default()
                    })
                };
            agent.register_initial_follow_up_fetcher(Arc::new(follow_up_fetcher));

            agent
                .run_continue_with_follow_up_with_abort(None, |_| {})
                .await
                .expect("full admitted follow-up batch");

            let calls = match calls.lock() {
                Ok(calls) => calls,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].messages.len(), expected.len());
            for (message, expected_text) in calls[0].messages.iter().zip(&expected) {
                assert_user_text(message, expected_text);
            }
        });
    }

    #[test]
    fn ordinary_idle_boundary_preserves_full_primary_follow_up_batch() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            agent.set_queue_modes(QueueMode::All, QueueMode::All);

            let expected = (0..128)
                .map(|index| format!("ordinary follow-up {index}"))
                .collect::<Vec<_>>();
            let queued_follow_up = Arc::new(StdTestMutex::new(Some(
                expected
                    .iter()
                    .map(|text| {
                        QueuedAgentMessage::from_authored_message(user_message(text.as_str()))
                    })
                    .collect::<Vec<_>>(),
            )));
            let follow_up_fetcher_state = Arc::clone(&queued_follow_up);
            let follow_up_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let follow_up_fetcher_state = Arc::clone(&follow_up_fetcher_state);
                    Box::pin(async move {
                        follow_up_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .unwrap_or_default()
                    })
                };
            agent.register_initial_follow_up_fetcher(Arc::new(follow_up_fetcher));

            agent
                .run_with_message_with_abort(user_message("initial turn"), None, |_| {})
                .await
                .expect("ordinary follow-up delivery");

            let calls = match calls.lock() {
                Ok(calls) => calls,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert_eq!(calls.len(), 2);
            let second_call = &calls[1].messages;
            assert!(second_call.len() >= expected.len());
            let delivered = &second_call[second_call.len() - expected.len()..];
            for (message, expected_text) in delivered.iter().zip(&expected) {
                assert_user_text(message, expected_text);
            }
        });
    }

    #[test]
    fn ordinary_follow_up_batch_remains_staged_when_max_time_expires() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    max_time: Some(std::time::Duration::from_millis(100)),
                    ..AgentConfig::default()
                },
            );
            agent.set_queue_modes(QueueMode::All, QueueMode::All);

            let queued_follow_up = Arc::new(StdTestMutex::new(Some(
                (0..128)
                    .map(|index| {
                        QueuedAgentMessage::from_authored_message(user_message(
                            format!("staged follow-up {index}").as_str(),
                        ))
                    })
                    .collect::<Vec<_>>(),
            )));
            let follow_up_fetcher_state = Arc::clone(&queued_follow_up);
            let follow_up_fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let follow_up_fetcher_state = Arc::clone(&follow_up_fetcher_state);
                    Box::pin(async move {
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        follow_up_fetcher_state
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .unwrap_or_default()
                    })
                };
            agent.register_initial_follow_up_fetcher(Arc::new(follow_up_fetcher));

            agent
                .run_with_message_with_abort(user_message("initial turn"), None, |_| {})
                .await
                .expect("max-time stop");

            let calls = match calls.lock() {
                Ok(calls) => calls,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert_eq!(
                calls.len(),
                1,
                "the expired time cap must prevent a follow-up provider turn"
            );
            assert_eq!(
                agent.message_queue.follow_up.len(),
                128,
                "the accepted batch must remain staged for a later run"
            );
        });
    }

    #[test]
    fn fetched_message_scans_only_explicit_authored_source() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);

            let expanded = "generated ultrathink and workflowz bytes".to_string();
            let queued = Arc::new(StdTestMutex::new(Some(QueuedAgentMessage::authored(
                user_message(&expanded),
                "please orchestrate this queued turn",
            ))));
            let fetch_queue = Arc::clone(&queued);
            let fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let fetch_queue = Arc::clone(&fetch_queue);
                    Box::pin(async move {
                        fetch_queue
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .into_iter()
                            .collect()
                    })
                };
            agent.register_message_fetchers(Some(Arc::new(fetcher)), None);

            agent
                .run_with_message_with_abort(user_message("start normally"), None, |_| {})
                .await
                .expect("source-aware queued message completes");

            let calls = match calls.lock() {
                Ok(calls) => calls,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::Low),
                "generated ultrathink must not raise effort"
            );
            let system_prompt = calls[0].system_prompt.as_deref().expect("directive");
            assert!(system_prompt.contains("invoked `orchestrate`"));
            assert!(!system_prompt.contains("invoked `workflowz`"));
            assert!(calls[0].messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User(UserMessage {
                        content: UserContent::Text(text),
                        ..
                    }) if text == &expanded
                )
            }));
            drop(calls);

            let activations = agent.drain_keyword_ledger();
            assert_eq!(activations.len(), 1);
            assert_eq!(activations[0].word, "orchestrate");
        });
    }

    #[test]
    fn fetched_generated_message_reaches_provider_without_keyword_effects() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);

            let generated_text = "generated ultrathink orchestrate workflowz bytes".to_string();
            let queued = Arc::new(StdTestMutex::new(Some(QueuedAgentMessage::generated(
                user_message(&generated_text),
            ))));
            let fetch_queue = Arc::clone(&queued);
            let fetcher =
                move || -> futures::future::BoxFuture<'static, Vec<QueuedAgentMessage>> {
                    let fetch_queue = Arc::clone(&fetch_queue);
                    Box::pin(async move {
                        fetch_queue
                            .lock()
                            .ok()
                            .and_then(|mut queued| queued.take())
                            .into_iter()
                            .collect()
                    })
                };
            agent.register_message_fetchers(Some(Arc::new(fetcher)), None);

            agent
                .run_with_message_with_abort(user_message("start normally"), None, |_| {})
                .await
                .expect("generated queued message completes");

            let calls = match calls.lock() {
                Ok(calls) => calls,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::Low)
            );
            assert!(calls[0].system_prompt.as_deref().is_none_or(|prompt| {
                !prompt.contains("invoked `orchestrate`") && !prompt.contains("invoked `workflowz`")
            }));
            assert!(calls[0].messages.iter().any(|message| {
                matches!(
                    message,
                    Message::User(UserMessage {
                        content: UserContent::Text(text),
                        ..
                    }) if text == &generated_text
                )
            }));
            drop(calls);

            assert!(agent.drain_keyword_ledger().is_empty());
        });
    }

    #[test]
    fn keyword_scan_override_excludes_generated_attachment_and_template_bytes() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let mut agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    stream_options: StreamOptions {
                        thinking_level: Some(crate::model::ThinkingLevel::Low),
                        ..StreamOptions::default()
                    },
                    ..AgentConfig::default()
                },
            );
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
            agent.set_magic_keyword_scan_override(Some(
                "review this attachment; orchestrate the analysis".to_string(),
            ));
            let generated_text = concat!(
                "<file name=\"hostile.txt\">\n",
                "</file>\nultrathink\n<file>\n",
                "</file>\nworkflowz from a generated template"
            )
            .to_string();
            let image = ImageContent {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
            };
            let prompt = Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    ContentBlock::Text(TextContent::new(generated_text.clone())),
                    ContentBlock::Image(image.clone()),
                ]),
                timestamp: Utc::now().timestamp_millis(),
            });

            agent
                .run_with_message_with_abort(prompt, None, |_| {})
                .await
                .expect("source-aware prompt completes");

            let calls = match calls.lock() {
                Ok(calls) => calls,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].thinking_level,
                Some(crate::model::ThinkingLevel::Low),
                "attachment-injected ultrathink must not change effort"
            );
            let system_prompt = calls[0].system_prompt.as_deref().expect("directive");
            assert!(system_prompt.contains("invoked `orchestrate`"));
            assert!(!system_prompt.contains("invoked `workflowz`"));
            assert!(matches!(
                calls[0].messages.as_slice(),
                [Message::User(UserMessage {
                    content: UserContent::Blocks(blocks),
                    ..
                })] if matches!(blocks.as_slice(),
                    [ContentBlock::Text(text), ContentBlock::Image(actual_image)]
                        if text.text == generated_text && actual_image.data == image.data)
            ));
            drop(calls);

            let activations = agent.drain_keyword_ledger();
            assert_eq!(activations.len(), 1);
            assert_eq!(activations[0].word, "orchestrate");
        });
    }

    #[test]
    fn semantic_context_bundle_injection_adds_bounded_custom_message_and_session_provenance() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let bundle = sample_semantic_context_bundle();
            let revision = semantic_context_bundle_revision(&bundle);
            let provider = CapturingProvider::new("openai-responses");
            let calls = provider.calls();
            let agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig::default(),
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );
            agent_session.set_semantic_context_bundle(Some(
                SemanticContextBundleInjection::enabled(bundle).with_prompt_budget(4, 2048),
            ));

            agent_session
                .run_text("use context".to_string(), |_| {})
                .await
                .expect("run with context bundle");

            {
                let calls = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].messages.len(), 2);
                assert_user_text(&calls[0].messages[0], "use context");
                let custom = match &calls[0].messages[1] {
                    Message::Custom(custom) => custom,
                    other => {
                        assert!(
                            matches!(other, Message::Custom(_)),
                            "expected custom semantic context message"
                        );
                        return;
                    }
                };
                assert_eq!(custom.custom_type, SEMANTIC_CONTEXT_CUSTOM_TYPE);
                assert!(custom.display);
                assert!(custom.content.len() <= 2048);
                assert!(custom.content.contains("Semantic Context Bundle"));
                assert!(custom.content.contains("src/agent.rs"));
                let details = custom.details.as_ref().expect("context provenance");
                assert_eq!(
                    details.get("bundleRevision").and_then(Value::as_str),
                    Some(revision.as_str())
                );
                assert_eq!(
                    details
                        .pointer("/provider/promptShape")
                        .and_then(Value::as_str),
                    Some("custom_user_message")
                );
                drop(calls);
            }

            let cx = crate::agent_cx::AgentCx::for_request();
            let stored = session
                .lock(cx.cx())
                .await
                .expect("session lock")
                .to_messages_for_current_path();
            assert!(
                stored.iter().any(|message| matches!(
                    message,
                    Message::Custom(CustomMessage { custom_type, details, display: true, .. })
                        if custom_type == SEMANTIC_CONTEXT_CUSTOM_TYPE
                            && details
                                .as_ref()
                                .and_then(|value| value.get("bundleRevision"))
                                .and_then(Value::as_str)
                                == Some(revision.as_str())
                )),
                "semantic context provenance was not persisted in session messages: {stored:?}"
            );
        });
    }

    #[test]
    fn semantic_context_bundle_uses_system_prompt_append_for_providers_without_custom_context() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let bundle = sample_semantic_context_bundle();
            let revision = semantic_context_bundle_revision(&bundle);
            let provider = CapturingProvider::new("gitlab-chat");
            let calls = provider.calls();
            let agent = Agent::new(
                Arc::new(provider),
                ToolRegistry::from_tools(Vec::new()),
                AgentConfig {
                    system_prompt: Some("base prompt".to_string()),
                    ..AgentConfig::default()
                },
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                false,
                ResolvedCompactionSettings::default(),
            );
            agent_session.set_semantic_context_bundle(Some(
                SemanticContextBundleInjection::enabled(bundle).with_prompt_budget(4, 2048),
            ));

            agent_session
                .run_text("gitlab turn".to_string(), |_| {})
                .await
                .expect("run with system prompt context");

            {
                let calls = match calls.lock() {
                    Ok(calls) => calls,
                    Err(poisoned) => poisoned.into_inner(),
                };
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].messages.len(), 1);
                assert_user_text(&calls[0].messages[0], "gitlab turn");
                let system_prompt = calls[0].system_prompt.as_deref().expect("system prompt");
                assert!(system_prompt.contains("base prompt"));
                assert!(system_prompt.contains("Semantic Context Bundle"));
                assert!(system_prompt.contains("src/agent.rs"));
                drop(calls);
            }

            let cx = crate::agent_cx::AgentCx::for_request();
            let stored = session
                .lock(cx.cx())
                .await
                .expect("session lock")
                .to_messages_for_current_path();
            assert!(
                stored.iter().any(|message| matches!(
                    message,
                    Message::Custom(CustomMessage { custom_type, details, display: false, .. })
                        if custom_type == SEMANTIC_CONTEXT_CUSTOM_TYPE
                            && details
                                .as_ref()
                                .and_then(|value| value.get("bundleRevision"))
                                .and_then(Value::as_str)
                                == Some(revision.as_str())
                )),
                "hidden semantic context provenance was not persisted in session messages: {stored:?}"
            );
            assert_eq!(agent_session.agent.system_prompt(), Some("base prompt"));
        });
    }

    #[test]
    fn enable_extensions_policy_resolution_defaults_to_permissive() {
        let policy = AgentSession::resolve_extension_policy_for_enable(None, None);
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
    }

    #[test]
    fn enable_extensions_policy_resolution_respects_config_default_toggle() {
        let config = crate::config::Config {
            extension_policy: Some(crate::config::ExtensionPolicyConfig {
                profile: None,
                default_permissive: Some(false),
                allow_dangerous: None,
            }),
            ..Default::default()
        };
        let policy = AgentSession::resolve_extension_policy_for_enable(Some(&config), None);
        assert_eq!(policy.mode, crate::extensions::ExtensionPolicyMode::Strict);
    }

    #[test]
    fn enable_extensions_policy_resolution_prefers_explicit_policy() {
        let config = crate::config::Config {
            extension_policy: Some(crate::config::ExtensionPolicyConfig {
                profile: None,
                default_permissive: Some(false),
                allow_dangerous: None,
            }),
            ..Default::default()
        };
        let explicit = crate::extensions::PolicyProfile::Permissive.to_policy();
        let policy =
            AgentSession::resolve_extension_policy_for_enable(Some(&config), Some(explicit));
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
    }

    #[test]
    fn test_extract_tool_calls() {
        let content = vec![
            ContentBlock::Text(TextContent::new("Hello")),
            ContentBlock::ToolCall(ToolCall {
                id: "tc1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({"path": "file.txt"}),
                thought_signature: None,
            }),
            ContentBlock::Text(TextContent::new("World")),
            ContentBlock::ToolCall(ToolCall {
                id: "tc2".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({"command": "ls"}),
                thought_signature: None,
            }),
        ];

        let tool_calls = extract_tool_calls(&content);
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].name, "read");
        assert_eq!(tool_calls[1].name, "bash");
    }

    #[test]
    fn test_agent_config_default() {
        // Tests don't mutate env (the crate forbids unsafe code, and
        // `std::env::set_var` is unsafe in 2024 edition); under typical
        // `cargo test` invocation `PI_MAX_TOOL_ITERATIONS` is unset, so
        // this assertion holds. If a developer's shell happens to export
        // that var, this test will reflect their effective default — which
        // is the correct behavior, not a bug.
        let config = AgentConfig::default();
        let expected = resolved_max_tool_iterations_default();
        assert_eq!(config.max_tool_iterations, expected);
        assert!(config.system_prompt.is_none());
        assert!(!config.block_images);
    }

    #[test]
    fn resolve_max_tool_iterations_handles_unset_empty_and_whitespace() {
        assert_eq!(
            resolve_max_tool_iterations(None),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("    ")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
    }

    #[test]
    fn resolve_max_tool_iterations_rejects_zero_and_invalid() {
        assert_eq!(
            resolve_max_tool_iterations(Some("0")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("not-a-number")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("-5")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(
            resolve_max_tool_iterations(Some("3.14")),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
    }

    #[test]
    fn resolve_max_tool_iterations_accepts_valid_overrides_and_trims_whitespace() {
        assert_eq!(resolve_max_tool_iterations(Some("1")), 1);
        assert_eq!(resolve_max_tool_iterations(Some("100")), 100);
        assert_eq!(resolve_max_tool_iterations(Some("  200  ")), 200);
        assert_eq!(resolve_max_tool_iterations(Some("999")), 999);
    }

    #[test]
    fn resolve_max_tool_iterations_clamps_above_ceiling() {
        assert_eq!(
            resolve_max_tool_iterations(Some("99999")),
            MAX_TOOL_ITERATIONS_CEILING
        );
        // The ceiling itself should pass through unchanged.
        assert_eq!(
            resolve_max_tool_iterations(Some("1000")),
            MAX_TOOL_ITERATIONS_CEILING
        );
    }

    #[test]
    fn clamp_max_tool_iterations_matches_resolve_semantics() {
        // None -> default, 0 -> default (with warning), >ceiling -> ceiling.
        assert_eq!(clamp_max_tool_iterations(None), MAX_TOOL_ITERATIONS_DEFAULT);
        assert_eq!(
            clamp_max_tool_iterations(Some(0)),
            MAX_TOOL_ITERATIONS_DEFAULT
        );
        assert_eq!(clamp_max_tool_iterations(Some(7)), 7);
        assert_eq!(
            clamp_max_tool_iterations(Some(usize::MAX)),
            MAX_TOOL_ITERATIONS_CEILING
        );
    }

    #[test]
    fn iteration_warning_fires_at_80_percent_for_default_cap() {
        // Default cap = 50; (50 * 4) / 5 = 40 → warn at 40+.
        assert!(!should_warn_at_iteration_threshold(39, 50));
        assert!(should_warn_at_iteration_threshold(40, 50));
        assert!(should_warn_at_iteration_threshold(50, 50));
        // Off-by-one regression guard: not at 39 even with default cap.
        assert!(!should_warn_at_iteration_threshold(0, 50));
    }

    #[test]
    fn iteration_warning_fires_at_80_percent_for_custom_caps() {
        for (cap, threshold) in [(100usize, 80usize), (200, 160), (1000, 800)] {
            assert!(
                !should_warn_at_iteration_threshold(threshold - 1, cap),
                "expected no warning below threshold (current=cap={cap}, threshold={threshold})"
            );
            assert!(
                should_warn_at_iteration_threshold(threshold, cap),
                "expected warning at threshold (cap={cap}, threshold={threshold})"
            );
        }
    }

    #[test]
    fn iteration_warning_skipped_for_caps_below_minimum() {
        // For caps under ITERATION_WARN_MIN_CAP (5), the warning never
        // fires regardless of `current`. This avoids noise on tiny ceilings
        // where the warning would land on iteration 0 or 1.
        for cap in 0..ITERATION_WARN_MIN_CAP {
            for current in 0..=cap.saturating_add(2) {
                assert!(
                    !should_warn_at_iteration_threshold(current, cap),
                    "should not warn at current={current} cap={cap}"
                );
            }
        }
    }

    #[test]
    fn iteration_warning_handles_minimum_warnable_cap_boundary() {
        // Cap == ITERATION_WARN_MIN_CAP (5): (5 * 4) / 5 = 4 → warn at 4+.
        assert!(!should_warn_at_iteration_threshold(3, 5));
        assert!(should_warn_at_iteration_threshold(4, 5));
        assert!(should_warn_at_iteration_threshold(5, 5));
    }

    #[test]
    fn iteration_warning_handles_overflow_resistant_caps() {
        // SDK callers that write `AgentConfig::max_tool_iterations = usize::MAX`
        // directly bypass the resolvers' clamp. Without `saturating_mul`,
        // `max * 4` would wrap to a tiny number and the warning would fire
        // on iteration ~0. The saturating multiply pins the threshold at
        // (saturated) usize::MAX / 5, so the warning effectively never
        // fires for absurd caps — which is the safer default.
        assert!(!should_warn_at_iteration_threshold(1_000_000, usize::MAX));
        assert!(!should_warn_at_iteration_threshold(
            usize::MAX / 6,
            usize::MAX
        ));
        // Conversely, a current at the saturated threshold should fire.
        assert!(should_warn_at_iteration_threshold(
            usize::MAX / 5,
            usize::MAX
        ));
    }

    #[test]
    fn iteration_handoff_steering_text_is_self_describing() {
        // Pinning the wording is intentional: this string is the load-bearing
        // contract between the runtime and the agent's iteration-aware-handoff
        // protocol. If it changes, downstream spec templates may need an
        // update, so the test forces a deliberate review on edits.
        let text = iteration_handoff_steering_text(42, 50);
        assert!(text.contains("[runtime]"));
        assert!(text.contains("Tool-iteration budget at >=80%"));
        assert!(text.contains("used 42 of 50"));
        assert!(text.contains("graceful handoff"));
        assert!(text.contains("incomplete-handoff"));
        assert!(text.contains("Do NOT compress"));
    }

    #[test]
    fn filter_image_blocks_replaces_images_with_deduped_placeholder_text() {
        let mut blocks = vec![
            sample_image_block(),
            sample_image_block(),
            ContentBlock::Text(TextContent::new("tail")),
            sample_image_block(),
        ];

        let removed = filter_image_blocks(&mut blocks);

        assert_eq!(removed, 3);
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Image(_)))
        );
        assert!(matches!(
            blocks.first(),
            Some(ContentBlock::Text(TextContent { text, .. }))
                if text.as_str().eq(BLOCK_IMAGES_PLACEHOLDER)
        ));
        assert!(matches!(
            blocks.get(1),
            Some(ContentBlock::Text(TextContent { text, .. })) if text.as_str().eq("tail")
        ));
        assert!(matches!(
            blocks.get(2),
            Some(ContentBlock::Text(TextContent { text, .. }))
                if text.as_str().eq(BLOCK_IMAGES_PLACEHOLDER)
        ));
    }

    #[test]
    fn filter_images_for_provider_filters_images_from_all_block_message_types() {
        let mut messages = vec![
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    ContentBlock::Text(TextContent::new("hello")),
                    sample_image_block(),
                ]),
                timestamp: 0,
            }),
            Message::Assistant(Arc::new(AssistantMessage {
                content: vec![sample_image_block()],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            })),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "tc1".to_string(),
                tool_name: "read".to_string(),
                content: vec![
                    sample_image_block(),
                    ContentBlock::Text(TextContent::new("ok")),
                ],
                details: None,
                is_error: false,
                timestamp: 0,
            }),
        ];

        let stats = filter_images_for_provider(&mut messages);

        assert_eq!(stats.removed_images, 3);
        assert_eq!(stats.affected_messages, 3);
        assert_eq!(
            messages.iter().map(image_count_in_message).sum::<usize>(),
            0,
            "no images should remain in provider-bound context"
        );
    }

    #[test]
    fn build_context_strips_images_when_block_images_enabled() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: true,
                model_accepts_images: true,
                fail_closed_hooks: false,
                tool_approval: None,
                keyword_settings: None,
                max_time: None,
                turn_recovery: crate::turn_recovery::TurnRecoveryMode::default(),
                approval_state: None,
                bash_settings: None,
                secrets: None,
            },
        );
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));

        let context = agent.build_context();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(image_count_in_message(&context.messages[0]), 0);
        assert!(matches!(
            &context.messages[0],
            Message::User(UserMessage {
                content: UserContent::Blocks(blocks),
                ..
            }) if blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Text(TextContent { text, .. }) if text.as_str().eq(BLOCK_IMAGES_PLACEHOLDER)))
        ));
    }

    #[test]
    fn build_context_keeps_images_when_block_images_disabled() {
        let mut agent = Agent::new(
            Arc::new(SilentProvider),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
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
            },
        );
        agent.add_message(Message::User(UserMessage {
            content: UserContent::Blocks(vec![sample_image_block()]),
            timestamp: 0,
        }));

        let context = agent.build_context();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(image_count_in_message(&context.messages[0]), 1);
    }

    #[test]
    fn auto_compaction_start_serializes_with_pi_mono_compatible_type_tag() {
        let event = AgentEvent::AutoCompactionStart {
            reason: "threshold".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_start");
        assert_eq!(json["reason"], "threshold");
    }

    #[test]
    fn auto_compaction_end_serializes_with_pi_mono_compatible_fields() {
        let event = AgentEvent::AutoCompactionEnd {
            result: Some(serde_json::json!({"tokens_before": 5000, "tokens_after": 2000})),
            aborted: false,
            will_retry: false,
            error_message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert_eq!(json["aborted"], false);
        assert_eq!(json["willRetry"], false);
        assert!(json.get("errorMessage").is_none()); // skipped when None
        assert!(json["result"].is_object());
    }

    #[test]
    fn auto_compaction_end_includes_error_message_when_present() {
        let event = AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: true,
            will_retry: false,
            error_message: Some("Compaction failed".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert_eq!(json["aborted"], true);
        assert_eq!(json["errorMessage"], "Compaction failed");
    }

    #[test]
    fn auto_retry_start_serializes_with_camel_case_fields() {
        let event = AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "Rate limited".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 1);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 2000);
        assert_eq!(json["errorMessage"], "Rate limited");
    }

    #[test]
    fn auto_retry_end_serializes_success_and_omits_null_final_error() {
        let event = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], true);
        assert_eq!(json["attempt"], 2);
        assert!(json.get("finalError").is_none());
    }

    #[test]
    fn auto_retry_end_includes_final_error_on_failure() {
        let event = AgentEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("Max retries exceeded".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], false);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["finalError"], "Max retries exceeded");
    }

    #[test]
    fn message_queue_push_increments_seq_and_counts_both_queues() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        assert_eq!(queue.pending_count(), 0);

        assert_eq!(queue.push_steering(queued_user_message("s1")), 0);
        assert_eq!(queue.push_follow_up(queued_user_message("f1")), 1);
        assert_eq!(queue.push_steering(queued_user_message("s2")), 2);

        assert_eq!(queue.pending_count(), 3);
    }

    #[test]
    fn message_queue_pop_steering_one_at_a_time_preserves_order() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(queued_user_message("s1"));
        queue.push_steering(queued_user_message("s2"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert_user_text(first[0].message(), "s1");
        assert_eq!(queue.pending_count(), 1);

        let second = queue.pop_steering();
        assert_eq!(second.len(), 1);
        assert_user_text(second[0].message(), "s2");
        assert_eq!(queue.pending_count(), 0);

        let empty = queue.pop_steering();
        assert!(empty.is_empty());
    }

    #[test]
    fn message_queue_pop_respects_queue_modes_per_kind() {
        let mut queue = MessageQueue::new(QueueMode::All, QueueMode::OneAtATime);
        queue.push_steering(queued_user_message("s1"));
        queue.push_steering(queued_user_message("s2"));
        queue.push_follow_up(queued_user_message("f1"));
        queue.push_follow_up(queued_user_message("f2"));

        let steering = queue.pop_steering();
        assert_eq!(steering.len(), 2);
        assert_user_text(steering[0].message(), "s1");
        assert_user_text(steering[1].message(), "s2");
        assert_eq!(queue.pending_count(), 2);

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 1);
        assert_user_text(follow_up[0].message(), "f1");
        assert_eq!(queue.pending_count(), 1);

        let follow_up = queue.pop_follow_up();
        assert_eq!(follow_up.len(), 1);
        assert_user_text(follow_up[0].message(), "f2");
        assert_eq!(queue.pending_count(), 0);
    }

    #[test]
    fn message_queue_set_modes_applies_to_existing_messages() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime, QueueMode::OneAtATime);
        queue.push_steering(queued_user_message("s1"));
        queue.push_steering(queued_user_message("s2"));

        let first = queue.pop_steering();
        assert_eq!(first.len(), 1);
        assert_user_text(first[0].message(), "s1");

        queue.set_modes(QueueMode::All, QueueMode::OneAtATime);
        let remaining = queue.pop_steering();
        assert_eq!(remaining.len(), 1);
        assert_user_text(remaining[0].message(), "s2");
    }

    fn build_switch_test_session(auth: &AuthStorage) -> AgentSession {
        let registry = ModelRegistry::load(auth, None);
        let current_entry = registry
            .find("anthropic", "claude-sonnet-4-5")
            .expect("anthropic model in registry");
        let provider = crate::providers::create_provider(&current_entry, None)
            .expect("create anthropic provider");
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut stream_options = StreamOptions {
            api_key: Some("stale-key".to_string()),
            ..Default::default()
        };
        let _ = stream_options
            .headers
            .insert("x-stale-header".to_string(), "stale-value".to_string());
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options,
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
            },
        );

        let mut session = Session::in_memory();
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-sonnet-4-5".to_string());

        let mut agent_session = AgentSession::new(
            agent,
            Arc::new(Mutex::new(session)),
            false,
            ResolvedCompactionSettings::default(),
        );
        agent_session.set_model_registry(registry);
        agent_session.set_auth_storage(auth.clone());
        agent_session
    }

    #[test]
    fn compaction_runtime_handle_creates_fallback_runtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("load auth");
        let mut agent_session = build_switch_test_session(&auth);

        assert!(agent_session.compaction_runtime.is_none());
        assert!(agent_session.runtime_handle.is_none());

        let runtime_handle = agent_session
            .compaction_runtime_handle()
            .expect("create fallback compaction runtime");
        let join = runtime_handle.spawn(async { 7_u8 });
        assert_eq!(futures::executor::block_on(join), 7);

        assert!(agent_session.compaction_runtime.is_some());
        assert!(agent_session.runtime_handle.is_some());
    }

    #[test]
    fn apply_session_model_selection_updates_stream_credentials_and_headers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key".to_string(),
            },
        );
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "openai-key".to_string(),
            },
        );

        let mut agent_session = build_switch_test_session(&auth);
        agent_session
            .apply_session_model_selection("openai", "gpt-4o")
            .expect("switch should update stream options");

        assert_eq!(agent_session.agent.provider().name(), "openai");
        assert_eq!(agent_session.agent.provider().model_id(), "gpt-4o");
        assert_eq!(
            agent_session.agent.stream_options().api_key.as_deref(),
            Some("openai-key")
        );
        assert!(
            agent_session.agent.stream_options().headers.is_empty(),
            "stream headers should be refreshed from selected model entry"
        );
    }

    #[test]
    fn apply_session_model_selection_clears_stale_key_for_keyless_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-key".to_string(),
            },
        );

        let mut registry = ModelRegistry::load(&auth, None);
        registry.merge_entries(vec![ModelEntry {
            model: Model {
                id: "local-model".to_string(),
                name: "Local Model".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme-local".to_string(),
                base_url: "https://example.invalid/v1".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: None,
            headers: HashMap::new(),
            auth_header: false,
            compat: None,
            oauth_config: None,
        }]);

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.set_model_registry(registry);
        agent_session
            .apply_session_model_selection("acme-local", "local-model")
            .expect("keyless local model should still activate");

        assert_eq!(agent_session.agent.provider().name(), "acme-local");
        assert_eq!(
            agent_session.agent.stream_options().api_key,
            None,
            "stale key must be cleared when target model has no configured key"
        );
    }

    #[test]
    fn apply_session_model_selection_treats_blank_model_key_as_missing_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("load auth");

        let mut registry = ModelRegistry::load(&auth, None);
        registry.merge_entries(vec![ModelEntry {
            model: Model {
                id: "blank-model".to_string(),
                name: "Blank Model".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme".to_string(),
                base_url: "https://example.invalid/v1".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: Some("   ".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }]);

        let mut agent_session = build_switch_test_session(&auth);
        agent_session.set_model_registry(registry);
        let err = agent_session
            .apply_session_model_selection("acme", "blank-model")
            .expect_err("blank keys must not satisfy credential requirements");

        assert!(
            err.to_string()
                .contains("Missing credentials for acme/blank-model"),
            "unexpected error: {err}"
        );
        assert_eq!(agent_session.agent.provider().name(), "anthropic");
        assert_eq!(
            agent_session.agent.stream_options().api_key,
            Some("stale-key".to_string()),
            "failed switches must preserve the prior runtime credentials"
        );
    }

    #[test]
    fn set_provider_model_preserves_session_header_when_switch_fails() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("anthropic".to_string());
                session.header.model_id = Some("claude-sonnet-4-5".to_string());
            }

            let err = agent_session
                .set_provider_model("missing-provider", "missing-model")
                .await
                .expect_err("missing model should not switch");
            assert!(
                err.to_string()
                    .contains("Unable to switch provider/model to missing-provider/missing-model"),
                "unexpected error: {err}"
            );
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
        });
    }

    #[test]
    fn set_provider_model_rejects_missing_credentials_without_switching() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("anthropic".to_string());
                session.header.model_id = Some("claude-sonnet-4-5".to_string());
            }

            let err = agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect_err("missing credentials should abort model switch");
            assert!(
                err.to_string()
                    .contains("Missing credentials for openai/gpt-4o"),
                "unexpected error: {err}"
            );
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
        });
    }

    #[test]
    fn set_provider_model_clamps_thinking_for_non_reasoning_targets() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");

            let mut registry = ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: false,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 128_000,
                    max_tokens: 8_192,
                    headers: HashMap::new(),
                },
                api_key: None,
                headers: HashMap::new(),
                auth_header: false,
                compat: None,
                oauth_config: None,
            }]);

            let mut agent_session = build_switch_test_session(&auth);
            agent_session.set_model_registry(registry);
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.thinking_level = Some("high".to_string());
            }

            agent_session
                .set_provider_model("acme", "plain-model")
                .await
                .expect("switch should clamp unsupported thinking");

            assert_eq!(agent_session.agent.provider().name(), "acme");
            assert_eq!(agent_session.agent.provider().model_id(), "plain-model");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );
            assert_eq!(
                agent_session.agent.keyword_max_thinking_level,
                crate::model::ThinkingLevel::Off,
                "ultrathink must use the target model's clamped maximum"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("acme"));
            assert_eq!(session.header.model_id.as_deref(), Some("plain-model"));
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
        });
    }

    #[test]
    fn bare_provider_replacement_resets_catalog_controls_fail_closed() {
        let tools = ToolRegistry::new(&[], Path::new("."), None);
        let mut agent = Agent::new(Arc::new(SilentProvider), tools, AgentConfig::default());
        agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
        agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);

        agent.set_provider(Arc::new(SilentProvider));

        assert_eq!(
            agent.keyword_max_thinking_level,
            crate::model::ThinkingLevel::Off,
            "provider replacement without registry metadata must not inherit the prior cap"
        );
        assert_eq!(
            agent.tool_call_dialect,
            crate::dialects::Dialect::Native,
            "provider replacement without registry metadata must not inherit repair opt-in"
        );
    }

    #[test]
    fn set_provider_model_records_model_change_once() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage::load(auth_path).expect("load auth");
            auth.set(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "anthropic-key".to_string(),
                },
            );
            auth.set(
                "openai",
                AuthCredential::ApiKey {
                    key: "openai-key".to_string(),
                },
            );

            let mut agent_session = build_switch_test_session(&auth);
            agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect("switch model");
            agent_session
                .set_provider_model("openai", "gpt-4o")
                .await
                .expect("repeat same model");

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            let model_changes = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| matches!(entry, crate::session::SessionEntry::ModelChange(_)))
                .count();
            assert_eq!(model_changes, 1);
        });
    }

    #[test]
    fn sync_runtime_selection_from_session_header_clamps_and_normalizes_thinking() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");

            let mut registry = ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: false,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 128_000,
                    max_tokens: 8_192,
                    headers: HashMap::new(),
                },
                api_key: None,
                headers: HashMap::new(),
                auth_header: false,
                compat: None,
                oauth_config: None,
            }]);

            let mut agent_session = build_switch_test_session(&auth);
            agent_session.set_model_registry(registry);
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("acme".to_string());
                session.header.model_id = Some("plain-model".to_string());
                session.header.thinking_level = Some("high".to_string());
            }

            agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect("sync runtime selection");

            assert_eq!(agent_session.agent.provider().name(), "acme");
            assert_eq!(agent_session.agent.provider().model_id(), "plain-model");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
            let thinking_changes = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(entry, crate::session::SessionEntry::ThinkingLevelChange(_))
                })
                .count();
            assert_eq!(thinking_changes, 1);
        });
    }

    #[test]
    fn sync_runtime_selection_from_session_header_clamps_current_thinking_when_header_omits_it() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");

            let mut registry = ModelRegistry::load(&auth, None);
            registry.merge_entries(vec![ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "openai-completions".to_string(),
                    provider: "acme".to_string(),
                    base_url: "https://example.invalid/v1".to_string(),
                    reasoning: false,
                    input: vec![InputType::Text],
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: 128_000,
                    max_tokens: 8_192,
                    headers: HashMap::new(),
                },
                api_key: None,
                headers: HashMap::new(),
                auth_header: false,
                compat: None,
                oauth_config: None,
            }]);

            let mut agent_session = build_switch_test_session(&auth);
            agent_session.set_model_registry(registry);
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("acme".to_string());
                session.header.model_id = Some("plain-model".to_string());
                session.header.thinking_level = None;
            }

            agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect("sync runtime selection");

            assert_eq!(agent_session.agent.provider().name(), "acme");
            assert_eq!(agent_session.agent.provider().model_id(), "plain-model");
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.thinking_level.as_deref(), Some("off"));
            let thinking_changes = session
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(entry, crate::session::SessionEntry::ThinkingLevelChange(_))
                })
                .count();
            assert_eq!(thinking_changes, 1);
        });
    }

    #[test]
    fn sync_runtime_selection_from_session_header_rejects_missing_credentials() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut session = agent_session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("session lock");
                session.header.provider = Some("openai".to_string());
                session.header.model_id = Some("gpt-4o".to_string());
            }

            let err = agent_session
                .sync_runtime_selection_from_session_header()
                .await
                .expect_err("sync should reject switching to a credentialed target without a key");
            assert!(
                err.to_string()
                    .contains("Missing credentials for openai/gpt-4o"),
                "unexpected error: {err}"
            );
            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("openai"));
            assert_eq!(session.header.model_id.as_deref(), Some("gpt-4o"));
        });
    }

    #[test]
    fn set_provider_model_allows_current_model_without_registry() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let auth_path = dir.path().join("auth.json");
            let auth = AuthStorage::load(auth_path).expect("load auth");
            let mut agent_session = build_switch_test_session(&auth);
            agent_session.model_registry = None;
            agent_session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::High);

            agent_session
                .set_provider_model("anthropic", "claude-sonnet-4-5")
                .await
                .expect("re-persisting the current model should succeed without a registry");

            assert_eq!(agent_session.agent.provider().name(), "anthropic");
            assert_eq!(
                agent_session.agent.provider().model_id(),
                "claude-sonnet-4-5"
            );
            assert_eq!(
                agent_session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::High)
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
            assert_eq!(
                session.header.model_id.as_deref(),
                Some("claude-sonnet-4-5")
            );
            assert_eq!(session.header.thinking_level.as_deref(), Some("high"));
        });
    }

    #[test]
    fn auto_compaction_start_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoCompactionStart {
            reason: "threshold".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_start");
        assert_eq!(json["reason"], "threshold");
    }

    #[test]
    fn auto_compaction_end_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoCompactionEnd {
            result: Some(serde_json::json!({
                "summary": "Compacted",
                "firstKeptEntryId": "abc123",
                "tokensBefore": 50000,
                "details": { "readFiles": [], "modifiedFiles": [] }
            })),
            aborted: false,
            will_retry: true,
            error_message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert!(json["result"].is_object());
        assert_eq!(json["aborted"], false);
        assert_eq!(json["willRetry"], true);
        assert!(json.get("errorMessage").is_none());
    }

    #[test]
    fn auto_compaction_end_with_error_serializes_error_message() {
        let event = AgentEvent::AutoCompactionEnd {
            result: None,
            aborted: false,
            will_retry: false,
            error_message: Some("compaction failed".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_compaction_end");
        assert!(json.get("result").is_none());
        assert_eq!(json["errorMessage"], "compaction failed");
    }

    #[test]
    fn apply_compaction_result_emits_structured_result_payload() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());

            let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let on_event: AgentEventHandler = Arc::new(move |event| {
                sink.lock().expect("lock compaction events").push(event);
            });

            let result = compaction::CompactionResult {
                summary: "Compacted 10 messages into 2".to_string(),
                first_kept_entry_id: "entry-5".to_string(),
                tokens_before: 12_000,
                details: compaction::CompactionDetails {
                    read_files: vec!["src/main.rs".to_string()],
                    modified_files: vec!["src/agent.rs".to_string()],
                    mode: None,
                },
                snap_payload: None,
            };

            agent_session
                .apply_compaction_result(result, on_event)
                .await
                .expect("apply compaction result");

            let payload = {
                let guard = events.lock().expect("lock captured events");
                guard
                    .iter()
                    .find_map(|event| match event {
                        AgentEvent::AutoCompactionEnd {
                            result: Some(result),
                            ..
                        } => Some(result.clone()),
                        _ => None,
                    })
                    .expect("auto compaction end payload")
            };

            assert_eq!(payload["summary"], "Compacted 10 messages into 2");
            assert_eq!(payload["firstKeptEntryId"], "entry-5");
            assert_eq!(payload["tokensBefore"], 12_000);
            // tokensAfter is the estimated post-compaction context: additive,
            // present, positive, and strictly smaller than tokensBefore (the
            // whole point of compaction — it shrinks the context). It is the
            // char-based heuristic over the post-compaction current path, not
            // the hand-set tokens_before.
            let tokens_after = payload["tokensAfter"]
                .as_u64()
                .expect("tokensAfter present and integer");
            assert!(tokens_after > 0, "tokensAfter must be positive");
            assert!(
                tokens_after < 12_000,
                "tokensAfter ({tokens_after}) must be < tokensBefore"
            );
            assert_eq!(payload["details"]["readFiles"], json!(["src/main.rs"]));
            assert_eq!(payload["details"]["modifiedFiles"], json!(["src/agent.rs"]));
        });
    }

    #[test]
    fn maybe_compact_forces_local_compaction_when_quota_blocked_and_oversized() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));

            // Small window so a handful of messages is "catastrophically"
            // oversized (>= 2x window) by heuristic estimation (chars / 3).
            let settings = ResolvedCompactionSettings {
                enabled: true,
                context_window_tokens: 100,
                reserve_tokens: 10,
                keep_recent_tokens: 30,
                mode: compaction::AutoCompactionMode::default(),
                render_mode: compaction::CompactionRenderMode::default(),
            };

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut guard = session.lock(cx.cx()).await.expect("session lock");
                for i in 0..6 {
                    guard.append_message(crate::session::SessionMessage::User {
                        content: UserContent::Text(format!(
                            "oversized turn {i}: {}",
                            "x".repeat(300)
                        )),
                        timestamp: Some(0),
                    });
                }
            }

            let mut agent_session = AgentSession::new(agent, session, false, settings);
            // Exhaust the per-session attempt quota so the background worker is
            // permanently blocked (the deadlock reported for oversized sessions).
            agent_session
                .compaction_worker
                .set_attempt_count_for_test(u32::MAX);
            assert!(!agent_session.compaction_worker.can_start());

            let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let on_event: AgentEventHandler = Arc::new(move |event| {
                sink.lock().expect("lock compaction events").push(event);
            });

            agent_session
                .maybe_compact(on_event)
                .await
                .expect("maybe_compact");

            let (start_reason, end_payload) = {
                let captured = events.lock().expect("lock captured events");
                let start_reason = captured.iter().find_map(|event| match event {
                    AgentEvent::AutoCompactionStart { reason } => Some(reason.clone()),
                    _ => None,
                });
                let end_payload = captured.iter().find_map(|event| match event {
                    AgentEvent::AutoCompactionEnd {
                        result: Some(result),
                        ..
                    } => Some(result.clone()),
                    _ => None,
                });
                drop(captured);
                (start_reason, end_payload)
            };
            let start_reason = start_reason.expect("forced local compaction should start");
            assert!(
                start_reason.starts_with("forced_local"),
                "unexpected start reason: {start_reason}"
            );
            let end_payload = end_payload.expect("forced local compaction should complete");
            assert!(
                end_payload["summary"]
                    .as_str()
                    .expect("summary string")
                    .contains("deterministic fallback")
            );

            // The session must now contain a compaction entry.
            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert!(
                guard
                    .entries_for_current_path()
                    .iter()
                    .any(|entry| matches!(entry, crate::session::SessionEntry::Compaction(_))),
                "forced local compaction should append a compaction entry"
            );
        });
    }

    #[test]
    fn maybe_compact_quota_blocked_is_noop_below_forced_threshold() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(&[], Path::new("."), None);
            let agent = Agent::new(provider, tools, AgentConfig::default());
            let session = Arc::new(Mutex::new(Session::in_memory()));

            // Over the compaction threshold (window - reserve) but below the
            // forced-local threshold (2x window): ~600 chars => ~200 tokens.
            let settings = ResolvedCompactionSettings {
                enabled: true,
                context_window_tokens: 150,
                reserve_tokens: 10,
                keep_recent_tokens: 30,
                mode: compaction::AutoCompactionMode::default(),
                render_mode: compaction::CompactionRenderMode::default(),
            };

            {
                let cx = crate::agent_cx::AgentCx::for_request();
                let mut guard = session.lock(cx.cx()).await.expect("session lock");
                for i in 0..2 {
                    guard.append_message(crate::session::SessionMessage::User {
                        content: UserContent::Text(format!("turn {i}: {}", "x".repeat(300))),
                        timestamp: Some(0),
                    });
                }
            }

            let mut agent_session = AgentSession::new(agent, session, false, settings);
            agent_session
                .compaction_worker
                .set_attempt_count_for_test(u32::MAX);

            let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let on_event: AgentEventHandler = Arc::new(move |event| {
                sink.lock().expect("lock compaction events").push(event);
            });

            agent_session
                .maybe_compact(on_event)
                .await
                .expect("maybe_compact");

            assert!(
                events.lock().expect("lock captured events").is_empty(),
                "quota-blocked compaction below the forced threshold must stay a no-op"
            );

            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = agent_session
                .session
                .lock(cx.cx())
                .await
                .expect("session lock");
            assert!(
                !guard
                    .entries_for_current_path()
                    .iter()
                    .any(|entry| matches!(entry, crate::session::SessionEntry::Compaction(_))),
            );
        });
    }

    #[test]
    fn auto_retry_start_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoRetryStart {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 4000,
            error_message: "rate limited".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 4000);
        assert_eq!(json["errorMessage"], "rate limited");
    }

    #[test]
    fn auto_retry_end_success_serializes_to_pi_mono_format() {
        let event = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 2,
            final_error: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], true);
        assert_eq!(json["attempt"], 2);
        assert!(json.get("finalError").is_none());
    }

    #[test]
    fn auto_retry_end_failure_serializes_final_error() {
        let event = AgentEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some("max retries exceeded".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert_eq!(json["success"], false);
        assert_eq!(json["attempt"], 3);
        assert_eq!(json["finalError"], "max retries exceeded");
    }

    // === Tool load modes / xdev dispatcher (bd-cv653.1.6) ===

    fn xdev_test_agent(enabled: &[&str], cwd: &Path) -> Agent {
        let provider = Arc::new(SilentProvider);
        let tools = ToolRegistry::new(enabled, cwd, None);
        Agent::new(provider, tools, AgentConfig::default())
    }

    fn xdev_call(action: &str, name: Option<&str>, args: Option<Value>) -> ToolCall {
        let mut arguments = json!({ "action": action });
        if let Some(name) = name {
            arguments["name"] = json!(name);
        }
        if let Some(inner) = args {
            arguments["args"] = inner;
        }
        ToolCall {
            id: "call-xdev-1".to_string(),
            name: "xdev".to_string(),
            arguments,
            thought_signature: None,
        }
    }

    #[test]
    fn xdev_list_and_describe_via_tool_execute() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let agent = xdev_test_agent(&["read", "ast_grep"], temp.path());
            let xdev = agent.tools.get("xdev").expect("xdev registered");

            let list = xdev
                .execute("c1", json!({"action": "list"}), None)
                .await
                .expect("list");
            let list_text = match &list.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(
                list_text.contains("ast_grep"),
                "list shows discoverable ast_grep"
            );
            assert!(!list.is_error);

            let describe = xdev
                .execute(
                    "c2",
                    json!({"action": "describe", "name": "ast_grep"}),
                    None,
                )
                .await
                .expect("describe");
            let describe_text = match &describe.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            let parsed: Value = serde_json::from_str(&describe_text).expect("describe JSON");
            assert_eq!(parsed["name"], "ast_grep");
            assert!(parsed["parameters"]["properties"]["pattern"].is_object());
        });
    }

    #[test]
    fn xdev_run_executes_discoverable_tool_through_agent() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(
                temp.path().join("main.rs"),
                "fn main() { let _ = compute().unwrap(); }\nfn compute() -> i32 { 1 }\n",
            )
            .expect("write fixture");
            let agent = xdev_test_agent(&["read", "ast_grep"], temp.path());
            let call = xdev_call(
                "run",
                Some("ast_grep"),
                Some(json!({"pattern": "$EXPR.unwrap()", "path": "."})),
            );
            let (output, is_error) = agent
                .execute_tool_without_hooks(&call, Arc::new(|_| {}))
                .await;
            assert!(!is_error, "run must succeed: {:?}", output.content);
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(
                text.contains("main.rs"),
                "run output names the fixture file: {text}"
            );
            assert_eq!(
                output
                    .details
                    .as_ref()
                    .and_then(|d| d.get("dispatchedVia"))
                    .and_then(Value::as_str),
                Some("xdev")
            );
        });
    }

    #[test]
    fn xdev_promote_moves_tool_into_live_schema() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let mut agent = xdev_test_agent(&["read", "ast_grep"], temp.path());

            // Before promotion: ast_grep is discoverable, so the schema omits it.
            let defs_before = agent
                .build_context()
                .tools
                .iter()
                .map(|def| def.name.clone())
                .collect::<Vec<_>>();
            assert!(defs_before.contains(&"read".to_string()));
            assert!(defs_before.contains(&"xdev".to_string()));
            assert!(
                !defs_before.contains(&"ast_grep".to_string()),
                "discoverable tool must be hidden pre-promotion"
            );

            let call = xdev_call("promote", Some("ast_grep"), None);
            let (output, is_error) = agent
                .execute_tool_without_hooks(&call, Arc::new(|_| {}))
                .await;
            assert!(!is_error);

            let has_promoted_tool = agent
                .build_context()
                .tools
                .iter()
                .any(|def| def.name == "ast_grep");
            assert!(
                has_promoted_tool,
                "promoted tool enters the schema mid-session"
            );
        });
    }

    #[test]
    fn xdev_run_unknown_tool_named_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let agent = xdev_test_agent(&["read", "ast_grep"], temp.path());
            let call = xdev_call("run", Some("nosuch_tool"), Some(json!({})));
            let (output, is_error) = agent
                .execute_tool_without_hooks(&call, Arc::new(|_| {}))
                .await;
            assert!(is_error, "unknown discoverable tool must error");
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("nosuch_tool"), "error names the tool: {text}");
        });
    }

    #[test]
    fn xdev_schema_token_accounting() {
        // Bead evidence: schema-token delta essential-only vs all-tools.
        let temp = tempfile::tempdir().expect("tempdir");
        let full_registry = ToolRegistry::new(
            &[
                "read",
                "bash",
                "edit",
                "write",
                "grep",
                "find",
                "ls",
                "hashline_edit",
                "ast_grep",
                "ast_edit",
            ],
            temp.path(),
            None,
        );
        let all_defs: usize = full_registry
            .tools()
            .iter()
            .map(|t| serde_json::to_string(&json!({"name": t.name(), "description": t.description(), "parameters": t.parameters()})).unwrap().len())
            .sum();
        let essential_defs: usize = full_registry
            .tools()
            .iter()
            .filter(|t| !full_registry.is_discoverable(t.name()))
            .map(|t| serde_json::to_string(&json!({"name": t.name(), "description": t.description(), "parameters": t.parameters()})).unwrap().len())
            .sum();
        println!(
            "xdev token accounting: all={all_defs} bytes, essential={essential_defs} bytes, saved={}",
            all_defs - essential_defs
        );
        assert!(
            essential_defs < all_defs,
            "essential schema must be strictly smaller ({essential_defs} < {all_defs})"
        );
        assert!(full_registry.is_discoverable("ast_grep"));
        assert!(!full_registry.is_discoverable("read"));
        assert!(full_registry.get("xdev").is_some(), "xdev auto-registered");
    }

    // === Plan mode (bd-cv653.3.5) ===

    #[test]
    fn plan_gate_blocks_mutation_and_allows_reads() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let target = temp.path().join("scratch.txt");
            std::fs::write(&target, "original").expect("write fixture");

            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(
                &["read", "write", "bash", "grep", "ls", "ast_grep"],
                temp.path(),
                None,
            );
            let agent = Agent::new(provider, tools, AgentConfig::default());
            agent.plan_state().enter_planning();

            // Mutation is blocked with the structured, model-readable error…
            let write_call = ToolCall {
                id: "w1".to_string(),
                name: "write".to_string(),
                arguments: json!({"path": "scratch.txt", "content": "changed"}),
                thought_signature: None,
            };
            let (output, is_error) = agent
                .execute_tool_without_hooks(&write_call, Arc::new(|_| {}))
                .await;
            assert!(is_error, "write must be blocked while planning");
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("PLAN_MODE_BLOCKED"), "gate error: {text}");
            // …and the file is untouched (zero bytes changed).
            assert_eq!(
                std::fs::read_to_string(&target).expect("read back"),
                "original"
            );

            // Reads flow freely.
            let read_call = ToolCall {
                id: "r1".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "scratch.txt"}),
                thought_signature: None,
            };
            let (output, is_error) = agent
                .execute_tool_without_hooks(&read_call, Arc::new(|_| {}))
                .await;
            assert!(!is_error, "read must pass the gate");
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("original"), "read returns content: {text}");

            // Bash (process effect) is blocked in plan mode.
            let bash_call = ToolCall {
                id: "b1".to_string(),
                name: "bash".to_string(),
                arguments: json!({"command": "echo hi"}),
                thought_signature: None,
            };
            let (_output, is_error) = agent
                .execute_tool_without_hooks(&bash_call, Arc::new(|_| {}))
                .await;
            assert!(is_error, "bash is blocked while planning");

            // Approval re-opens mutation.
            agent
                .plan_state()
                .submit_plan("goal: test; steps: 1) write".to_string());
            assert!(agent.plan_state().approve().is_some());
            let write_call = ToolCall {
                id: "w2".to_string(),
                name: "write".to_string(),
                arguments: json!({"path": "scratch.txt", "content": "changed"}),
                thought_signature: None,
            };
            let (_output, is_error) = agent
                .execute_tool_without_hooks(&write_call, Arc::new(|_| {}))
                .await;
            assert!(!is_error, "write allowed after approval");
            assert_eq!(
                std::fs::read_to_string(&target).expect("read back"),
                "changed"
            );
        });
    }

    #[test]
    fn plan_gate_xdev_run_uses_inner_tool_effects() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").expect("fixture");
            let provider = Arc::new(SilentProvider);
            let tools = ToolRegistry::new(
                &["read", "write", "ast_grep", "ast_edit"],
                temp.path(),
                None,
            );
            let agent = Agent::new(provider, tools, AgentConfig::default());
            agent.plan_state().enter_planning();

            // xdev run on a read-only tool (ast_grep) passes the gate.
            let run_read = ToolCall {
                id: "x1".to_string(),
                name: "xdev".to_string(),
                arguments: json!({
                    "action": "run",
                    "name": "ast_grep",
                    "args": {"pattern": "fn $NAME($$$)", "path": "."}
                }),
                thought_signature: None,
            };
            let (_output, is_error) = agent
                .execute_tool_without_hooks(&run_read, Arc::new(|_| {}))
                .await;
            assert!(
                !is_error,
                "xdev run on a read-only tool passes while planning"
            );

            // xdev run on a mutating tool (ast_edit) is blocked.
            let run_write = ToolCall {
                id: "x2".to_string(),
                name: "xdev".to_string(),
                arguments: json!({
                    "action": "run",
                    "name": "ast_edit",
                    "args": {"ops": [{"pat": "fn main() {}", "out": ""}], "path": "."}
                }),
                thought_signature: None,
            };
            let (output, is_error) = agent
                .execute_tool_without_hooks(&run_write, Arc::new(|_| {}))
                .await;
            assert!(
                is_error,
                "xdev run on a mutating tool is blocked while planning"
            );
            let text = match &output.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("PLAN_MODE_BLOCKED"), "gate error: {text}");
        });
    }

    #[test]
    fn plan_mode_session_entries_round_trip() {
        let mut session = Session::in_memory();
        session.append_custom_entry("plan_mode".to_string(), Some(json!({"mode": "planning"})));
        session.append_custom_entry("plan_mode".to_string(), Some(json!({"mode": "approved"})));
        let json = serde_json::to_string(session.entries_for_current_path()[1]).expect("ser");
        let parsed: crate::session::SessionEntry = serde_json::from_str(&json).expect("reparse");
        let crate::session::SessionEntry::Custom(custom) = &parsed else {
            panic!("expected custom entry");
        };
        assert_eq!(custom.custom_type, "plan_mode");
        assert_eq!(
            custom
                .data
                .as_ref()
                .and_then(|d| d.get("mode"))
                .and_then(Value::as_str),
            Some("approved")
        );
    }

    // === Dialect repair turn (bd-cv653.7.8) ===

    /// Emits a text-embedded tool call on stream 1, plain text on stream 2.
    struct TextCallProvider {
        calls: std::sync::atomic::AtomicUsize,
        model_id: &'static str,
        /// When true, the request after the repaired tool result drops
        /// mid-stream once so retry cleanup can be exercised.
        fail_after_tool_once: bool,
        /// Optional mutation-sensitive assertion for the resumed request.
        expected_retry_thinking: Option<crate::model::ThinkingLevel>,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for TextCallProvider {
        fn name(&self) -> &str {
            "test-provider"
        }
        fn api(&self) -> &str {
            "test-api"
        }
        fn model_id(&self) -> &str {
            self.model_id
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            let call_number = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call_number >= 2
                && let Some(expected) = self.expected_retry_thinking
            {
                assert_eq!(
                    options.thinking_level,
                    Some(expected),
                    "retry must preserve the original turn's magic-keyword effort"
                );
            }
            let make = |content: Vec<ContentBlock>, reason: StopReason| AssistantMessage {
                content,
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "qwen3-mock".to_string(),
                usage: Usage::default(),
                stop_reason: reason,
                stop_details: None,
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            let events: Vec<Result<StreamEvent>> = if call_number == 0 {
                vec![
                    Ok(StreamEvent::TextDelta {
                        content_index: 0,
                        delta: "Checking the file. <tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"fixture.txt\"}}</tool_call>".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        reason: StopReason::Stop,
                        message: make(
                            vec![ContentBlock::Text(TextContent::new(
                                "Checking the file. <tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"fixture.txt\"}}</tool_call>",
                            ))],
                            StopReason::Stop,
                        ),
                    }),
                ]
            } else if self.fail_after_tool_once && call_number == 1 {
                vec![
                    Ok(StreamEvent::Start {
                        partial: make(Vec::new(), StopReason::Stop),
                    }),
                    Err(Error::api(
                        "SSE error: connection reset by peer (transient connection drop)",
                    )),
                ]
            } else {
                assert!(
                    !context.messages.iter().any(|message| {
                        matches!(
                            message,
                            Message::Assistant(assistant)
                                if matches!(
                                    assistant.stop_reason,
                                    StopReason::Error | StopReason::Aborted
                                )
                        )
                    }),
                    "retry context must not retain the incomplete assistant"
                );
                vec![
                    Ok(StreamEvent::TextDelta {
                        content_index: 0,
                        delta: "The file says hello-fixture.".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        reason: StopReason::Stop,
                        message: make(
                            vec![ContentBlock::Text(TextContent::new(
                                "The file says hello-fixture.",
                            ))],
                            StopReason::Stop,
                        ),
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn dialect_repair_preserves_other_blocks_and_uses_unique_ids() {
        let provider = Arc::new(TextCallProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            model_id: "qwen3-mock",
            fail_after_tool_once: false,
            expected_retry_thinking: None,
        });
        let tools = ToolRegistry::new(&["read"], std::path::Path::new("."), None);
        let mut agent = Agent::new(provider, tools, AgentConfig::default());
        agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);
        let original = AssistantMessage {
            content: vec![
                ContentBlock::Thinking(ThinkingContent {
                    thinking: "reasoning".to_string(),
                    thinking_signature: Some("thinking-sig".to_string()),
                }),
                ContentBlock::Text(TextContent {
                    text: "preface".to_string(),
                    text_signature: Some("preface-sig".to_string()),
                }),
                ContentBlock::Image(ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                }),
                ContentBlock::Text(TextContent {
                    text: r#"<tool_call>{"name":"read","arguments":{"path":"fixture.txt"}}</tool_call>"#
                        .to_string(),
                    text_signature: Some("candidate-sig".to_string()),
                }),
                ContentBlock::Text(TextContent {
                    text: "tail".to_string(),
                    text_signature: Some("tail-sig".to_string()),
                }),
            ],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "qwen3-mock".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };

        let first = agent.maybe_repair_dialect_tool_calls(original.clone());
        let second = agent.maybe_repair_dialect_tool_calls(original);
        assert!(matches!(
            &first.content[0],
            ContentBlock::Thinking(thinking)
                if thinking.thinking_signature.as_deref() == Some("thinking-sig")
        ));
        assert!(matches!(
            &first.content[1],
            ContentBlock::Text(text)
                if text.text == "preface"
                    && text.text_signature.as_deref() == Some("preface-sig")
        ));
        assert!(matches!(&first.content[2], ContentBlock::Image(_)));
        assert!(matches!(
            &first.content[4],
            ContentBlock::Text(text)
                if text.text == "tail" && text.text_signature.as_deref() == Some("tail-sig")
        ));
        let first_id = extract_tool_calls(&first.content)[0].id.clone();
        let second_id = extract_tool_calls(&second.content)[0].id.clone();
        assert_ne!(
            first_id, second_id,
            "repaired call IDs must be session-unique"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn dialect_repair_continues_turn_and_executes_tool() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            // CONTROL: Native dialect (no repair path) — a plain-text turn
            // must complete without hitting the iteration cap. If this
            // loops, the test provider is the artifact, not the repair.
            for model_id in ["gpt-4o", "gpt-5.5"] {
                let control = Arc::new(TextCallProvider {
                    calls: std::sync::atomic::AtomicUsize::new(0),
                    model_id,
                    fail_after_tool_once: false,
                    expected_retry_thinking: None,
                });
                let control_tools = ToolRegistry::new(&["read"], std::path::Path::new("."), None);
                let mut control_agent = Agent::new(
                    Arc::clone(&control) as Arc<dyn Provider>,
                    control_tools,
                    AgentConfig::default(),
                );
                control_agent.config.max_tool_iterations = 5;
                let session = Arc::new(Mutex::new(Session::in_memory()));
                let mut control_session = AgentSession::new(
                    control_agent,
                    Arc::clone(&session),
                    false,
                    ResolvedCompactionSettings::default(),
                );
                let tool_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let tool_starts_for_events = Arc::clone(&tool_starts);
                let control_result = control_session
                    .run_text_with_abort("hello".to_string(), None, move |event| {
                        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                            tool_starts_for_events.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                    .await
                    .expect("non-repairable control completes");
                assert_eq!(control_result.stop_reason, StopReason::Stop);
                assert_eq!(control.calls.load(Ordering::SeqCst), 1, "{model_id}");
                assert_eq!(tool_starts.load(Ordering::SeqCst), 0, "{model_id}");
                let cx = asupersync::Cx::for_request();
                let inner = session.lock(&cx).await.expect("control session lock");
                assert!(
                    inner
                        .entries_for_current_path()
                        .iter()
                        .all(|entry| !matches!(
                            entry,
                            crate::session::SessionEntry::Custom(custom)
                                if custom.custom_type == "dialect_repair"
                        )),
                    "{model_id} must not persist repair telemetry"
                );
            }
        });
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("fixture.txt"), "hello-fixture")
                .expect("write fixture");
            let provider = Arc::new(TextCallProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                model_id: "qwen3-mock",
                fail_after_tool_once: false,
                expected_retry_thinking: None,
            });
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let mut agent = Agent::new(provider, tools, AgentConfig::default());
            agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);
            let session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                temp.path().join("sessions"),
            ))));
            let mut agent_session =
                AgentSession::new(agent, session, true, ResolvedCompactionSettings::default());

            let final_message = agent_session
                .run_text_with_abort("check the fixture".to_string(), None, |_| {})
                .await
                .expect("run completes");

            let texts: Vec<String> = agent_session
                .agent
                .messages()
                .iter()
                .flat_map(|m| match m {
                    crate::model::Message::Assistant(msg) => msg.content.clone(),
                    crate::model::Message::User(u) => match &u.content {
                        crate::model::UserContent::Text(t) => {
                            vec![ContentBlock::Text(TextContent::new(t))]
                        }
                        crate::model::UserContent::Blocks(blocks) => blocks.clone(),
                    },
                    crate::model::Message::ToolResult(r) => r.content.clone(),
                    crate::model::Message::Custom(_) => Vec::new(),
                })
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text),
                    _ => None,
                })
                .collect();
            assert!(
                texts.iter().any(|t| t.contains("hello-fixture")),
                "tool result with fixture content present: {texts:?}"
            );
            assert_eq!(
                final_message.stop_reason,
                StopReason::Stop,
                "turn ends cleanly after the repair"
            );
            // The session layer drained the ledger into a dialect_repair
            // Custom entry at run completion (bd-cv653.7.8).
            let entries = {
                let cx = asupersync::Cx::for_request();
                let inner = agent_session.session.lock(&cx).await.expect("session lock");
                let found: Vec<_> = inner
                    .entries_for_current_path()
                    .iter()
                    .filter_map(|e| match e {
                        crate::session::SessionEntry::Custom(c)
                            if c.custom_type == "dialect_repair" =>
                        {
                            Some(c.data.clone())
                        }
                        _ => None,
                    })
                    .collect();
                found
            };
            assert_eq!(entries.len(), 1, "one repair entry in the session");
            assert_eq!(
                entries[0]
                    .as_ref()
                    .and_then(|d| d.get("tool"))
                    .and_then(Value::as_str),
                Some("read")
            );

            let persisted_path = {
                let cx = asupersync::Cx::for_request();
                let inner = agent_session.session.lock(&cx).await.expect("session lock");
                inner.path.clone().expect("autosave created session file")
            };
            let reopened = Session::open(persisted_path.to_string_lossy().as_ref())
                .await
                .expect("reopen autosaved session");
            let persisted_repairs = reopened
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "dialect_repair"
                    )
                })
                .count();
            assert_eq!(persisted_repairs, 1, "repair audit entry survives reopen");
        });
    }

    #[test]
    fn dialect_repair_on_resume_persists_before_exit() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("fixture.txt"), "hello-fixture")
                .expect("write fixture");
            let provider = Arc::new(TextCallProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                model_id: "qwen3-mock",
                fail_after_tool_once: false,
                expected_retry_thinking: None,
            });
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let mut agent = Agent::new(provider, tools, AgentConfig::default());
            agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);
            let mut durable_session =
                Session::create_with_dir(Some(temp.path().join("resume-sessions")));
            durable_session.append_model_message(Message::User(UserMessage {
                content: UserContent::Text("check the fixture".to_string()),
                timestamp: Utc::now().timestamp_millis(),
            }));
            let session = Arc::new(Mutex::new(durable_session));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                true,
                ResolvedCompactionSettings::default(),
            );

            let final_message = agent_session
                .run_continue_with_abort(None, |_| {})
                .await
                .expect("resume completes");
            assert_eq!(final_message.stop_reason, StopReason::Stop);

            let persisted_path = {
                let cx = asupersync::Cx::for_request();
                let inner = session.lock(&cx).await.expect("session lock");
                inner.path.clone().expect("autosave created session file")
            };
            let reopened = Session::open(persisted_path.to_string_lossy().as_ref())
                .await
                .expect("reopen autosaved resume session");
            let persisted_repairs = reopened
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "dialect_repair"
                                && custom.data.as_ref().is_some_and(|data| {
                                    data["tool"] == json!("read")
                                })
                    )
                })
                .count();
            assert_eq!(
                persisted_repairs, 1,
                "resume repair audit entry survives an immediate reopen"
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn failed_keyword_repair_turn_keeps_audits_and_reverts_incomplete_tail() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("fixture.txt"), "hello-fixture")
                .expect("write fixture");
            let provider = Arc::new(TextCallProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                model_id: "qwen3-mock",
                fail_after_tool_once: true,
                expected_retry_thinking: Some(crate::model::ThinkingLevel::High),
            });
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let mut agent = Agent::new(
                Arc::clone(&provider) as Arc<dyn Provider>,
                tools,
                AgentConfig::default(),
            );
            agent.set_tool_call_dialect(crate::dialects::Dialect::Xmlish);
            agent.set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
            let session = Arc::new(Mutex::new(Session::create_with_dir(Some(
                temp.path().join("retry-sessions"),
            ))));
            let mut agent_session = AgentSession::new(
                agent,
                Arc::clone(&session),
                true,
                ResolvedCompactionSettings::default(),
            );
            let tool_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let tool_starts_for_events = Arc::clone(&tool_starts);

            let failed = agent_session
                .run_text_with_abort(
                    "ultrathink check the fixture".to_string(),
                    None,
                    move |event| {
                        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                            tool_starts_for_events.fetch_add(1, Ordering::SeqCst);
                        }
                    },
                )
                .await
                .expect("mid-stream failure is represented by an assistant message");
            assert_eq!(failed.stop_reason, StopReason::Error);

            {
                let cx = asupersync::Cx::for_request();
                let inner = session.lock(&cx).await.expect("failed session lock");
                let entries = inner.entries_for_current_path();
                assert!(matches!(
                    entries.last(),
                    Some(crate::session::SessionEntry::Message(message))
                        if matches!(
                            &message.message,
                            crate::session::SessionMessage::Assistant { message }
                                if message.stop_reason == StopReason::Error
                        )
                ));
                assert_eq!(
                    entries
                        .iter()
                        .filter(|entry| matches!(
                            entry,
                            crate::session::SessionEntry::Custom(custom)
                                if custom.custom_type == "dialect_repair"
                        ))
                        .count(),
                    1
                );
                assert_eq!(
                    entries
                        .iter()
                        .filter(|entry| matches!(
                            entry,
                            crate::session::SessionEntry::Custom(custom)
                                if custom.custom_type == "magic_keyword"
                        ))
                        .count(),
                    1
                );
            }

            assert!(
                agent_session
                    .revert_incomplete_response()
                    .await
                    .expect("revert succeeds"),
                "the failed assistant must remain the removable session tail"
            );
            let resumed = agent_session
                .run_continue_with_abort(None, |_| {})
                .await
                .expect("retry resumes after the completed tool cycle");
            assert_eq!(resumed.stop_reason, StopReason::Stop);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
            assert_eq!(tool_starts.load(Ordering::SeqCst), 1);

            let persisted_path = {
                let cx = asupersync::Cx::for_request();
                let inner = session.lock(&cx).await.expect("resumed session lock");
                inner.path.clone().expect("autosave created session file")
            };
            let reopened = Session::open(persisted_path.to_string_lossy().as_ref())
                .await
                .expect("reopen autosaved retry session");
            let entries = reopened.entries_for_current_path();
            assert!(entries.iter().all(|entry| !matches!(
                entry,
                crate::session::SessionEntry::Message(message)
                    if matches!(
                        &message.message,
                        crate::session::SessionMessage::Assistant { message }
                            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
                    )
            )));
            let repair_data = entries.iter().find_map(|entry| match entry {
                crate::session::SessionEntry::Custom(custom)
                    if custom.custom_type == "dialect_repair" =>
                {
                    custom.data.as_ref()
                }
                _ => None,
            });
            assert_eq!(
                repair_data
                    .and_then(|data| data.get("tool"))
                    .and_then(Value::as_str),
                Some("read")
            );
            assert!(
                repair_data
                    .and_then(|data| data.get("strippedBytes"))
                    .and_then(Value::as_u64)
                    .is_some_and(|bytes| bytes > 0),
                "repair telemetry must prove that text was actually stripped"
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "dialect_repair"
                    ))
                    .count(),
                1
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| matches!(
                        entry,
                        crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "magic_keyword"
                    ))
                    .count(),
                1
            );
        });
    }

    // === Advisor turn review (bd-cv653.3.3) ===

    /// Doer: issues one structured read call, then finishes with text.
    struct ScriptedDoerProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ScriptedDoerProvider {
        fn name(&self) -> &str {
            "doer"
        }
        fn api(&self) -> &str {
            "test-api"
        }
        fn model_id(&self) -> &str {
            "doer-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            let call_number = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let make = |content: Vec<ContentBlock>| AssistantMessage {
                content,
                api: "test-api".to_string(),
                provider: "doer".to_string(),
                model: "doer-model".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            let events: Vec<Result<StreamEvent>> = if call_number == 0 {
                vec![Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: make(vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-1".to_string(),
                        name: "read".to_string(),
                        arguments: json!({"path": "fixture.txt"}),
                        thought_signature: None,
                    })]),
                })]
            } else {
                vec![Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: make(vec![ContentBlock::Text(TextContent::new("all fixed"))]),
                })]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// Advisor: returns a CONCERN verdict.
    struct ScriptedAdvisorProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ScriptedAdvisorProvider {
        fn name(&self) -> &str {
            "advisor"
        }
        fn api(&self) -> &str {
            "test-api"
        }
        fn model_id(&self) -> &str {
            "advisor-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            let events: Vec<Result<StreamEvent>> = vec![
                Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "CONCERN: the read covers the whole tree".to_string(),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: AssistantMessage {
                        content: vec![ContentBlock::Text(TextContent::new(
                            "CONCERN: the read covers the whole tree",
                        ))],
                        api: "test-api".to_string(),
                        provider: "advisor".to_string(),
                        model: "advisor-model".to_string(),
                        usage: Usage::default(),
                        stop_reason: StopReason::Stop,
                        stop_details: None,
                        error_message: None,
                        timestamp: chrono::Utc::now().timestamp_millis(),
                    },
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn advisor_review_injects_concern_into_steering_queue() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp.path().join("fixture.txt"), "x").expect("fixture");
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let agent = Agent::new(
                Arc::new(ScriptedDoerProvider {
                    calls: std::sync::atomic::AtomicUsize::new(0),
                }),
                tools,
                AgentConfig::default(),
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());
            agent_session.advisor = Some(crate::advisor::AdvisorRuntime::new(
                Arc::new(ScriptedAdvisorProvider),
                "advisor/advisor-model".to_string(),
            ));

            let _ = agent_session
                .run_text_with_abort("check the fixture".to_string(), None, |_| {})
                .await
                .expect("turn 1 completes");

            // Turn 2 delivers the queued advisor note into the session
            // context (the next-turn injection is the acceptance behavior).
            let _ = agent_session
                .run_text_with_abort("continue".to_string(), None, |_| {})
                .await
                .expect("turn 2 completes");

            let steered = agent_session
                .agent
                .messages()
                .iter()
                .filter_map(|m| match m {
                    crate::model::Message::User(u) => match &u.content {
                        crate::model::UserContent::Text(t) => Some(t.clone()),
                        crate::model::UserContent::Blocks(_) => None,
                    },
                    _ => None,
                })
                .any(|text| text.contains("ADVISOR:CONCERN"));
            assert!(
                steered,
                "advisor concern must be delivered into the next turn"
            );
            let has_entry = {
                let cx = asupersync::Cx::for_request();
                let inner = agent_session.session.lock(&cx).await.expect("session lock");
                inner.entries_for_current_path().iter().any(|e| {
                    matches!(
                        e,
                        crate::session::SessionEntry::Custom(c) if c.custom_type == "advisor_note"
                    )
                })
            };
            assert!(has_entry, "advisor_note session entry recorded");
        });
    }

    #[test]
    fn advisor_absent_means_zero_overhead() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let tools = ToolRegistry::new(&["read"], temp.path(), None);
            let agent = Agent::new(
                Arc::new(ScriptedDoerProvider {
                    calls: std::sync::atomic::AtomicUsize::new(0),
                }),
                tools,
                AgentConfig::default(),
            );
            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session =
                AgentSession::new(agent, session, false, ResolvedCompactionSettings::default());
            assert!(agent_session.advisor.is_none());
            let result = agent_session
                .run_text_with_abort("work".to_string(), None, |_| {})
                .await;
            assert!(result.is_ok(), "no advisor → turn unaffected");
        });
    }
}
