//! Versioned extension messages, validation, and hostcall reactor protocol.

// This canonical protocol bridge intentionally retains a broad parent import:
// it implements the façade-owned public wire types using private reactor,
// rewrite, telemetry, and structured-concurrency services. The module is
// private, so this import does not widen the external API; narrower production
// seams use explicit dependencies.
use super::*;

// ============================================================================
// Protocol (v1)
// ============================================================================

const HOSTCALL_OPCODE_CONTEXT_KEY: &str = "typed_opcode";
const HOSTCALL_IO_URING_CONTEXT_KEY: &str = "io_uring_lane_input";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostcallOpcodeSource {
    ContextV1,
    DerivedV1,
}

impl CommonHostcallOpcode {
    pub(super) const fn code(self) -> &'static str {
        match self {
            Self::ToolRead => "tool.read",
            Self::ToolWrite => "tool.write",
            Self::ToolEdit => "tool.edit",
            Self::ToolBash => "tool.bash",
            Self::SessionGetState => "session.get_state",
            Self::SessionGetMessages => "session.get_messages",
            Self::SessionGetEntries => "session.get_entries",
            Self::SessionGetBranch => "session.get_branch",
            Self::SessionGetFile => "session.get_file",
            Self::SessionGetName => "session.get_name",
            Self::SessionSetName => "session.set_name",
            Self::SessionGetModel => "session.get_model",
            Self::SessionSetModel => "session.set_model",
            Self::SessionGetThinkingLevel => "session.get_thinking_level",
            Self::SessionSetThinkingLevel => "session.set_thinking_level",
            Self::SessionSetLabel => "session.set_label",
            Self::EventsGetActiveTools => "events.get_active_tools",
            Self::EventsGetAllTools => "events.get_all_tools",
            Self::EventsSetActiveTools => "events.set_active_tools",
            Self::EventsEmit => "events.emit",
            Self::EventsList => "events.list",
            Self::EventsGetModel => "events.get_model",
            Self::EventsSetModel => "events.set_model",
            Self::EventsGetThinkingLevel => "events.get_thinking_level",
            Self::EventsSetThinkingLevel => "events.set_thinking_level",
            Self::EventsGetFlag => "events.get_flag",
            Self::EventsListFlags => "events.list_flags",
            Self::EventsAppendEntry => "events.append_entry",
            Self::EventsRegisterCommand => "events.register_command",
        }
    }

    pub(super) const fn method(self) -> &'static str {
        match self {
            Self::ToolRead | Self::ToolWrite | Self::ToolEdit | Self::ToolBash => "tool",
            Self::SessionGetState
            | Self::SessionGetMessages
            | Self::SessionGetEntries
            | Self::SessionGetBranch
            | Self::SessionGetFile
            | Self::SessionGetName
            | Self::SessionSetName
            | Self::SessionGetModel
            | Self::SessionSetModel
            | Self::SessionGetThinkingLevel
            | Self::SessionSetThinkingLevel
            | Self::SessionSetLabel => "session",
            Self::EventsGetActiveTools
            | Self::EventsGetAllTools
            | Self::EventsSetActiveTools
            | Self::EventsEmit
            | Self::EventsList
            | Self::EventsGetModel
            | Self::EventsSetModel
            | Self::EventsGetThinkingLevel
            | Self::EventsSetThinkingLevel
            | Self::EventsGetFlag
            | Self::EventsListFlags
            | Self::EventsAppendEntry
            | Self::EventsRegisterCommand => "events",
        }
    }

    pub(super) const fn required_capability(self) -> &'static str {
        match self {
            Self::ToolRead => "read",
            Self::ToolWrite | Self::ToolEdit => "write",
            Self::ToolBash => "exec",
            Self::SessionGetState
            | Self::SessionGetMessages
            | Self::SessionGetEntries
            | Self::SessionGetBranch
            | Self::SessionGetFile
            | Self::SessionGetName
            | Self::SessionSetName
            | Self::SessionGetModel
            | Self::SessionSetModel
            | Self::SessionGetThinkingLevel
            | Self::SessionSetThinkingLevel
            | Self::SessionSetLabel => "session",
            Self::EventsGetActiveTools
            | Self::EventsGetAllTools
            | Self::EventsSetActiveTools
            | Self::EventsEmit
            | Self::EventsList
            | Self::EventsGetModel
            | Self::EventsSetModel
            | Self::EventsGetThinkingLevel
            | Self::EventsSetThinkingLevel
            | Self::EventsGetFlag
            | Self::EventsListFlags
            | Self::EventsAppendEntry
            | Self::EventsRegisterCommand => "events",
        }
    }

    pub(super) const fn capability_class(self) -> &'static str {
        match self {
            Self::ToolRead | Self::ToolWrite | Self::ToolEdit => "filesystem",
            Self::ToolBash => "execution",
            Self::SessionGetState
            | Self::SessionGetMessages
            | Self::SessionGetEntries
            | Self::SessionGetBranch
            | Self::SessionGetFile
            | Self::SessionGetName
            | Self::SessionSetName
            | Self::SessionGetModel
            | Self::SessionSetModel
            | Self::SessionGetThinkingLevel
            | Self::SessionSetThinkingLevel
            | Self::SessionSetLabel => "session",
            Self::EventsGetActiveTools
            | Self::EventsGetAllTools
            | Self::EventsSetActiveTools
            | Self::EventsEmit
            | Self::EventsList
            | Self::EventsGetModel
            | Self::EventsSetModel
            | Self::EventsGetThinkingLevel
            | Self::EventsSetThinkingLevel
            | Self::EventsGetFlag
            | Self::EventsListFlags
            | Self::EventsAppendEntry
            | Self::EventsRegisterCommand => "events",
        }
    }

    pub(super) const fn lane_matrix_key(self) -> &'static str {
        match self {
            Self::ToolRead => "tool|tool.read|filesystem",
            Self::ToolWrite => "tool|tool.write|filesystem",
            Self::ToolEdit => "tool|tool.edit|filesystem",
            Self::ToolBash => "tool|tool.bash|execution",
            Self::SessionGetState => "session|session.get_state|session",
            Self::SessionGetMessages => "session|session.get_messages|session",
            Self::SessionGetEntries => "session|session.get_entries|session",
            Self::SessionGetBranch => "session|session.get_branch|session",
            Self::SessionGetFile => "session|session.get_file|session",
            Self::SessionGetName => "session|session.get_name|session",
            Self::SessionSetName => "session|session.set_name|session",
            Self::SessionGetModel => "session|session.get_model|session",
            Self::SessionSetModel => "session|session.set_model|session",
            Self::SessionGetThinkingLevel => "session|session.get_thinking_level|session",
            Self::SessionSetThinkingLevel => "session|session.set_thinking_level|session",
            Self::SessionSetLabel => "session|session.set_label|session",
            Self::EventsGetActiveTools => "events|events.get_active_tools|events",
            Self::EventsGetAllTools => "events|events.get_all_tools|events",
            Self::EventsSetActiveTools => "events|events.set_active_tools|events",
            Self::EventsEmit => "events|events.emit|events",
            Self::EventsList => "events|events.list|events",
            Self::EventsGetModel => "events|events.get_model|events",
            Self::EventsSetModel => "events|events.set_model|events",
            Self::EventsGetThinkingLevel => "events|events.get_thinking_level|events",
            Self::EventsSetThinkingLevel => "events|events.set_thinking_level|events",
            Self::EventsGetFlag => "events|events.get_flag|events",
            Self::EventsListFlags => "events|events.list_flags|events",
            Self::EventsAppendEntry => "events|events.append_entry|events",
            Self::EventsRegisterCommand => "events|events.register_command|events",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostcallOpcodeResolution {
    FastPath {
        opcode: CommonHostcallOpcode,
        source: HostcallOpcodeSource,
    },
    Fallback {
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostcallDispatchLane {
    Fast,
    Compat,
}

impl HostcallDispatchLane {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Compat => "compat",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HostcallLaneDecision {
    pub(super) lane: HostcallDispatchLane,
    pub(super) reason: &'static str,
    pub(super) opcode: Option<CommonHostcallOpcode>,
    pub(super) capability_class: &'static str,
    pub(super) matrix_key: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostcallLaneExecution {
    pub(super) lane: HostcallDispatchLane,
    pub(super) decision_reason: String,
    pub(super) fallback_reason: Option<String>,
    pub(super) matrix_key: &'static str,
    pub(super) dispatch_latency_ms: u64,
}

pub(super) const HOSTCALL_MARSHALLING_PATH_CANONICAL_GENERIC: &str = "canonical_generic_v1";
pub(super) const HOSTCALL_MARSHALLING_PATH_FAST_OPCODE: &str = "interned_opcode_arena_v1";
pub(super) const HOSTCALL_MARSHALLING_PATH_CANONICAL_FALLBACK: &str = "canonical_fallback_v1";
pub(super) const HOSTCALL_MARSHALLING_FALLBACK_OPCODE_SHAPE_MISS: &str =
    "opcode_payload_shape_miss";
const HOSTCALL_MARSHALLING_FALLBACK_REWRITE_DIVERGENCE: &str = "rewrite_semantic_divergence";
const HOSTCALL_REWRITE_RULE_BASELINE: &str = "baseline_canonical";
pub(super) const HOSTCALL_REWRITE_RULE_FAST_OPCODE_FUSION: &str = "fuse_hash_dispatch_fast_opcode";
const HOSTCALL_REWRITE_COST_BASELINE: u32 = 100;
const HOSTCALL_REWRITE_COST_FAST_OPCODE: u32 = 35;
const HOSTCALL_SUPERINSTRUCTION_TRACE_HISTORY_LIMIT: usize = 256;
const HOSTCALL_SUPERINSTRUCTION_RECOMPILE_INTERVAL: u64 = 16;

fn hostcall_rewrite_engine() -> &'static HostcallRewriteEngine {
    static ENGINE: OnceLock<HostcallRewriteEngine> = OnceLock::new();
    ENGINE.get_or_init(HostcallRewriteEngine::from_env)
}

#[derive(Debug, Default)]
struct HostcallSuperinstructionRuntimeState {
    compiler: HostcallSuperinstructionCompiler,
    trace_history: VecDeque<String>,
    plans: Vec<HostcallSuperinstructionPlan>,
    observation_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct HostcallSuperinstructionTelemetry {
    trace_signature: Option<String>,
    plan_id: Option<String>,
    expected_cost_delta: i64,
    observed_cost_delta: i64,
    deopt_reason: Option<String>,
    /// Whether the trace-JIT tier dispatched this call.
    jit_hit: bool,
    /// JIT tier improvement delta over tier-1 fused cost.
    jit_cost_delta: i64,
}

fn hostcall_superinstruction_state() -> &'static Mutex<HostcallSuperinstructionRuntimeState> {
    static STATE: OnceLock<Mutex<HostcallSuperinstructionRuntimeState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HostcallSuperinstructionRuntimeState::default()))
}

#[allow(clippy::option_if_let_else)]
fn hostcall_superinstruction_telemetry(
    opcode: Option<CommonHostcallOpcode>,
) -> HostcallSuperinstructionTelemetry {
    let Some(opcode) = opcode else {
        return HostcallSuperinstructionTelemetry {
            deopt_reason: Some("no_opcode_hint".to_string()),
            ..HostcallSuperinstructionTelemetry::default()
        };
    };

    let Ok(mut state) = hostcall_superinstruction_state().lock() else {
        return HostcallSuperinstructionTelemetry {
            deopt_reason: Some("superinstruction_state_lock_poisoned".to_string()),
            ..HostcallSuperinstructionTelemetry::default()
        };
    };
    if !state.compiler.enabled() {
        return HostcallSuperinstructionTelemetry {
            deopt_reason: Some("superinstructions_disabled".to_string()),
            ..HostcallSuperinstructionTelemetry::default()
        };
    }

    state.trace_history.push_back(opcode.code().to_string());
    while state.trace_history.len() > HOSTCALL_SUPERINSTRUCTION_TRACE_HISTORY_LIMIT {
        let _ = state.trace_history.pop_front();
    }
    state.observation_count = state.observation_count.saturating_add(1);

    if state.trace_history.len() >= 2
        && (state.plans.is_empty()
            || state.observation_count % HOSTCALL_SUPERINSTRUCTION_RECOMPILE_INTERVAL == 0)
    {
        let trace = state.trace_history.iter().cloned().collect::<Vec<_>>();
        state.plans = state.compiler.compile_plans(&[trace]);
    }

    let max_window = state.compiler.max_window().min(state.trace_history.len());
    let mut recent_window = state
        .trace_history
        .iter()
        .rev()
        .take(max_window)
        .cloned()
        .collect::<Vec<_>>();
    recent_window.reverse();
    if recent_window.len() < 2 {
        return HostcallSuperinstructionTelemetry {
            deopt_reason: Some("insufficient_trace_history".to_string()),
            ..HostcallSuperinstructionTelemetry::default()
        };
    }

    let mut best_hit = None;
    for start in 0..recent_window.len() - 1 {
        let candidate = execute_with_superinstruction(&recent_window[start..], &state.plans);
        if candidate.selection.hit() {
            let replace =
                best_hit
                    .as_ref()
                    .is_none_or(|current: &HostcallSuperinstructionTelemetry| {
                        candidate.selection.expected_cost_delta > current.expected_cost_delta
                    });
            if replace {
                // Attempt JIT promotion and dispatch for this plan.
                let (jit_hit, jit_cost_delta) = if let Some(ref pid) =
                    candidate.selection.selected_plan_id
                {
                    // Find the matching plan to record execution.
                    let matched_plan = state.plans.iter().find(|p| p.plan_id == *pid);
                    if let Some(plan) = matched_plan {
                        TRACE_JIT.with(|cell| {
                            let mut jit = cell.borrow_mut();
                            jit.record_plan_execution(plan);
                            let ctx = GuardContext::default();
                            let result = jit.try_jit_dispatch(pid, &recent_window[start..], &ctx);
                            (result.jit_hit, result.cost_delta)
                        })
                    } else {
                        (false, 0)
                    }
                } else {
                    (false, 0)
                };

                best_hit = Some(HostcallSuperinstructionTelemetry {
                    trace_signature: Some(candidate.selection.trace_signature),
                    plan_id: candidate.selection.selected_plan_id,
                    expected_cost_delta: candidate.selection.expected_cost_delta,
                    observed_cost_delta: candidate.selection.expected_cost_delta,
                    deopt_reason: None,
                    jit_hit,
                    jit_cost_delta,
                });
            }
        }
    }
    if let Some(hit) = best_hit {
        return hit;
    }

    let fallback = execute_with_superinstruction(&recent_window, &state.plans);
    HostcallSuperinstructionTelemetry {
        trace_signature: Some(fallback.selection.trace_signature),
        plan_id: None,
        expected_cost_delta: 0,
        observed_cost_delta: 0,
        deopt_reason: fallback.selection.deopt_reason.map(str::to_string),
        jit_hit: false,
        jit_cost_delta: 0,
    }
}

#[cfg(test)]
pub(super) fn reset_hostcall_superinstruction_state_for_tests() {
    if let Ok(mut state) = hostcall_superinstruction_state().lock() {
        *state = HostcallSuperinstructionRuntimeState::default();
    }
}

/// Guard that serializes tests which touch the global superinstruction state.
/// Acquire via `superinstruction_test_lock()` at the start of each such test.
#[cfg(test)]
pub(super) fn superinstruction_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostcallMarshallingTelemetry {
    pub(super) path: String,
    pub(super) latency_us: u64,
    pub(super) fallback_reason: Option<String>,
    pub(super) fallback_count: u64,
    pub(super) rewrite_rule: Option<String>,
    pub(super) rewrite_expected_cost_delta: i64,
    pub(super) rewrite_observed_cost_delta: i64,
    pub(super) rewrite_fallback_reason: Option<String>,
    pub(super) superinstruction_trace_signature: Option<String>,
    pub(super) superinstruction_plan_id: Option<String>,
    pub(super) superinstruction_expected_cost_delta: i64,
    pub(super) superinstruction_observed_cost_delta: i64,
    pub(super) superinstruction_deopt_reason: Option<String>,
    pub(super) superinstruction_jit_hit: bool,
    pub(super) superinstruction_jit_cost_delta: i64,
}

impl Default for HostcallMarshallingTelemetry {
    fn default() -> Self {
        Self {
            path: HOSTCALL_MARSHALLING_PATH_CANONICAL_GENERIC.to_string(),
            latency_us: 0,
            fallback_reason: None,
            fallback_count: 0,
            rewrite_rule: None,
            rewrite_expected_cost_delta: 0,
            rewrite_observed_cost_delta: 0,
            rewrite_fallback_reason: None,
            superinstruction_trace_signature: None,
            superinstruction_plan_id: None,
            superinstruction_expected_cost_delta: 0,
            superinstruction_observed_cost_delta: 0,
            superinstruction_deopt_reason: None,
            superinstruction_jit_hit: false,
            superinstruction_jit_cost_delta: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostcallMarshallingArtifacts {
    pub(super) params_hash: String,
    pub(super) args_shape_hash: String,
    pub(super) telemetry: HostcallMarshallingTelemetry,
}

/// Borrowed arena view for hot hostcall marshalling paths.
///
/// The arena keeps references into the existing payload so fast opcode lanes
/// can hash canonical envelopes without cloning or reconstructing top-level
/// parameter objects.
pub(super) struct HostcallPayloadArena<'a> {
    method: &'a str,
    params: &'a Value,
    opcode: Option<CommonHostcallOpcode>,
}

impl<'a> HostcallPayloadArena<'a> {
    pub(super) const fn new(
        method: &'a str,
        params: &'a Value,
        opcode: Option<CommonHostcallOpcode>,
    ) -> Self {
        Self {
            method,
            params,
            opcode,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn marshal(&self) -> HostcallMarshallingArtifacts {
        let baseline_started = Instant::now();
        let baseline_params_hash = hostcall_params_hash(self.method, self.params);
        let baseline_args_shape_hash = hostcall_params_shape_hash(self.method, self.params);
        let baseline_latency_us =
            u64::try_from(baseline_started.elapsed().as_micros()).unwrap_or(u64::MAX);

        let baseline_plan = HostcallRewritePlan {
            kind: HostcallRewritePlanKind::BaselineCanonical,
            estimated_cost: HOSTCALL_REWRITE_COST_BASELINE,
            rule_id: HOSTCALL_REWRITE_RULE_BASELINE,
        };

        let mut fallback_reason = self
            .opcode
            .map(|_| HOSTCALL_MARSHALLING_FALLBACK_OPCODE_SHAPE_MISS.to_string());
        let mut fast_candidate_hashes: Option<(String, String)> = None;
        let mut fast_candidate_latency_us = 0_u64;
        let mut rewrite_candidates = Vec::new();

        if let Some(opcode) = self.opcode {
            let fast_started = Instant::now();
            let maybe_fast = self.hash_fast_opcode(opcode);
            let fast_latency =
                u64::try_from(fast_started.elapsed().as_micros()).unwrap_or(u64::MAX);
            if let Some((fast_params_hash, fast_args_shape_hash)) = maybe_fast {
                if fast_params_hash == baseline_params_hash
                    && fast_args_shape_hash == baseline_args_shape_hash
                {
                    fallback_reason = None;
                    fast_candidate_latency_us = fast_latency;
                    fast_candidate_hashes = Some((fast_params_hash, fast_args_shape_hash));
                    rewrite_candidates.push(HostcallRewritePlan {
                        kind: HostcallRewritePlanKind::FastOpcodeFusion,
                        estimated_cost: HOSTCALL_REWRITE_COST_FAST_OPCODE,
                        rule_id: HOSTCALL_REWRITE_RULE_FAST_OPCODE_FUSION,
                    });
                } else {
                    fallback_reason =
                        Some(HOSTCALL_MARSHALLING_FALLBACK_REWRITE_DIVERGENCE.to_string());
                }
            }
        }

        let rewrite_decision =
            hostcall_rewrite_engine().select_plan(baseline_plan, &rewrite_candidates);
        let use_fast_rewrite = rewrite_decision.selected.kind
            == HostcallRewritePlanKind::FastOpcodeFusion
            && fast_candidate_hashes.is_some();

        let (params_hash, args_shape_hash, path, latency_us, rewrite_rule) = if use_fast_rewrite {
            let (params_hash, args_shape_hash) =
                fast_candidate_hashes.expect("fast rewrite selected without hashes");
            (
                params_hash,
                args_shape_hash,
                HOSTCALL_MARSHALLING_PATH_FAST_OPCODE.to_string(),
                fast_candidate_latency_us,
                Some(rewrite_decision.selected.rule_id.to_string()),
            )
        } else {
            let path = if self.opcode.is_some() {
                HOSTCALL_MARSHALLING_PATH_CANONICAL_FALLBACK.to_string()
            } else {
                HOSTCALL_MARSHALLING_PATH_CANONICAL_GENERIC.to_string()
            };
            (
                baseline_params_hash,
                baseline_args_shape_hash,
                path,
                baseline_latency_us,
                None,
            )
        };

        let rewrite_fallback_reason = if use_fast_rewrite {
            None
        } else {
            rewrite_decision.fallback_reason.map(str::to_string)
        };
        let rewrite_observed_cost_delta = if use_fast_rewrite {
            let baseline_latency = i64::try_from(baseline_latency_us).unwrap_or(i64::MAX);
            let fast_latency = i64::try_from(fast_candidate_latency_us).unwrap_or(i64::MAX);
            baseline_latency.saturating_sub(fast_latency)
        } else {
            0
        };
        let superinstruction = hostcall_superinstruction_telemetry(self.opcode);

        HostcallMarshallingArtifacts {
            params_hash,
            args_shape_hash,
            telemetry: HostcallMarshallingTelemetry {
                path,
                latency_us,
                fallback_reason,
                fallback_count: 0,
                rewrite_rule,
                rewrite_expected_cost_delta: rewrite_decision.expected_cost_delta,
                rewrite_observed_cost_delta,
                rewrite_fallback_reason,
                superinstruction_trace_signature: superinstruction.trace_signature,
                superinstruction_plan_id: superinstruction.plan_id,
                superinstruction_expected_cost_delta: superinstruction.expected_cost_delta,
                superinstruction_observed_cost_delta: superinstruction.observed_cost_delta,
                superinstruction_deopt_reason: superinstruction.deopt_reason,
                superinstruction_jit_hit: superinstruction.jit_hit,
                superinstruction_jit_cost_delta: superinstruction.jit_cost_delta,
            },
        }
    }

    fn hash_fast_opcode(&self, opcode: CommonHostcallOpcode) -> Option<(String, String)> {
        if let Some(tool_name) = hostcall_tool_opcode_name(opcode) {
            return self.hash_fast_tool_payload(tool_name);
        }
        if let Some(op_value) = hostcall_opcode_param_op(opcode) {
            return self.hash_fast_op_only_payload(op_value);
        }
        None
    }

    fn hash_fast_tool_payload(&self, expected_name: &str) -> Option<(String, String)> {
        use sha2::Digest as _;

        let map = self.params.as_object()?;
        if map.len() != 2 {
            return None;
        }
        let name = map.get("name").and_then(Value::as_str)?;
        if !token_eq_ascii_folded(name, expected_name) {
            return None;
        }
        let input = map.get("input")?;
        let mut hasher = sha2::Sha256::new();
        hash_hostcall_envelope(self.method, br#","params":"#, &mut hasher, |h| {
            h.update(b"{");
            hash_json_escaped_str("input", h);
            h.update(b":");
            hash_canonical_json(input, h);
            h.update(b",");
            hash_json_escaped_str("name", h);
            h.update(b":");
            hash_json_escaped_str(expected_name, h);
            h.update(b"}");
        });
        let params_hash = sha256_to_hex(hasher.finalize().as_slice());

        let mut shape_hasher = sha2::Sha256::new();
        hash_hostcall_envelope(
            self.method,
            br#","params_shape":"#,
            &mut shape_hasher,
            |h| {
                h.update(b"{");
                hash_json_escaped_str("input", h);
                h.update(b":");
                hash_canonical_shape(input, h);
                h.update(b",");
                hash_json_escaped_str("name", h);
                h.update(b":");
                h.update(br#""string""#);
                h.update(b"}");
            },
        );
        let args_shape_hash = sha256_to_hex(shape_hasher.finalize().as_slice());
        Some((params_hash, args_shape_hash))
    }

    fn hash_fast_op_only_payload(&self, expected_op: &str) -> Option<(String, String)> {
        use sha2::Digest as _;

        let map = self.params.as_object()?;
        if map.len() != 1 {
            return None;
        }
        let op = map.get("op").and_then(Value::as_str)?;
        if !token_eq_ascii_folded(op, expected_op) {
            return None;
        }
        let mut hasher = sha2::Sha256::new();
        hash_hostcall_envelope(self.method, br#","params":"#, &mut hasher, |h| {
            h.update(b"{");
            hash_json_escaped_str("op", h);
            h.update(b":");
            hash_json_escaped_str(expected_op, h);
            h.update(b"}");
        });
        let params_hash = sha256_to_hex(hasher.finalize().as_slice());

        let mut shape_hasher = sha2::Sha256::new();
        hash_hostcall_envelope(
            self.method,
            br#","params_shape":"#,
            &mut shape_hasher,
            |h| {
                h.update(b"{");
                hash_json_escaped_str("op", h);
                h.update(b":");
                h.update(br#""string""#);
                h.update(b"}");
            },
        );
        let args_shape_hash = sha256_to_hex(shape_hasher.finalize().as_slice());
        Some((params_hash, args_shape_hash))
    }
}

const fn hostcall_tool_opcode_name(opcode: CommonHostcallOpcode) -> Option<&'static str> {
    match opcode {
        CommonHostcallOpcode::ToolRead => Some("read"),
        CommonHostcallOpcode::ToolWrite => Some("write"),
        CommonHostcallOpcode::ToolEdit => Some("edit"),
        CommonHostcallOpcode::ToolBash => Some("bash"),
        _ => None,
    }
}

const fn hostcall_opcode_param_op(opcode: CommonHostcallOpcode) -> Option<&'static str> {
    match opcode {
        CommonHostcallOpcode::SessionGetState => Some("get_state"),
        CommonHostcallOpcode::SessionGetMessages => Some("get_messages"),
        CommonHostcallOpcode::SessionGetEntries => Some("get_entries"),
        CommonHostcallOpcode::SessionGetBranch => Some("get_branch"),
        CommonHostcallOpcode::SessionGetFile => Some("get_file"),
        CommonHostcallOpcode::SessionGetName => Some("get_name"),
        CommonHostcallOpcode::SessionGetModel | CommonHostcallOpcode::EventsGetModel => {
            Some("get_model")
        }
        CommonHostcallOpcode::SessionGetThinkingLevel
        | CommonHostcallOpcode::EventsGetThinkingLevel => Some("get_thinking_level"),
        CommonHostcallOpcode::EventsGetActiveTools => Some("get_active_tools"),
        CommonHostcallOpcode::EventsGetAllTools => Some("get_all_tools"),
        CommonHostcallOpcode::EventsList => Some("list"),
        CommonHostcallOpcode::EventsListFlags => Some("list_flags"),
        _ => None,
    }
}

// ============================================================================
// Shared Hostcall Dispatch (bd-1uy.1.3)
// ============================================================================

/// Map a string error code to the taxonomy enum, defaulting to `Internal`.
pub(super) fn parse_error_code(code: &str) -> HostCallErrorCode {
    match code {
        "timeout" => HostCallErrorCode::Timeout,
        "denied" => HostCallErrorCode::Denied,
        "io" => HostCallErrorCode::Io,
        "invalid_request" => HostCallErrorCode::InvalidRequest,
        _ => HostCallErrorCode::Internal,
    }
}

/// Convert a taxonomy error code to its string representation.
pub(super) const fn host_call_error_code_str(code: HostCallErrorCode) -> &'static str {
    match code {
        HostCallErrorCode::Timeout => "timeout",
        HostCallErrorCode::Denied => "denied",
        HostCallErrorCode::Io => "io",
        HostCallErrorCode::InvalidRequest => "invalid_request",
        HostCallErrorCode::Internal => "internal",
    }
}

fn token_eq_ascii_folded(left: &str, right: &str) -> bool {
    let mut left_iter = left
        .trim()
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|b| b.to_ascii_lowercase());
    let mut right_iter = right
        .trim()
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|b| b.to_ascii_lowercase());
    loop {
        match (left_iter.next(), right_iter.next()) {
            (Some(left_b), Some(right_b)) if left_b == right_b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

#[inline]
pub(super) fn with_folded_ascii_alnum_token<T>(token: &str, f: impl FnOnce(&[u8]) -> T) -> T {
    const INLINE_CAP: usize = 64;
    let mut inline = [0_u8; INLINE_CAP];
    let mut inline_len = 0_usize;
    let mut heap: Option<Vec<u8>> = None;

    for byte in token.trim().bytes() {
        if !byte.is_ascii_alphanumeric() {
            continue;
        }
        let folded = byte.to_ascii_lowercase();
        if let Some(buf) = heap.as_mut() {
            buf.push(folded);
            continue;
        }
        if inline_len < INLINE_CAP {
            inline[inline_len] = folded;
            inline_len += 1;
        } else {
            let mut buf = Vec::with_capacity(token.len());
            buf.extend_from_slice(&inline[..inline_len]);
            buf.push(folded);
            heap = Some(buf);
        }
    }

    if let Some(buf) = heap {
        f(buf.as_slice())
    } else {
        f(&inline[..inline_len])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostcallMethodAtom {
    Tool,
    Session,
    Events,
    Unknown,
}

fn intern_hostcall_method_atom(method: &str) -> HostcallMethodAtom {
    with_folded_ascii_alnum_token(method, |folded| match folded {
        b"tool" => HostcallMethodAtom::Tool,
        b"session" => HostcallMethodAtom::Session,
        b"events" => HostcallMethodAtom::Events,
        _ => HostcallMethodAtom::Unknown,
    })
}

fn hostcall_param_op(params: &Value) -> Option<&str> {
    params
        .get("op")
        .or_else(|| params.get("method"))
        .or_else(|| params.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(super) fn parse_common_hostcall_opcode_code(code: &str) -> Option<CommonHostcallOpcode> {
    match code.trim() {
        "tool.read" => Some(CommonHostcallOpcode::ToolRead),
        "tool.write" => Some(CommonHostcallOpcode::ToolWrite),
        "tool.edit" => Some(CommonHostcallOpcode::ToolEdit),
        "tool.bash" => Some(CommonHostcallOpcode::ToolBash),
        "session.get_state" => Some(CommonHostcallOpcode::SessionGetState),
        "session.get_messages" => Some(CommonHostcallOpcode::SessionGetMessages),
        "session.get_entries" => Some(CommonHostcallOpcode::SessionGetEntries),
        "session.get_branch" => Some(CommonHostcallOpcode::SessionGetBranch),
        "session.get_file" => Some(CommonHostcallOpcode::SessionGetFile),
        "session.get_name" => Some(CommonHostcallOpcode::SessionGetName),
        "session.set_name" => Some(CommonHostcallOpcode::SessionSetName),
        "session.get_model" => Some(CommonHostcallOpcode::SessionGetModel),
        "session.set_model" => Some(CommonHostcallOpcode::SessionSetModel),
        "session.get_thinking_level" => Some(CommonHostcallOpcode::SessionGetThinkingLevel),
        "session.set_thinking_level" => Some(CommonHostcallOpcode::SessionSetThinkingLevel),
        "session.set_label" => Some(CommonHostcallOpcode::SessionSetLabel),
        "events.get_active_tools" => Some(CommonHostcallOpcode::EventsGetActiveTools),
        "events.get_all_tools" => Some(CommonHostcallOpcode::EventsGetAllTools),
        "events.set_active_tools" => Some(CommonHostcallOpcode::EventsSetActiveTools),
        "events.emit" => Some(CommonHostcallOpcode::EventsEmit),
        "events.list" => Some(CommonHostcallOpcode::EventsList),
        "events.get_model" => Some(CommonHostcallOpcode::EventsGetModel),
        "events.set_model" => Some(CommonHostcallOpcode::EventsSetModel),
        "events.get_thinking_level" => Some(CommonHostcallOpcode::EventsGetThinkingLevel),
        "events.set_thinking_level" => Some(CommonHostcallOpcode::EventsSetThinkingLevel),
        "events.get_flag" => Some(CommonHostcallOpcode::EventsGetFlag),
        "events.list_flags" => Some(CommonHostcallOpcode::EventsListFlags),
        "events.append_entry" => Some(CommonHostcallOpcode::EventsAppendEntry),
        "events.register_command" => Some(CommonHostcallOpcode::EventsRegisterCommand),
        _ => None,
    }
}

fn parse_tool_opcode_atom(name: &str) -> Option<CommonHostcallOpcode> {
    with_folded_ascii_alnum_token(name, |folded| match folded {
        b"read" => Some(CommonHostcallOpcode::ToolRead),
        b"write" => Some(CommonHostcallOpcode::ToolWrite),
        b"edit" => Some(CommonHostcallOpcode::ToolEdit),
        b"bash" => Some(CommonHostcallOpcode::ToolBash),
        _ => None,
    })
}

pub(super) fn parse_session_opcode_atom(op: &str) -> Option<CommonHostcallOpcode> {
    with_folded_ascii_alnum_token(op, |folded| match folded {
        b"getstate" => Some(CommonHostcallOpcode::SessionGetState),
        b"getmessages" => Some(CommonHostcallOpcode::SessionGetMessages),
        b"getentries" => Some(CommonHostcallOpcode::SessionGetEntries),
        b"getbranch" => Some(CommonHostcallOpcode::SessionGetBranch),
        b"getfile" => Some(CommonHostcallOpcode::SessionGetFile),
        b"getname" => Some(CommonHostcallOpcode::SessionGetName),
        b"setname" => Some(CommonHostcallOpcode::SessionSetName),
        b"getmodel" => Some(CommonHostcallOpcode::SessionGetModel),
        b"setmodel" => Some(CommonHostcallOpcode::SessionSetModel),
        b"getthinkinglevel" => Some(CommonHostcallOpcode::SessionGetThinkingLevel),
        b"setthinkinglevel" => Some(CommonHostcallOpcode::SessionSetThinkingLevel),
        b"setlabel" => Some(CommonHostcallOpcode::SessionSetLabel),
        _ => None,
    })
}

fn parse_events_opcode_atom(op: &str) -> Option<CommonHostcallOpcode> {
    with_folded_ascii_alnum_token(op, |folded| match folded {
        b"getactivetools" => Some(CommonHostcallOpcode::EventsGetActiveTools),
        b"getalltools" => Some(CommonHostcallOpcode::EventsGetAllTools),
        b"setactivetools" => Some(CommonHostcallOpcode::EventsSetActiveTools),
        b"emit" => Some(CommonHostcallOpcode::EventsEmit),
        b"list" => Some(CommonHostcallOpcode::EventsList),
        b"getmodel" => Some(CommonHostcallOpcode::EventsGetModel),
        b"setmodel" => Some(CommonHostcallOpcode::EventsSetModel),
        b"getthinkinglevel" => Some(CommonHostcallOpcode::EventsGetThinkingLevel),
        b"setthinkinglevel" => Some(CommonHostcallOpcode::EventsSetThinkingLevel),
        b"getflag" => Some(CommonHostcallOpcode::EventsGetFlag),
        b"listflags" => Some(CommonHostcallOpcode::EventsListFlags),
        b"appendentry" => Some(CommonHostcallOpcode::EventsAppendEntry),
        b"registercommand" => Some(CommonHostcallOpcode::EventsRegisterCommand),
        _ => None,
    })
}

fn derive_common_hostcall_opcode(method: &str, params: &Value) -> Option<CommonHostcallOpcode> {
    match intern_hostcall_method_atom(method) {
        HostcallMethodAtom::Tool => params
            .get("name")
            .and_then(Value::as_str)
            .and_then(parse_tool_opcode_atom),
        HostcallMethodAtom::Session => {
            hostcall_param_op(params).and_then(parse_session_opcode_atom)
        }
        HostcallMethodAtom::Events => hostcall_param_op(params).and_then(parse_events_opcode_atom),
        HostcallMethodAtom::Unknown => None,
    }
}

fn parse_opcode_from_context(call: &HostCallPayload) -> Result<Option<CommonHostcallOpcode>> {
    let Some(context) = call.context.as_ref() else {
        return Ok(None);
    };
    let Some(context_obj) = context.as_object() else {
        return Err(Error::validation(
            "host_call context must be an object when typed opcode metadata is provided",
        ));
    };
    let Some(opcode_meta) = context_obj.get(HOSTCALL_OPCODE_CONTEXT_KEY) else {
        return Ok(None);
    };
    let Some(meta_obj) = opcode_meta.as_object() else {
        return Err(Error::validation(
            "host_call context.typed_opcode must be an object",
        ));
    };

    let Some(schema) = meta_obj
        .get("schema")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Err(Error::validation(
            "host_call context.typed_opcode.schema is required",
        ));
    };
    if schema != HOSTCALL_OPCODE_SCHEMA_VERSION {
        return Err(Error::validation(format!(
            "Unsupported host_call typed opcode schema: {schema}"
        )));
    }

    let Some(version) = meta_obj.get("version").and_then(Value::as_u64) else {
        return Err(Error::validation(
            "host_call context.typed_opcode.version is required",
        ));
    };
    if version != u64::from(HOSTCALL_OPCODE_VERSION) {
        return Err(Error::validation(format!(
            "Unsupported host_call typed opcode version: {version}"
        )));
    }

    let Some(code) = meta_obj
        .get("code")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Err(Error::validation(
            "host_call context.typed_opcode.code is required",
        ));
    };
    let Some(opcode) = parse_common_hostcall_opcode_code(code) else {
        return Err(Error::validation(format!(
            "Unknown host_call typed opcode code: {code}"
        )));
    };
    Ok(Some(opcode))
}

pub(super) fn resolve_hostcall_opcode(call: &HostCallPayload) -> Result<HostcallOpcodeResolution> {
    if let Some(opcode) = parse_opcode_from_context(call)? {
        let Some(derived) = derive_common_hostcall_opcode(&call.method, &call.params) else {
            return Err(Error::validation(format!(
                "host_call typed opcode '{}' is not compatible with method '{}'",
                opcode.code(),
                call.method
            )));
        };
        if derived != opcode {
            return Err(Error::validation(format!(
                "host_call typed opcode '{}' does not match payload-derived opcode '{}'",
                opcode.code(),
                derived.code()
            )));
        }
        return Ok(HostcallOpcodeResolution::FastPath {
            opcode,
            source: HostcallOpcodeSource::ContextV1,
        });
    }

    if let Some(opcode) = derive_common_hostcall_opcode(&call.method, &call.params) {
        return Ok(HostcallOpcodeResolution::FastPath {
            opcode,
            source: HostcallOpcodeSource::DerivedV1,
        });
    }

    Ok(HostcallOpcodeResolution::Fallback {
        reason: "opcode_not_declared_or_not_supported",
    })
}

pub(super) fn hostcall_capability_class_from_capability(capability: &str) -> &'static str {
    match capability {
        "read" | "write" => "filesystem",
        "exec" => "execution",
        "env" => "environment",
        "http" => "network",
        "session" => "session",
        "events" => "events",
        "ui" => "ui",
        "log" => "telemetry",
        "tool" => "tool",
        _ => "unknown",
    }
}

fn hostcall_capability_class(call: &HostCallPayload) -> &'static str {
    let capability = call.capability.trim().to_ascii_lowercase();
    hostcall_capability_class_from_capability(capability.as_str())
}

fn fallback_lane_matrix_key(
    call: &HostCallPayload,
    capability_class: &'static str,
) -> &'static str {
    let method = call.method.trim().to_ascii_lowercase();
    match (method.as_str(), capability_class) {
        ("tool", "tool") => "tool|fallback|tool",
        ("tool", "filesystem") => "tool|fallback|filesystem",
        ("tool", "execution") => "tool|fallback|execution",
        ("fs", "filesystem") => "fs|fallback|filesystem",
        ("exec", "execution") => "exec|fallback|execution",
        ("env", "environment") => "env|fallback|environment",
        ("http", "network") => "http|fallback|network",
        ("session", "session") => "session|fallback|session",
        ("events", "events") => "events|fallback|events",
        ("ui", "ui") => "ui|fallback|ui",
        ("log", "telemetry") => "log|fallback|telemetry",
        _ => "unknown|fallback|unknown",
    }
}

pub(super) fn select_hostcall_lane(call: &HostCallPayload) -> Result<HostcallLaneDecision> {
    let declared_capability = call.capability.trim().to_ascii_lowercase();
    if declared_capability.is_empty() {
        return Err(Error::validation("Host call capability is empty"));
    }
    match resolve_hostcall_opcode(call)? {
        HostcallOpcodeResolution::FastPath { opcode, source } => {
            let required = opcode.required_capability();
            if declared_capability != required {
                return Err(Error::validation(format!(
                    "Host call capability mismatch: declared {declared_capability}, required \
                     {required}"
                )));
            }
            Ok(HostcallLaneDecision {
                lane: HostcallDispatchLane::Fast,
                reason: match source {
                    HostcallOpcodeSource::ContextV1 => "typed_opcode_context_v1",
                    HostcallOpcodeSource::DerivedV1 => "typed_opcode_derived_v1",
                },
                opcode: Some(opcode),
                capability_class: opcode.capability_class(),
                matrix_key: opcode.lane_matrix_key(),
            })
        }
        HostcallOpcodeResolution::Fallback { reason } => {
            if let Some(required) = required_capability_for_host_call_static_legacy(call)
                && declared_capability != required
            {
                return Err(Error::validation(format!(
                    "Host call capability mismatch: declared {declared_capability}, required \
                     {required}"
                )));
            }
            let capability_class = hostcall_capability_class(call);
            Ok(HostcallLaneDecision {
                lane: HostcallDispatchLane::Compat,
                reason,
                opcode: None,
                capability_class,
                matrix_key: fallback_lane_matrix_key(call, capability_class),
            })
        }
    }
}

pub(super) fn apply_hostcall_lane_kill_switch(
    ctx: &HostCallContext<'_>,
    call: &HostCallPayload,
    lane: HostcallLaneDecision,
) -> HostcallLaneDecision {
    if lane.lane == HostcallDispatchLane::Compat {
        return lane;
    }
    let Some(manager) = ctx.manager.as_ref() else {
        return lane;
    };
    let Some(reason) = manager.hostcall_compat_kill_switch_reason(ctx.extension_id) else {
        return lane;
    };
    let capability_class = hostcall_capability_class(call);
    HostcallLaneDecision {
        lane: HostcallDispatchLane::Compat,
        reason,
        opcode: None,
        capability_class,
        matrix_key: fallback_lane_matrix_key(call, capability_class),
    }
}

pub(super) fn hostcall_opcode_context_for_params(method: &str, params: &Value) -> Option<Value> {
    let opcode = derive_common_hostcall_opcode(method, params)?;
    Some(json!({
        HOSTCALL_OPCODE_CONTEXT_KEY: {
            "schema": HOSTCALL_OPCODE_SCHEMA_VERSION,
            "version": HOSTCALL_OPCODE_VERSION,
            "code": opcode.code(),
        }
    }))
}

pub(super) fn hostcall_io_uring_context_for_request(request: &HostcallRequest) -> Value {
    json!({
        HOSTCALL_IO_URING_CONTEXT_KEY: {
            "schema": HOSTCALL_IO_URING_CONTEXT_SCHEMA_VERSION,
            "capability_class": request.io_uring_capability_class(),
            "io_hint": request.io_uring_io_hint(),
        }
    })
}

pub(super) fn merge_hostcall_context(base: Option<Value>, extra: Value) -> Option<Value> {
    let mut merged = serde_json::Map::new();
    if let Some(Value::Object(base_obj)) = base {
        merged.extend(base_obj);
    }
    if let Value::Object(extra_obj) = extra {
        merged.extend(extra_obj);
    }
    if merged.is_empty() {
        None
    } else {
        Some(Value::Object(merged))
    }
}

pub(super) fn params_without_key(params: &Value, key: &str) -> Value {
    if let Value::Object(map) = params {
        let mut out = map.clone();
        out.remove(key);
        Value::Object(out)
    } else {
        Value::Null
    }
}

// ============================================================================
// Hostcall Reactor Mesh (bd-3ar8v.4.20)
// ============================================================================

pub(super) const HOSTCALL_REACTOR_DEFAULT_LANE_CAPACITY: usize = 256;
const HOSTCALL_REACTOR_MAX_SHARDS: usize = 64;
const HOSTCALL_REACTOR_MAX_LANE_CAPACITY: usize = 4096;
const HOSTCALL_REACTOR_PRESSURE_NUMERATOR: usize = 3;
const HOSTCALL_REACTOR_PRESSURE_DENOMINATOR: usize = 4;
const HOSTCALL_REACTOR_LATENCY_WINDOW: usize = 128;

fn hostcall_reactor_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

impl HostcallReactorConfig {
    /// Size a reactor from the current host parallelism with no prior pressure data.
    #[must_use]
    pub fn auto_sized() -> Self {
        Self::auto_sized_for(
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            None,
        )
    }

    /// Size a reactor from available parallelism and optional queue-pressure telemetry.
    #[must_use]
    pub fn auto_sized_for(
        parallelism: usize,
        telemetry: Option<&HostcallReactorTelemetry>,
    ) -> Self {
        let parallelism = parallelism.max(1);
        let max_shards = parallelism.clamp(1, HOSTCALL_REACTOR_MAX_SHARDS);
        let base_shards = parallelism.div_ceil(2).clamp(1, max_shards);
        let mut shard_count = base_shards;
        let mut lane_capacity = HOSTCALL_REACTOR_DEFAULT_LANE_CAPACITY;

        if let Some(telemetry) = telemetry {
            let observed_capacity = telemetry.lane_capacity.max(1);
            lane_capacity = observed_capacity.clamp(
                HOSTCALL_REACTOR_DEFAULT_LANE_CAPACITY,
                HOSTCALL_REACTOR_MAX_LANE_CAPACITY,
            );
            let max_depth = telemetry
                .max_queue_depths
                .iter()
                .copied()
                .max()
                .unwrap_or(0);
            let pressure = telemetry.rejected_enqueues > 0
                || max_depth.saturating_mul(HOSTCALL_REACTOR_PRESSURE_DENOMINATOR)
                    >= observed_capacity.saturating_mul(HOSTCALL_REACTOR_PRESSURE_NUMERATOR);

            if pressure {
                shard_count = max_shards;
                lane_capacity = observed_capacity.saturating_mul(2).clamp(
                    HOSTCALL_REACTOR_DEFAULT_LANE_CAPACITY,
                    HOSTCALL_REACTOR_MAX_LANE_CAPACITY,
                );
            }
        }

        Self {
            shard_count,
            lane_capacity,
            core_ids: None,
        }
    }
}

impl Default for HostcallReactorConfig {
    fn default() -> Self {
        Self::auto_sized()
    }
}

/// Per-shard bounded SPSC lane for hostcall requests.
#[derive(Debug)]
pub(super) struct HostcallSpscLane {
    capacity: usize,
    queue: std::collections::VecDeque<HostcallReactorRequest>,
    max_depth: usize,
    total_enqueued: u64,
    dispatch_latency_ns_samples: std::collections::VecDeque<u64>,
}

impl HostcallSpscLane {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: std::collections::VecDeque::with_capacity(capacity),
            max_depth: 0,
            total_enqueued: 0,
            dispatch_latency_ns_samples: std::collections::VecDeque::with_capacity(
                HOSTCALL_REACTOR_LATENCY_WINDOW,
            ),
        }
    }

    fn len(&self) -> usize {
        self.queue.len()
    }

    fn push(&mut self, req: HostcallReactorRequest) -> std::result::Result<(), usize> {
        if self.queue.len() >= self.capacity {
            return Err(self.queue.len());
        }
        self.queue.push_back(req);
        self.max_depth = self.max_depth.max(self.queue.len());
        self.total_enqueued = self.total_enqueued.saturating_add(1);
        Ok(())
    }

    fn record_latency_ns(&mut self, latency_ns: u64) {
        if self.dispatch_latency_ns_samples.len() >= HOSTCALL_REACTOR_LATENCY_WINDOW {
            self.dispatch_latency_ns_samples.pop_front();
        }
        self.dispatch_latency_ns_samples.push_back(latency_ns);
    }

    fn pop_recording_latency(&mut self, now_ns: u64) -> Option<HostcallReactorRequest> {
        let req = self.queue.pop_front()?;
        self.record_latency_ns(now_ns.saturating_sub(req.enqueued_at_ns));
        Some(req)
    }

    fn record_completion(&mut self, global_seq: u64, now_ns: u64) -> bool {
        let Some(pos) = self
            .queue
            .iter()
            .position(|req| req.global_seq == global_seq)
        else {
            return false;
        };
        let Some(req) = self.queue.remove(pos) else {
            return false;
        };
        self.record_latency_ns(now_ns.saturating_sub(req.enqueued_at_ns));
        true
    }

    fn drain_batch(&mut self, budget: usize, now_ns: u64) -> Vec<HostcallReactorRequest> {
        let n = budget.min(self.queue.len());
        let mut batch = Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(req) = self.pop_recording_latency(now_ns) {
                batch.push(req);
            }
        }
        batch
    }

    fn latency_percentile_ns(&self, percentile_bps: usize) -> u64 {
        if self.dispatch_latency_ns_samples.is_empty() {
            return 0;
        }
        let mut samples: Vec<u64> = self.dispatch_latency_ns_samples.iter().copied().collect();
        samples.sort_unstable();
        let idx = samples
            .len()
            .saturating_sub(1)
            .saturating_mul(percentile_bps)
            .div_ceil(10_000)
            .min(samples.len().saturating_sub(1));
        samples[idx]
    }
}

impl HostcallReactorMesh {
    /// Create a new reactor mesh with the given configuration.
    #[must_use]
    #[allow(clippy::option_if_let_else)]
    pub fn new(config: HostcallReactorConfig) -> Self {
        if config.shard_count == 0 || config.lane_capacity == 0 {
            return Self {
                config: HostcallReactorConfig {
                    shard_count: 0,
                    lane_capacity: 0,
                    core_ids: None,
                },
                lanes: Vec::new(),
                shard_seq: Vec::new(),
                global_seq: 0,
                rr_cursor: 0,
                rejected_enqueues: 0,
                total_dispatched: 0,
                numa_pool: None,
                affinity_advice: Vec::new(),
            };
        }

        let shard_count = config.shard_count;
        let lane_capacity = config.lane_capacity;
        let lanes = (0..shard_count)
            .map(|_| HostcallSpscLane::new(lane_capacity))
            .collect();

        // Build NUMA slab pool and thread affinity advice from core_ids if configured.
        let (numa_pool, affinity_advice) = if let Some(ref core_ids) = config.core_ids {
            use crate::scheduler::{
                AffinityEnforcement, NumaSlabConfig, NumaSlabPool, ReactorPlacementManifest,
                ReactorShardBinding,
            };
            let bindings: Vec<ReactorShardBinding> = core_ids
                .iter()
                .enumerate()
                .map(|(shard_id, &core_id)| ReactorShardBinding {
                    shard_id,
                    core_id,
                    numa_node: core_id / 4, // heuristic: 4 cores per NUMA node
                })
                .collect();
            let numa_node_count = bindings
                .iter()
                .map(|b| b.numa_node)
                .collect::<std::collections::HashSet<_>>()
                .len()
                .max(1);
            let manifest = ReactorPlacementManifest {
                shard_count,
                numa_node_count,
                bindings,
                fallback_reason: None,
            };
            let pool = NumaSlabPool::from_manifest(&manifest, NumaSlabConfig::default());
            let advice = manifest.affinity_advice(AffinityEnforcement::Advisory);

            tracing::debug!(
                shard_count,
                numa_nodes = pool.node_count(),
                affinity_entries = advice.len(),
                "Hostcall reactor mesh initialized with NUMA slab pool and affinity advice"
            );

            (Some(pool), advice)
        } else {
            (None, Vec::new())
        };

        Self {
            config: HostcallReactorConfig {
                shard_count,
                lane_capacity,
                ..config
            },
            lanes,
            shard_seq: vec![0; shard_count],
            global_seq: 0,
            rr_cursor: 0,
            rejected_enqueues: 0,
            total_dispatched: 0,
            numa_pool,
            affinity_advice,
        }
    }

    /// Number of shard lanes.
    #[must_use]
    pub const fn shard_count(&self) -> usize {
        self.lanes.len()
    }

    /// Total pending requests across all shards.
    #[must_use]
    pub fn total_depth(&self) -> usize {
        self.lanes.iter().map(HostcallSpscLane::len).sum()
    }

    /// Whether any lane has pending requests.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.total_depth() > 0
    }

    /// Snapshot queueing telemetry.
    #[must_use]
    pub fn telemetry(&self) -> HostcallReactorTelemetry {
        let overload_reason = if self.rejected_enqueues > 0 {
            Some("rejected_enqueues".to_string())
        } else if self.lanes.iter().any(|lane| {
            lane.capacity > 0
                && lane
                    .max_depth
                    .saturating_mul(HOSTCALL_REACTOR_PRESSURE_DENOMINATOR)
                    >= lane
                        .capacity
                        .saturating_mul(HOSTCALL_REACTOR_PRESSURE_NUMERATOR)
        }) {
            Some("queue_pressure".to_string())
        } else {
            None
        };
        HostcallReactorTelemetry {
            shard_count: self.lanes.len(),
            lane_capacity: self.config.lane_capacity,
            queue_depths: self.lanes.iter().map(HostcallSpscLane::len).collect(),
            max_queue_depths: self.lanes.iter().map(|l| l.max_depth).collect(),
            total_enqueued: self.lanes.iter().map(|l| l.total_enqueued).collect(),
            rejected_enqueues: self.rejected_enqueues,
            total_dispatched: self.total_dispatched,
            lane_dispatch_latency_p95_ns: self
                .lanes
                .iter()
                .map(|lane| lane.latency_percentile_ns(9_500))
                .collect(),
            lane_dispatch_latency_p99_ns: self
                .lanes
                .iter()
                .map(|lane| lane.latency_percentile_ns(9_900))
                .collect(),
            overloaded: overload_reason.is_some(),
            overload_reason,
            numa_pool_active: self.numa_pool.is_some(),
            affinity_advisory_count: self.affinity_advice.len(),
        }
    }

    /// Core affinity configuration (if any).
    #[must_use]
    pub fn core_id_for_shard(&self, shard_id: usize) -> Option<usize> {
        self.config
            .core_ids
            .as_ref()
            .and_then(|ids| ids.get(shard_id).copied())
    }

    /// NUMA-aware slab pool (if core-ids were configured).
    #[must_use]
    pub const fn numa_pool(&self) -> Option<&crate::scheduler::NumaSlabPool> {
        self.numa_pool.as_ref()
    }

    /// Thread affinity advice derived from reactor core mapping.
    #[must_use]
    pub fn affinity_advice(&self) -> &[crate::scheduler::ThreadAffinityAdvice] {
        &self.affinity_advice
    }

    /// FNV-1a 64-bit hash for deterministic, process-independent routing.
    fn stable_hash(input: &str) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in input.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3_u64);
        }
        hash
    }

    /// Route a hostcall by call_id hash (shard affinity).
    fn hash_route(&self, call_id: &str) -> usize {
        if self.lanes.len() <= 1 {
            return 0;
        }
        let lanes = u64::try_from(self.lanes.len()).unwrap_or(1);
        usize::try_from(Self::stable_hash(call_id) % lanes).unwrap_or(0)
    }

    /// Route using round-robin for load distribution.
    const fn rr_route(&mut self) -> usize {
        if self.lanes.len() <= 1 {
            return 0;
        }
        let idx = self.rr_cursor % self.lanes.len();
        self.rr_cursor = self.rr_cursor.saturating_add(1);
        idx
    }

    /// Select shard based on opcode class:
    /// - Session + Tool opcodes: hash by call_id (affinity)
    /// - Events opcodes: round-robin (load distribution)
    fn route_for_opcode(&mut self, opcode: CommonHostcallOpcode, call_id: &str) -> usize {
        match opcode.method() {
            "events" => self.rr_route(),
            _ => self.hash_route(call_id),
        }
    }

    const fn next_global_seq(&mut self) -> u64 {
        let seq = self.global_seq;
        self.global_seq = self.global_seq.saturating_add(1);
        seq
    }

    fn next_shard_seq(&mut self, shard_id: usize) -> u64 {
        let Some(seq) = self.shard_seq.get_mut(shard_id) else {
            return 0;
        };
        let current = *seq;
        *seq = seq.saturating_add(1);
        current
    }

    /// Enqueue a fast-lane hostcall request for shard-local dispatch.
    ///
    /// Returns the shard assignment and sequence metadata on success,
    /// or backpressure info on lane overflow.
    pub(crate) fn submit(
        &mut self,
        call_id: String,
        opcode: CommonHostcallOpcode,
        params: Value,
    ) -> std::result::Result<HostcallReactorRequest, HostcallReactorBackpressure> {
        let shard_id = self.route_for_opcode(opcode, &call_id);
        let global_seq = self.next_global_seq();
        let shard_seq = self.next_shard_seq(shard_id);
        let now_ns = hostcall_reactor_now_ns();

        let request = HostcallReactorRequest {
            call_id,
            opcode,
            params,
            shard_id,
            shard_seq,
            global_seq,
            enqueued_at_ns: now_ns,
        };

        let Some(lane) = self.lanes.get_mut(shard_id) else {
            self.rejected_enqueues = self.rejected_enqueues.saturating_add(1);
            return Err(HostcallReactorBackpressure {
                shard_id,
                depth: 0,
                capacity: 0,
            });
        };
        match lane.push(request.clone()) {
            Ok(()) => Ok(request),
            Err(depth) => {
                self.rejected_enqueues = self.rejected_enqueues.saturating_add(1);
                Err(HostcallReactorBackpressure {
                    shard_id,
                    depth,
                    capacity: lane.capacity,
                })
            }
        }
    }

    /// Drain up to `budget` requests from a specific shard.
    pub fn drain_shard(&mut self, shard_id: usize, budget: usize) -> Vec<HostcallReactorRequest> {
        let Some(lane) = self.lanes.get_mut(shard_id) else {
            return Vec::new();
        };
        let batch = lane.drain_batch(budget, hostcall_reactor_now_ns());
        self.total_dispatched = self
            .total_dispatched
            .saturating_add(u64::try_from(batch.len()).unwrap_or(0));
        batch
    }

    /// Drain across all shards in deterministic global sequence order.
    pub fn drain_global_order(&mut self, budget: usize) -> Vec<HostcallReactorRequest> {
        let mut drained = Vec::with_capacity(budget);
        for _ in 0..budget {
            let mut best_lane: Option<usize> = None;
            let mut best_seq: Option<u64> = None;
            for (idx, lane) in self.lanes.iter().enumerate() {
                let Some(front) = lane.queue.front() else {
                    continue;
                };
                if best_seq.is_none_or(|seq| front.global_seq < seq) {
                    best_seq = Some(front.global_seq);
                    best_lane = Some(idx);
                }
            }
            let Some(lane_idx) = best_lane else {
                break;
            };
            if let Some(req) = self.lanes[lane_idx].pop_recording_latency(hostcall_reactor_now_ns())
            {
                drained.push(req);
            }
        }
        self.total_dispatched = self
            .total_dispatched
            .saturating_add(u64::try_from(drained.len()).unwrap_or(0));
        drained
    }

    /// Record that a batch of completions was produced by shard processing.
    pub const fn record_completions(&mut self, count: u64) {
        self.total_dispatched = self.total_dispatched.saturating_add(count);
    }

    /// Record completion of a directly dispatched hostcall and clear its queue slot.
    pub(crate) fn record_completion(&mut self, shard_id: usize, global_seq: u64) -> bool {
        let Some(lane) = self.lanes.get_mut(shard_id) else {
            return false;
        };
        let completed = lane.record_completion(global_seq, hostcall_reactor_now_ns());
        if completed {
            self.total_dispatched = self.total_dispatched.saturating_add(1);
        }
        completed
    }
}

// ============================================================================
// Extension UI + Session Bridge
// ============================================================================

impl ExtensionUiRequest {
    pub fn new(id: impl Into<String>, method: impl Into<String>, payload: Value) -> Self {
        Self {
            id: id.into(),
            method: method.into(),
            payload,
            timeout_ms: None,
            extension_id: None,
        }
    }

    /// Set the extension ID for provenance tracking.
    #[must_use]
    pub fn with_extension_id(mut self, ext_id: Option<String>) -> Self {
        self.extension_id = ext_id;
        self
    }

    pub fn expects_response(&self) -> bool {
        matches!(
            self.method.as_str(),
            "select"
                | "confirm"
                | "input"
                | "editor"
                | "custom"
                | "getEditorText"
                | "get_editor_text"
                | "getAllThemes"
                | "get_all_themes"
                | "getTheme"
                | "get_theme"
                | "setTheme"
                | "set_theme"
        )
    }

    pub fn effective_timeout_ms(&self) -> Option<u64> {
        self.timeout_ms.or_else(|| {
            self.payload
                .get("timeout")
                .and_then(serde_json::Value::as_u64)
        })
    }

    pub fn to_rpc_event(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "type".to_string(),
            Value::String("extension_ui_request".to_string()),
        );
        map.insert("id".to_string(), Value::String(self.id.clone()));
        map.insert("method".to_string(), Value::String(self.method.clone()));

        match &self.payload {
            Value::Object(obj) => {
                for (key, value) in obj {
                    map.insert(key.clone(), value.clone());
                }
            }
            other => {
                map.insert("payload".to_string(), other.clone());
            }
        }

        Value::Object(map)
    }
}

impl ExtensionDeliverAs {
    pub(super) fn parse(value: Option<&str>) -> Option<Self> {
        let value = value?.trim();
        if value.is_empty() {
            return None;
        }
        match value {
            "steer" => Some(Self::Steer),
            "followUp" | "follow_up" | "follow-up" => Some(Self::FollowUp),
            "nextTurn" | "next_turn" | "next-turn" => Some(Self::NextTurn),
            _ => None,
        }
    }
}

impl ExtensionMessage {
    pub fn parse_and_validate(json: &str) -> Result<Self> {
        let msg: Self = serde_json::from_str(json)?;
        msg.validate()?;
        Ok(msg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(Error::validation("Extension message id is empty"));
        }
        if self.version != PROTOCOL_VERSION {
            return Err(Error::validation(format!(
                "Unsupported extension protocol version: {}",
                self.version
            )));
        }

        match &self.body {
            ExtensionBody::Register(payload) => validate_register(payload),
            ExtensionBody::ToolCall(payload) => validate_tool_call(payload),
            ExtensionBody::ToolResult(payload) => validate_tool_result(payload),
            ExtensionBody::SlashCommand(payload) => validate_slash_command(payload),
            ExtensionBody::SlashResult(_) => Ok(()),
            ExtensionBody::EventHook(payload) => validate_event_hook(payload),
            ExtensionBody::HostCall(payload) => validate_host_call(payload),
            ExtensionBody::HostResult(payload) => validate_host_result(payload),
            ExtensionBody::Log(payload) => validate_log(payload),
            ExtensionBody::Error(payload) => validate_error(payload),
        }
    }
}

pub(super) fn validate_register(payload: &RegisterPayload) -> Result<()> {
    if payload.name.trim().is_empty() {
        return Err(Error::validation("Extension name is empty"));
    }
    if payload.version.trim().is_empty() {
        return Err(Error::validation("Extension version is empty"));
    }
    if payload.api_version.trim().is_empty() {
        return Err(Error::validation("Extension api_version is empty"));
    }

    if let Some(manifest) = &payload.capability_manifest {
        validate_capability_manifest(manifest)?;
    }
    Ok(())
}

fn capability_manifest_sha256_digest_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-fA-F0-9]{64}$").expect("regex"))
}

fn is_known_v2_capability(value: &str) -> bool {
    matches!(
        value,
        "read" | "write" | "exec" | "env" | "http" | "session" | "events" | "ui" | "log" | "tool"
    )
}

fn is_known_v2_intent(value: &str) -> bool {
    matches!(
        value,
        "file_read"
            | "file_write"
            | "process_exec"
            | "environment_access"
            | "network_egress"
            | "session_state_access"
            | "event_stream_access"
            | "ui_interaction"
            | "telemetry_logging"
    )
}

fn is_known_v2_connector_class(value: &str) -> bool {
    matches!(
        value,
        "tool" | "fs" | "exec" | "env" | "http" | "session" | "events" | "ui" | "log"
    )
}

fn is_known_v2_hostcall_class(value: &str) -> bool {
    matches!(
        value,
        "tool"
            | "exec"
            | "env"
            | "http"
            | "session"
            | "events"
            | "ui"
            | "log"
            | "fs.read"
            | "fs.write"
            | "fs.list"
            | "fs.stat"
            | "fs.mkdir"
            | "fs.delete"
    )
}

fn is_known_v2_risk_tier(value: &str) -> bool {
    matches!(value, "low" | "medium" | "high" | "critical")
}

fn is_known_v2_provenance_source(value: &str) -> bool {
    matches!(value, "npm" | "github" | "registry" | "local" | "builtin")
}

fn is_known_v2_publisher_verification(value: &str) -> bool {
    matches!(
        value,
        "unsigned" | "self_attested" | "key_attested" | "registry_attested"
    )
}

#[allow(clippy::too_many_lines)]
pub(super) fn validate_capability_manifest(manifest: &CapabilityManifest) -> Result<()> {
    match manifest.schema.as_str() {
        CAPABILITY_MANIFEST_SCHEMA_V1 => {
            for (idx, req) in manifest.capabilities.iter().enumerate() {
                if req.capability.trim().is_empty() {
                    return Err(Error::validation(format!(
                        "Capability manifest v1 entry {idx} includes empty capability"
                    )));
                }
                if !req.intents.is_empty()
                    || !req.connector_classes.is_empty()
                    || !req.hostcall_classes.is_empty()
                    || req.risk_tier.is_some()
                    || req.provenance.is_some()
                    || req
                        .scope
                        .as_ref()
                        .is_some_and(|scope| scope.allowed_tools.is_some())
                {
                    return Err(Error::validation(format!(
                        "Capability manifest v1 entry {idx} contains v2-only fields"
                    )));
                }
            }
            Ok(())
        }
        CAPABILITY_MANIFEST_SCHEMA_V2 => {
            for (idx, req) in manifest.capabilities.iter().enumerate() {
                let capability = req.capability.trim();
                if capability.is_empty() {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} includes empty capability"
                    )));
                }
                if !is_known_v2_capability(capability) {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} has unsupported capability '{capability}'"
                    )));
                }
                if !req.methods.is_empty() {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} must not include legacy methods"
                    )));
                }
                if req.intents.is_empty() {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} must include at least one intent"
                    )));
                }
                for intent in &req.intents {
                    let intent = intent.trim();
                    if intent.is_empty() || !is_known_v2_intent(intent) {
                        return Err(Error::validation(format!(
                            "Capability manifest v2 entry {idx} has unsupported intent '{intent}'"
                        )));
                    }
                }
                if req.connector_classes.is_empty() {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} must include at least one connector class"
                    )));
                }
                for class_name in &req.connector_classes {
                    let class_name = class_name.trim();
                    if class_name.is_empty() || !is_known_v2_connector_class(class_name) {
                        return Err(Error::validation(format!(
                            "Capability manifest v2 entry {idx} has unsupported connector class '{class_name}'"
                        )));
                    }
                }
                if req.hostcall_classes.is_empty() {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} must include at least one hostcall class"
                    )));
                }
                for class_name in &req.hostcall_classes {
                    let class_name = class_name.trim();
                    if class_name.is_empty() || !is_known_v2_hostcall_class(class_name) {
                        return Err(Error::validation(format!(
                            "Capability manifest v2 entry {idx} has unsupported hostcall class '{class_name}'"
                        )));
                    }
                }
                if let Some(risk_tier) = req.risk_tier.as_deref() {
                    let risk_tier = risk_tier.trim();
                    if risk_tier.is_empty() || !is_known_v2_risk_tier(risk_tier) {
                        return Err(Error::validation(format!(
                            "Capability manifest v2 entry {idx} has unsupported risk_tier '{risk_tier}'"
                        )));
                    }
                }
                let Some(provenance) = req.provenance.as_ref() else {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} is missing provenance"
                    )));
                };
                let source = provenance.source.trim();
                if source.is_empty() || !is_known_v2_provenance_source(source) {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} has unsupported provenance source '{source}'"
                    )));
                }
                let algorithm = provenance.integrity.algorithm.trim();
                if algorithm != "sha256" {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} has unsupported integrity algorithm '{algorithm}'"
                    )));
                }
                let digest = provenance.integrity.digest.trim();
                if !capability_manifest_sha256_digest_regex().is_match(digest) {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} has invalid integrity digest"
                    )));
                }
                let publisher_id = provenance.publisher.id.trim();
                if publisher_id.is_empty() {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} has empty publisher id"
                    )));
                }
                let verification = provenance.publisher.verification.trim();
                if verification.is_empty() || !is_known_v2_publisher_verification(verification) {
                    return Err(Error::validation(format!(
                        "Capability manifest v2 entry {idx} has unsupported publisher verification '{verification}'"
                    )));
                }
            }
            Ok(())
        }
        _ => Err(Error::validation(format!(
            "Unsupported capability manifest schema: {}",
            manifest.schema
        ))),
    }
}

fn validate_tool_call(payload: &ToolCallPayload) -> Result<()> {
    if payload.call_id.trim().is_empty() {
        return Err(Error::validation("Tool call_id is empty"));
    }
    if payload.name.trim().is_empty() {
        return Err(Error::validation("Tool name is empty"));
    }
    Ok(())
}

fn validate_tool_result(payload: &ToolResultPayload) -> Result<()> {
    if payload.call_id.trim().is_empty() {
        return Err(Error::validation("Tool result call_id is empty"));
    }
    Ok(())
}

pub fn validate_host_call(payload: &HostCallPayload) -> Result<()> {
    if payload.call_id.trim().is_empty() {
        return Err(Error::validation("Host call_id is empty"));
    }

    if !payload.params.is_object() {
        return Err(Error::validation("Host call params must be an object"));
    }

    let declared_capability = payload.capability.trim().to_ascii_lowercase();
    if declared_capability.is_empty() {
        return Err(Error::validation("Host call capability is empty"));
    }

    if payload.method.trim().is_empty() {
        return Err(Error::validation("Host call method is empty"));
    }

    let required = match resolve_hostcall_opcode(payload)? {
        HostcallOpcodeResolution::FastPath { opcode, .. } => opcode.required_capability(),
        HostcallOpcodeResolution::Fallback { .. } => {
            required_capability_for_host_call_static_legacy(payload).ok_or_else(|| {
                Error::validation(format!(
                    "Unknown or invalid host call method: {}",
                    payload.method
                ))
            })?
        }
    };

    if declared_capability != required {
        return Err(Error::validation(format!(
            "Host call capability mismatch: declared {declared_capability}, required {required}"
        )));
    }
    Ok(())
}

pub(super) fn validate_host_result(payload: &HostResultPayload) -> Result<()> {
    if payload.call_id.trim().is_empty() {
        return Err(Error::validation("Host result call_id is empty"));
    }
    if !payload.output.is_object() {
        return Err(Error::validation("Host result output must be an object"));
    }
    if payload.is_error {
        if payload.error.is_none() {
            return Err(Error::validation(
                "Host result marked is_error=true but error payload is missing",
            ));
        }
    } else if payload.error.is_some() {
        return Err(Error::validation(
            "Host result includes error payload but is_error=false",
        ));
    }
    Ok(())
}

fn validate_slash_command(payload: &SlashCommandPayload) -> Result<()> {
    if payload.name.trim().is_empty() {
        return Err(Error::validation("Slash command name is empty"));
    }
    Ok(())
}

fn validate_event_hook(payload: &EventHookPayload) -> Result<()> {
    if payload.event.trim().is_empty() {
        return Err(Error::validation("Event hook name is empty"));
    }
    Ok(())
}

pub(super) fn validate_log(payload: &LogPayload) -> Result<()> {
    if payload.schema != LOG_SCHEMA_VERSION {
        return Err(Error::validation(format!(
            "Unsupported log schema: {}",
            payload.schema
        )));
    }
    if payload.ts.trim().is_empty() {
        return Err(Error::validation("Log timestamp is empty"));
    }
    if payload.event.trim().is_empty() {
        return Err(Error::validation("Log event is empty"));
    }
    if payload.message.trim().is_empty() {
        return Err(Error::validation("Log message is empty"));
    }
    if payload.correlation.extension_id.trim().is_empty() {
        return Err(Error::validation("Log correlation extension_id is empty"));
    }
    if payload.correlation.scenario_id.trim().is_empty() {
        return Err(Error::validation("Log correlation scenario_id is empty"));
    }
    Ok(())
}

fn validate_error(payload: &ErrorPayload) -> Result<()> {
    if payload.code.trim().is_empty() {
        return Err(Error::validation("Error code is empty"));
    }
    if payload.message.trim().is_empty() {
        return Err(Error::validation("Error message is empty"));
    }
    Ok(())
}
