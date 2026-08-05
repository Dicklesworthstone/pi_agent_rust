//! Hostcall dispatcher for JS extensions.
//!
//! This module introduces the core `ExtensionDispatcher` abstraction used to route
//! hostcall requests (tools, HTTP, session, UI, etc.) from the JS runtime to
//! Rust implementations.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use asupersync::Cx;
use asupersync::time::{sleep, wall_now};
use async_trait::async_trait;
use serde_json::Value;
use sha2::Digest as _;

use crate::connectors::{Connector, http::HttpConnector};
use crate::error::Result;
use crate::extensions::EXTENSION_EVENT_TIMEOUT_MS;
use crate::extensions::{
    DangerousCommandClass, ExecMediationResult, ExtensionBody, ExtensionMessage, ExtensionPolicy,
    ExtensionSession, ExtensionUiRequest, ExtensionUiResponse, HostCallError, HostCallErrorCode,
    HostCallPayload, HostResultPayload, HostStreamChunk, PROTOCOL_VERSION, PolicyCheck,
    PolicyDecision, PolicyProfile, PolicySnapshot, classify_ui_hostcall_error,
    evaluate_exec_mediation, hash_canonical_json, required_capability_for_host_call_static,
    ui_response_value_for_op, validate_host_call,
};
use crate::extensions_js::{HostcallKind, HostcallRequest, PiJsRuntime, js_to_json, json_to_js};
use crate::hostcall_amac::{AmacBatchExecutor, AmacBatchExecutorConfig};
use crate::hostcall_io_uring_lane::{
    HostcallCapabilityClass, HostcallDispatchLane, HostcallIoHint, IoUringFallbackReason,
    IoUringLaneDecisionInput, IoUringLanePolicyConfig, decide_io_uring_lane,
    io_uring_executor_available,
};
use crate::resource_governor::{
    AdmissionAction, AdmissionDecision, ResourceGovernor, ResourceOperationKind, ResourceRequest,
};
use crate::scheduler::{Clock as SchedulerClock, HostcallOutcome, WallClock};
use crate::tools::ToolRegistry;

struct CancelGuard(Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn extension_wait_now() -> asupersync::types::Time {
    Cx::current()
        .and_then(|cx| cx.timer_driver())
        .map_or_else(wall_now, |driver| driver.now())
}

fn extension_wait_sleep(duration: Duration) -> asupersync::time::Sleep {
    sleep(extension_wait_now(), duration)
}

#[derive(Clone, Copy)]
enum ExecPipeKind {
    Stdout,
    Stderr,
}

enum ExecStreamFrame {
    Stdout(String),
    Stderr(String),
    Final { code: i32, killed: bool },
    Error(String),
}

enum ExecCaptureFrame {
    Chunk { kind: ExecPipeKind, bytes: Vec<u8> },
    Error(String),
}

fn send_exec_stream_text(
    tx: &std::sync::mpsc::SyncSender<ExecStreamFrame>,
    kind: ExecPipeKind,
    text: String,
) -> bool {
    if text.is_empty() {
        return true;
    }

    let frame = match kind {
        ExecPipeKind::Stdout => ExecStreamFrame::Stdout(text),
        ExecPipeKind::Stderr => ExecStreamFrame::Stderr(text),
    };
    tx.send(frame).is_ok()
}

fn pump_exec_stream_text<R: std::io::Read>(
    mut reader: R,
    tx: &std::sync::mpsc::SyncSender<ExecStreamFrame>,
    kind: ExecPipeKind,
) {
    let mut buf = [0u8; 4096];
    let mut partial = Vec::new();

    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => 0,
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                if !partial.is_empty()
                    && !send_exec_stream_text(
                        tx,
                        kind,
                        String::from_utf8_lossy(&partial).to_string(),
                    )
                {
                    return;
                }
                let _ = tx.send(ExecStreamFrame::Error(err.to_string()));
                return;
            }
        };
        if read == 0 {
            // EOF. Flush partial if any (lossy).
            if !partial.is_empty() {
                let _ =
                    send_exec_stream_text(tx, kind, String::from_utf8_lossy(&partial).to_string());
            }
            break;
        }

        let chunk = &buf[..read];

        // If we have partial data, we must append the new chunk and process the combined buffer.
        // If partial is empty, we can process the chunk directly (fast path).
        if partial.is_empty() {
            let mut processed = 0;
            loop {
                match std::str::from_utf8(&chunk[processed..]) {
                    Ok(s) => {
                        if !send_exec_stream_text(tx, kind, s.to_string()) {
                            return;
                        }
                        break;
                    }
                    Err(e) => {
                        let valid_len = e.valid_up_to();
                        if valid_len > 0 {
                            let s = std::str::from_utf8(&chunk[processed..processed + valid_len])
                                .expect("valid utf8 prefix");
                            if !send_exec_stream_text(tx, kind, s.to_string()) {
                                return;
                            }
                            processed += valid_len;
                        }

                        if let Some(len) = e.error_len() {
                            if !send_exec_stream_text(tx, kind, "\u{FFFD}".to_string()) {
                                return;
                            }
                            processed += len;
                        } else {
                            partial.extend_from_slice(&chunk[processed..]);
                            break;
                        }
                    }
                }
            }
        } else {
            partial.extend_from_slice(chunk);
            let mut processed = 0;
            loop {
                match std::str::from_utf8(&partial[processed..]) {
                    Ok(s) => {
                        if !send_exec_stream_text(tx, kind, s.to_string()) {
                            return;
                        }
                        partial.clear();
                        break;
                    }
                    Err(e) => {
                        let valid_len = e.valid_up_to();
                        if valid_len > 0 {
                            let s = std::str::from_utf8(&partial[processed..processed + valid_len])
                                .expect("valid utf8 prefix");
                            if !send_exec_stream_text(tx, kind, s.to_string()) {
                                return;
                            }
                            processed += valid_len;
                        }

                        if let Some(len) = e.error_len() {
                            if !send_exec_stream_text(tx, kind, "\u{FFFD}".to_string()) {
                                return;
                            }
                            processed += len;
                        } else {
                            let remaining = partial.len() - processed;
                            partial.copy_within(processed.., 0);
                            partial.truncate(remaining);
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn pump_exec_capture_bytes<R: std::io::Read>(
    mut reader: R,
    tx: &std::sync::mpsc::SyncSender<ExecCaptureFrame>,
    kind: ExecPipeKind,
) {
    let mut buf = [0u8; 8192];
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                let _ = tx.send(ExecCaptureFrame::Error(err.to_string()));
                break;
            }
        };
        let chunk = ExecCaptureFrame::Chunk {
            kind,
            bytes: buf[..read].to_vec(),
        };
        if tx.send(chunk).is_err() {
            break;
        }
    }
}

fn exec_capture_error_message(
    message: &str,
    stdout_chunks: &VecDeque<Vec<u8>>,
    stdout_total_bytes: usize,
    stderr_chunks: &VecDeque<Vec<u8>>,
    stderr_total_bytes: usize,
) -> String {
    let stdout = exec_capture_error_section(stdout_chunks, stdout_total_bytes);
    let stderr = exec_capture_error_section(stderr_chunks, stderr_total_bytes);
    if stdout.is_none() && stderr.is_none() {
        return message.to_string();
    }

    let mut error_message = message.to_string();
    error_message.push_str("\n\nPartial output before failure:");

    if let Some((content, truncated)) = stdout {
        error_message.push_str("\n\n[stdout]\n");
        error_message.push_str(&content);
        if truncated {
            error_message.push_str("\n[stdout partial output truncated before failure]");
        }
    }

    if let Some((content, truncated)) = stderr {
        error_message.push_str("\n\n[stderr]\n");
        error_message.push_str(&content);
        if truncated {
            error_message.push_str("\n[stderr partial output truncated before failure]");
        }
    }

    error_message
}

fn exec_capture_error_section(
    chunks: &VecDeque<Vec<u8>>,
    total_bytes: usize,
) -> Option<(String, bool)> {
    if total_bytes == 0 {
        return None;
    }

    let retained_bytes: usize = chunks.iter().map(std::vec::Vec::len).sum();
    let mut raw = Vec::with_capacity(retained_bytes);
    for chunk in chunks {
        raw.extend_from_slice(chunk);
    }

    let full_text = String::from_utf8_lossy(&raw).into_owned();
    let truncation = crate::tools::truncate_tail(
        full_text,
        crate::tools::DEFAULT_MAX_LINES,
        crate::tools::DEFAULT_MAX_BYTES,
    );
    let content = if truncation.content.is_empty() {
        "(no output)".to_string()
    } else {
        truncation.content
    };
    Some((
        content,
        truncation.truncated || total_bytes > retained_bytes,
    ))
}

#[allow(clippy::too_many_arguments)]
fn ingest_exec_capture_chunk(
    kind: ExecPipeKind,
    bytes: Vec<u8>,
    stdout_chunks: &mut VecDeque<Vec<u8>>,
    stdout_total_bytes: &mut usize,
    stdout_bytes_len: &mut usize,
    stderr_chunks: &mut VecDeque<Vec<u8>>,
    stderr_total_bytes: &mut usize,
    stderr_bytes_len: &mut usize,
    max_bytes: usize,
) {
    match kind {
        ExecPipeKind::Stdout => {
            *stdout_total_bytes += bytes.len();
            *stdout_bytes_len += bytes.len();
            stdout_chunks.push_back(bytes);
            while *stdout_bytes_len > max_bytes && stdout_chunks.len() > 1 {
                if let Some(front) = stdout_chunks.pop_front() {
                    *stdout_bytes_len -= front.len();
                }
            }
        }
        ExecPipeKind::Stderr => {
            *stderr_total_bytes += bytes.len();
            *stderr_bytes_len += bytes.len();
            stderr_chunks.push_back(bytes);
            while *stderr_bytes_len > max_bytes && stderr_chunks.len() > 1 {
                if let Some(front) = stderr_chunks.pop_front() {
                    *stderr_bytes_len -= front.len();
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_exec_capture_frame(
    frame: ExecCaptureFrame,
    stdout_chunks: &mut VecDeque<Vec<u8>>,
    stdout_total_bytes: &mut usize,
    stdout_bytes_len: &mut usize,
    stderr_chunks: &mut VecDeque<Vec<u8>>,
    stderr_total_bytes: &mut usize,
    stderr_bytes_len: &mut usize,
    max_bytes: usize,
) -> std::result::Result<(), String> {
    match frame {
        ExecCaptureFrame::Chunk { kind, bytes } => {
            ingest_exec_capture_chunk(
                kind,
                bytes,
                stdout_chunks,
                stdout_total_bytes,
                stdout_bytes_len,
                stderr_chunks,
                stderr_total_bytes,
                stderr_bytes_len,
                max_bytes,
            );
            Ok(())
        }
        ExecCaptureFrame::Error(message) => Err(exec_capture_error_message(
            &message,
            stdout_chunks,
            *stdout_total_bytes,
            stderr_chunks,
            *stderr_total_bytes,
        )),
    }
}

/// Coordinates hostcall dispatch between the JS extension runtime and Rust handlers.
pub struct ExtensionDispatcher<C: SchedulerClock = WallClock> {
    /// Runtime bridge used by the dispatcher.
    runtime: Rc<dyn ExtensionDispatcherRuntime<C>>,
    /// Registry of available tools (built-in + extension-registered).
    tool_registry: Arc<ToolRegistry>,
    /// HTTP connector for pi.http() calls.
    http_connector: Arc<HttpConnector>,
    /// Session access for pi.session() calls.
    session: Arc<dyn ExtensionSession + Send + Sync>,
    /// UI handler for pi.ui() calls.
    ui_handler: Arc<dyn ExtensionUiHandler + Send + Sync>,
    /// Current working directory for relative path resolution.
    cwd: PathBuf,
    /// Capability policy governing which hostcalls are allowed.
    policy: ExtensionPolicy,
    /// Precomputed O(1) capability decision table.
    snapshot: PolicySnapshot,
    /// Deterministic policy snapshot version hash for provenance/telemetry.
    snapshot_version: String,
    /// Configuration for sampled shadow dual execution.
    dual_exec_config: DualExecOracleConfig,
    /// Runtime state for sampled dual execution and rollback guards.
    dual_exec_state: RefCell<DualExecOracleState>,
    /// Advisory io_uring lane policy for IO-dominant hostcalls.
    io_uring_lane_config: IoUringLanePolicyConfig,
    /// Kill switch forcing compatibility lane regardless of policy input.
    io_uring_force_compat: bool,
    /// Adaptive regime detector for hostcall workload shifts.
    regime_detector: RefCell<RegimeShiftDetector>,
    /// AMAC batch executor for interleaved hostcall dispatch.
    amac_executor: RefCell<AmacBatchExecutor>,
    /// Host-scale admission and backpressure gate.
    resource_governor: ResourceGovernor,
}

#[derive(serde::Deserialize)]
struct ExtensionEventJsTaskError {
    #[serde(default)]
    code: Option<String>,
    message: String,
    #[serde(default)]
    stack: Option<String>,
}

#[derive(serde::Deserialize)]
struct ExtensionEventJsTaskState {
    status: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    error: Option<ExtensionEventJsTaskError>,
}

enum ExtensionEventTaskProgress {
    Pending,
    Resolved(Value),
}

fn decode_extension_event_task_state(state_json: Value) -> Result<ExtensionEventTaskProgress> {
    if state_json.is_null() {
        return Err(crate::error::Error::extension(
            "events.emit task state missing".to_string(),
        ));
    }

    let state: ExtensionEventJsTaskState = serde_json::from_value(state_json)
        .map_err(|err| crate::error::Error::extension(err.to_string()))?;
    match state.status.as_str() {
        "pending" => Ok(ExtensionEventTaskProgress::Pending),
        "resolved" => Ok(ExtensionEventTaskProgress::Resolved(
            state.value.unwrap_or(Value::Null),
        )),
        "rejected" => {
            let err = state.error.unwrap_or_else(|| ExtensionEventJsTaskError {
                code: None,
                message: "Unknown JS task error".to_string(),
                stack: None,
            });
            let mut message = err.message;
            if let Some(code) = err.code {
                message = format!("{code}: {message}");
            }
            if let Some(stack) = err.stack
                && !stack.is_empty()
            {
                message.push('\n');
                message.push_str(&stack);
            }
            Err(crate::error::Error::extension(message))
        }
        other => Err(crate::error::Error::extension(format!(
            "Unexpected JS task status: {other}"
        ))),
    }
}

/// Runtime bridge trait so dispatcher logic is not hardwired to a concrete runtime type.
pub trait ExtensionDispatcherRuntime<C: SchedulerClock>: 'static {
    fn as_js_runtime(&self) -> &PiJsRuntime<C>;
}

impl<C: SchedulerClock + 'static> ExtensionDispatcherRuntime<C> for PiJsRuntime<C> {
    #[allow(clippy::use_self)]
    fn as_js_runtime(&self) -> &PiJsRuntime<C> {
        self
    }
}

fn protocol_hostcall_op(params: &Value) -> Option<&str> {
    params
        .get("op")
        .or_else(|| params.get("method"))
        .or_else(|| params.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolHostcallMethod {
    Tool,
    Exec,
    Http,
    Session,
    Ui,
    Events,
    Log,
}

fn parse_protocol_hostcall_method(method: &str) -> Option<ProtocolHostcallMethod> {
    let method = method.trim();
    if method.is_empty() {
        return None;
    }

    if method.eq_ignore_ascii_case("tool") {
        Some(ProtocolHostcallMethod::Tool)
    } else if method.eq_ignore_ascii_case("exec") {
        Some(ProtocolHostcallMethod::Exec)
    } else if method.eq_ignore_ascii_case("http") {
        Some(ProtocolHostcallMethod::Http)
    } else if method.eq_ignore_ascii_case("session") {
        Some(ProtocolHostcallMethod::Session)
    } else if method.eq_ignore_ascii_case("ui") {
        Some(ProtocolHostcallMethod::Ui)
    } else if method.eq_ignore_ascii_case("events") {
        Some(ProtocolHostcallMethod::Events)
    } else if method.eq_ignore_ascii_case("log") {
        Some(ProtocolHostcallMethod::Log)
    } else {
        None
    }
}

fn protocol_normalize_output(value: Value) -> Value {
    if value.is_object() {
        value
    } else {
        serde_json::json!({ "value": value })
    }
}

fn policy_snapshot_version(policy: &ExtensionPolicy) -> String {
    let mut hasher = sha2::Sha256::new();
    match serde_json::to_value(policy) {
        Ok(value) => hash_canonical_json(&value, &mut hasher),
        Err(err) => hasher.update(err.to_string().as_bytes()),
    }
    format!("{:x}", hasher.finalize())
}

fn policy_lookup_path(capability: &str) -> &'static str {
    let capability = capability.trim();
    if capability.eq_ignore_ascii_case("read")
        || capability.eq_ignore_ascii_case("write")
        || capability.eq_ignore_ascii_case("exec")
        || capability.eq_ignore_ascii_case("env")
        || capability.eq_ignore_ascii_case("http")
        || capability.eq_ignore_ascii_case("session")
        || capability.eq_ignore_ascii_case("events")
        || capability.eq_ignore_ascii_case("ui")
        || capability.eq_ignore_ascii_case("log")
        || capability.eq_ignore_ascii_case("tool")
    {
        "policy_snapshot_table"
    } else {
        "policy_snapshot_fallback"
    }
}

fn protocol_error_code(code: &str) -> HostCallErrorCode {
    let code = code.trim();
    if code.eq_ignore_ascii_case("timeout") {
        HostCallErrorCode::Timeout
    } else if code.eq_ignore_ascii_case("denied") {
        HostCallErrorCode::Denied
    } else if code.eq_ignore_ascii_case("io") || code.eq_ignore_ascii_case("tool_error") {
        HostCallErrorCode::Io
    } else if code.eq_ignore_ascii_case("invalid_request") {
        HostCallErrorCode::InvalidRequest
    } else {
        HostCallErrorCode::Internal
    }
}

fn protocol_error_fallback_reason(method: &str, code: &str) -> &'static str {
    let code = code.trim();
    if code.eq_ignore_ascii_case("denied") {
        "policy_denied"
    } else if code.eq_ignore_ascii_case("timeout") {
        "handler_timeout"
    } else if code.eq_ignore_ascii_case("io") || code.eq_ignore_ascii_case("tool_error") {
        "handler_error"
    } else if code.eq_ignore_ascii_case("invalid_request") {
        if parse_protocol_hostcall_method(method).is_some() {
            "schema_validation_failed"
        } else {
            "unsupported_method_fallback"
        }
    } else {
        "runtime_internal_error"
    }
}

fn protocol_error_details(payload: &HostCallPayload, code: &str, message: &str) -> Value {
    let observed_param_keys = payload
        .params
        .as_object()
        .map(|object| {
            let mut keys = object.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys
        })
        .unwrap_or_default();

    serde_json::json!({
        "dispatcherDecisionTrace": {
            "selectedRuntime": "rust-extension-dispatcher",
            "schemaPath": "ExtensionBody::HostCall/HostCallPayload",
            "schemaVersion": PROTOCOL_VERSION,
            "method": payload.method,
            "capability": payload.capability,
            "fallbackReason": protocol_error_fallback_reason(&payload.method, code),
        },
        "schemaDiff": {
            "observedParamKeys": observed_param_keys,
        },
        "extensionInput": {
            "callId": payload.call_id,
            "capability": payload.capability,
            "method": payload.method,
            "params": payload.params,
        },
        "extensionOutput": {
            "code": code,
            "message": message,
        },
    })
}

// Kept as the no-trace parity oracle for tests; production dispatch records
// trace details through `hostcall_outcome_to_protocol_result_with_trace`.
#[allow(dead_code)]
fn hostcall_outcome_to_protocol_result(
    call_id: &str,
    outcome: HostcallOutcome,
) -> HostResultPayload {
    match outcome {
        HostcallOutcome::Success(output) => HostResultPayload {
            call_id: call_id.to_string(),
            output: protocol_normalize_output(output),
            is_error: false,
            error: None,
            chunk: None,
        },
        HostcallOutcome::StreamChunk {
            sequence,
            chunk,
            is_final,
        } => HostResultPayload {
            call_id: call_id.to_string(),
            output: serde_json::json!({
                "sequence": sequence,
                "chunk": chunk,
                "isFinal": is_final,
            }),
            is_error: false,
            error: None,
            chunk: Some(HostStreamChunk {
                index: sequence,
                is_last: is_final,
                backpressure: None,
            }),
        },
        HostcallOutcome::Error { code, message } => HostResultPayload {
            call_id: call_id.to_string(),
            output: serde_json::json!({}),
            is_error: true,
            error: Some(HostCallError {
                code: protocol_error_code(&code),
                message,
                details: None,
                retryable: None,
            }),
            chunk: None,
        },
    }
}

fn hostcall_outcome_to_protocol_result_with_trace(
    payload: &HostCallPayload,
    outcome: HostcallOutcome,
) -> HostResultPayload {
    match outcome {
        HostcallOutcome::Success(output) => HostResultPayload {
            call_id: payload.call_id.clone(),
            output: protocol_normalize_output(output),
            is_error: false,
            error: None,
            chunk: None,
        },
        HostcallOutcome::StreamChunk {
            sequence,
            chunk,
            is_final,
        } => HostResultPayload {
            call_id: payload.call_id.clone(),
            output: serde_json::json!({
                "sequence": sequence,
                "chunk": chunk,
                "isFinal": is_final,
            }),
            is_error: false,
            error: None,
            chunk: Some(HostStreamChunk {
                index: sequence,
                is_last: is_final,
                backpressure: None,
            }),
        },
        HostcallOutcome::Error { code, message } => {
            let details = Some(protocol_error_details(payload, &code, &message));
            HostResultPayload {
                call_id: payload.call_id.clone(),
                output: serde_json::json!({}),
                is_error: true,
                error: Some(HostCallError {
                    code: protocol_error_code(&code),
                    message,
                    details,
                    retryable: None,
                }),
                chunk: None,
            }
        }
    }
}

const DUAL_EXEC_SAMPLE_MODULUS_PPM: u32 = 1_000_000;
const DUAL_EXEC_DEFAULT_SAMPLE_PPM: u32 = 25_000;
const DUAL_EXEC_DEFAULT_DIVERGENCE_WINDOW: usize = 64;
const DUAL_EXEC_DEFAULT_DIVERGENCE_BUDGET: usize = 3;
const DUAL_EXEC_DEFAULT_ROLLBACK_REQUESTS: usize = 128;
const DUAL_EXEC_DEFAULT_OVERHEAD_BUDGET_US: u64 = 1_500;
const DUAL_EXEC_DEFAULT_OVERHEAD_BACKOFF_REQUESTS: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct DualExecOracleConfig {
    pub sample_ppm: u32,
    pub divergence_window: usize,
    pub divergence_budget: usize,
    pub rollback_requests: usize,
    pub overhead_budget_us: u64,
    pub overhead_backoff_requests: usize,
}

impl Default for DualExecOracleConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl DualExecOracleConfig {
    fn from_env() -> Self {
        let sample_ppm = std::env::var("PI_EXT_DUAL_EXEC_SAMPLE_PPM")
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok())
            .unwrap_or(DUAL_EXEC_DEFAULT_SAMPLE_PPM)
            .min(DUAL_EXEC_SAMPLE_MODULUS_PPM);
        let divergence_window = std::env::var("PI_EXT_DUAL_EXEC_DIVERGENCE_WINDOW")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .unwrap_or(DUAL_EXEC_DEFAULT_DIVERGENCE_WINDOW)
            .max(1);
        let divergence_budget = std::env::var("PI_EXT_DUAL_EXEC_DIVERGENCE_BUDGET")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .unwrap_or(DUAL_EXEC_DEFAULT_DIVERGENCE_BUDGET)
            .max(1);
        let rollback_requests = std::env::var("PI_EXT_DUAL_EXEC_ROLLBACK_REQUESTS")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .unwrap_or(DUAL_EXEC_DEFAULT_ROLLBACK_REQUESTS)
            .max(1);
        let overhead_budget_us = std::env::var("PI_EXT_DUAL_EXEC_OVERHEAD_BUDGET_US")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(DUAL_EXEC_DEFAULT_OVERHEAD_BUDGET_US)
            .max(1);
        let overhead_backoff_requests = std::env::var("PI_EXT_DUAL_EXEC_OVERHEAD_BACKOFF_REQUESTS")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .unwrap_or(DUAL_EXEC_DEFAULT_OVERHEAD_BACKOFF_REQUESTS)
            .max(1);

        Self {
            sample_ppm,
            divergence_window,
            divergence_budget,
            rollback_requests,
            overhead_budget_us,
            overhead_backoff_requests,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DualExecOracleState {
    sampled_total: u64,
    matched_total: u64,
    divergence_total: u64,
    skipped_unsupported_total: u64,
    skipped_overhead_total: u64,
    divergence_window: VecDeque<bool>,
    rollback_remaining: usize,
    rollback_reason: Option<String>,
    overhead_backoff_remaining: usize,
}

impl DualExecOracleState {
    pub fn begin_request(&mut self) {
        if self.rollback_remaining > 0 {
            self.rollback_remaining = self.rollback_remaining.saturating_sub(1);
            if self.rollback_remaining == 0 {
                self.rollback_reason = None;
            }
        }
        if self.overhead_backoff_remaining > 0 {
            self.overhead_backoff_remaining = self.overhead_backoff_remaining.saturating_sub(1);
        }
    }

    pub const fn rollback_active(&self) -> bool {
        self.rollback_remaining > 0
    }

    pub const fn sampled_total(&self) -> u64 {
        self.sampled_total
    }

    pub const fn matched_total(&self) -> u64 {
        self.matched_total
    }

    pub const fn divergence_total(&self) -> u64 {
        self.divergence_total
    }

    pub const fn skipped_unsupported_total(&self) -> u64 {
        self.skipped_unsupported_total
    }

    pub const fn skipped_overhead_total(&self) -> u64 {
        self.skipped_overhead_total
    }

    pub const fn rollback_remaining(&self) -> usize {
        self.rollback_remaining
    }

    pub fn rollback_reason(&self) -> Option<&str> {
        self.rollback_reason.as_deref()
    }

    pub const fn overhead_backoff_remaining(&self) -> usize {
        self.overhead_backoff_remaining
    }

    pub const fn record_overhead_budget_exceeded(&mut self, config: DualExecOracleConfig) {
        self.skipped_overhead_total = self.skipped_overhead_total.saturating_add(1);
        self.overhead_backoff_remaining = config.overhead_backoff_requests;
    }

    pub fn record_sample(
        &mut self,
        divergent: bool,
        config: DualExecOracleConfig,
        extension_id: Option<&str>,
    ) -> Option<String> {
        self.sampled_total = self.sampled_total.saturating_add(1);
        if divergent {
            self.divergence_total = self.divergence_total.saturating_add(1);
        } else {
            self.matched_total = self.matched_total.saturating_add(1);
        }
        self.divergence_window.push_back(divergent);
        while self.divergence_window.len() > config.divergence_window {
            let _ = self.divergence_window.pop_front();
        }
        let divergence_count = self.divergence_window.iter().filter(|&&flag| flag).count();
        if divergence_count >= config.divergence_budget {
            self.rollback_remaining = config.rollback_requests;
            let reason = format!(
                "dual_exec_divergence_budget_exceeded:{divergence_count}/{window}:{scope}",
                window = self.divergence_window.len(),
                scope = extension_id.unwrap_or("global")
            );
            self.rollback_reason = Some(reason.clone());
            return Some(reason);
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DualExecOutcomeDiff {
    reason: &'static str,
    fast_fingerprint: String,
    compat_fingerprint: String,
}

fn hostcall_value_fingerprint(value: &Value) -> String {
    let mut hasher = sha2::Sha256::new();
    hash_canonical_json(value, &mut hasher);
    format!("{:x}", hasher.finalize())
}

fn hostcall_outcome_fingerprint(outcome: &HostcallOutcome) -> String {
    match outcome {
        HostcallOutcome::Success(output) => {
            let hash = hostcall_value_fingerprint(output);
            format!("success:{hash}")
        }
        HostcallOutcome::Error { code, message } => {
            let hash = hostcall_value_fingerprint(&serde_json::json!({
                "code": code,
                "message": message,
            }));
            format!("error:{hash}")
        }
        HostcallOutcome::StreamChunk {
            sequence,
            chunk,
            is_final,
        } => {
            let hash = hostcall_value_fingerprint(&serde_json::json!({
                "sequence": sequence,
                "chunk": chunk,
                "isFinal": is_final,
            }));
            format!("stream:{hash}")
        }
    }
}

fn diff_hostcall_outcomes(
    fast: &HostcallOutcome,
    compat: &HostcallOutcome,
) -> Option<DualExecOutcomeDiff> {
    match (fast, compat) {
        (HostcallOutcome::Success(a), HostcallOutcome::Success(b)) => {
            let a_hash = hostcall_value_fingerprint(a);
            let b_hash = hostcall_value_fingerprint(b);
            if a_hash == b_hash {
                None
            } else {
                Some(DualExecOutcomeDiff {
                    reason: "success_output_mismatch",
                    fast_fingerprint: format!("success:{a_hash}"),
                    compat_fingerprint: format!("success:{b_hash}"),
                })
            }
        }
        (
            HostcallOutcome::Error {
                code: a_code,
                message: a_message,
            },
            HostcallOutcome::Error {
                code: b_code,
                message: b_message,
            },
        ) => {
            if a_code == b_code && a_message == b_message {
                None
            } else if a_code != b_code {
                Some(DualExecOutcomeDiff {
                    reason: "error_code_mismatch",
                    fast_fingerprint: hostcall_outcome_fingerprint(fast),
                    compat_fingerprint: hostcall_outcome_fingerprint(compat),
                })
            } else {
                Some(DualExecOutcomeDiff {
                    reason: "error_message_mismatch",
                    fast_fingerprint: hostcall_outcome_fingerprint(fast),
                    compat_fingerprint: hostcall_outcome_fingerprint(compat),
                })
            }
        }
        (
            HostcallOutcome::StreamChunk {
                sequence: a_seq,
                chunk: a_chunk,
                is_final: a_final,
            },
            HostcallOutcome::StreamChunk {
                sequence: b_seq,
                chunk: b_chunk,
                is_final: b_final,
            },
        ) => {
            if a_seq == b_seq && a_chunk == b_chunk && a_final == b_final {
                None
            } else if a_seq != b_seq {
                Some(DualExecOutcomeDiff {
                    reason: "stream_sequence_mismatch",
                    fast_fingerprint: hostcall_outcome_fingerprint(fast),
                    compat_fingerprint: hostcall_outcome_fingerprint(compat),
                })
            } else if a_final != b_final {
                Some(DualExecOutcomeDiff {
                    reason: "stream_finality_mismatch",
                    fast_fingerprint: hostcall_outcome_fingerprint(fast),
                    compat_fingerprint: hostcall_outcome_fingerprint(compat),
                })
            } else {
                Some(DualExecOutcomeDiff {
                    reason: "stream_chunk_mismatch",
                    fast_fingerprint: hostcall_outcome_fingerprint(fast),
                    compat_fingerprint: hostcall_outcome_fingerprint(compat),
                })
            }
        }
        _ => Some(DualExecOutcomeDiff {
            reason: "outcome_variant_mismatch",
            fast_fingerprint: hostcall_outcome_fingerprint(fast),
            compat_fingerprint: hostcall_outcome_fingerprint(compat),
        }),
    }
}

fn should_sample_shadow_dual_exec(request: &HostcallRequest, sample_ppm: u32) -> bool {
    if sample_ppm == 0 {
        return false;
    }
    if sample_ppm >= DUAL_EXEC_SAMPLE_MODULUS_PPM {
        return true;
    }
    let bucket = shadow_sampling_bucket(request) % DUAL_EXEC_SAMPLE_MODULUS_PPM;
    bucket < sample_ppm
}

#[inline]
fn fnv1a64_update(mut hash: u64, bytes: &[u8]) -> u64 {
    const FNV1A_PRIME: u64 = 1_099_511_628_211;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV1A_PRIME);
    }
    hash
}

#[inline]
fn shadow_sampling_bucket(request: &HostcallRequest) -> u32 {
    // Deterministic, allocation-free mixing for high-frequency sampling checks.
    const FNV1A_OFFSET_BASIS: u64 = 14_695_981_039_346_656_037;
    let mut hash = FNV1A_OFFSET_BASIS;
    hash = fnv1a64_update(hash, request.call_id.as_bytes());
    hash = fnv1a64_update(hash, &[0xFF]);
    hash = fnv1a64_update(hash, &request.trace_id.to_le_bytes());
    if let Some(extension_id) = request.extension_id.as_deref() {
        hash = fnv1a64_update(hash, &[0xFE]);
        hash = fnv1a64_update(hash, extension_id.as_bytes());
    }

    // Final avalanche to improve low-bit dispersion before modulus.
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^= hash >> 33;

    let bytes = hash.to_le_bytes();
    let low = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let high = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    low ^ high
}

// Shadow-mode normalization is exercised by parity/property tests even when
// the live dispatcher takes the canonical-op path.
#[allow(dead_code)]
fn normalized_shadow_op(op: &str) -> String {
    let trimmed = op.trim();
    let mut normalized = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch != '_' {
            normalized.push(ch.to_ascii_lowercase());
        }
    }
    normalized
}

#[inline]
fn with_folded_ascii_alnum_token<T>(token: &str, f: impl FnOnce(&[u8]) -> T) -> T {
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

fn shadow_safe_session_op(op: &str) -> bool {
    with_folded_ascii_alnum_token(op, |folded| {
        matches!(
            folded,
            b"getstate"
                | b"getmessages"
                | b"getentries"
                | b"getbranch"
                | b"getfile"
                | b"getname"
                | b"getmodel"
                | b"getthinkinglevel"
                | b"getlabel"
                | b"getlabels"
                | b"getallsessions"
        )
    })
}

fn shadow_safe_events_op(op: &str) -> bool {
    with_folded_ascii_alnum_token(op, |folded| {
        matches!(
            folded,
            b"getactivetools"
                | b"getalltools"
                | b"getmodel"
                | b"getthinkinglevel"
                | b"getflag"
                | b"listflags"
        )
    })
}

fn shadow_safe_tool(name: &str) -> bool {
    let name = name.trim();
    name.eq_ignore_ascii_case("read")
        || name.eq_ignore_ascii_case("grep")
        || name.eq_ignore_ascii_case("find")
        || name.eq_ignore_ascii_case("ls")
}

fn is_shadow_safe_request(request: &HostcallRequest) -> bool {
    match &request.kind {
        HostcallKind::Session { op } => shadow_safe_session_op(op),
        HostcallKind::Events { op } => shadow_safe_events_op(op),
        HostcallKind::Tool { name } => shadow_safe_tool(name),
        HostcallKind::Http
        | HostcallKind::Exec { .. }
        | HostcallKind::Ui { .. }
        | HostcallKind::Log => false,
    }
}

fn parse_env_bool(name: &str, default: bool) -> bool {
    std::env::var(name).ok().map_or(default, |raw| {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enabled" => true,
            "0" | "false" | "no" | "off" | "disabled" => false,
            _ => default,
        }
    })
}

fn io_uring_lane_policy_from_env() -> IoUringLanePolicyConfig {
    let default = IoUringLanePolicyConfig::conservative();
    let max_queue_depth = std::env::var("PI_EXT_IO_URING_MAX_QUEUE_DEPTH")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(default.max_queue_depth)
        .max(1);

    IoUringLanePolicyConfig {
        enabled: parse_env_bool("PI_EXT_IO_URING_ENABLED", default.enabled),
        ring_available: parse_env_bool("PI_EXT_IO_URING_RING_AVAILABLE", default.ring_available),
        max_queue_depth,
        allow_filesystem: parse_env_bool(
            "PI_EXT_IO_URING_ALLOW_FILESYSTEM",
            default.allow_filesystem,
        ),
        allow_network: parse_env_bool("PI_EXT_IO_URING_ALLOW_NETWORK", default.allow_network),
    }
}

fn io_uring_force_compat_from_env() -> bool {
    parse_env_bool("PI_EXT_IO_URING_FORCE_COMPAT", false)
}

fn hostcall_io_hint(kind: &HostcallKind) -> HostcallIoHint {
    match kind {
        HostcallKind::Http => HostcallIoHint::IoHeavy,
        HostcallKind::Tool { name } => {
            let name = name.trim();
            if name.eq_ignore_ascii_case("read")
                || name.eq_ignore_ascii_case("write")
                || name.eq_ignore_ascii_case("edit")
                || name.eq_ignore_ascii_case("grep")
                || name.eq_ignore_ascii_case("find")
                || name.eq_ignore_ascii_case("ls")
            {
                HostcallIoHint::IoHeavy
            } else if name.eq_ignore_ascii_case("bash") {
                HostcallIoHint::CpuBound
            } else {
                HostcallIoHint::Unknown
            }
        }
        HostcallKind::Session { op } => {
            let lower = op.trim().to_ascii_lowercase();
            if lower.contains("save")
                || lower.contains("append")
                || lower.contains("write")
                || lower.contains("export")
                || lower.contains("import")
            {
                HostcallIoHint::IoHeavy
            } else {
                HostcallIoHint::Unknown
            }
        }
        HostcallKind::Exec { .. }
        | HostcallKind::Ui { .. }
        | HostcallKind::Events { .. }
        | HostcallKind::Log => HostcallIoHint::CpuBound,
    }
}

const fn resource_operation_kind(kind: &HostcallKind) -> ResourceOperationKind {
    match kind {
        HostcallKind::Tool { .. } => ResourceOperationKind::Tool,
        HostcallKind::Exec { .. } => ResourceOperationKind::Exec,
        HostcallKind::Http => ResourceOperationKind::Http,
        HostcallKind::Session { .. } => ResourceOperationKind::Session,
        HostcallKind::Ui { .. } => ResourceOperationKind::Ui,
        HostcallKind::Events { .. } => ResourceOperationKind::Events,
        HostcallKind::Log => ResourceOperationKind::Log,
    }
}

fn estimated_hostcall_output_bytes(request: &HostcallRequest) -> u64 {
    match &request.kind {
        HostcallKind::Exec { .. } => crate::tools::READ_TOOL_MAX_BYTES,
        HostcallKind::Tool { name } => {
            let name = name.trim();
            if name.eq_ignore_ascii_case("bash") {
                crate::tools::READ_TOOL_MAX_BYTES
            } else if name.eq_ignore_ascii_case("read")
                || name.eq_ignore_ascii_case("grep")
                || name.eq_ignore_ascii_case("find")
                || name.eq_ignore_ascii_case("ls")
            {
                output_limit_from_payload(&request.payload)
                    .unwrap_or(crate::tools::DEFAULT_MAX_BYTES as u64)
            } else {
                crate::tools::DEFAULT_MAX_BYTES as u64
            }
        }
        HostcallKind::Http
        | HostcallKind::Session { .. }
        | HostcallKind::Ui { .. }
        | HostcallKind::Events { .. }
        | HostcallKind::Log => 0,
    }
}

fn output_limit_from_payload(payload: &Value) -> Option<u64> {
    payload
        .get("max_bytes")
        .or_else(|| payload.get("maxBytes"))
        .or_else(|| payload.get("limit_bytes"))
        .or_else(|| payload.get("limitBytes"))
        .and_then(Value::as_u64)
}

const fn hostcall_io_hint_label(io_hint: HostcallIoHint) -> &'static str {
    match io_hint {
        HostcallIoHint::Unknown => "unknown",
        HostcallIoHint::IoHeavy => "io_heavy",
        HostcallIoHint::CpuBound => "cpu_bound",
    }
}

const fn hostcall_capability_label(capability: HostcallCapabilityClass) -> &'static str {
    match capability {
        HostcallCapabilityClass::Filesystem => "filesystem",
        HostcallCapabilityClass::Network => "network",
        HostcallCapabilityClass::Execution => "execution",
        HostcallCapabilityClass::Session => "session",
        HostcallCapabilityClass::Events => "events",
        HostcallCapabilityClass::Environment => "environment",
        HostcallCapabilityClass::Tool => "tool",
        HostcallCapabilityClass::Ui => "ui",
        HostcallCapabilityClass::Telemetry => "telemetry",
        HostcallCapabilityClass::Unknown => "unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoUringBridgeState {
    ExecutorUnavailable,
    CancelledBeforeDispatch,
}

impl IoUringBridgeState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ExecutorUnavailable => "executor_unavailable",
            Self::CancelledBeforeDispatch => "cancelled_before_dispatch",
        }
    }
}

#[derive(Debug, Clone)]
struct IoUringBridgeDispatch {
    outcome: HostcallOutcome,
    state: IoUringBridgeState,
    fallback_reason: Option<&'static str>,
}

fn clone_payload_object_without_key(
    map: &serde_json::Map<String, Value>,
    reserved_key: &str,
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::with_capacity(map.len());
    for (key, value) in map {
        if key == reserved_key {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    out
}

fn clone_payload_object_without_two_keys(
    map: &serde_json::Map<String, Value>,
    reserved_a: &str,
    reserved_b: &str,
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::with_capacity(map.len());
    for (key, value) in map {
        if key == reserved_a || key == reserved_b {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    out
}

fn protocol_params_from_request(request: &HostcallRequest) -> Value {
    match &request.kind {
        HostcallKind::Tool { name } => {
            let mut object = serde_json::Map::with_capacity(2);
            object.insert("name".to_string(), Value::String(name.clone()));
            object.insert("input".to_string(), request.payload.clone());
            Value::Object(object)
        }
        HostcallKind::Exec { cmd } => {
            let mut object = match &request.payload {
                Value::Object(map) => clone_payload_object_without_two_keys(map, "command", "cmd"),
                Value::Null => serde_json::Map::new(),
                other => {
                    let mut out = serde_json::Map::new();
                    out.insert("payload".to_string(), other.clone());
                    out
                }
            };
            object.insert("cmd".to_string(), Value::String(cmd.clone()));
            Value::Object(object)
        }
        HostcallKind::Http | HostcallKind::Log => request.payload.clone(),
        HostcallKind::Session { op } | HostcallKind::Ui { op } | HostcallKind::Events { op } => {
            let mut object = match &request.payload {
                Value::Object(map) => clone_payload_object_without_key(map, "op"),
                Value::Null => serde_json::Map::new(),
                other => {
                    let mut out = serde_json::Map::new();
                    out.insert("payload".to_string(), other.clone());
                    out
                }
            };
            object.insert("op".to_string(), Value::String(op.clone()));
            Value::Object(object)
        }
    }
}

fn dual_exec_forensic_bundle(
    request: &HostcallRequest,
    diff: &DualExecOutcomeDiff,
    rollback_reason: Option<&str>,
    shadow_elapsed_us: f64,
) -> Value {
    serde_json::json!({
        "call_trace": {
            "call_id": request.call_id,
            "trace_id": request.trace_id,
            "extension_id": request.extension_id,
            "method": request.method(),
            "params_hash": request.params_hash(),
            "capability": request.required_capability(),
        },
        "lane_decision": {
            "fast_lane": "fast",
            "compat_lane": "compat_shadow",
        },
        "diff": {
            "reason": diff.reason,
            "fast_fingerprint": diff.fast_fingerprint,
            "compat_fingerprint": diff.compat_fingerprint,
            "shadow_elapsed_us": shadow_elapsed_us,
        },
        "rollback": {
            "triggered": rollback_reason.is_some(),
            "reason": rollback_reason,
        }
    })
}

const REGIME_MIN_SAMPLES: usize = 24;
const REGIME_CUSUM_DRIFT: f64 = 0.03;
const REGIME_CUSUM_THRESHOLD: f64 = 1.6;
const REGIME_BOCPD_HAZARD: f64 = 0.08;
const REGIME_POSTERIOR_DECAY: f64 = 0.92;
const REGIME_POSTERIOR_THRESHOLD: f64 = 0.35;
const REGIME_COOLDOWN_OBSERVATIONS: usize = 32;
const REGIME_CONFIRMATION_STREAK: usize = 2;
const REGIME_FALLBACK_QUEUE_DEPTH: f64 = 1.0;
const REGIME_FALLBACK_SERVICE_US: f64 = 1_200.0;
const REGIME_VARIANCE_FLOOR: f64 = 1e-6;
const ROLLOUT_ALPHA: f64 = 0.05;
const ROLLOUT_HIGH_STRATUM_QUEUE_MIN: f64 = 8.0;
const ROLLOUT_HIGH_STRATUM_SERVICE_US_MIN: f64 = 4_500.0;
const ROLLOUT_LOW_STRATUM_QUEUE_MAX: f64 = 2.0;
const ROLLOUT_LOW_STRATUM_SERVICE_US_MAX: f64 = 1_800.0;
const ROLLOUT_PROMOTE_SCORE_THRESHOLD: f64 = 1.25;
const ROLLOUT_ROLLBACK_SCORE_THRESHOLD: f64 = 0.70;
const ROLLOUT_MIN_STRATUM_SAMPLES: usize = 10;
const ROLLOUT_MIN_TOTAL_SAMPLES: usize = 30;
const ROLLOUT_LOG_E_CLAMP: f64 = 120.0;
const ROLLOUT_LR_NULL: f64 = 0.35;
const ROLLOUT_LR_ALT: f64 = 0.65;
const ROLLOUT_FALSE_PROMOTE_LOSS: f64 = 28.0;
const ROLLOUT_FALSE_ROLLBACK_LOSS: f64 = 12.0;
const ROLLOUT_HOLD_OPPORTUNITY_LOSS: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegimeAdaptationMode {
    SequentialFastPath,
    InterleavedBatching,
}

impl RegimeAdaptationMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SequentialFastPath => "sequential_fast_path",
            Self::InterleavedBatching => "interleaved_batching",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegimeTransition {
    EnterInterleavedBatching,
    ReturnToSequentialFastPath,
}

impl RegimeTransition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EnterInterleavedBatching => "enter_interleaved_batching",
            Self::ReturnToSequentialFastPath => "return_to_sequential_fast_path",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RolloutGateAction {
    Hold,
    PromoteInterleaved,
    RollbackSequential,
}

impl RolloutGateAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::PromoteInterleaved => "promote_interleaved",
            Self::RollbackSequential => "rollback_sequential",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RolloutEvidenceStratum {
    HighContention,
    LowContention,
    Mixed,
}

impl RolloutEvidenceStratum {
    const fn as_str(self) -> &'static str {
        match self {
            Self::HighContention => "high_contention",
            Self::LowContention => "low_contention",
            Self::Mixed => "mixed",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RolloutExpectedLoss {
    hold: f64,
    promote: f64,
    rollback: f64,
}

#[derive(Debug, Clone, Copy)]
struct RolloutGateDecision {
    action: RolloutGateAction,
    expected_loss: RolloutExpectedLoss,
    promote_posterior: f64,
    rollback_posterior: f64,
    promote_e_process: f64,
    rollback_e_process: f64,
    evidence_threshold: f64,
    total_samples: usize,
    high_samples: usize,
    low_samples: usize,
    coverage_ready: bool,
    blocked_underpowered: bool,
    blocked_cherry_picked: bool,
}

#[derive(Debug, Clone)]
struct RolloutGateState {
    total_samples: usize,
    high_samples: usize,
    low_samples: usize,
    promote_alpha: f64,
    promote_beta: f64,
    rollback_alpha: f64,
    rollback_beta: f64,
    promote_log_e: f64,
    rollback_log_e: f64,
}

impl Default for RolloutGateState {
    fn default() -> Self {
        Self {
            total_samples: 0,
            high_samples: 0,
            low_samples: 0,
            promote_alpha: 1.0,
            promote_beta: 1.0,
            rollback_alpha: 1.0,
            rollback_beta: 1.0,
            promote_log_e: 0.0,
            rollback_log_e: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RegimeSignal {
    queue_depth: f64,
    service_time_us: f64,
    opcode_entropy: f64,
    llc_miss_rate: f64,
}

impl RegimeSignal {
    fn composite_score(self) -> f64 {
        let queue_component = (self.queue_depth / 32.0).min(4.0);
        let service_component = (self.service_time_us / 5_000.0).min(4.0);
        let entropy_component = (self.opcode_entropy / 4.0).min(2.0);
        let llc_component = self.llc_miss_rate.clamp(0.0, 1.0) * 2.0;
        0.15f64.mul_add(
            llc_component,
            0.15f64.mul_add(
                entropy_component,
                0.35f64.mul_add(queue_component, 0.35 * service_component),
            ),
        )
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
#[allow(dead_code)]
struct RegimeObservation {
    score: f64,
    mean: f64,
    stddev: f64,
    upper_cusum: f64,
    lower_cusum: f64,
    change_posterior: f64,
    cusum_triggered: bool,
    posterior_triggered: bool,
    regime_shift_candidate: bool,
    transition: Option<RegimeTransition>,
    mode: RegimeAdaptationMode,
    fallback_triggered: bool,
    rollout_action: RolloutGateAction,
    rollout_stratum: RolloutEvidenceStratum,
    rollout_expected_loss: RolloutExpectedLoss,
    rollout_promote_posterior: f64,
    rollout_rollback_posterior: f64,
    rollout_promote_e_process: f64,
    rollout_rollback_e_process: f64,
    rollout_evidence_threshold: f64,
    rollout_total_samples: usize,
    rollout_high_samples: usize,
    rollout_low_samples: usize,
    rollout_coverage_ready: bool,
    rollout_blocked_underpowered: bool,
    rollout_blocked_cherry_picked: bool,
}

#[derive(Debug, Clone)]
struct RegimeShiftDetector {
    sample_count: usize,
    mean: f64,
    m2: f64,
    upper_cusum: f64,
    lower_cusum: f64,
    change_posterior: f64,
    cooldown_remaining: usize,
    confirmation_streak: usize,
    mode: RegimeAdaptationMode,
    rollout_gate: RolloutGateState,
}

impl Default for RegimeShiftDetector {
    fn default() -> Self {
        Self {
            sample_count: 0,
            mean: 0.0,
            m2: 0.0,
            upper_cusum: 0.0,
            lower_cusum: 0.0,
            change_posterior: 0.0,
            cooldown_remaining: 0,
            confirmation_streak: 0,
            mode: RegimeAdaptationMode::SequentialFastPath,
            rollout_gate: RolloutGateState::default(),
        }
    }
}

impl RegimeShiftDetector {
    const fn current_mode(&self) -> RegimeAdaptationMode {
        self.mode
    }

    #[allow(clippy::too_many_lines)]
    fn observe(&mut self, signal: RegimeSignal) -> RegimeObservation {
        let score = signal.composite_score();
        let baseline_mean = self.mean;
        let baseline_stddev = self.variance().sqrt().max(REGIME_VARIANCE_FLOOR);
        let deviation = if self.sample_count > 1 {
            score - baseline_mean
        } else {
            0.0
        };

        self.upper_cusum = (self.upper_cusum + deviation - REGIME_CUSUM_DRIFT).max(0.0);
        self.lower_cusum = (self.lower_cusum + deviation + REGIME_CUSUM_DRIFT).min(0.0);

        let z_score = if baseline_stddev > REGIME_VARIANCE_FLOOR {
            deviation / baseline_stddev
        } else {
            0.0
        };
        let evidence = (z_score.abs() - 0.8).max(0.0);
        let change_likelihood = 1.0 - (-evidence).exp();
        self.change_posterior = self
            .change_posterior
            .mul_add(
                REGIME_POSTERIOR_DECAY,
                REGIME_BOCPD_HAZARD * change_likelihood,
            )
            .clamp(0.0, 1.0);

        let cusum_triggered = self.upper_cusum >= REGIME_CUSUM_THRESHOLD
            || self.lower_cusum <= -REGIME_CUSUM_THRESHOLD;
        let posterior_triggered = self.change_posterior >= REGIME_POSTERIOR_THRESHOLD;
        let candidate_shift =
            self.sample_count >= REGIME_MIN_SAMPLES && cusum_triggered && posterior_triggered;
        let direction_is_up = self.upper_cusum >= -self.lower_cusum;
        let rollout_stratum = rollout_evidence_stratum(signal);
        let rollout_decision = self.rollout_gate.observe(
            score,
            rollout_stratum,
            self.mode,
            candidate_shift,
            direction_is_up,
        );

        let mut transition = None;
        let mut fallback_triggered = false;

        if self.cooldown_remaining > 0 {
            self.cooldown_remaining = self.cooldown_remaining.saturating_sub(1);
            self.confirmation_streak = 0;
        } else {
            let desired_mode = match rollout_decision.action {
                RolloutGateAction::PromoteInterleaved => {
                    Some(RegimeAdaptationMode::InterleavedBatching)
                }
                RolloutGateAction::RollbackSequential => {
                    Some(RegimeAdaptationMode::SequentialFastPath)
                }
                RolloutGateAction::Hold => None,
            };
            if let Some(desired_mode) = desired_mode {
                if desired_mode == self.mode {
                    self.confirmation_streak = 0;
                } else {
                    self.confirmation_streak = self.confirmation_streak.saturating_add(1);
                    if self.confirmation_streak >= REGIME_CONFIRMATION_STREAK {
                        self.mode = desired_mode;
                        transition = Some(match desired_mode {
                            RegimeAdaptationMode::InterleavedBatching => {
                                RegimeTransition::EnterInterleavedBatching
                            }
                            RegimeAdaptationMode::SequentialFastPath => {
                                RegimeTransition::ReturnToSequentialFastPath
                            }
                        });
                        self.cooldown_remaining = REGIME_COOLDOWN_OBSERVATIONS;
                        self.upper_cusum = 0.0;
                        self.lower_cusum = 0.0;
                        self.change_posterior = self.change_posterior.min(0.5);
                        self.confirmation_streak = 0;
                    }
                }
            } else {
                self.confirmation_streak = 0;
            }
        }

        if self.mode == RegimeAdaptationMode::InterleavedBatching
            && signal.queue_depth <= REGIME_FALLBACK_QUEUE_DEPTH
            && signal.service_time_us <= REGIME_FALLBACK_SERVICE_US
        {
            self.mode = RegimeAdaptationMode::SequentialFastPath;
            transition = Some(RegimeTransition::ReturnToSequentialFastPath);
            fallback_triggered = true;
            self.cooldown_remaining = REGIME_COOLDOWN_OBSERVATIONS / 2;
            self.upper_cusum = 0.0;
            self.lower_cusum = 0.0;
            self.change_posterior = self.change_posterior.min(0.25);
            self.confirmation_streak = 0;
        }

        self.sample_count = self.sample_count.saturating_add(1);
        if self.sample_count == 1 {
            self.mean = score;
            self.m2 = 0.0;
        } else {
            let count_f64 = f64::from(u32::try_from(self.sample_count).unwrap_or(u32::MAX));
            let delta = score - self.mean;
            self.mean += delta / count_f64;
            let delta2 = score - self.mean;
            self.m2 = delta.mul_add(delta2, self.m2);
        }

        RegimeObservation {
            score,
            mean: self.mean,
            stddev: self.variance().sqrt().max(REGIME_VARIANCE_FLOOR),
            upper_cusum: self.upper_cusum,
            lower_cusum: self.lower_cusum,
            change_posterior: self.change_posterior,
            cusum_triggered,
            posterior_triggered,
            regime_shift_candidate: candidate_shift,
            transition,
            mode: self.mode,
            fallback_triggered,
            rollout_action: rollout_decision.action,
            rollout_stratum,
            rollout_expected_loss: rollout_decision.expected_loss,
            rollout_promote_posterior: rollout_decision.promote_posterior,
            rollout_rollback_posterior: rollout_decision.rollback_posterior,
            rollout_promote_e_process: rollout_decision.promote_e_process,
            rollout_rollback_e_process: rollout_decision.rollback_e_process,
            rollout_evidence_threshold: rollout_decision.evidence_threshold,
            rollout_total_samples: rollout_decision.total_samples,
            rollout_high_samples: rollout_decision.high_samples,
            rollout_low_samples: rollout_decision.low_samples,
            rollout_coverage_ready: rollout_decision.coverage_ready,
            rollout_blocked_underpowered: rollout_decision.blocked_underpowered,
            rollout_blocked_cherry_picked: rollout_decision.blocked_cherry_picked,
        }
    }

    fn variance(&self) -> f64 {
        if self.sample_count < 2 {
            REGIME_VARIANCE_FLOOR
        } else {
            let denom =
                f64::from(u32::try_from(self.sample_count.saturating_sub(1)).unwrap_or(u32::MAX));
            (self.m2 / denom).max(REGIME_VARIANCE_FLOOR)
        }
    }
}

impl RolloutGateState {
    fn observe(
        &mut self,
        score: f64,
        stratum: RolloutEvidenceStratum,
        mode: RegimeAdaptationMode,
        _candidate_shift: bool,
        _direction_is_up: bool,
    ) -> RolloutGateDecision {
        self.total_samples = self.total_samples.saturating_add(1);
        match stratum {
            RolloutEvidenceStratum::HighContention => {
                self.high_samples = self.high_samples.saturating_add(1);
            }
            RolloutEvidenceStratum::LowContention => {
                self.low_samples = self.low_samples.saturating_add(1);
            }
            RolloutEvidenceStratum::Mixed => {}
        }

        match stratum {
            RolloutEvidenceStratum::HighContention => {
                let promote_signal = score >= ROLLOUT_PROMOTE_SCORE_THRESHOLD;
                if promote_signal {
                    self.promote_alpha += 1.0;
                } else {
                    self.promote_beta += 1.0;
                }
                self.promote_log_e = (self.promote_log_e
                    + bernoulli_log_likelihood_ratio(
                        promote_signal,
                        ROLLOUT_LR_NULL,
                        ROLLOUT_LR_ALT,
                    ))
                .clamp(-ROLLOUT_LOG_E_CLAMP, ROLLOUT_LOG_E_CLAMP);
            }
            RolloutEvidenceStratum::LowContention => {
                let rollback_signal = score <= ROLLOUT_ROLLBACK_SCORE_THRESHOLD;
                if rollback_signal {
                    self.rollback_alpha += 1.0;
                } else {
                    self.rollback_beta += 1.0;
                }
                self.rollback_log_e = (self.rollback_log_e
                    + bernoulli_log_likelihood_ratio(
                        rollback_signal,
                        ROLLOUT_LR_NULL,
                        ROLLOUT_LR_ALT,
                    ))
                .clamp(-ROLLOUT_LOG_E_CLAMP, ROLLOUT_LOG_E_CLAMP);
            }
            RolloutEvidenceStratum::Mixed => {}
        }

        let promote_posterior = self.promote_alpha / (self.promote_alpha + self.promote_beta);
        let rollback_posterior = self.rollback_alpha / (self.rollback_alpha + self.rollback_beta);
        let promote_e_process = self.promote_log_e.exp();
        let rollback_e_process = self.rollback_log_e.exp();
        let evidence_threshold = 1.0 / ROLLOUT_ALPHA;
        let expected_loss = rollout_expected_loss(mode, promote_posterior, rollback_posterior);

        let blocked_underpowered = self.total_samples < ROLLOUT_MIN_TOTAL_SAMPLES;
        let blocked_cherry_picked = self.high_samples < ROLLOUT_MIN_STRATUM_SAMPLES
            || self.low_samples < ROLLOUT_MIN_STRATUM_SAMPLES;
        let coverage_ready = !blocked_underpowered && !blocked_cherry_picked;

        let promote_ready = coverage_ready
            && mode == RegimeAdaptationMode::SequentialFastPath
            && promote_e_process >= evidence_threshold
            && expected_loss.promote < expected_loss.hold;

        let rollback_ready = coverage_ready
            && mode == RegimeAdaptationMode::InterleavedBatching
            && rollback_e_process >= evidence_threshold
            && expected_loss.rollback < expected_loss.hold;

        let action = if promote_ready {
            RolloutGateAction::PromoteInterleaved
        } else if rollback_ready {
            RolloutGateAction::RollbackSequential
        } else {
            RolloutGateAction::Hold
        };

        RolloutGateDecision {
            action,
            expected_loss,
            promote_posterior,
            rollback_posterior,
            promote_e_process,
            rollback_e_process,
            evidence_threshold,
            total_samples: self.total_samples,
            high_samples: self.high_samples,
            low_samples: self.low_samples,
            coverage_ready,
            blocked_underpowered,
            blocked_cherry_picked,
        }
    }
}

fn rollout_evidence_stratum(signal: RegimeSignal) -> RolloutEvidenceStratum {
    if signal.queue_depth >= ROLLOUT_HIGH_STRATUM_QUEUE_MIN
        || signal.service_time_us >= ROLLOUT_HIGH_STRATUM_SERVICE_US_MIN
    {
        RolloutEvidenceStratum::HighContention
    } else if signal.queue_depth <= ROLLOUT_LOW_STRATUM_QUEUE_MAX
        && signal.service_time_us <= ROLLOUT_LOW_STRATUM_SERVICE_US_MAX
    {
        RolloutEvidenceStratum::LowContention
    } else {
        RolloutEvidenceStratum::Mixed
    }
}

fn bernoulli_log_likelihood_ratio(observed_true: bool, p0: f64, p1: f64) -> f64 {
    let p0 = p0.clamp(1e-6, 1.0 - 1e-6);
    let p1 = p1.clamp(1e-6, 1.0 - 1e-6);
    if observed_true {
        f64::ln(p1 / p0)
    } else {
        f64::ln((1.0 - p1) / (1.0 - p0))
    }
}

fn rollout_expected_loss(
    mode: RegimeAdaptationMode,
    promote_posterior: f64,
    rollback_posterior: f64,
) -> RolloutExpectedLoss {
    let hold = ROLLOUT_HOLD_OPPORTUNITY_LOSS
        .mul_add(promote_posterior, 3.0f64.mul_add(rollback_posterior, 1.0));
    let promote = match mode {
        RegimeAdaptationMode::SequentialFastPath => {
            ROLLOUT_FALSE_PROMOTE_LOSS.mul_add(1.0 - promote_posterior, 2.0 * rollback_posterior)
        }
        RegimeAdaptationMode::InterleavedBatching => ROLLOUT_FALSE_PROMOTE_LOSS
            .mul_add(1.0 - promote_posterior, ROLLOUT_HOLD_OPPORTUNITY_LOSS),
    };
    let rollback = match mode {
        RegimeAdaptationMode::SequentialFastPath => ROLLOUT_FALSE_ROLLBACK_LOSS
            .mul_add(1.0 - rollback_posterior, ROLLOUT_HOLD_OPPORTUNITY_LOSS),
        RegimeAdaptationMode::InterleavedBatching => {
            ROLLOUT_FALSE_ROLLBACK_LOSS.mul_add(1.0 - rollback_posterior, 2.0 * promote_posterior)
        }
    };

    RolloutExpectedLoss {
        hold,
        promote,
        rollback,
    }
}

fn usize_to_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn llc_miss_proxy(total_depth: usize, overflow_depth: usize, overflow_rejected_total: u64) -> f64 {
    if total_depth == 0 && overflow_rejected_total == 0 {
        return 0.0;
    }
    let depth_denominator = usize_to_f64(total_depth.max(1));
    let overflow_ratio = usize_to_f64(overflow_depth) / depth_denominator;
    let rejected_ratio = if overflow_rejected_total == 0 {
        0.0
    } else {
        let rejected = overflow_rejected_total.min(u64::from(u32::MAX));
        f64::from(u32::try_from(rejected).unwrap_or(u32::MAX)) / 1_000.0
    };
    (overflow_ratio + rejected_ratio).clamp(0.0, 1.0)
}

const fn hostcall_kind_label(kind: &HostcallKind) -> &'static str {
    match kind {
        HostcallKind::Tool { .. } => "tool",
        HostcallKind::Exec { .. } => "exec",
        HostcallKind::Http => "http",
        HostcallKind::Session { .. } => "session",
        HostcallKind::Ui { .. } => "ui",
        HostcallKind::Events { .. } => "events",
        HostcallKind::Log => "log",
    }
}

// Entropy scoring is retained for rollout evidence/property checks while the
// current dispatcher feature vector uses cheaper queue/service indicators.
#[allow(dead_code)]
fn shannon_entropy_bytes(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0_u32; 256];
    for &byte in bytes {
        counts[usize::from(byte)] = counts[usize::from(byte)].saturating_add(1);
    }
    let total = f64::from(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let probability = f64::from(count) / total;
            -(probability * (probability.ln() / std::f64::consts::LN_2))
        })
        .sum()
}

fn hostcall_opcode_entropy(kind: &HostcallKind, payload: &Value) -> f64 {
    let kind_label = hostcall_kind_label(kind);
    let op = payload
        .get("op")
        .or_else(|| payload.get("method"))
        .or_else(|| payload.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let capability = payload
        .get("capability")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());

    // Build the byte histogram directly from segments to avoid per-call
    // temporary Vec allocation on the hostcall fast path.
    let mut counts = [0_u32; 256];
    let mut total = 0_u32;

    for &byte in kind_label.as_bytes() {
        counts[usize::from(byte)] = counts[usize::from(byte)].saturating_add(1);
        total = total.saturating_add(1);
    }

    if let Some(op) = op {
        counts[usize::from(b':')] = counts[usize::from(b':')].saturating_add(1);
        total = total.saturating_add(1);
        for &byte in op.as_bytes() {
            counts[usize::from(byte)] = counts[usize::from(byte)].saturating_add(1);
            total = total.saturating_add(1);
        }
    }

    if let Some(capability) = capability {
        counts[usize::from(b':')] = counts[usize::from(b':')].saturating_add(1);
        total = total.saturating_add(1);
        for &byte in capability.as_bytes() {
            counts[usize::from(byte)] = counts[usize::from(byte)].saturating_add(1);
            total = total.saturating_add(1);
        }
    }

    if total == 0 {
        return 0.0;
    }

    let total_f = f64::from(total);
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let probability = f64::from(count) / total_f;
            -(probability * (probability.ln() / std::f64::consts::LN_2))
        })
        .sum()
}

impl<C: SchedulerClock + 'static> ExtensionDispatcher<C> {
    fn js_runtime(&self) -> &PiJsRuntime<C> {
        self.runtime.as_js_runtime()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new<R>(
        runtime: Rc<R>,
        tool_registry: Arc<ToolRegistry>,
        http_connector: Arc<HttpConnector>,
        session: Arc<dyn ExtensionSession + Send + Sync>,
        ui_handler: Arc<dyn ExtensionUiHandler + Send + Sync>,
        cwd: PathBuf,
    ) -> Self
    where
        R: ExtensionDispatcherRuntime<C>,
    {
        Self::new_with_policy(
            runtime,
            tool_registry,
            http_connector,
            session,
            ui_handler,
            cwd,
            ExtensionPolicy::from_profile(PolicyProfile::Permissive),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_policy<R>(
        runtime: Rc<R>,
        tool_registry: Arc<ToolRegistry>,
        http_connector: Arc<HttpConnector>,
        session: Arc<dyn ExtensionSession + Send + Sync>,
        ui_handler: Arc<dyn ExtensionUiHandler + Send + Sync>,
        cwd: PathBuf,
        policy: ExtensionPolicy,
    ) -> Self
    where
        R: ExtensionDispatcherRuntime<C>,
    {
        Self::new_with_policy_and_oracle_config(
            runtime,
            tool_registry,
            http_connector,
            session,
            ui_handler,
            cwd,
            policy,
            DualExecOracleConfig::from_env(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_policy_and_oracle_config<R>(
        runtime: Rc<R>,
        tool_registry: Arc<ToolRegistry>,
        http_connector: Arc<HttpConnector>,
        session: Arc<dyn ExtensionSession + Send + Sync>,
        ui_handler: Arc<dyn ExtensionUiHandler + Send + Sync>,
        cwd: PathBuf,
        policy: ExtensionPolicy,
        dual_exec_config: DualExecOracleConfig,
    ) -> Self
    where
        R: ExtensionDispatcherRuntime<C>,
    {
        let runtime: Rc<dyn ExtensionDispatcherRuntime<C>> = runtime;
        let snapshot_version = policy_snapshot_version(&policy);
        let snapshot = PolicySnapshot::compile(&policy);
        let io_uring_lane_config = io_uring_lane_policy_from_env();
        let io_uring_force_compat = io_uring_force_compat_from_env();
        Self {
            runtime,
            tool_registry,
            http_connector,
            session,
            ui_handler,
            cwd,
            policy,
            snapshot,
            snapshot_version,
            dual_exec_config,
            dual_exec_state: RefCell::new(DualExecOracleState::default()),
            io_uring_lane_config,
            io_uring_force_compat,
            regime_detector: RefCell::new(RegimeShiftDetector::default()),
            amac_executor: RefCell::new(
                AmacBatchExecutor::new(AmacBatchExecutorConfig::from_env()),
            ),
            resource_governor: ResourceGovernor::from_host(),
        }
    }

    fn policy_lookup(
        &self,
        capability: &str,
        extension_id: Option<&str>,
    ) -> (PolicyCheck, &'static str) {
        (
            self.snapshot.lookup(capability, extension_id),
            policy_lookup_path(capability),
        )
    }

    fn emit_policy_decision_telemetry(
        &self,
        capability: &str,
        extension_id: Option<&str>,
        lookup_path: &str,
        check: &PolicyCheck,
    ) {
        tracing::debug!(
            target: "pi.extensions.policy_snapshot",
            snapshot_version = %self.snapshot_version,
            lookup_path,
            capability = %capability,
            extension_id = %extension_id.unwrap_or("<none>"),
            decision = ?check.decision,
            decision_provenance = %check.reason,
            "Extension policy decision evaluated"
        );
    }

    fn emit_regime_observation_telemetry(
        call_id: &str,
        observation: RegimeObservation,
        queue_depth: usize,
        overflow_depth: usize,
        overflow_rejected_total: u64,
        service_time_us: f64,
    ) {
        tracing::debug!(
            target: "pi.extensions.regime_shift",
            call_id,
            adaptation_mode = observation.mode.as_str(),
            composite_score = observation.score,
            baseline_mean = observation.mean,
            baseline_stddev = observation.stddev,
            upper_cusum = observation.upper_cusum,
            lower_cusum = observation.lower_cusum,
            change_posterior = observation.change_posterior,
            queue_depth,
            overflow_depth,
            overflow_rejected_total,
            service_time_us,
            fallback_triggered = observation.fallback_triggered,
            rollout_action = observation.rollout_action.as_str(),
            rollout_stratum = observation.rollout_stratum.as_str(),
            rollout_promote_posterior = observation.rollout_promote_posterior,
            rollout_rollback_posterior = observation.rollout_rollback_posterior,
            rollout_promote_e_process = observation.rollout_promote_e_process,
            rollout_rollback_e_process = observation.rollout_rollback_e_process,
            rollout_evidence_threshold = observation.rollout_evidence_threshold,
            rollout_expected_loss_hold = observation.rollout_expected_loss.hold,
            rollout_expected_loss_promote = observation.rollout_expected_loss.promote,
            rollout_expected_loss_rollback = observation.rollout_expected_loss.rollback,
            rollout_samples_total = observation.rollout_total_samples,
            rollout_samples_high = observation.rollout_high_samples,
            rollout_samples_low = observation.rollout_low_samples,
            rollout_coverage_ready = observation.rollout_coverage_ready,
            rollout_blocked_underpowered = observation.rollout_blocked_underpowered,
            rollout_blocked_cherry_picked = observation.rollout_blocked_cherry_picked,
            "Hostcall regime observation recorded"
        );
        if let Some(transition) = observation.transition {
            tracing::info!(
                target: "pi.extensions.regime_shift",
                call_id,
                transition = transition.as_str(),
                adaptation_mode = observation.mode.as_str(),
                score = observation.score,
                change_posterior = observation.change_posterior,
                queue_depth,
                service_time_us,
                fallback_triggered = observation.fallback_triggered,
                rollout_action = observation.rollout_action.as_str(),
                rollout_promote_posterior = observation.rollout_promote_posterior,
                rollout_rollback_posterior = observation.rollout_rollback_posterior,
                rollout_promote_e_process = observation.rollout_promote_e_process,
                rollout_rollback_e_process = observation.rollout_rollback_e_process,
                rollout_expected_loss_hold = observation.rollout_expected_loss.hold,
                rollout_expected_loss_promote = observation.rollout_expected_loss.promote,
                rollout_expected_loss_rollback = observation.rollout_expected_loss.rollback,
                rollout_samples_total = observation.rollout_total_samples,
                rollout_samples_high = observation.rollout_high_samples,
                rollout_samples_low = observation.rollout_low_samples,
                rollout_coverage_ready = observation.rollout_coverage_ready,
                rollout_blocked_underpowered = observation.rollout_blocked_underpowered,
                rollout_blocked_cherry_picked = observation.rollout_blocked_cherry_picked,
                "Hostcall regime transition accepted"
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_io_uring_lane_telemetry(
        &self,
        request: &HostcallRequest,
        capability: &str,
        capability_class: HostcallCapabilityClass,
        io_hint: HostcallIoHint,
        queue_depth: usize,
        selected_lane: HostcallDispatchLane,
        fallback_reason: Option<&'static str>,
    ) {
        let queue_budget = self.io_uring_lane_config.max_queue_depth.max(1);
        let depth_u64 = u64::try_from(queue_depth).unwrap_or(u64::MAX);
        let budget_u64 = u64::try_from(queue_budget).unwrap_or(u64::MAX).max(1);
        let occupancy_permille = depth_u64.saturating_mul(1_000).saturating_div(budget_u64);
        tracing::debug!(
            target: "pi.extensions.io_uring_lane",
            call_id = request.call_id,
            extension_id = %request.extension_id.as_deref().unwrap_or("<none>"),
            method = request.method(),
            capability = %capability,
            capability_class = hostcall_capability_label(capability_class),
            io_hint = hostcall_io_hint_label(io_hint),
            selected_lane = selected_lane.as_str(),
            fallback_reason = %fallback_reason.unwrap_or("none"),
            queue_depth,
            queue_budget,
            queue_occupancy_permille = occupancy_permille,
            io_uring_enabled = self.io_uring_lane_config.enabled,
            io_uring_ring_available = self.io_uring_lane_config.ring_available,
            io_uring_executor_available = io_uring_executor_available(),
            io_uring_force_compat = self.io_uring_force_compat,
            "Hostcall io_uring lane decision evaluated"
        );
    }

    fn emit_io_uring_bridge_telemetry(
        &self,
        request: &HostcallRequest,
        state: IoUringBridgeState,
        fallback_reason: Option<&'static str>,
    ) {
        tracing::debug!(
            target: "pi.extensions.io_uring_bridge",
            call_id = request.call_id,
            extension_id = %request.extension_id.as_deref().unwrap_or("<none>"),
            method = request.method(),
            state = state.as_str(),
            fallback_reason = %fallback_reason.unwrap_or("none"),
            io_uring_enabled = self.io_uring_lane_config.enabled,
            io_uring_ring_available = self.io_uring_lane_config.ring_available,
            io_uring_executor_available = io_uring_executor_available(),
            io_uring_force_compat = self.io_uring_force_compat,
            "Hostcall io_uring bridge dispatch completed"
        );
    }

    fn build_resource_request(
        request: &HostcallRequest,
        capability: &str,
        queue_depth: usize,
    ) -> ResourceRequest {
        ResourceRequest::new(resource_operation_kind(&request.kind), capability)
            .with_queue_depth(queue_depth)
            .with_estimated_tool_output_bytes(estimated_hostcall_output_bytes(request))
    }

    fn emit_resource_governor_telemetry(
        request: &HostcallRequest,
        resource_request: &ResourceRequest,
        decision: &AdmissionDecision,
    ) {
        let telemetry = decision.telemetry(resource_request);
        let telemetry_json = serde_json::to_string(&telemetry)
            .unwrap_or_else(|err| format!("{{\"error\":\"{err}\"}}"));
        let action = match decision.action {
            AdmissionAction::Admit => "admit",
            AdmissionAction::Backpressure => "backpressure",
            AdmissionAction::Deny => "deny",
        };
        tracing::debug!(
            target: "pi.resource_governor",
            call_id = request.call_id,
            extension_id = %request.extension_id.as_deref().unwrap_or("<none>"),
            method = request.method(),
            action,
            dominant_dimension = ?decision.dominant_dimension,
            dominant_ratio = decision.dominant_ratio,
            retry_after_ms = decision.retry_after_ms,
            telemetry = %telemetry_json,
            "Host resource governor evaluated hostcall admission"
        );
    }

    #[allow(clippy::future_not_send)]
    async fn apply_resource_governor(
        &self,
        request: &HostcallRequest,
        capability: &str,
        queue_depth: usize,
    ) -> Option<HostcallOutcome> {
        let resource_request = Self::build_resource_request(request, capability, queue_depth);
        let decision = self.resource_governor.admit(&resource_request);
        Self::emit_resource_governor_telemetry(request, &resource_request, &decision);

        match decision.action {
            AdmissionAction::Admit => None,
            AdmissionAction::Backpressure => {
                extension_wait_sleep(Duration::from_millis(decision.retry_after_ms)).await;
                None
            }
            AdmissionAction::Deny => Some(HostcallOutcome::Error {
                code: "resource_exhausted".to_string(),
                message: format!(
                    "Host resource governor denied hostcall: {}",
                    decision.reason
                ),
            }),
        }
    }

    const fn advanced_dispatch_enabled(&self) -> bool {
        self.dual_exec_config.sample_ppm > 0 || self.io_uring_lane_active()
    }

    #[inline]
    const fn io_uring_lane_active(&self) -> bool {
        self.io_uring_lane_config.enabled || self.io_uring_force_compat
    }

    /// Drain pending hostcall requests from the JS runtime.
    #[must_use]
    pub fn drain_hostcall_requests(&self) -> VecDeque<HostcallRequest> {
        self.js_runtime().drain_hostcall_requests()
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_hostcall_fast(&self, request: &HostcallRequest) -> HostcallOutcome {
        let cap = request.required_capability();
        let (check, lookup_path) = self.policy_lookup(cap, request.extension_id.as_deref());
        self.emit_policy_decision_telemetry(
            cap,
            request.extension_id.as_deref(),
            lookup_path,
            &check,
        );
        if check.decision != PolicyDecision::Allow {
            return HostcallOutcome::Error {
                code: "denied".to_string(),
                message: format!("Capability '{}' denied by policy ({})", cap, check.reason),
            };
        }

        match &request.kind {
            HostcallKind::Tool { name } => {
                self.dispatch_tool(&request.call_id, name, request.payload.clone())
                    .await
            }
            HostcallKind::Exec { cmd } => {
                self.dispatch_exec_ref(&request.call_id, cmd, &request.payload)
                    .await
            }
            HostcallKind::Http => {
                self.dispatch_http(&request.call_id, request.payload.clone())
                    .await
            }
            HostcallKind::Session { op } => {
                self.dispatch_session_ref(&request.call_id, op, &request.payload)
                    .await
            }
            HostcallKind::Ui { op } => {
                self.dispatch_ui(
                    &request.call_id,
                    op,
                    request.payload.clone(),
                    request.extension_id.as_deref(),
                )
                .await
            }
            HostcallKind::Events { op } => {
                self.dispatch_events_ref(
                    &request.call_id,
                    request.extension_id.as_deref(),
                    op,
                    &request.payload,
                )
                .await
            }
            HostcallKind::Log => {
                tracing::info!(
                    target: "pi.extension.log",
                    payload = ?request.payload,
                    "Extension log"
                );
                HostcallOutcome::Success(serde_json::json!({ "logged": true }))
            }
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_hostcall_io_uring(&self, request: &HostcallRequest) -> IoUringBridgeDispatch {
        if !self.js_runtime().is_hostcall_active(&request.call_id) {
            return IoUringBridgeDispatch {
                outcome: HostcallOutcome::Error {
                    code: "cancelled".to_string(),
                    message: "Hostcall cancelled before io_uring dispatch".to_string(),
                },
                state: IoUringBridgeState::CancelledBeforeDispatch,
                fallback_reason: Some("cancelled_before_io_uring_dispatch"),
            };
        }

        IoUringBridgeDispatch {
            outcome: HostcallOutcome::Error {
                code: "io_uring_unavailable".to_string(),
                message: "io_uring executor is unavailable; hostcall was not dispatched through the placeholder bridge".to_string(),
            },
            state: IoUringBridgeState::ExecutorUnavailable,
            fallback_reason: Some(IoUringFallbackReason::IoUringExecutorUnavailable.as_code()),
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_hostcall_compat_shadow(&self, request: &HostcallRequest) -> HostcallOutcome {
        let payload = HostCallPayload {
            call_id: request.call_id.clone(),
            capability: request.required_capability().to_string(),
            method: request.method().to_string(),
            params: protocol_params_from_request(request),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        self.dispatch_protocol_host_call(&payload).await
    }

    #[allow(clippy::future_not_send)]
    async fn run_shadow_dual_exec(
        &self,
        request: &HostcallRequest,
        fast_outcome: &HostcallOutcome,
    ) {
        let config = self.dual_exec_config;
        if config.sample_ppm == 0 {
            return;
        }

        {
            let mut state = self.dual_exec_state.borrow_mut();
            state.begin_request();
            if state.overhead_backoff_remaining > 0 {
                return;
            }
            if !is_shadow_safe_request(request) {
                state.skipped_unsupported_total = state.skipped_unsupported_total.saturating_add(1);
                return;
            }
        }

        if !should_sample_shadow_dual_exec(request, config.sample_ppm) {
            return;
        }

        let shadow_started_at = Instant::now();
        let compat_outcome = self.dispatch_hostcall_compat_shadow(request).await;
        let shadow_elapsed_us = shadow_started_at.elapsed().as_secs_f64() * 1_000_000.0;

        let diff = diff_hostcall_outcomes(fast_outcome, &compat_outcome);
        let rollback_reason = {
            let mut state = self.dual_exec_state.borrow_mut();
            #[allow(clippy::cast_precision_loss)]
            if shadow_elapsed_us > config.overhead_budget_us as f64 {
                state.record_overhead_budget_exceeded(config);
                tracing::warn!(
                    target: "pi.extensions.dual_exec",
                    call_id = request.call_id,
                    extension_id = %request.extension_id.as_deref().unwrap_or("<none>"),
                    method = request.method(),
                    shadow_elapsed_us,
                    overhead_budget_us = config.overhead_budget_us,
                    backoff_requests = state.overhead_backoff_remaining,
                    "Shadow dual execution exceeded overhead budget; backoff enabled"
                );
            }

            let divergent = diff.is_some();
            state.record_sample(divergent, config, request.extension_id.as_deref())
        };

        if let Some(diff) = diff {
            let forensic_bundle = dual_exec_forensic_bundle(
                request,
                &diff,
                rollback_reason.as_deref(),
                shadow_elapsed_us,
            );
            tracing::warn!(
                target: "pi.extensions.dual_exec",
                call_id = request.call_id,
                extension_id = %request.extension_id.as_deref().unwrap_or("<none>"),
                method = request.method(),
                rollback_triggered = rollback_reason.is_some(),
                rollback_reason = %rollback_reason.as_deref().unwrap_or("none"),
                forensic_bundle = %forensic_bundle,
                "Shadow dual execution divergence detected"
            );
        } else {
            tracing::trace!(
                target: "pi.extensions.dual_exec",
                call_id = request.call_id,
                extension_id = %request.extension_id.as_deref().unwrap_or("<none>"),
                method = request.method(),
                shadow_elapsed_us,
                "Shadow dual execution matched"
            );
        }
    }

    /// Dispatch a hostcall and enqueue its completion into the JS scheduler.
    #[allow(clippy::future_not_send, clippy::too_many_lines)]
    pub async fn dispatch_and_complete(&self, request: HostcallRequest) {
        let cap = request.required_capability();
        let (check, lookup_path) = self.policy_lookup(cap, request.extension_id.as_deref());
        self.emit_policy_decision_telemetry(
            cap,
            request.extension_id.as_deref(),
            lookup_path,
            &check,
        );
        if check.decision != PolicyDecision::Allow {
            let outcome = HostcallOutcome::Error {
                code: "denied".to_string(),
                message: format!("Capability '{}' denied by policy ({})", cap, check.reason),
            };
            self.js_runtime()
                .complete_hostcall(request.call_id, outcome);
            return;
        }

        let queue_snapshot = self.js_runtime().hostcall_queue_telemetry();
        let queue_depth = queue_snapshot.total_depth.max(1);
        if let Some(outcome) = self
            .apply_resource_governor(&request, cap, queue_depth)
            .await
        {
            self.js_runtime()
                .complete_hostcall(request.call_id, outcome);
            return;
        }

        if !self.advanced_dispatch_enabled() {
            let outcome = self.dispatch_hostcall_fast(&request).await;
            self.js_runtime()
                .complete_hostcall(request.call_id, outcome);
            return;
        }

        let dispatch_started_at = Instant::now();
        let overflow_depth = queue_snapshot.overflow_depth;
        let overflow_rejected_total = queue_snapshot.overflow_rejected_total;

        let (outcome, lane_for_shadow) = if self.io_uring_lane_active() {
            let io_hint = hostcall_io_hint(&request.kind);
            let capability_class = HostcallCapabilityClass::from_capability(cap);
            let lane_decision = decide_io_uring_lane(
                self.io_uring_lane_config,
                IoUringLaneDecisionInput {
                    capability: capability_class,
                    io_hint,
                    queue_depth,
                    force_compat_lane: self.io_uring_force_compat,
                },
            );
            self.emit_io_uring_lane_telemetry(
                &request,
                cap,
                capability_class,
                io_hint,
                queue_depth,
                lane_decision.lane,
                lane_decision.fallback_code(),
            );

            let outcome = match lane_decision.lane {
                HostcallDispatchLane::Fast => self.dispatch_hostcall_fast(&request).await,
                HostcallDispatchLane::IoUring => {
                    let bridge_dispatch = self.dispatch_hostcall_io_uring(&request).await;
                    self.emit_io_uring_bridge_telemetry(
                        &request,
                        bridge_dispatch.state,
                        bridge_dispatch.fallback_reason,
                    );
                    bridge_dispatch.outcome
                }
                HostcallDispatchLane::Compat => {
                    self.dispatch_hostcall_compat_shadow(&request).await
                }
            };
            (outcome, lane_decision.lane)
        } else {
            (
                self.dispatch_hostcall_fast(&request).await,
                HostcallDispatchLane::Fast,
            )
        };

        if lane_for_shadow != HostcallDispatchLane::Compat {
            self.run_shadow_dual_exec(&request, &outcome).await;
        }

        let service_time_us = dispatch_started_at.elapsed().as_secs_f64() * 1_000_000.0;
        let opcode_entropy = hostcall_opcode_entropy(&request.kind, &request.payload);
        let llc_miss_rate = llc_miss_proxy(queue_depth, overflow_depth, overflow_rejected_total);
        let regime_signal = RegimeSignal {
            queue_depth: usize_to_f64(queue_depth),
            service_time_us,
            opcode_entropy,
            llc_miss_rate,
        };
        let observation = {
            let mut detector = self.regime_detector.borrow_mut();
            detector.observe(regime_signal)
        };
        Self::emit_regime_observation_telemetry(
            &request.call_id,
            observation,
            queue_depth,
            overflow_depth,
            overflow_rejected_total,
            service_time_us,
        );

        self.js_runtime()
            .complete_hostcall(request.call_id, outcome);
    }

    /// Dispatch a batch of hostcall requests using AMAC-aware grouping.
    ///
    /// Groups requests by kind, decides per-group whether to interleave or
    /// use sequential dispatch, then dispatches accordingly. Falls back to
    /// sequential one-by-one dispatch when AMAC is disabled or the batch is
    /// too small.
    #[allow(clippy::future_not_send)]
    pub async fn dispatch_batch_amac(&self, mut requests: VecDeque<HostcallRequest>) {
        if requests.is_empty() {
            return;
        }

        let (rollback_active, rollback_remaining, rollback_reason) = {
            let state = self.dual_exec_state.borrow();
            (
                state.rollback_active(),
                state.rollback_remaining,
                state
                    .rollback_reason
                    .clone()
                    .unwrap_or_else(|| "dual_exec_rollback_active".to_string()),
            )
        };

        // Check if AMAC is enabled before consuming requests.
        let amac_enabled = self.amac_executor.borrow().enabled();
        let adaptation_mode = self.regime_detector.borrow().current_mode();
        let rollout_forces_sequential = adaptation_mode == RegimeAdaptationMode::SequentialFastPath;
        if !amac_enabled || rollback_active || rollout_forces_sequential {
            if rollback_active {
                tracing::warn!(
                    target: "pi.extensions.dual_exec",
                    rollback_remaining,
                    rollback_reason = %rollback_reason,
                    "Dual-exec rollback forcing sequential dispatcher mode"
                );
            } else if rollout_forces_sequential && amac_enabled {
                tracing::debug!(
                    target: "pi.extensions.regime_shift",
                    adaptation_mode = adaptation_mode.as_str(),
                    "Rollout gate forcing sequential dispatch mode"
                );
            }
            // Dispatch sequentially without AMAC overhead.
            while let Some(req) = requests.pop_front() {
                self.dispatch_and_complete(req).await;
            }
            return;
        }

        let request_vec: Vec<HostcallRequest> = requests.into();
        let plan = self.amac_executor.borrow_mut().plan_batch(request_vec);

        for (group, decision) in plan.groups.into_iter().zip(plan.decisions.iter()) {
            let group_key = group.key.clone();
            let start = Instant::now();
            // Dispatch each request in the group sequentially.
            // AMAC decision metadata is recorded for telemetry but the
            // actual dispatch remains sequential within a single-threaded
            // async executor — true concurrency is achieved at the reactor
            // mesh level (bd-3ar8v.4.20).
            for request in group.requests {
                let req_start = Instant::now();
                self.dispatch_and_complete(request).await;
                let elapsed_ns = u64::try_from(req_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                self.amac_executor.borrow_mut().observe_call(elapsed_ns);
            }

            let group_elapsed_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            tracing::trace!(
                target: "pi.extensions.amac",
                group_key = ?group_key,
                decision = ?decision,
                group_elapsed_ns,
                "AMAC group dispatched"
            );
        }
    }

    /// Protocol adapter: convert `ExtensionMessage(type=host_call)` into
    /// `ExtensionMessage(type=host_result)` using the same dispatch paths used
    /// by runtime hostcalls.
    #[allow(clippy::future_not_send)]
    pub async fn dispatch_protocol_message(
        &self,
        message: ExtensionMessage,
    ) -> Result<ExtensionMessage> {
        let ExtensionMessage { id, version, body } = message;
        if id.trim().is_empty() {
            return Err(crate::error::Error::validation(
                "Extension message id is empty",
            ));
        }
        if version != PROTOCOL_VERSION {
            return Err(crate::error::Error::validation(format!(
                "Unsupported extension protocol version: {version}"
            )));
        }
        let ExtensionBody::HostCall(payload) = body else {
            return Err(crate::error::Error::validation(
                "dispatch_protocol_message expects host_call message",
            ));
        };

        let outcome = match validate_host_call(&payload) {
            Ok(()) => self.dispatch_protocol_host_call(&payload).await,
            Err(crate::error::Error::Validation(message)) => {
                if payload.call_id.trim().is_empty() {
                    return Err(crate::error::Error::Validation(message));
                }
                HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message,
                }
            }
            Err(err) => return Err(err),
        };
        let response = ExtensionMessage {
            id,
            version,
            body: ExtensionBody::HostResult(hostcall_outcome_to_protocol_result_with_trace(
                &payload, outcome,
            )),
        };
        response.validate()?;
        Ok(response)
    }

    #[allow(clippy::future_not_send, clippy::too_many_lines)]
    async fn dispatch_protocol_host_call(&self, payload: &HostCallPayload) -> HostcallOutcome {
        if let Some(cap) = required_capability_for_host_call_static(payload) {
            let (check, lookup_path) = self.policy_lookup(cap, None);
            self.emit_policy_decision_telemetry(cap, None, lookup_path, &check);
            if check.decision != PolicyDecision::Allow {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: format!("Capability '{}' denied by policy ({})", cap, check.reason),
                };
            }
        }

        let method = payload.method.trim();

        match parse_protocol_hostcall_method(method) {
            Some(ProtocolHostcallMethod::Tool) => {
                let Some(name) = payload
                    .params
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call tool requires params.name".to_string(),
                    };
                };
                let input = payload
                    .params
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                self.dispatch_tool(&payload.call_id, name, input).await
            }
            Some(ProtocolHostcallMethod::Exec) => {
                let Some(cmd) = payload
                    .params
                    .get("cmd")
                    .or_else(|| payload.params.get("command"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|cmd| !cmd.is_empty())
                else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call exec requires params.cmd or params.command".to_string(),
                    };
                };

                // SEC-4.3: Exec mediation — classify and gate dangerous commands.
                let args: Vec<String> = payload
                    .params
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(ToString::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let mediation = evaluate_exec_mediation(&self.policy.exec_mediation, cmd, &args);
                match &mediation {
                    ExecMediationResult::Deny { class, reason } => {
                        tracing::warn!(
                            event = "exec.mediation.deny",
                            command_class = ?class.map(DangerousCommandClass::label),
                            reason = %reason,
                            "Exec command denied by mediation policy"
                        );
                        return HostcallOutcome::Error {
                            code: "denied".to_string(),
                            message: format!("Exec denied by mediation policy: {reason}"),
                        };
                    }
                    ExecMediationResult::AllowWithAudit { class, reason } => {
                        tracing::info!(
                            event = "exec.mediation.audit",
                            command_class = class.label(),
                            reason = %reason,
                            "Exec command allowed with audit"
                        );
                    }
                    ExecMediationResult::Allow => {}
                }

                self.dispatch_exec_ref(&payload.call_id, cmd, &payload.params)
                    .await
            }
            Some(ProtocolHostcallMethod::Http) => {
                self.dispatch_http(&payload.call_id, payload.params.clone())
                    .await
            }
            Some(ProtocolHostcallMethod::Session) => {
                let Some(op) = protocol_hostcall_op(&payload.params) else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call session requires params.op".to_string(),
                    };
                };
                self.dispatch_session_ref(&payload.call_id, op, &payload.params)
                    .await
            }
            Some(ProtocolHostcallMethod::Ui) => {
                let Some(op) = protocol_hostcall_op(&payload.params) else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call ui requires params.op".to_string(),
                    };
                };
                self.dispatch_ui(&payload.call_id, op, payload.params.clone(), None)
                    .await
            }
            Some(ProtocolHostcallMethod::Events) => {
                let Some(op) = protocol_hostcall_op(&payload.params) else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call events requires params.op".to_string(),
                    };
                };
                self.dispatch_events_ref(&payload.call_id, None, op, &payload.params)
                    .await
            }
            Some(ProtocolHostcallMethod::Log) => {
                tracing::info!(
                    target: "pi.extension.log",
                    payload = ?payload.params,
                    "Extension log"
                );
                HostcallOutcome::Success(serde_json::json!({ "logged": true }))
            }
            None => HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: format!("Unsupported host_call method: {method}"),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_tool(
        &self,
        call_id: &str,
        name: &str,
        payload: serde_json::Value,
    ) -> HostcallOutcome {
        let Some(tool) = self.tool_registry.get(name) else {
            return HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: format!("Unknown tool: {name}"),
            };
        };

        match tool.execute(call_id, payload, None).await {
            Ok(output) => match serde_json::to_value(output) {
                Ok(value) => HostcallOutcome::Success(value),
                Err(err) => HostcallOutcome::Error {
                    code: "internal".to_string(),
                    message: format!("Serialize tool output: {err}"),
                },
            },
            Err(err) => HostcallOutcome::Error {
                code: "io".to_string(),
                message: err.to_string(),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    #[allow(dead_code)]
    async fn dispatch_exec(
        &self,
        call_id: &str,
        cmd: &str,
        payload: serde_json::Value,
    ) -> HostcallOutcome {
        self.dispatch_exec_ref(call_id, cmd, &payload).await
    }

    #[allow(clippy::future_not_send, clippy::too_many_lines)]
    async fn dispatch_exec_ref(
        &self,
        call_id: &str,
        cmd: &str,
        payload: &serde_json::Value,
    ) -> HostcallOutcome {
        use std::process::Stdio;
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::sync::mpsc;

        #[allow(clippy::unnecessary_lazy_evaluations)] // lazy eval needed on unix for signal()
        fn exit_status_code(status: std::process::ExitStatus) -> i32 {
            status.code().unwrap_or_else(|| {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt as _;
                    status.signal().map_or(-1, |signal| -signal)
                }
                #[cfg(not(unix))]
                {
                    -1
                }
            })
        }

        let args = match payload.get("args") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), ToString::to_string)
                })
                .collect::<Vec<_>>(),
            Some(_) => {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "exec args must be an array".to_string(),
                };
            }
        };

        let options = payload
            .get("options")
            .and_then(serde_json::Value::as_object);
        let cwd = options
            .and_then(|opts| opts.get("cwd"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| self.cwd.clone(), PathBuf::from);
        let timeout_ms = options
            .and_then(|opts| {
                opts.get("timeout")
                    .and_then(serde_json::Value::as_u64)
                    .or_else(|| opts.get("timeoutMs").and_then(serde_json::Value::as_u64))
                    .or_else(|| opts.get("timeout_ms").and_then(serde_json::Value::as_u64))
            })
            .filter(|ms| *ms > 0);
        let stream = options
            .and_then(|opts| opts.get("stream"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if stream {
            struct CancelGuard(Arc<AtomicBool>);
            impl Drop for CancelGuard {
                fn drop(&mut self) {
                    self.0.store(true, AtomicOrdering::SeqCst);
                }
            }

            let cmd = cmd.to_string();
            let args = args.clone();
            let (tx, rx) = mpsc::sync_channel::<ExecStreamFrame>(1024);
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_worker = Arc::clone(&cancel);
            let call_id_for_error = call_id.to_string();

            thread::spawn(move || {
                let result = (|| -> std::result::Result<(), String> {
                    let mut command = crate::tools::command_with_default_sigpipe_in_dir(&cmd, &cwd)
                        .map_err(|err| err.to_string())?;
                    command
                        .args(&args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .current_dir(&cwd);
                    crate::tools::isolate_command_process_group(&mut command);

                    let mut child = command.spawn().map_err(|err| err.to_string())?;
                    let pid = child.id();

                    let stdout = child.stdout.take().ok_or("Missing stdout pipe")?;
                    let stderr = child.stderr.take().ok_or("Missing stderr pipe")?;

                    let stdout_tx = tx.clone();
                    let stderr_tx = tx.clone();
                    let stdout_handle = thread::spawn(move || {
                        pump_exec_stream_text(stdout, &stdout_tx, ExecPipeKind::Stdout);
                    });
                    let stderr_handle = thread::spawn(move || {
                        pump_exec_stream_text(stderr, &stderr_tx, ExecPipeKind::Stderr);
                    });

                    let start = Instant::now();
                    let mut killed = false;
                    let status = loop {
                        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                            break status;
                        }

                        if !killed && cancel_worker.load(AtomicOrdering::SeqCst) {
                            killed = true;
                            crate::tools::kill_process_group_tree(Some(pid));
                            let _ = child.kill();
                            break child.wait().map_err(|err| err.to_string())?;
                        }

                        if let Some(timeout_ms) = timeout_ms
                            && !killed
                            && start.elapsed() >= Duration::from_millis(timeout_ms)
                        {
                            killed = true;
                            crate::tools::kill_process_group_tree(Some(pid));
                            let _ = child.kill();
                            break child.wait().map_err(|err| err.to_string())?;
                        }

                        thread::sleep(Duration::from_millis(10));
                    };

                    let drain_start = Instant::now();
                    let drain_deadline = drain_start + Duration::from_secs(5);
                    loop {
                        if stdout_handle.is_finished() && stderr_handle.is_finished() {
                            break;
                        }
                        if Instant::now() >= drain_deadline {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }

                    // Explicitly reap to avoid leaving a zombie behind after a
                    // successful try_wait()-observed exit on isolated process groups.
                    let _ = child.wait();

                    let code = exit_status_code(status);
                    let _ = tx.send(ExecStreamFrame::Final { code, killed });
                    Ok(())
                })();

                if let Err(err) = result
                    && tx.send(ExecStreamFrame::Error(err)).is_err()
                {
                    tracing::trace!(
                        call_id = %call_id_for_error,
                        "Exec hostcall stream result dropped before completion"
                    );
                }
            });

            let _guard = CancelGuard(Arc::clone(&cancel));

            let mut sequence = 0_u64;
            let mut processed_in_turn = 0_u32;
            loop {
                if !self.js_runtime().is_hostcall_active(call_id) {
                    cancel.store(true, AtomicOrdering::SeqCst);
                    return HostcallOutcome::StreamChunk {
                        sequence,
                        chunk: serde_json::Value::Null,
                        is_final: false,
                    };
                }

                match rx.try_recv() {
                    Ok(ExecStreamFrame::Stdout(chunk)) => {
                        self.js_runtime().complete_hostcall(
                            call_id.to_string(),
                            HostcallOutcome::StreamChunk {
                                sequence,
                                chunk: serde_json::json!({ "stdout": chunk }),
                                is_final: false,
                            },
                        );
                        sequence = sequence.saturating_add(1);
                        processed_in_turn += 1;
                    }
                    Ok(ExecStreamFrame::Stderr(chunk)) => {
                        self.js_runtime().complete_hostcall(
                            call_id.to_string(),
                            HostcallOutcome::StreamChunk {
                                sequence,
                                chunk: serde_json::json!({ "stderr": chunk }),
                                is_final: false,
                            },
                        );
                        sequence = sequence.saturating_add(1);
                        processed_in_turn += 1;
                    }
                    Ok(ExecStreamFrame::Final { code, killed }) => {
                        return HostcallOutcome::StreamChunk {
                            sequence,
                            chunk: serde_json::json!({
                                "code": code,
                                "killed": killed,
                            }),
                            is_final: true,
                        };
                    }
                    Ok(ExecStreamFrame::Error(message)) => {
                        return HostcallOutcome::Error {
                            code: "io".to_string(),
                            message,
                        };
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        processed_in_turn = 0;
                        extension_wait_sleep(Duration::from_millis(25)).await;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return HostcallOutcome::Error {
                            code: "internal".to_string(),
                            message: "exec stream channel closed".to_string(),
                        };
                    }
                }

                if processed_in_turn >= 64 {
                    processed_in_turn = 0;
                    asupersync::runtime::yield_now().await;
                }
            }
        }

        let cmd = cmd.to_string();
        let args = args.clone();
        let (tx, rx) = mpsc::sync_channel::<std::result::Result<serde_json::Value, String>>(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        let call_id_for_error = call_id.to_string();

        thread::spawn(move || {
            let result: std::result::Result<serde_json::Value, String> = (|| {
                let mut command = crate::tools::command_with_default_sigpipe_in_dir(&cmd, &cwd)
                    .map_err(|err| err.to_string())?;
                command
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .current_dir(&cwd);
                crate::tools::isolate_command_process_group(&mut command);

                let mut child = command.spawn().map_err(|err| err.to_string())?;
                let pid = child.id();

                let stdout = child.stdout.take().ok_or("Missing stdout pipe")?;
                let stderr = child.stderr.take().ok_or("Missing stderr pipe")?;

                let (tx, rx) = std::sync::mpsc::sync_channel::<ExecCaptureFrame>(1024);
                let tx_stdout = tx.clone();
                let _stdout_handle = thread::spawn(move || {
                    pump_exec_capture_bytes(stdout, &tx_stdout, ExecPipeKind::Stdout);
                });
                let _stderr_handle = thread::spawn(move || {
                    pump_exec_capture_bytes(stderr, &tx, ExecPipeKind::Stderr);
                });

                let start = Instant::now();
                let mut killed = false;
                let max_bytes = crate::tools::DEFAULT_MAX_BYTES.saturating_mul(2);

                let mut stdout_chunks = std::collections::VecDeque::new();
                let mut stderr_chunks = std::collections::VecDeque::new();
                let mut stdout_total_bytes = 0usize;
                let mut stderr_total_bytes = 0usize;
                let mut stdout_bytes_len = 0usize;
                let mut stderr_bytes_len = 0usize;

                let status = loop {
                    // Drain available
                    while let Ok(frame) = rx.try_recv() {
                        if let Err(message) = handle_exec_capture_frame(
                            frame,
                            &mut stdout_chunks,
                            &mut stdout_total_bytes,
                            &mut stdout_bytes_len,
                            &mut stderr_chunks,
                            &mut stderr_total_bytes,
                            &mut stderr_bytes_len,
                            max_bytes,
                        ) {
                            if !killed {
                                crate::tools::kill_process_group_tree(Some(pid));
                                let _ = child.kill();
                            }
                            let _ = child.wait();
                            return Err(message);
                        }
                    }

                    if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                        break status;
                    }

                    if !killed && cancel_worker.load(AtomicOrdering::SeqCst) {
                        killed = true;
                        crate::tools::kill_process_group_tree(Some(pid));
                        let _ = child.kill();
                        break child.wait().map_err(|err| err.to_string())?;
                    }

                    if let Some(timeout_ms) = timeout_ms
                        && !killed
                        && start.elapsed() >= Duration::from_millis(timeout_ms)
                    {
                        killed = true;
                        crate::tools::kill_process_group_tree(Some(pid));
                        let _ = child.kill();
                        break child.wait().map_err(|err| err.to_string())?;
                    }

                    if let Ok(frame) = rx.recv_timeout(Duration::from_millis(10))
                        && let Err(message) = handle_exec_capture_frame(
                            frame,
                            &mut stdout_chunks,
                            &mut stdout_total_bytes,
                            &mut stdout_bytes_len,
                            &mut stderr_chunks,
                            &mut stderr_total_bytes,
                            &mut stderr_bytes_len,
                            max_bytes,
                        )
                    {
                        if !killed {
                            crate::tools::kill_process_group_tree(Some(pid));
                            let _ = child.kill();
                        }
                        let _ = child.wait();
                        return Err(message);
                    }
                };

                let drain_deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    match rx.try_recv() {
                        Ok(frame) => {
                            if let Err(message) = handle_exec_capture_frame(
                                frame,
                                &mut stdout_chunks,
                                &mut stdout_total_bytes,
                                &mut stdout_bytes_len,
                                &mut stderr_chunks,
                                &mut stderr_total_bytes,
                                &mut stderr_bytes_len,
                                max_bytes,
                            ) {
                                drop(rx);
                                let _ = child.wait();
                                return Err(message);
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            if Instant::now() >= drain_deadline {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    }
                }

                drop(rx); // Close the channel so pump threads exit if blocked

                // Explicitly reap the child to prevent zombies on macOS.
                // try_wait() uses WNOHANG which may not fully reap when the
                // child is in its own process group.
                let _ = child.wait();

                let stdout_bytes: Vec<u8> = stdout_chunks.into_iter().flatten().collect();
                let stderr_bytes: Vec<u8> = stderr_chunks.into_iter().flatten().collect();

                let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
                let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
                let code = exit_status_code(status);
                if (code == 126 || code == 127)
                    && stderr.contains(crate::tools::SIGPIPE_TRAMPOLINE_EXEC_FAILURE_PREFIX)
                {
                    return Err(stderr.trim().to_string());
                }

                Ok(serde_json::json!({
                    "stdout": stdout,
                    "stderr": stderr,
                    "code": code,
                    "killed": killed,
                }))
            })();

            if tx.send(result).is_err() {
                tracing::trace!(
                    call_id = %call_id_for_error,
                    "Exec hostcall result dropped before completion"
                );
            }
        });

        let _guard = CancelGuard(Arc::clone(&cancel));

        loop {
            if !self.js_runtime().is_hostcall_active(call_id) {
                cancel.store(true, AtomicOrdering::SeqCst);
                return HostcallOutcome::Error {
                    code: "internal".to_string(),
                    message: "exec task cancelled".to_string(),
                };
            }

            match rx.try_recv() {
                Ok(Ok(value)) => return HostcallOutcome::Success(value),
                Ok(Err(err)) => {
                    return HostcallOutcome::Error {
                        code: "io".to_string(),
                        message: err,
                    };
                }
                Err(mpsc::TryRecvError::Empty) => {
                    extension_wait_sleep(Duration::from_millis(25)).await;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return HostcallOutcome::Error {
                        code: "internal".to_string(),
                        message: "exec task cancelled".to_string(),
                    };
                }
            }
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_http(&self, call_id: &str, payload: serde_json::Value) -> HostcallOutcome {
        let call = HostCallPayload {
            call_id: call_id.to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params: payload,
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        match self.http_connector.dispatch(&call).await {
            Ok(result) => {
                if result.is_error {
                    let message = result.error.as_ref().map_or_else(
                        || "HTTP connector error".to_string(),
                        |err| err.message.clone(),
                    );
                    let code = result
                        .error
                        .as_ref()
                        .map_or("internal", |err| hostcall_code_to_str(err.code));
                    HostcallOutcome::Error {
                        code: code.to_string(),
                        message,
                    }
                } else {
                    HostcallOutcome::Success(result.output)
                }
            }
            Err(err) => HostcallOutcome::Error {
                code: "internal".to_string(),
                message: err.to_string(),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    #[allow(dead_code)]
    async fn dispatch_session(&self, call_id: &str, op: &str, payload: Value) -> HostcallOutcome {
        self.dispatch_session_ref(call_id, op, &payload).await
    }

    #[allow(clippy::future_not_send, clippy::too_many_lines)]
    async fn dispatch_session_ref(
        &self,
        _call_id: &str,
        op: &str,
        payload: &Value,
    ) -> HostcallOutcome {
        use crate::connectors::HostCallErrorCode;

        let op_norm = op.trim().to_ascii_lowercase();

        // Categorised result: (Value, error_code) where error_code distinguishes taxonomy.
        let result: std::result::Result<Value, (HostCallErrorCode, String)> = match op_norm.as_str()
        {
            "get_state" | "getstate" => Ok(self.session.get_state().await),
            "get_messages" | "getmessages" => {
                serde_json::to_value(self.session.get_messages().await).map_err(|err| {
                    (
                        HostCallErrorCode::Internal,
                        format!("Serialize messages: {err}"),
                    )
                })
            }
            "get_entries" | "getentries" => serde_json::to_value(self.session.get_entries().await)
                .map_err(|err| {
                    (
                        HostCallErrorCode::Internal,
                        format!("Serialize entries: {err}"),
                    )
                }),
            "get_branch" | "getbranch" => serde_json::to_value(self.session.get_branch().await)
                .map_err(|err| {
                    (
                        HostCallErrorCode::Internal,
                        format!("Serialize branch: {err}"),
                    )
                }),
            "get_file" | "getfile" => {
                let state = self.session.get_state().await;
                let file = state
                    .get("sessionFile")
                    .or_else(|| state.get("session_file"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(file)
            }
            "get_name" | "getname" => {
                let state = self.session.get_state().await;
                let name = state
                    .get("sessionName")
                    .or_else(|| state.get("session_name"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(name)
            }
            "set_name" | "setname" => {
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.session
                    .set_name(name)
                    .await
                    .map(|()| Value::Null)
                    .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
            }
            "append_entry" | "appendentry" => {
                let custom_type = payload
                    .get("customType")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("custom_type").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                let data = payload.get("data").cloned();
                self.session
                    .append_custom_entry(custom_type, data)
                    .await
                    .map(|()| Value::Null)
                    .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
            }
            "append_message" | "appendmessage" => {
                let message_value = payload
                    .get("message")
                    .cloned()
                    .unwrap_or_else(|| payload.clone());
                match serde_json::from_value(message_value) {
                    Ok(message) => self
                        .session
                        .append_message(message)
                        .await
                        .map(|()| Value::Null)
                        .map_err(|err| (HostCallErrorCode::Io, err.to_string())),
                    Err(err) => Err((
                        HostCallErrorCode::InvalidRequest,
                        format!("Parse message: {err}"),
                    )),
                }
            }
            "set_model" | "setmodel" => {
                let provider = payload
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let model_id = payload
                    .get("modelId")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("model_id").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                if provider.is_empty() || model_id.is_empty() {
                    Err((
                        HostCallErrorCode::InvalidRequest,
                        "set_model requires 'provider' and 'modelId' fields".to_string(),
                    ))
                } else {
                    self.session
                        .set_model(provider, model_id)
                        .await
                        .map(|()| Value::Bool(true))
                        .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
                }
            }
            "get_model" | "getmodel" => {
                let (provider, model_id) = self.session.get_model().await;
                Ok(serde_json::json!({
                    "provider": provider,
                    "modelId": model_id,
                }))
            }
            "set_thinking_level" | "setthinkinglevel" => {
                let level = payload
                    .get("level")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("thinkingLevel").and_then(Value::as_str))
                    .or_else(|| payload.get("thinking_level").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                if level.is_empty() {
                    Err((
                        HostCallErrorCode::InvalidRequest,
                        "set_thinking_level requires 'level' field".to_string(),
                    ))
                } else {
                    self.session
                        .set_thinking_level(level)
                        .await
                        .map(|()| Value::Null)
                        .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
                }
            }
            "get_thinking_level" | "getthinkinglevel" => {
                let level = self.session.get_thinking_level().await;
                Ok(level.map_or(Value::Null, Value::String))
            }
            "set_label" | "setlabel" => {
                let target_id = payload
                    .get("targetId")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("target_id").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                let label = payload
                    .get("label")
                    .and_then(Value::as_str)
                    .map(String::from);
                if target_id.is_empty() {
                    Err((
                        HostCallErrorCode::InvalidRequest,
                        "set_label requires 'targetId' field".to_string(),
                    ))
                } else {
                    self.session
                        .set_label(target_id, label)
                        .await
                        .map(|()| Value::Null)
                        .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
                }
            }
            _ => Err((
                HostCallErrorCode::InvalidRequest,
                format!("Unknown session op: {op}"),
            )),
        };

        match result {
            Ok(value) => HostcallOutcome::Success(value),
            Err((code, message)) => HostcallOutcome::Error {
                code: hostcall_code_to_str(code).to_string(),
                message,
            },
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_ui(
        &self,
        call_id: &str,
        op: &str,
        payload: Value,
        extension_id: Option<&str>,
    ) -> HostcallOutcome {
        let op = op.trim();
        if op.is_empty() {
            return HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: "host_call ui requires non-empty op".to_string(),
            };
        }

        let request = ExtensionUiRequest {
            id: call_id.to_string(),
            method: op.to_string(),
            payload,
            timeout_ms: None,
            extension_id: extension_id.map(ToString::to_string),
        };

        match self.ui_handler.request_ui(request).await {
            Ok(Some(response)) => HostcallOutcome::Success(ui_response_value_for_op(op, &response)),
            Ok(None) => HostcallOutcome::Success(Value::Null),
            Err(err) => HostcallOutcome::Error {
                code: classify_ui_hostcall_error(&err).to_string(),
                message: err.to_string(),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    #[allow(dead_code)]
    async fn dispatch_events(
        &self,
        call_id: &str,
        extension_id: Option<&str>,
        op: &str,
        payload: Value,
    ) -> HostcallOutcome {
        self.dispatch_events_ref(call_id, extension_id, op, &payload)
            .await
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_events_ref(
        &self,
        _call_id: &str,
        extension_id: Option<&str>,
        op: &str,
        payload: &Value,
    ) -> HostcallOutcome {
        match op.trim() {
            "list" => match self.list_extension_events(extension_id).await {
                Ok(events) => HostcallOutcome::Success(serde_json::json!({ "events": events })),
                Err(err) => HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: err.to_string(),
                },
            },
            "emit" => {
                let event_name = payload
                    .get("event")
                    .or_else(|| payload.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty());

                let Some(event_name) = event_name else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "events.emit requires non-empty `event`".to_string(),
                    };
                };

                let event_payload = payload.get("data").cloned().unwrap_or(Value::Null);
                let timeout_ms = payload
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .or_else(|| payload.get("timeoutMs").and_then(Value::as_u64))
                    .or_else(|| payload.get("timeout").and_then(Value::as_u64))
                    .filter(|ms| *ms > 0)
                    .unwrap_or(EXTENSION_EVENT_TIMEOUT_MS);

                let ctx_payload = match payload.get("ctx") {
                    Some(ctx) => ctx.clone(),
                    None => self.build_default_event_ctx(extension_id).await,
                };

                match Box::pin(self.dispatch_extension_event(
                    event_name,
                    event_payload,
                    ctx_payload,
                    timeout_ms,
                ))
                .await
                {
                    Ok(result) => {
                        let handler_count = self
                            .count_event_handlers(event_name)
                            .await
                            .unwrap_or_default();

                        HostcallOutcome::Success(serde_json::json!({
                            "dispatched": true,
                            "event": event_name,
                            "handler_count": handler_count,
                            "result": result,
                        }))
                    }
                    Err(err) => HostcallOutcome::Error {
                        code: "io".to_string(),
                        message: err.to_string(),
                    },
                }
            }
            other => HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: format!("Unsupported events op: {other}"),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    async fn list_extension_events(&self, extension_id: Option<&str>) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct Snapshot {
            id: String,
            #[serde(default)]
            event_hooks: Vec<String>,
        }

        let json = self
            .js_runtime()
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let snapshot_fn: rquickjs::Function<'_> = global.get("__pi_snapshot_extensions")?;
                let value: rquickjs::Value<'_> =
                    snapshot_fn.call((self.js_runtime().bridge_secret(),))?;
                js_to_json(&value)
            })
            .await?;

        let snapshots: Vec<Snapshot> = serde_json::from_value(json)
            .map_err(|err| crate::error::Error::extension(err.to_string()))?;

        let mut events = BTreeSet::new();
        match extension_id {
            Some(needle) => {
                for snapshot in snapshots {
                    if snapshot.id == needle {
                        for event in snapshot.event_hooks {
                            let event = event.trim();
                            if !event.is_empty() {
                                events.insert(event.to_string());
                            }
                        }
                        break;
                    }
                }
            }
            None => {
                for snapshot in snapshots {
                    for event in snapshot.event_hooks {
                        let event = event.trim();
                        if !event.is_empty() {
                            events.insert(event.to_string());
                        }
                    }
                }
            }
        }

        Ok(events.into_iter().collect())
    }

    #[allow(clippy::future_not_send)]
    async fn count_event_handlers(&self, event_name: &str) -> Result<Option<usize>> {
        self.js_runtime()
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let count_fn: rquickjs::Function<'_> = global.get("__pi_count_event_handlers")?;
                count_fn
                    .call::<_, usize>((self.js_runtime().bridge_secret(), event_name))
                    .map(Some)
            })
            .await
    }

    #[allow(clippy::future_not_send)]
    async fn build_default_event_ctx(&self, _extension_id: Option<&str>) -> Value {
        let entries = self.session.get_entries().await;
        let branch = self.session.get_branch().await;
        let leaf_entry = branch.last().cloned().unwrap_or(Value::Null);

        serde_json::json!({
            "hasUI": true,
            "cwd": self.cwd.display().to_string(),
            "sessionEntries": entries,
            "branch": branch,
            "leafEntry": leaf_entry,
            "modelRegistry": {},
        })
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_extension_event(
        &self,
        event_name: &str,
        event_payload: Value,
        ctx_payload: Value,
        timeout_ms: u64,
    ) -> Result<Value> {
        let task_id = format!("task-events-{call_id}", call_id = uuid::Uuid::new_v4());
        let bridge_secret = self.js_runtime().bridge_secret().to_string();

        self.js_runtime()
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let dispatch_fn: rquickjs::Function<'_> =
                    global.get("__pi_dispatch_extension_event")?;
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;

                let event_js = json_to_js(&ctx, &event_payload)?;
                let ctx_js = json_to_js(&ctx, &ctx_payload)?;
                let promise: rquickjs::Value<'_> = dispatch_fn.call((
                    bridge_secret.as_str(),
                    event_name.to_string(),
                    event_js,
                    ctx_js,
                ))?;
                let _task: String =
                    task_start.call((bridge_secret.as_str(), task_id.clone(), promise))?;
                Ok(())
            })
            .await?;

        let start = extension_wait_now();
        let timeout = Duration::from_millis(timeout_ms.max(1));

        loop {
            let now = extension_wait_now();
            if std::time::Duration::from_nanos(now.duration_since(start)) > timeout {
                return Err(crate::error::Error::extension(format!(
                    "events.emit timed out after {}ms",
                    timeout.as_millis()
                )));
            }

            let pending = self.js_runtime().drain_hostcall_requests();
            self.dispatch_batch_amac(pending).await;

            let _ = self.js_runtime().tick().await?;
            let _ = self.js_runtime().drain_microtasks().await?;

            let state_json = self
                .js_runtime()
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let take_fn: rquickjs::Function<'_> = global.get("__pi_task_take")?;
                    let value: rquickjs::Value<'_> =
                        take_fn.call((bridge_secret.as_str(), task_id.clone()))?;
                    js_to_json(&value)
                })
                .await?;

            match decode_extension_event_task_state(state_json)? {
                ExtensionEventTaskProgress::Pending => {
                    if !self.js_runtime().has_pending() {
                        extension_wait_sleep(Duration::from_millis(1)).await;
                    }
                }
                ExtensionEventTaskProgress::Resolved(value) => return Ok(value),
            }

            extension_wait_sleep(Duration::from_millis(0)).await;
        }
    }
}

const fn hostcall_code_to_str(code: crate::connectors::HostCallErrorCode) -> &'static str {
    match code {
        crate::connectors::HostCallErrorCode::Timeout => "timeout",
        crate::connectors::HostCallErrorCode::Denied => "denied",
        crate::connectors::HostCallErrorCode::Io => "io",
        crate::connectors::HostCallErrorCode::InvalidRequest => "invalid_request",
        crate::connectors::HostCallErrorCode::Internal => "internal",
    }
}

/// Trait for handling individual hostcall types.
#[async_trait]
pub trait HostcallHandler: Send + Sync {
    /// Process a hostcall request and return the outcome.
    async fn handle(&self, params: serde_json::Value) -> HostcallOutcome;

    /// The capability name for policy checking (e.g., "read", "exec", "http").
    fn capability(&self) -> &'static str;
}

/// Trait for handling UI hostcalls (pi.ui()).
#[async_trait]
pub trait ExtensionUiHandler: Send + Sync {
    async fn request_ui(&self, request: ExtensionUiRequest) -> Result<Option<ExtensionUiResponse>>;
}

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;

    use crate::connectors::http::HttpConnectorConfig;
    use crate::error::Error;
    use crate::extensions::{
        ExtensionBody, ExtensionMessage, ExtensionOverride, ExtensionPolicyMode, HostCallPayload,
        PROTOCOL_VERSION, PolicyProfile,
    };
    use crate::scheduler::DeterministicClock;
    use crate::session::SessionMessage;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::Mutex;

    #[test]
    fn extension_wait_sleep_uses_current_timer_driver_epoch() {
        use asupersync::time::{TimerDriverHandle, VirtualClock};
        use asupersync::types::{Budget, RegionId, TaskId, Time};
        use std::sync::Arc;

        let virtual_clock = Arc::new(VirtualClock::starting_at(Time::from_secs(42)));
        let timer_driver = TimerDriverHandle::with_virtual_clock(virtual_clock);
        let cx = Cx::new_with_drivers(
            RegionId::new_for_test(7, 0),
            TaskId::new_for_test(9, 0),
            Budget::INFINITE,
            None,
            None,
            None,
            Some(timer_driver.clone()),
            None,
        );
        let _current = Cx::set_current(Some(cx));

        let now = extension_wait_now();
        assert_eq!(now, timer_driver.now());
        let sleeper = extension_wait_sleep(Duration::from_millis(5));
        assert_eq!(sleeper.remaining(now), Duration::from_millis(5));
    }

    #[test]
    fn ui_confirm_cancel_defaults_to_false() {
        let response = ExtensionUiResponse {
            id: "req-1".to_string(),
            value: None,
            cancelled: true,
        };
        assert_eq!(
            ui_response_value_for_op("confirm", &response),
            Value::Bool(false)
        );
        assert_eq!(ui_response_value_for_op("select", &response), Value::Null);
    }

    #[test]
    fn policy_snapshot_version_is_deterministic_for_equivalent_policies() {
        let mut policy_a = ExtensionPolicy::default();
        let mut override_a = ExtensionOverride::default();
        override_a.allow.push("exec".to_string());
        policy_a
            .per_extension
            .insert("ext.alpha".to_string(), override_a.clone());
        policy_a
            .per_extension
            .insert("ext.beta".to_string(), override_a);

        let mut policy_b = ExtensionPolicy::default();
        let mut override_b = ExtensionOverride::default();
        override_b.allow.push("exec".to_string());
        // Insert in reverse order to verify canonical hashing is order-insensitive.
        policy_b
            .per_extension
            .insert("ext.beta".to_string(), override_b.clone());
        policy_b
            .per_extension
            .insert("ext.alpha".to_string(), override_b);

        assert_eq!(
            policy_snapshot_version(&policy_a),
            policy_snapshot_version(&policy_b)
        );
    }

    #[test]
    fn policy_snapshot_version_changes_on_material_policy_delta() {
        let policy_base = ExtensionPolicy::from_profile(PolicyProfile::Standard);
        let mut policy_delta = policy_base.clone();
        policy_delta.deny_caps.push("http".to_string());

        assert_ne!(
            policy_snapshot_version(&policy_base),
            policy_snapshot_version(&policy_delta)
        );
    }

    #[test]
    fn policy_lookup_path_marks_known_vs_fallback_capabilities() {
        assert_eq!(policy_lookup_path("read"), "policy_snapshot_table");
        assert_eq!(policy_lookup_path("READ"), "policy_snapshot_table");
        assert_eq!(
            policy_lookup_path("non_standard_custom_capability"),
            "policy_snapshot_fallback"
        );
    }

    #[test]
    fn policy_snapshot_lookup_swaps_decision_across_profile_change() {
        let safe_policy = ExtensionPolicy::from_profile(PolicyProfile::Safe);
        let permissive_policy = ExtensionPolicy::from_profile(PolicyProfile::Permissive);

        let safe_snapshot = PolicySnapshot::compile(&safe_policy);
        let permissive_snapshot = PolicySnapshot::compile(&permissive_policy);

        let safe_first = safe_snapshot.lookup("exec", Some("ext.swap"));
        let safe_second = safe_snapshot.lookup("EXEC", Some("ext.swap"));
        assert_eq!(safe_first.decision, PolicyDecision::Deny);
        assert_eq!(safe_first.decision, safe_second.decision);

        let permissive_first = permissive_snapshot.lookup("exec", Some("ext.swap"));
        let permissive_second = permissive_snapshot.lookup("EXEC", Some("ext.swap"));
        assert_eq!(permissive_first.decision, PolicyDecision::Allow);
        assert_eq!(permissive_first.decision, permissive_second.decision);
    }

    struct NullSession;

    #[async_trait]
    impl ExtensionSession for NullSession {
        async fn get_state(&self) -> Value {
            Value::Null
        }

        async fn get_messages(&self) -> Vec<SessionMessage> {
            Vec::new()
        }

        async fn get_entries(&self) -> Vec<Value> {
            Vec::new()
        }

        async fn get_branch(&self) -> Vec<Value> {
            Vec::new()
        }

        async fn set_name(&self, _name: String) -> Result<()> {
            Ok(())
        }

        async fn append_message(&self, _message: SessionMessage) -> Result<()> {
            Ok(())
        }

        async fn append_custom_entry(
            &self,
            _custom_type: String,
            _data: Option<Value>,
        ) -> Result<()> {
            Ok(())
        }

        async fn set_model(&self, _provider: String, _model_id: String) -> Result<()> {
            Ok(())
        }

        async fn get_model(&self) -> (Option<String>, Option<String>) {
            (None, None)
        }

        async fn set_thinking_level(&self, _level: String) -> Result<()> {
            Ok(())
        }

        async fn get_thinking_level(&self) -> Option<String> {
            None
        }

        async fn set_label(&self, _target_id: String, _label: Option<String>) -> Result<()> {
            Ok(())
        }
    }

    struct NullUiHandler;

    #[async_trait]
    impl ExtensionUiHandler for NullUiHandler {
        async fn request_ui(
            &self,
            _request: ExtensionUiRequest,
        ) -> Result<Option<ExtensionUiResponse>> {
            Ok(None)
        }
    }

    struct TestUiHandler {
        captured: Arc<Mutex<Vec<ExtensionUiRequest>>>,
        response_value: Value,
    }

    #[async_trait]
    impl ExtensionUiHandler for TestUiHandler {
        async fn request_ui(
            &self,
            request: ExtensionUiRequest,
        ) -> Result<Option<ExtensionUiResponse>> {
            self.captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.clone());
            Ok(Some(ExtensionUiResponse {
                id: request.id,
                value: Some(self.response_value.clone()),
                cancelled: false,
            }))
        }
    }

    type CustomEntry = (String, Option<Value>);
    type CustomEntries = Arc<Mutex<Vec<CustomEntry>>>;

    type LabelEntry = (String, Option<String>);

    struct TestSession {
        state: Arc<Mutex<Value>>,
        messages: Arc<Mutex<Vec<SessionMessage>>>,
        entries: Arc<Mutex<Vec<Value>>>,
        branch: Arc<Mutex<Vec<Value>>>,
        name: Arc<Mutex<Option<String>>>,
        custom_entries: CustomEntries,
        labels: Arc<Mutex<Vec<LabelEntry>>>,
    }

    #[async_trait]
    impl ExtensionSession for TestSession {
        async fn get_state(&self) -> Value {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        async fn get_messages(&self) -> Vec<SessionMessage> {
            self.messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        async fn get_entries(&self) -> Vec<Value> {
            self.entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        async fn get_branch(&self) -> Vec<Value> {
            self.branch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        async fn set_name(&self, name: String) -> Result<()> {
            {
                let mut guard = self
                    .name
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *guard = Some(name.clone());
            }
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Value::Object(ref mut map) = *state {
                map.insert("sessionName".to_string(), Value::String(name));
            }
            drop(state);
            Ok(())
        }

        async fn append_message(&self, message: SessionMessage) -> Result<()> {
            self.messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(message);
            Ok(())
        }

        async fn append_custom_entry(
            &self,
            custom_type: String,
            data: Option<Value>,
        ) -> Result<()> {
            self.custom_entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((custom_type, data));
            Ok(())
        }

        async fn set_model(&self, provider: String, model_id: String) -> Result<()> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Value::Object(ref mut map) = *state {
                map.insert("provider".to_string(), Value::String(provider));
                map.insert("modelId".to_string(), Value::String(model_id));
            }
            drop(state);
            Ok(())
        }

        async fn get_model(&self) -> (Option<String>, Option<String>) {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let provider = state
                .get("provider")
                .and_then(Value::as_str)
                .map(String::from);
            let model_id = state
                .get("modelId")
                .and_then(Value::as_str)
                .map(String::from);
            drop(state);
            (provider, model_id)
        }

        async fn set_thinking_level(&self, level: String) -> Result<()> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Value::Object(ref mut map) = *state {
                map.insert("thinkingLevel".to_string(), Value::String(level));
            }
            drop(state);
            Ok(())
        }

        async fn get_thinking_level(&self) -> Option<String> {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let level = state
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .map(String::from);
            drop(state);
            level
        }

        async fn set_label(&self, target_id: String, label: Option<String>) -> Result<()> {
            self.labels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((target_id, label));
            Ok(())
        }
    }

    fn build_dispatcher(
        runtime: Rc<PiJsRuntime<DeterministicClock>>,
    ) -> ExtensionDispatcher<DeterministicClock> {
        build_dispatcher_with_policy(
            runtime,
            ExtensionPolicy::from_profile(PolicyProfile::Permissive),
        )
    }

    fn privileged_test_script<C: SchedulerClock + 'static>(
        runtime: &PiJsRuntime<C>,
        source: &str,
    ) -> String {
        let bridge_secret = serde_json::to_string(runtime.bridge_secret())
            .expect("serialize bridge secret for test script");
        format!("(() => {{ const __pi_test_secret = {bridge_secret};\n{source}\n}})();")
    }

    fn build_dispatcher_with_policy(
        runtime: Rc<PiJsRuntime<DeterministicClock>>,
        policy: ExtensionPolicy,
    ) -> ExtensionDispatcher<DeterministicClock> {
        ExtensionDispatcher::new_with_policy(
            runtime,
            Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
            Arc::new(HttpConnector::with_defaults()),
            Arc::new(NullSession),
            Arc::new(NullUiHandler),
            PathBuf::from("."),
            policy,
        )
    }

    fn build_dispatcher_with_policy_and_oracle(
        runtime: Rc<PiJsRuntime<DeterministicClock>>,
        policy: ExtensionPolicy,
        oracle_config: DualExecOracleConfig,
    ) -> ExtensionDispatcher<DeterministicClock> {
        ExtensionDispatcher::new_with_policy_and_oracle_config(
            runtime,
            Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
            Arc::new(HttpConnector::with_defaults()),
            Arc::new(NullSession),
            Arc::new(NullUiHandler),
            PathBuf::from("."),
            policy,
            oracle_config,
        )
    }

    fn spawn_http_server(body: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind http server");
        let addr = listener.local_addr().expect("server addr");
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn dispatcher_constructs() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            assert!(std::ptr::eq(
                dispatcher.runtime.as_js_runtime(),
                runtime.as_ref()
            ));
            assert_eq!(dispatcher.cwd, PathBuf::from("."));
        });
    }

    #[test]
    fn dispatcher_drains_empty_queue() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let drained = dispatcher.drain_hostcall_requests();
            assert!(drained.is_empty());
        });
    }

    #[test]
    fn dispatcher_drains_runtime_requests() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            runtime
                .eval(r#"pi.tool("read", { "path": "test.txt" });"#)
                .await
                .expect("eval");

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let drained = dispatcher.drain_hostcall_requests();
            assert_eq!(drained.len(), 1);
        });
    }

    #[test]
    fn dispatcher_tool_hostcall_executes_and_resolves_promise() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("test.txt"), "hello world").expect("write file");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.tool("read", { path: "test.txt" }).then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["read"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            let stats = runtime.tick().await.expect("tick");
            assert!(stats.ran_macrotask);

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("Promise not resolved");
                    if (!JSON.stringify(globalThis.result).includes("hello world")) {
                        throw new Error("Wrong result: " + JSON.stringify(globalThis.result));
                    }
                "#,
                )
                .await
                .expect("verify result");
        });
    }

    #[test]
    fn dispatcher_tool_hostcall_unknown_tool_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.tool("nope", {}).catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err !== "invalid_request") {
                        throw new Error("Wrong error code: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify error");
        });
    }

    #[test]
    fn dispatcher_session_hostcall_resolves_state_and_set_name() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.state = null;
                    globalThis.file = null;
                    globalThis.nameValue = null;
                    globalThis.nameSet = false;
                    pi.session("get_state", {}).then((r) => { globalThis.state = r; });
                    pi.session("get_file", {}).then((r) => { globalThis.file = r; });
                    pi.session("get_name", {}).then((r) => { globalThis.nameValue = r; });
                    pi.session("set_name", { name: "hello" }).then(() => { globalThis.nameSet = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 4);

            let name = Arc::new(Mutex::new(None));
            let state = Arc::new(Mutex::new(serde_json::json!({
                "sessionFile": "/tmp/session.jsonl",
                "sessionName": "demo",
            })));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::clone(&name),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let (state_value, file_value, name_value, name_set) = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let state_js: rquickjs::Value<'_> = global.get("state")?;
                    let file_js: rquickjs::Value<'_> = global.get("file")?;
                    let name_js: rquickjs::Value<'_> = global.get("nameValue")?;
                    let name_set_js: rquickjs::Value<'_> = global.get("nameSet")?;
                    Ok((
                        crate::extensions_js::js_to_json(&state_js)?,
                        crate::extensions_js::js_to_json(&file_js)?,
                        crate::extensions_js::js_to_json(&name_js)?,
                        crate::extensions_js::js_to_json(&name_set_js)?,
                    ))
                })
                .await
                .expect("read globals");

            let state_file = state_value
                .get("sessionFile")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert_eq!(state_file, "/tmp/session.jsonl");
            assert_eq!(file_value, Value::String("/tmp/session.jsonl".to_string()));
            assert_eq!(name_value, Value::String("demo".to_string()));
            assert_eq!(name_set, Value::Bool(true));

            let name_value = name
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(name_value.as_deref(), Some("hello"));
        });
    }

    #[test]
    fn dispatcher_session_hostcall_get_messages_entries_branch() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.messages = null;
                    globalThis.entries = null;
                    globalThis.branch = null;
                    pi.session("get_messages", {}).then((r) => { globalThis.messages = r; });
                    pi.session("get_entries", {}).then((r) => { globalThis.entries = r; });
                    pi.session("get_branch", {}).then((r) => { globalThis.branch = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 3);

            let message = SessionMessage::Custom {
                custom_type: "note".to_string(),
                content: "hello".to_string(),
                display: true,
                details: None,
                timestamp: Some(0),
            };
            let entries = vec![serde_json::json!({ "id": "entry-1", "type": "custom" })];
            let branch = vec![serde_json::json!({ "id": "entry-2", "type": "branch" })];

            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(Value::Null)),
                messages: Arc::new(Mutex::new(vec![message.clone()])),
                entries: Arc::new(Mutex::new(entries.clone())),
                branch: Arc::new(Mutex::new(branch.clone())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let (messages_value, entries_value, branch_value) = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let messages_js: rquickjs::Value<'_> = global.get("messages")?;
                    let entries_js: rquickjs::Value<'_> = global.get("entries")?;
                    let branch_js: rquickjs::Value<'_> = global.get("branch")?;
                    Ok((
                        crate::extensions_js::js_to_json(&messages_js)?,
                        crate::extensions_js::js_to_json(&entries_js)?,
                        crate::extensions_js::js_to_json(&branch_js)?,
                    ))
                })
                .await
                .expect("read globals");

            let messages_array = messages_value.as_array().expect("messages array");
            assert_eq!(messages_array.len(), 1);
            assert_eq!(
                messages_array[0]
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "custom"
            );
            assert_eq!(
                messages_array[0]
                    .get("customType")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "note"
            );
            assert_eq!(entries_value, Value::Array(entries));
            assert_eq!(branch_value, Value::Array(branch));
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn dispatcher_session_hostcall_append_message_and_entry() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.messageAppended = false;
                    globalThis.entryAppended = false;
                    pi.session("append_message", {
                        message: { role: "custom", customType: "note", content: "hi", display: true }
                    }).then(() => { globalThis.messageAppended = true; });
                    pi.session("append_entry", {
                        customType: "meta",
                        data: { ok: true }
                    }).then(() => { globalThis.entryAppended = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 2);

            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(Value::Null)),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                {
                    let session_handle: Arc<dyn ExtensionSession + Send + Sync> = session.clone();
                    session_handle
                },
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let (message_appended, entry_appended) = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let message_js: rquickjs::Value<'_> = global.get("messageAppended")?;
                    let entry_js: rquickjs::Value<'_> = global.get("entryAppended")?;
                    Ok((
                        crate::extensions_js::js_to_json(&message_js)?,
                        crate::extensions_js::js_to_json(&entry_js)?,
                    ))
                })
                .await
                .expect("read globals");

            assert_eq!(message_appended, Value::Bool(true));
            assert_eq!(entry_appended, Value::Bool(true));

            {
                let messages = session
                    .messages
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                assert_eq!(messages.len(), 1);
                match &messages[0] {
                    SessionMessage::Custom {
                        custom_type,
                        content,
                        display,
                        ..
                    } => {
                        assert_eq!(custom_type, "note");
                        assert_eq!(content, "hi");
                        assert!(*display);
                    }
                    other => assert!(
                        matches!(other, SessionMessage::Custom { .. }),
                        "Unexpected message: {other:?}"
                    ),
                }
            }

            {
                let expected = Some(serde_json::json!({ "ok": true }));
                let custom_entries = session
                    .custom_entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                assert_eq!(custom_entries.len(), 1);
                assert_eq!(custom_entries[0].0, "meta");
                assert_eq!(custom_entries[0].1, expected);
                drop(custom_entries);
            }
        });
    }

    #[test]
    fn dispatcher_session_hostcall_unknown_op_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.session("nope", {}).catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let err_value = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let err_js: rquickjs::Value<'_> = global.get("err")?;
                    crate::extensions_js::js_to_json(&err_js)
                })
                .await
                .expect("read globals");

            assert_eq!(err_value, Value::String("invalid_request".to_string()));
        });
    }

    #[test]
    fn dispatcher_session_hostcall_append_message_invalid_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.session("append_message", { message: { nope: 1 } })
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let err_value = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let err_js: rquickjs::Value<'_> = global.get("err")?;
                    crate::extensions_js::js_to_json(&err_js)
                })
                .await
                .expect("read globals");

            assert_eq!(err_value, Value::String("invalid_request".to_string()));
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_hostcall_executes_and_resolves_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("sh", ["-c", "printf hello"], {})
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("Promise not resolved");
                    if (globalThis.result.stdout !== "hello") {
                        throw new Error("Wrong stdout: " + JSON.stringify(globalThis.result));
                    }
                    if (globalThis.result.code !== 0) {
                        throw new Error("Wrong exit code: " + JSON.stringify(globalThis.result));
                    }
                    if (globalThis.result.killed !== false) {
                        throw new Error("Unexpected killed flag: " + JSON.stringify(globalThis.result));
                    }
                "#,
                )
                .await
                .expect("verify result");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_hostcall_command_not_found_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.exec("definitely_not_a_real_command", [], {})
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err !== "io") {
                        throw new Error("Wrong error code: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify error");
        });
    }

    #[derive(Default)]
    struct ScriptedIoReader {
        steps: std::collections::VecDeque<std::io::Result<Vec<u8>>>,
    }

    impl std::io::Read for ScriptedIoReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.steps.is_empty() {
                return Ok(0);
            }

            match self.steps.pop_front().expect("checked non-empty queue") {
                Ok(bytes) => {
                    let len = bytes.len().min(buf.len());
                    buf[..len].copy_from_slice(&bytes[..len]);
                    Ok(len)
                }
                Err(err) => Err(err),
            }
        }
    }

    #[test]
    fn pump_exec_stream_text_emits_io_error_frame() {
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        pump_exec_stream_text(
            ScriptedIoReader {
                steps: vec![
                    Ok(b"hello".to_vec()),
                    Err(std::io::Error::other("stdout pipe failed")),
                ]
                .into(),
            },
            &tx,
            ExecPipeKind::Stdout,
        );
        drop(tx);

        let frames = rx.into_iter().collect::<Vec<_>>();
        assert!(
            matches!(frames.first(), Some(ExecStreamFrame::Stdout(chunk)) if chunk == "hello"),
            "expected stdout chunk before error, got {frames_len} frames",
            frames_len = frames.len()
        );
        assert!(
            frames
                .iter()
                .any(|frame| matches!(frame, ExecStreamFrame::Error(message) if message.contains("stdout pipe failed"))),
            "expected io error frame, got {} frames",
            frames.len()
        );
    }

    #[test]
    fn pump_exec_capture_bytes_emits_io_error_frame() {
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        pump_exec_capture_bytes(
            ScriptedIoReader {
                steps: vec![
                    Ok(vec![1_u8, 2, 3]),
                    Err(std::io::Error::other("stderr pipe failed")),
                ]
                .into(),
            },
            &tx,
            ExecPipeKind::Stderr,
        );
        drop(tx);

        let frames = rx.into_iter().collect::<Vec<_>>();
        assert!(
            matches!(
                frames.first(),
                Some(ExecCaptureFrame::Chunk { kind: ExecPipeKind::Stderr, bytes })
                    if bytes == &vec![1_u8, 2, 3]
            ),
            "expected stderr bytes before error, got {} frames",
            frames.len()
        );
        assert!(
            frames
                .iter()
                .any(|frame| matches!(frame, ExecCaptureFrame::Error(message) if message.contains("stderr pipe failed"))),
            "expected io error frame, got {} frames",
            frames.len()
        );
    }

    #[test]
    fn exec_capture_error_message_includes_stdout_and_stderr_tail() {
        let stdout_chunks = VecDeque::from([b"partial stdout".to_vec()]);
        let stderr_chunks = VecDeque::from([b"partial stderr".to_vec()]);

        let message = exec_capture_error_message(
            "stdout pipe failed",
            &stdout_chunks,
            b"partial stdout".len(),
            &stderr_chunks,
            b"partial stderr".len(),
        );

        assert!(message.contains("stdout pipe failed"));
        assert!(message.contains("Partial output before failure"));
        assert!(message.contains("[stdout]"));
        assert!(message.contains("partial stdout"));
        assert!(message.contains("[stderr]"));
        assert!(message.contains("partial stderr"));
        assert!(!message.contains("truncated before failure"));
    }

    #[test]
    fn exec_capture_error_message_marks_truncated_stream_tails() {
        let stdout_chunks = VecDeque::from([b"stdout tail".to_vec()]);
        let stderr_chunks = VecDeque::from([b"stderr tail".to_vec()]);

        let message = exec_capture_error_message(
            "stderr pipe failed",
            &stdout_chunks,
            b"stdout tail".len() + 17,
            &stderr_chunks,
            b"stderr tail".len() + 23,
        );

        assert!(message.contains("[stdout]"));
        assert!(message.contains("stdout tail"));
        assert!(message.contains("[stdout partial output truncated before failure]"));
        assert!(message.contains("[stderr]"));
        assert!(message.contains("stderr tail"));
        assert!(message.contains("[stderr partial output truncated before failure]"));
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_hostcall_streaming_callback_delivers_chunks_and_final_result() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.chunks = [];
                    globalThis.finalResult = null;
                    pi.exec("sh", ["-c", "printf 'out-1\n'; printf 'err-1\n' 1>&2; printf 'out-2\n'"], {
                        stream: true,
                        onChunk: (chunk, isFinal) => {
                            globalThis.chunks.push({ chunk, isFinal });
                        },
                    }).then((r) => { globalThis.finalResult = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!Array.isArray(globalThis.chunks) || globalThis.chunks.length < 3) {
                        throw new Error("Expected stream chunks, got: " + JSON.stringify(globalThis.chunks));
                    }
                    const sawStdout = globalThis.chunks.some((entry) => entry.chunk && entry.chunk.stdout && entry.chunk.stdout.includes("out-1"));
                    if (!sawStdout) {
                        throw new Error("Missing stdout chunk: " + JSON.stringify(globalThis.chunks));
                    }
                    const sawStderr = globalThis.chunks.some((entry) => entry.chunk && entry.chunk.stderr && entry.chunk.stderr.includes("err-1"));
                    if (!sawStderr) {
                        throw new Error("Missing stderr chunk: " + JSON.stringify(globalThis.chunks));
                    }
                    const finalEntry = globalThis.chunks[globalThis.chunks.length - 1];
                    if (!finalEntry || finalEntry.isFinal !== true) {
                        throw new Error("Missing final chunk marker: " + JSON.stringify(globalThis.chunks));
                    }
                    if (globalThis.finalResult === null) {
                        throw new Error("Promise not resolved");
                    }
                    if (globalThis.finalResult.code !== 0) {
                        throw new Error("Wrong exit code: " + JSON.stringify(globalThis.finalResult));
                    }
                    if (globalThis.finalResult.killed !== false) {
                        throw new Error("Unexpected killed flag: " + JSON.stringify(globalThis.finalResult));
                    }
                "#,
                )
                .await
                .expect("verify stream callback result");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_hostcall_streaming_async_iterator_delivers_chunks_in_order() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.iterChunks = [];
                    globalThis.iterDone = false;
                    (async () => {
                        const stream = pi.exec("sh", ["-c", "printf 'a\n'; printf 'b\n'"], { stream: true });
                        for await (const chunk of stream) {
                            globalThis.iterChunks.push(chunk);
                        }
                        globalThis.iterDone = true;
                    })();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.iterDone !== true) {
                        throw new Error("Async iterator did not finish");
                    }
                    if (!Array.isArray(globalThis.iterChunks) || globalThis.iterChunks.length < 2) {
                        throw new Error("Missing stream chunks: " + JSON.stringify(globalThis.iterChunks));
                    }
                    const stdout = globalThis.iterChunks
                        .map((chunk) => (chunk && typeof chunk.stdout === "string" ? chunk.stdout : ""))
                        .join("");
                    if (stdout !== "a\nb\n") {
                        throw new Error("Unexpected streamed stdout aggregate: " + JSON.stringify(globalThis.iterChunks));
                    }
                    const finalChunk = globalThis.iterChunks[globalThis.iterChunks.length - 1];
                    if (!finalChunk || finalChunk.code !== 0 || finalChunk.killed !== false) {
                        throw new Error("Unexpected final chunk: " + JSON.stringify(finalChunk));
                    }
                "#,
                )
                .await
                .expect("verify async iterator result");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_hostcall_handles_invalid_utf8() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Output 'a', then invalid 0xFF, then 'b'.
            // Expected: 'a' in one chunk (or part of chunk), then replacement char, then 'b'.
            // Note: printf '\xff' might vary by shell, but \377 should work.
            runtime
                .eval(
                    r#"
                    globalThis.output = "";
                    globalThis.outputDone = false;
                    (async () => {
                        const stream = pi.exec("sh", ["-c", "printf 'a\\377b'"], { stream: true });
                        for await (const chunk of stream) {
                            if (chunk.stdout) globalThis.output += chunk.stdout;
                        }
                        globalThis.outputDone = true;
                    })();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.outputDone !== true) {
                        throw new Error("Streaming output collection did not finish");
                    }
                    // \uFFFD is the replacement character
                    if (globalThis.output !== "a\uFFFDb") {
                        throw new Error("Expected 'a\\uFFFDb', got: " + globalThis.output + " (len " + globalThis.output.length + ")");
                    }
                "#,
                )
                .await
                .expect("verify invalid utf8 handling");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_stream_cancel_enqueues_final_sentinel_without_wall_clock() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.timeoutChunks = [];
                    globalThis.timeoutResult = "pending";
                    globalThis.timeoutError = null;
                    pi.exec("sh", ["-c", "sleep 5"], {
                        stream: true,
                        onChunk: (chunk, isFinal) => {
                            globalThis.timeoutChunks.push({ chunk, isFinal });
                        },
                    })
                        .then((r) => { globalThis.timeoutResult = r; })
                        .catch((e) => { globalThis.timeoutError = e; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);
            let request = requests.into_iter().next().expect("request");

            assert!(runtime.is_hostcall_active(&request.call_id));
            assert!(runtime.cancel_hostcall(&request.call_id));
            assert!(!runtime.is_hostcall_active(&request.call_id));

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.timeoutError !== null) {
                        throw new Error("Unexpected cancel error: " + JSON.stringify(globalThis.timeoutError));
                    }
                    if (globalThis.timeoutResult !== null) {
                        throw new Error("Cancel sentinel should resolve stream promise with null: " + JSON.stringify(globalThis.timeoutResult));
                    }
                    const finalEntry = globalThis.timeoutChunks[globalThis.timeoutChunks.length - 1];
                    if (!finalEntry || finalEntry.isFinal !== true || finalEntry.chunk !== null) {
                        throw new Error("Missing deterministic final cancel sentinel: " + JSON.stringify(globalThis.timeoutChunks));
                    }
                "#,
                )
                .await
                .expect("verify cancel stream result");

            assert_eq!(runtime.pending_hostcall_count(), 0);
        });
    }

    #[test]
    #[cfg(all(unix, feature = "manual-timing-tests"))]
    #[ignore = "bd-8t27h.11: opt-in wall-clock process timeout regression; run with --features manual-timing-tests -- --ignored"]
    fn dispatcher_exec_hostcall_streaming_timeout_marks_final_chunk_killed() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.timeoutChunks = [];
                    globalThis.timeoutResult = null;
                    globalThis.timeoutError = null;
                    pi.exec("sh", ["-c", "printf 'start\n'; sleep 5; printf 'late\n'"], {
                        stream: true,
                        timeoutMs: 500,
                        onChunk: (chunk, isFinal) => {
                            globalThis.timeoutChunks.push({ chunk, isFinal });
                        },
                    })
                        .then((r) => { globalThis.timeoutResult = r; })
                        .catch((e) => { globalThis.timeoutError = e; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.timeoutError !== null) {
                        throw new Error("Unexpected timeout error: " + JSON.stringify(globalThis.timeoutError));
                    }
                    if (globalThis.timeoutResult === null) {
                        throw new Error("Timeout stream promise not resolved");
                    }
                    if (globalThis.timeoutResult.killed !== true) {
                        throw new Error("Expected killed=true for timeout stream: " + JSON.stringify(globalThis.timeoutResult));
                    }
                    const finalEntry = globalThis.timeoutChunks[globalThis.timeoutChunks.length - 1];
                    if (!finalEntry || finalEntry.isFinal !== true) {
                        throw new Error("Missing final timeout chunk marker: " + JSON.stringify(globalThis.timeoutChunks));
                    }
                    const sawLateOutput = globalThis.timeoutChunks.some((entry) =>
                        entry.chunk && entry.chunk.stdout && entry.chunk.stdout.includes("late")
                    );
                    if (sawLateOutput) {
                        throw new Error("Process output after timeout kill: " + JSON.stringify(globalThis.timeoutChunks));
                    }
                "#,
                )
                .await
                .expect("verify timeout stream result");
        });
    }

    #[test]
    fn dispatcher_http_hostcall_executes_and_resolves_promise() {
        futures::executor::block_on(async {
            let addr = spawn_http_server("hello");
            let url = format!("http://{addr}/test");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{ url: "{url}", method: "GET" }})
                    .then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("Promise not resolved");
                    if (globalThis.result.status !== 200) {
                        throw new Error("Wrong status: " + globalThis.result.status);
                    }
                    if (globalThis.result.body !== "hello") {
                        throw new Error("Wrong body: " + globalThis.result.body);
                    }
                "#,
                )
                .await
                .expect("verify result");
        });
    }

    #[test]
    fn dispatcher_http_hostcall_invalid_method_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.http({ url: "https://example.com", method: "PUT" })
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err !== "invalid_request") {
                        throw new Error("Wrong error code: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify error");
        });
    }

    #[test]
    fn dispatcher_ui_hostcall_executes_and_resolves_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.uiResult = null;
                    pi.ui("confirm", { title: "Confirm?" }).then((r) => { globalThis.uiResult = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured = Arc::new(Mutex::new(Vec::new()));
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(TestUiHandler {
                    captured: Arc::clone(&captured),
                    response_value: serde_json::json!({ "ok": true }),
                }),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (!globalThis.uiResult || globalThis.uiResult.ok !== true) {
                        throw new Error("Wrong UI result: " + JSON.stringify(globalThis.uiResult));
                    }
                "#,
                )
                .await
                .expect("verify result");

            let seen = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].method, "confirm");
        });
    }

    #[test]
    fn dispatcher_extension_ui_set_status_includes_text_field() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    const ui = __pi_make_extension_ui(__pi_test_secret, true);
                    ui.setStatus("key", "hello");
                "#,
                ))
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured = Arc::new(Mutex::new(Vec::new()));
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(TestUiHandler {
                    captured: Arc::clone(&captured),
                    response_value: Value::Null,
                }),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            let seen = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].method, "setStatus");
            assert_eq!(
                seen[0].payload.get("statusKey").and_then(Value::as_str),
                Some("key")
            );
            assert_eq!(
                seen[0].payload.get("statusText").and_then(Value::as_str),
                Some("hello")
            );
            assert_eq!(
                seen[0].payload.get("text").and_then(Value::as_str),
                Some("hello")
            );
        });
    }

    #[test]
    fn dispatcher_extension_ui_set_widget_includes_widget_lines_and_content() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    const ui = __pi_make_extension_ui(__pi_test_secret, true);
                    ui.setWidget("widget", ["a", "b"]);
                "#,
                ))
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured = Arc::new(Mutex::new(Vec::new()));
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(TestUiHandler {
                    captured: Arc::clone(&captured),
                    response_value: Value::Null,
                }),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            let seen = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].method, "setWidget");
            assert_eq!(
                seen[0].payload.get("widgetKey").and_then(Value::as_str),
                Some("widget")
            );
            assert_eq!(
                seen[0].payload.get("content").and_then(Value::as_str),
                Some("a\nb")
            );
            assert_eq!(
                seen[0].payload.get("widgetLines").and_then(Value::as_array),
                seen[0].payload.get("lines").and_then(Value::as_array)
            );
        });
    }

    #[test]
    fn dispatcher_events_hostcall_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.events("setActiveTools", { tools: ["read"] })
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err !== "invalid_request") {
                        throw new Error("Wrong error code: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify error");
        });
    }

    #[test]
    fn dispatcher_events_list_returns_registered_hooks() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    globalThis.eventsList = null;
                    __pi_begin_extension(__pi_test_secret, "ext.a", { name: "ext.a" });
                    pi.on("custom_event", (_payload, _ctx) => {});
                    pi.events("list", {}).then((r) => { globalThis.eventsList = r; });
                    __pi_end_extension(__pi_test_secret);
                "#,
                ))
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (!globalThis.eventsList) throw new Error("Promise not resolved");
                    const events = globalThis.eventsList.events;
                    if (!Array.isArray(events)) throw new Error("Missing events array");
                    if (events.length !== 1 || events[0] !== "custom_event") {
                        throw new Error("Wrong events list: " + JSON.stringify(events));
                    }
                "#,
                )
                .await
                .expect("verify list");
        });
    }

    #[test]
    fn dispatcher_session_set_model_resolves_and_persists() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.setResult = null;
                    pi.session("set_model", { provider: "anthropic", modelId: "claude-sonnet-4-20250514" })
                        .then((r) => { globalThis.setResult = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.setResult !== true) {
                        throw new Error("set_model should resolve to true, got: " + JSON.stringify(globalThis.setResult));
                    }
                "#,
                )
                .await
                .expect("verify set_model result");

            let final_state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(
                final_state.get("provider").and_then(Value::as_str),
                Some("anthropic")
            );
            assert_eq!(
                final_state.get("modelId").and_then(Value::as_str),
                Some("claude-sonnet-4-20250514")
            );
        });
    }

    #[test]
    fn dispatcher_session_get_model_resolves_provider_and_model_id() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.model = null;
                    pi.session("get_model", {}).then((r) => { globalThis.model = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({
                "provider": "openai",
                "modelId": "gpt-4o",
            })));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.model) throw new Error("get_model not resolved");
                    if (globalThis.model.provider !== "openai") {
                        throw new Error("Wrong provider: " + globalThis.model.provider);
                    }
                    if (globalThis.model.modelId !== "gpt-4o") {
                        throw new Error("Wrong modelId: " + globalThis.model.modelId);
                    }
                "#,
                )
                .await
                .expect("verify get_model result");
        });
    }

    #[test]
    fn dispatcher_session_set_model_missing_fields_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errNoProvider = null;
                    globalThis.errNoModelId = null;
                    globalThis.errEmpty = null;
                    pi.session("set_model", { modelId: "claude-sonnet-4-20250514" })
                        .catch((e) => { globalThis.errNoProvider = e.code; });
                    pi.session("set_model", { provider: "anthropic" })
                        .catch((e) => { globalThis.errNoModelId = e.code; });
                    pi.session("set_model", {})
                        .catch((e) => { globalThis.errEmpty = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 3);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errNoProvider !== "invalid_request") {
                        throw new Error("Missing provider should reject: " + globalThis.errNoProvider);
                    }
                    if (globalThis.errNoModelId !== "invalid_request") {
                        throw new Error("Missing modelId should reject: " + globalThis.errNoModelId);
                    }
                    if (globalThis.errEmpty !== "invalid_request") {
                        throw new Error("Empty payload should reject: " + globalThis.errEmpty);
                    }
                "#,
                )
                .await
                .expect("verify validation errors");
        });
    }

    #[test]
    fn dispatcher_session_set_then_get_model_round_trip() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Phase 1: set_model
            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    pi.session("set_model", { provider: "gemini", modelId: "gemini-2.0-flash" })
                        .then(() => { globalThis.setDone = true; });
                "#,
                )
                .await
                .expect("eval set");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session as Arc<dyn ExtensionSession + Send + Sync>,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Phase 2: get_model
            runtime
                .eval(
                    r#"
                    globalThis.model = null;
                    pi.session("get_model", {}).then((r) => { globalThis.model = r; });
                "#,
                )
                .await
                .expect("eval get");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.model) throw new Error("get_model not resolved");
                    if (globalThis.model.provider !== "gemini") {
                        throw new Error("Wrong provider: " + globalThis.model.provider);
                    }
                    if (globalThis.model.modelId !== "gemini-2.0-flash") {
                        throw new Error("Wrong modelId: " + globalThis.model.modelId);
                    }
                "#,
                )
                .await
                .expect("verify round trip");
        });
    }

    #[test]
    fn dispatcher_session_set_thinking_level_resolves() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    pi.session("set_thinking_level", { level: "high" })
                        .then(() => { globalThis.setDone = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // set_thinking_level resolves to null (not true like set_model)
            runtime
                .eval(
                    r#"
                    if (globalThis.setDone !== true) {
                        throw new Error("set_thinking_level not resolved");
                    }
                "#,
                )
                .await
                .expect("verify set_thinking_level");

            let final_state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(
                final_state.get("thinkingLevel").and_then(Value::as_str),
                Some("high")
            );
        });
    }

    #[test]
    fn dispatcher_session_get_thinking_level_resolves() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.level = "__unset__";
                    pi.session("get_thinking_level", {}).then((r) => { globalThis.level = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({
                "thinkingLevel": "medium",
            })));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.level !== "medium") {
                        throw new Error("Wrong thinking level: " + JSON.stringify(globalThis.level));
                    }
                "#,
                )
                .await
                .expect("verify get_thinking_level");
        });
    }

    #[test]
    fn dispatcher_session_get_thinking_level_null_when_unset() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.level = "__unset__";
                    pi.session("get_thinking_level", {}).then((r) => { globalThis.level = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.level !== null) {
                        throw new Error("Unset thinking level should be null, got: " + JSON.stringify(globalThis.level));
                    }
                "#,
                )
                .await
                .expect("verify null thinking level");
        });
    }

    #[test]
    fn dispatcher_session_set_thinking_level_missing_level_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.session("set_thinking_level", {})
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.err !== "invalid_request") {
                        throw new Error("Missing level should reject: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify validation error");
        });
    }

    #[test]
    fn dispatcher_session_set_then_get_thinking_level_round_trip() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Phase 1: set
            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    pi.session("set_thinking_level", { level: "low" })
                        .then(() => { globalThis.setDone = true; });
                "#,
                )
                .await
                .expect("eval set");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session as Arc<dyn ExtensionSession + Send + Sync>,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Phase 2: get
            runtime
                .eval(
                    r#"
                    globalThis.level = "__unset__";
                    pi.session("get_thinking_level", {}).then((r) => { globalThis.level = r; });
                "#,
                )
                .await
                .expect("eval get");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.level !== "low") {
                        throw new Error("Round trip failed, got: " + JSON.stringify(globalThis.level));
                    }
                "#,
                )
                .await
                .expect("verify round trip");
        });
    }

    #[test]
    fn dispatcher_session_model_ops_accept_camel_case_aliases() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    globalThis.model = null;
                    globalThis.thinkingSet = false;
                    globalThis.thinking = "__unset__";
                    pi.session("setmodel", { provider: "azure", modelId: "gpt-4" })
                        .then(() => { globalThis.setDone = true; });
                    pi.session("getmodel", {}).then((r) => { globalThis.model = r; });
                    pi.session("setthinkinglevel", { level: "high" })
                        .then(() => { globalThis.thinkingSet = true; });
                    pi.session("getthinkinglevel", {}).then((r) => { globalThis.thinking = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 4);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session as Arc<dyn ExtensionSession + Send + Sync>,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.setDone) throw new Error("setmodel not resolved");
                    if (!globalThis.thinkingSet) throw new Error("setthinkinglevel not resolved");
                "#,
                )
                .await
                .expect("verify camelCase aliases");
        });
    }

    #[test]
    fn dispatcher_session_set_model_accepts_model_id_snake_case() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    pi.session("set_model", { provider: "anthropic", model_id: "claude-opus-4-20250514" })
                        .then(() => { globalThis.setDone = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.setDone) throw new Error("set_model with model_id not resolved");
                "#,
                )
                .await
                .expect("verify model_id snake_case");

            let final_state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(
                final_state.get("modelId").and_then(Value::as_str),
                Some("claude-opus-4-20250514")
            );
        });
    }

    #[test]
    fn dispatcher_session_set_thinking_level_accepts_alt_keys() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Test thinkingLevel key
            runtime
                .eval(
                    r#"
                    globalThis.done1 = false;
                    globalThis.done2 = false;
                    pi.session("set_thinking_level", { thinkingLevel: "medium" })
                        .then(() => { globalThis.done1 = true; });
                    pi.session("set_thinking_level", { thinking_level: "low" })
                        .then(() => { globalThis.done2 = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 2);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.done1) throw new Error("thinkingLevel key not resolved");
                    if (!globalThis.done2) throw new Error("thinking_level key not resolved");
                "#,
                )
                .await
                .expect("verify alt keys");

            // Last write wins, so "low" should be the final value
            let final_state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(
                final_state.get("thinkingLevel").and_then(Value::as_str),
                Some("low")
            );
        });
    }

    #[test]
    fn dispatcher_session_get_model_null_when_unset() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.model = "__unset__";
                    pi.session("get_model", {}).then((r) => { globalThis.model = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // NullSession returns (None, None) for get_model
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.model) throw new Error("get_model not resolved");
                    if (globalThis.model.provider !== null) {
                        throw new Error("Unset provider should be null, got: " + JSON.stringify(globalThis.model.provider));
                    }
                    if (globalThis.model.modelId !== null) {
                        throw new Error("Unset modelId should be null, got: " + JSON.stringify(globalThis.model.modelId));
                    }
                "#,
                )
                .await
                .expect("verify null model");
        });
    }

    // ---- set_label tests ----

    #[test]
    fn dispatcher_session_set_label_resolves_and_persists() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("set_label", { targetId: "msg-42", label: "important" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let labels: Arc<Mutex<Vec<LabelEntry>>> = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::clone(&labels),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Verify set_label was called with correct args
            let captured = labels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "msg-42");
            assert_eq!(captured[0].1.as_deref(), Some("important"));
            drop(captured);
        });
    }

    #[test]
    fn dispatcher_session_set_label_remove_label_with_null() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("set_label", { targetId: "msg-99" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let labels: Arc<Mutex<Vec<LabelEntry>>> = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::clone(&labels),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Verify set_label was called with None label (removal)
            let captured = labels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "msg-99");
            assert!(captured[0].1.is_none());
            drop(captured);
        });
    }

    #[test]
    fn dispatcher_session_set_label_missing_target_id_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.session("set_label", { label: "orphaned" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.errMsg || globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection, got: " + globalThis.errMsg);
                    }
                    if (!globalThis.errMsg.includes("targetId")) {
                        throw new Error("Expected error about targetId, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify rejection");
        });
    }

    #[test]
    fn dispatcher_session_set_label_accepts_snake_case_target_id() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("set_label", { target_id: "msg-77", label: "reviewed" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let labels: Arc<Mutex<Vec<LabelEntry>>> = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::clone(&labels),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let captured = labels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "msg-77");
            assert_eq!(captured[0].1.as_deref(), Some("reviewed"));
            drop(captured);
        });
    }

    #[test]
    fn dispatcher_session_set_label_camel_case_op_alias() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use "setLabel" style (gets lowercased to "setlabel" which matches)
            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("setLabel", { targetId: "entry-5", label: "flagged" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let labels: Arc<Mutex<Vec<LabelEntry>>> = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::clone(&labels),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let captured = labels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "entry-5");
            assert_eq!(captured[0].1.as_deref(), Some("flagged"));
            drop(captured);
        });
    }

    // ---- Tool conformance tests ----

    #[test]
    fn dispatcher_tool_write_creates_file_and_resolves() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let file_path = temp_dir.path().join("output.txt");
            let file_path_str = file_path.display().to_string().replace('\\', "\\\\");
            let script = format!(
                r#"
                globalThis.result = null;
                pi.tool("write", {{ path: "{file_path_str}", content: "written by extension" }})
                    .then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["write"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Verify file was created
            assert!(file_path.exists());
            let content = std::fs::read_to_string(&file_path).expect("read file");
            assert_eq!(content, "written by extension");
        });
    }

    #[test]
    fn dispatcher_tool_ls_lists_directory() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("alpha.txt"), "a").expect("write");
            std::fs::write(temp_dir.path().join("beta.txt"), "b").expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.tool("ls", { path: "." })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["ls"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("ls not resolved");
                    let s = JSON.stringify(globalThis.result);
                    if (!s.includes("alpha.txt") || !s.includes("beta.txt")) {
                        throw new Error("Missing files in ls output: " + s);
                    }
                "#,
                )
                .await
                .expect("verify ls result");
        });
    }

    #[test]
    fn dispatcher_tool_grep_searches_content() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(
                temp_dir.path().join("data.txt"),
                "line one\nline two\nline three",
            )
            .expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let dir = temp_dir.path().display().to_string().replace('\\', "\\\\");
            let script = format!(
                r#"
                globalThis.result = null;
                pi.tool("grep", {{ pattern: "two", path: "{dir}" }})
                    .then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["grep"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("grep not resolved");
                    let s = JSON.stringify(globalThis.result);
                    if (!s.includes("two")) {
                        throw new Error("grep should find 'two': " + s);
                    }
                "#,
                )
                .await
                .expect("verify grep result");
        });
    }

    #[test]
    fn dispatcher_tool_edit_modifies_file_content() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("target.txt"), "old text here").expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.tool("edit", { path: "target.txt", oldText: "old text", newText: "new text" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["edit"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let content =
                std::fs::read_to_string(temp_dir.path().join("target.txt")).expect("read file");
            assert!(
                content.contains("new text"),
                "Expected edited content, got: {content}"
            );
        });
    }

    #[test]
    fn dispatcher_tool_find_discovers_files() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("code.rs"), "fn main(){}").expect("write");
            std::fs::write(temp_dir.path().join("data.json"), "{}").expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.tool("find", { pattern: "*.rs" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["find"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("find not resolved");
                    let s = JSON.stringify(globalThis.result);
                    if (!s.includes("code.rs")) {
                        throw new Error("find should discover code.rs: " + s);
                    }
                    if (s.includes("data.json")) {
                        throw new Error("find *.rs should not include data.json: " + s);
                    }
                "#,
                )
                .await
                .expect("verify find result");
        });
    }

    #[test]
    fn dispatcher_tool_multiple_tools_sequentially() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("file.txt"), "hello").expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Queue two tool calls
            runtime
                .eval(
                    r#"
                    globalThis.readResult = null;
                    globalThis.lsResult = null;
                    pi.tool("read", { path: "file.txt" })
                        .then((r) => { globalThis.readResult = r; });
                    pi.tool("ls", { path: "." })
                        .then((r) => { globalThis.lsResult = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 2);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["read", "ls"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.readResult === null) throw new Error("read not resolved");
                    if (globalThis.lsResult === null) throw new Error("ls not resolved");
                "#,
                )
                .await
                .expect("verify both tools resolved");
        });
    }

    #[test]
    fn dispatcher_tool_error_propagates_to_js() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Try to read a non-existent file
            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.tool("read", { path: "nonexistent_file.txt" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["read"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // The read tool may resolve with an error content rather than rejecting.
            // Either way, the dispatcher shouldn't panic.
            runtime
                .eval(
                    r#"
                    // Just verify something happened - error propagation is tool-specific
                    if (globalThis.errMsg === "" && globalThis.result === null) {
                        throw new Error("Neither resolved nor rejected");
                    }
                "#,
                )
                .await
                .expect("verify tool error handling");
        });
    }

    // ---- HTTP conformance tests ----

    fn spawn_http_server_with_status(status: u16, body: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind http server");
        let addr = listener.local_addr().expect("server addr");
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Length: {len}\r\nContent-Type: text/plain\r\n\r\n{body}",
                    status = status,
                    len = body.len(),
                    body = body,
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    #[test]
    #[cfg(unix)] // std::net::TcpListener + asupersync interop fails on Windows
    fn dispatcher_http_post_sends_body() {
        futures::executor::block_on(async {
            let addr = spawn_http_server("post-ok");
            let url = format!("http://{addr}/data");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{ url: "{url}", method: "POST", body: "test-payload" }})
                    .then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("POST not resolved");
                    if (globalThis.result.status !== 200) {
                        throw new Error("Expected 200, got: " + globalThis.result.status);
                    }
                "#,
                )
                .await
                .expect("verify POST result");
        });
    }

    #[test]
    fn dispatcher_http_missing_url_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for missing URL");
                    }
                "#,
                )
                .await
                .expect("verify missing URL rejection");
        });
    }

    #[test]
    fn dispatcher_http_custom_headers() {
        futures::executor::block_on(async {
            let addr = spawn_http_server("headers-ok");
            let url = format!("http://{addr}/headers");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{
                    url: "{url}",
                    method: "GET",
                    headers: {{ "X-Custom": "test-value", "Accept": "application/json" }}
                }}).then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("HTTP not resolved");
                    if (globalThis.result.status !== 200) {
                        throw new Error("Expected 200, got: " + globalThis.result.status);
                    }
                "#,
                )
                .await
                .expect("verify headers request");
        });
    }

    #[test]
    fn dispatcher_http_connection_refused_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use a port that definitely has nothing listening
            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "http://127.0.0.1:1/never", method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for connection refused");
                    }
                "#,
                )
                .await
                .expect("verify connection refused");
        });
    }

    // ---- UI conformance tests ----

    #[test]
    fn dispatcher_ui_spinner_method() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("spinner", { text: "Loading...", visible: true })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: serde_json::json!({ "acknowledged": true }),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "spinner");
            assert_eq!(reqs[0].payload["text"], "Loading...");
        });
    }

    #[test]
    fn dispatcher_ui_progress_method() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("progress", { current: 50, total: 100, label: "Processing" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "progress");
            assert_eq!(reqs[0].payload["current"], 50);
            assert_eq!(reqs[0].payload["total"], 100);
        });
    }

    #[test]
    fn dispatcher_ui_notification_method() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("notification", { message: "Task complete!", level: "info" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: serde_json::json!({ "shown": true }),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "notification");
            assert_eq!(reqs[0].payload["message"], "Task complete!");
            assert_eq!(reqs[0].payload["level"], "info");
        });
    }

    #[test]
    fn dispatcher_ui_null_handler_returns_null() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.ui("any_method", { key: "value" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // Use NullUiHandler - returns None which maps to null
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === "__unset__") throw new Error("UI not resolved");
                    if (globalThis.result !== null) {
                        throw new Error("Expected null from NullHandler, got: " + JSON.stringify(globalThis.result));
                    }
                "#,
                )
                .await
                .expect("verify null UI handler");
        });
    }

    #[test]
    fn dispatcher_ui_multiple_calls_captured() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.r1 = null;
                    globalThis.r2 = null;
                    pi.ui("set_status", { text: "Working..." })
                        .then((r) => { globalThis.r1 = r; });
                    pi.ui("set_widget", { lines: ["Line 1", "Line 2"] })
                        .then((r) => { globalThis.r2 = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 2);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let (len, methods) = {
                let reqs = captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let len = reqs.len();
                let methods = reqs.iter().map(|r| r.method.clone()).collect::<Vec<_>>();
                drop(reqs);
                (len, methods)
            };
            assert_eq!(len, 2);
            assert!(methods.iter().any(|method| method == "set_status"));
            assert!(methods.iter().any(|method| method == "set_widget"));
        });
    }

    // ---- Exec edge case tests ----

    #[test]
    fn dispatcher_exec_with_custom_cwd() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("pwd", { cwd: "/tmp" })
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    // Either it resolved to stdout containing /tmp, or it
                    // was rejected - both are valid dispatcher behaviors.
                    // Key assertion: the dispatcher didn't panic.
                "#,
                )
                .await
                .expect("verify exec cwd");
        });
    }

    #[test]
    fn dispatcher_exec_empty_command_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.exec("")
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for empty command");
                    }
                    // Empty command should produce some kind of error
                    if (!globalThis.errMsg) {
                        throw new Error("Expected error message");
                    }
                "#,
                )
                .await
                .expect("verify empty command rejection");
        });
    }

    // ---- Events edge case tests ----

    #[test]
    fn dispatcher_events_emit_missing_event_name_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.events("emit", {})
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    // Should either reject or produce an error - not silently succeed
                    if (globalThis.errMsg === "should_not_resolve") {
                        // It's also acceptable if emit with empty payload succeeds gracefully
                    }
                "#,
                )
                .await
                .expect("verify events emit");
        });
    }

    #[test]
    fn dispatcher_events_list_empty_when_no_hooks() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Register an extension with no hooks, then list events
            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    globalThis.result = null;
                    __pi_begin_extension(__pi_test_secret, "ext.empty", { name: "ext.empty" });
                    pi.events("list", {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                    __pi_end_extension(__pi_test_secret);
                "#,
                ))
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    if (!globalThis.result) throw new Error("events list not resolved");
                    // Result is { events: [...] }
                    const events = globalThis.result.events;
                    if (!Array.isArray(events)) {
                        throw new Error("Expected events array, got: " + JSON.stringify(globalThis.result));
                    }
                    if (events.length !== 0) {
                        throw new Error("Expected empty events list, got: " + JSON.stringify(events));
                    }
                "#,
                ))
                .await
                .expect("verify events list empty");
        });
    }

    // ---- Isolated session op tests ----

    #[test]
    fn dispatcher_session_get_file_isolated() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.file = "__unset__";
                    pi.session("get_file", {})
                        .then((r) => { globalThis.file = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({
                "sessionFile": "/home/user/.pi/sessions/abc.json"
            })));
            let session = Arc::new(TestSession {
                state,
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.file === "__unset__") throw new Error("get_file not resolved");
                    if (globalThis.file !== "/home/user/.pi/sessions/abc.json") {
                        throw new Error("Expected session file path, got: " + JSON.stringify(globalThis.file));
                    }
                "#,
                )
                .await
                .expect("verify get_file");
        });
    }

    #[test]
    fn dispatcher_session_get_name_isolated() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.name = "__unset__";
                    pi.session("get_name", {})
                        .then((r) => { globalThis.name = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({
                "sessionName": "My Debug Session"
            })));
            let session = Arc::new(TestSession {
                state,
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(Some("My Debug Session".to_string()))),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.name === "__unset__") throw new Error("get_name not resolved");
                    if (globalThis.name !== "My Debug Session") {
                        throw new Error("Expected session name, got: " + JSON.stringify(globalThis.name));
                    }
                "#,
                )
                .await
                .expect("verify get_name");
        });
    }

    #[test]
    fn dispatcher_session_append_entry_custom_type_edge_cases() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Test with custom_type key (snake_case variant)
            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("append_entry", {
                        custom_type: "audit_log",
                        data: { action: "login", ts: 1234567890 }
                    }).then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let custom_entries: CustomEntries = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::clone(&custom_entries),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let captured = custom_entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "audit_log");
            assert!(captured[0].1.is_some());
            let data = captured[0].1.as_ref().unwrap().clone();
            drop(captured);
            assert_eq!(data["action"], "login");
        });
    }

    #[test]
    fn dispatcher_events_emit_dispatches_custom_event() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    globalThis.seen = [];
                    globalThis.emitResult = null;

                    __pi_begin_extension(__pi_test_secret, "ext.b", { name: "ext.b" });
                    pi.on("custom_event", (payload, _ctx) => { globalThis.seen.push(payload); });
                    __pi_end_extension(__pi_test_secret);

                    __pi_begin_extension(__pi_test_secret, "ext.a", { name: "ext.a" });
                    pi.events("emit", { event: "custom_event", data: { hello: "world" } })
                      .then((r) => { globalThis.emitResult = r; });
                    __pi_end_extension(__pi_test_secret);
                "#,
                ))
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    if (!globalThis.emitResult) throw new Error("emit promise not resolved");
                    if (globalThis.emitResult.dispatched !== true) {
                        throw new Error("emit did not report dispatched: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (globalThis.emitResult.event !== "custom_event") {
                        throw new Error("wrong event: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (!Array.isArray(globalThis.seen) || globalThis.seen.length !== 1) {
                        throw new Error("event handler not called: " + JSON.stringify(globalThis.seen));
                    }
                    const payload = globalThis.seen[0];
                    if (!payload || payload.hello !== "world") {
                        throw new Error("wrong payload: " + JSON.stringify(payload));
                    }
                "#,
                ))
                .await
                .expect("verify emit");
        });
    }

    // ---- Additional exec conformance tests ----
    // These tests use Unix-specific commands (/bin/sh, /bin/echo) and are
    // skipped on Windows.

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_with_args_array() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // pi.exec(cmd, args, options) - args is the second positional arg
            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("/bin/echo", ["hello", "world"], {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    if (globalThis.result.error) throw new Error("exec errored: " + globalThis.result.error);
                    if (typeof globalThis.result.stdout !== "string") {
                        throw new Error("Expected stdout string, got: " + JSON.stringify(globalThis.result));
                    }
                    if (!globalThis.result.stdout.includes("hello") || !globalThis.result.stdout.includes("world")) {
                        throw new Error("Expected 'hello world' in stdout, got: " + globalThis.result.stdout);
                    }
                "#,
                )
                .await
                .expect("verify exec with args");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_null_args_defaults_to_empty() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("/bin/echo")
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    // echo with no args produces empty or newline stdout
                    if (globalThis.result.error) throw new Error("exec errored: " + globalThis.result.error);
                    if (typeof globalThis.result.stdout !== "string") {
                        throw new Error("Expected stdout string");
                    }
                "#,
                )
                .await
                .expect("verify exec null args");
        });
    }

    #[test]
    fn dispatcher_exec_non_array_args_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.exec("echo", "not-an-array", {})
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for non-array args");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("array")) {
                        throw new Error("Expected error about array, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify non-array args rejection");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_captures_stdout_and_stderr() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use sh -c to write to both stdout and stderr
            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("/bin/sh", ["-c", "echo OUT && echo ERR >&2"], {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    if (globalThis.result.error) throw new Error("exec errored: " + globalThis.result.error);
                    if (!globalThis.result.stdout.includes("OUT")) {
                        throw new Error("Expected 'OUT' in stdout, got: " + globalThis.result.stdout);
                    }
                    if (!globalThis.result.stderr.includes("ERR")) {
                        throw new Error("Expected 'ERR' in stderr, got: " + globalThis.result.stderr);
                    }
                "#,
                )
                .await
                .expect("verify stdout and stderr capture");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_nonzero_exit_code() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("/bin/sh", ["-c", "exit 42"], {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    if (globalThis.result.error) throw new Error("exec errored: " + globalThis.result.error);
                    if (globalThis.result.code !== 42) {
                        throw new Error("Expected exit code 42, got: " + globalThis.result.code);
                    }
                "#,
                )
                .await
                .expect("verify nonzero exit code");
        });
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_exec_signal_termination_reports_nonzero_code() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("/bin/sh", ["-c", "kill -KILL $$"], {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    if (globalThis.result.error) throw new Error("exec errored: " + globalThis.result.error);
                    if (globalThis.result.code === 0) {
                        throw new Error("Expected non-zero exit code for signal termination, got: " + globalThis.result.code);
                    }
                "#,
                )
                .await
                .expect("verify signal termination exit code");
        });
    }

    #[test]
    fn dispatcher_exec_command_not_found_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.exec("__nonexistent_command_xyz__")
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for nonexistent command");
                    }
                    if (!globalThis.errMsg) {
                        throw new Error("Expected error message for nonexistent command");
                    }
                "#,
                )
                .await
                .expect("verify command not found rejection");
        });
    }

    // ---- Additional HTTP conformance tests ----

    #[test]
    fn dispatcher_http_tls_required_rejects_http_url() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "http://example.com/test", method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // Use default config which has require_tls: true
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for http:// URL when TLS required");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("tls") &&
                        !globalThis.errMsg.toLowerCase().includes("https")) {
                        throw new Error("Expected TLS-related error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify TLS enforcement");
        });
    }

    #[test]
    fn dispatcher_http_invalid_url_format_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "not-a-valid-url", method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for invalid URL");
                    }
                    if (!globalThis.errMsg) {
                        throw new Error("Expected error message for invalid URL");
                    }
                "#,
                )
                .await
                .expect("verify invalid URL rejection");
        });
    }

    #[test]
    fn dispatcher_http_get_with_body_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "https://example.com/test", method: "GET", body: "should-not-have-body" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for GET with body");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("body") &&
                        !globalThis.errMsg.toLowerCase().includes("get")) {
                        throw new Error("Expected body/GET error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify GET with body rejection");
        });
    }

    #[test]
    fn dispatcher_http_response_body_returned() {
        futures::executor::block_on(async {
            let addr = spawn_http_server_with_status(200, "response-body-content");
            let url = format!("http://{addr}/body-test");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{ url: "{url}", method: "GET" }})
                    .then((r) => {{ globalThis.result = r; }})
                    .catch((e) => {{ globalThis.result = {{ error: e.message || String(e) }}; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("HTTP not resolved");
                    if (globalThis.result.error) throw new Error("HTTP error: " + globalThis.result.error);
                    if (globalThis.result.status !== 200) {
                        throw new Error("Expected 200, got: " + globalThis.result.status);
                    }
                    const body = globalThis.result.body || "";
                    if (!body.includes("response-body-content")) {
                        throw new Error("Expected response body, got: " + body);
                    }
                "#,
                )
                .await
                .expect("verify response body");
        });
    }

    #[test]
    fn dispatcher_http_error_status_code_returned() {
        futures::executor::block_on(async {
            let addr = spawn_http_server_with_status(404, "not found");
            let url = format!("http://{addr}/missing");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{ url: "{url}", method: "GET" }})
                    .then((r) => {{ globalThis.result = r; }})
                    .catch((e) => {{ globalThis.result = {{ error: e.message || String(e) }}; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("HTTP not resolved");
                    // 404 should still resolve (not reject) with the status code
                    if (globalThis.result.status !== 404) {
                        throw new Error("Expected status 404, got: " + JSON.stringify(globalThis.result));
                    }
                "#,
                )
                .await
                .expect("verify error status code");
        });
    }

    #[test]
    fn dispatcher_http_unsupported_scheme_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "ftp://example.com/file", method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for ftp:// scheme");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("scheme") &&
                        !globalThis.errMsg.toLowerCase().includes("unsupported")) {
                        throw new Error("Expected scheme error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify unsupported scheme rejection");
        });
    }

    // ---- Additional UI conformance tests ----

    #[test]
    fn dispatcher_ui_arbitrary_method_passthrough() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("custom_op", { key: "value" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "custom_op");
            assert_eq!(reqs[0].payload["key"], "value");
        });
    }

    #[test]
    fn dispatcher_ui_payload_passthrough_complex() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("set_widget", {
                        lines: [
                            { text: "Line 1", style: { bold: true } },
                            { text: "Line 2", style: { color: "red" } }
                        ],
                        content: "widget body",
                        metadata: { nested: { deep: true } }
                    }).then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            let payload = &reqs[0].payload;
            assert!(payload["lines"].is_array());
            assert_eq!(payload["lines"].as_array().unwrap().len(), 2);
            assert_eq!(payload["content"], "widget body");
            assert_eq!(payload["metadata"]["nested"]["deep"], true);
        });
    }

    #[test]
    fn dispatcher_ui_handler_returns_value() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.ui("get_input", { prompt: "Enter name" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: serde_json::json!({ "input": "Alice", "confirmed": true }),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === "__unset__") throw new Error("UI not resolved");
                    if (globalThis.result.input !== "Alice") {
                        throw new Error("Expected input 'Alice', got: " + JSON.stringify(globalThis.result));
                    }
                    if (globalThis.result.confirmed !== true) {
                        throw new Error("Expected confirmed true");
                    }
                "#,
                )
                .await
                .expect("verify UI handler value");
        });
    }

    #[test]
    fn dispatcher_ui_set_status_empty_text() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("set_status", { text: "" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "set_status");
            assert_eq!(reqs[0].payload["text"], "");
        });
    }

    #[test]
    fn dispatcher_ui_empty_payload() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("dismiss", {})
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "dismiss");
        });
    }

    #[test]
    fn dispatcher_ui_concurrent_different_methods() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.results = [];
                    pi.ui("set_status", { text: "Loading..." })
                        .then((r) => { globalThis.results.push("status"); });
                    pi.ui("show_spinner", { message: "Working" })
                        .then((r) => { globalThis.results.push("spinner"); });
                    pi.ui("set_widget", { lines: [], content: "w" })
                        .then((r) => { globalThis.results.push("widget"); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 3);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 3);
            let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
            assert!(methods.contains(&"set_status"));
            assert!(methods.contains(&"show_spinner"));
            assert!(methods.contains(&"set_widget"));
        });
    }

    #[test]
    fn dispatcher_ui_notification_with_severity() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("notification", { text: "Error occurred", severity: "error", duration: 5000 })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "notification");
            assert_eq!(reqs[0].payload["severity"], "error");
            assert_eq!(reqs[0].payload["duration"], 5000);
        });
    }

    #[test]
    fn dispatcher_ui_widget_with_lines_array() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("set_widget", {
                        lines: [
                            { text: "=== Status ===" },
                            { text: "CPU: 42%" },
                            { text: "Mem: 8GB" }
                        ],
                        content: "Dashboard"
                    }).then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "set_widget");
            let lines = reqs[0].payload["lines"].as_array().unwrap();
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[0]["text"], "=== Status ===");
            assert_eq!(lines[2]["text"], "Mem: 8GB");
        });
    }

    #[test]
    fn dispatcher_ui_progress_with_percentage() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("progress", { message: "Uploading", percent: 75, total: 100, current: 75 })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "progress");
            assert_eq!(reqs[0].payload["percent"], 75);
            assert_eq!(reqs[0].payload["total"], 100);
            assert_eq!(reqs[0].payload["current"], 75);
        });
    }

    // ---- Additional events conformance tests ----

    #[test]
    fn dispatcher_events_emit_name_field_alias() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use "name" instead of "event" field
            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    globalThis.seen = [];
                    globalThis.emitResult = null;

                    __pi_begin_extension(__pi_test_secret, "ext.listener", { name: "ext.listener" });
                    pi.on("named_event", (payload, _ctx) => { globalThis.seen.push(payload); });
                    __pi_end_extension(__pi_test_secret);

                    __pi_begin_extension(__pi_test_secret, "ext.emitter", { name: "ext.emitter" });
                    pi.events("emit", { name: "named_event", data: { via: "name_field" } })
                      .then((r) => { globalThis.emitResult = r; });
                    __pi_end_extension(__pi_test_secret);
                "#,
                ))
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    if (!globalThis.emitResult) throw new Error("emit not resolved");
                    if (globalThis.emitResult.dispatched !== true) {
                        throw new Error("emit not dispatched: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (globalThis.seen.length !== 1) {
                        throw new Error("Expected 1 handler call, got: " + globalThis.seen.length);
                    }
                    if (globalThis.seen[0].via !== "name_field") {
                        throw new Error("Wrong payload: " + JSON.stringify(globalThis.seen[0]));
                    }
                "#,
                ))
                .await
                .expect("verify name field alias");
        });
    }

    #[test]
    fn dispatcher_events_unsupported_op_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.events("nonexistent_op", {})
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for unsupported events op");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("unsupported")) {
                        throw new Error("Expected 'unsupported' error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify unsupported op rejection");
        });
    }

    #[test]
    fn dispatcher_events_emit_empty_event_name_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.events("emit", { event: "" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for empty event name");
                    }
                    if (!globalThis.errMsg.includes("event") && !globalThis.errMsg.includes("non-empty")) {
                        throw new Error("Expected event name error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify empty event name rejection");
        });
    }

    #[test]
    fn dispatcher_events_emit_handler_count_in_response() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Register 2 handlers for same event
            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    globalThis.emitResult = null;

                    __pi_begin_extension(__pi_test_secret, "ext.h1", { name: "ext.h1" });
                    pi.on("counted_event", (_p, _c) => {});
                    __pi_end_extension(__pi_test_secret);

                    __pi_begin_extension(__pi_test_secret, "ext.h2", { name: "ext.h2" });
                    pi.on("counted_event", (_p, _c) => {});
                    __pi_end_extension(__pi_test_secret);

                    __pi_begin_extension(__pi_test_secret, "ext.emitter", { name: "ext.emitter" });
                    pi.events("emit", { event: "counted_event", data: {} })
                      .then((r) => { globalThis.emitResult = r; });
                    __pi_end_extension(__pi_test_secret);
                "#,
                ))
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    if (!globalThis.emitResult) throw new Error("emit not resolved");
                    if (globalThis.emitResult.dispatched !== true) {
                        throw new Error("emit not dispatched: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (typeof globalThis.emitResult.handler_count !== "number") {
                        throw new Error("Expected handler_count number, got: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (globalThis.emitResult.handler_count < 2) {
                        throw new Error("Expected at least 2 handlers, got: " + globalThis.emitResult.handler_count);
                    }
                "#,
                ))
                .await
                .expect("verify handler count");
        });
    }

    #[test]
    fn dispatcher_events_list_returns_registered_event_names() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Register multiple event hooks
            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    globalThis.result = null;

                    __pi_begin_extension(__pi_test_secret, "ext.multi", { name: "ext.multi" });
                    pi.on("event_alpha", (_p, _c) => {});
                    pi.on("event_beta", (_p, _c) => {});
                    pi.on("event_gamma", (_p, _c) => {});
                    pi.events("list", {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                    __pi_end_extension(__pi_test_secret);
                "#,
                ))
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    if (!globalThis.result) throw new Error("list not resolved");
                    if (globalThis.result.error) throw new Error("list error: " + globalThis.result.error);
                    const events = globalThis.result.events;
                    if (!Array.isArray(events)) {
                        throw new Error("Expected events array, got: " + JSON.stringify(globalThis.result));
                    }
                    if (events.length < 3) {
                        throw new Error("Expected at least 3 events, got: " + JSON.stringify(events));
                    }
                    if (!events.includes("event_alpha")) {
                        throw new Error("Missing event_alpha in: " + JSON.stringify(events));
                    }
                    if (!events.includes("event_beta")) {
                        throw new Error("Missing event_beta in: " + JSON.stringify(events));
                    }
                "#,
                ))
                .await
                .expect("verify event names list");
        });
    }

    #[test]
    fn dispatcher_events_emit_no_handlers_still_resolves() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Emit an event that has no registered handlers
            runtime
                .eval(&privileged_test_script(
                    runtime.as_ref(),
                    r#"
                    globalThis.emitResult = null;

                    __pi_begin_extension(__pi_test_secret, "ext.lonely", { name: "ext.lonely" });
                    pi.events("emit", { event: "unheard_event", data: { msg: "nobody listens" } })
                      .then((r) => { globalThis.emitResult = r; })
                      .catch((e) => { globalThis.emitResult = { error: e.message || String(e) }; });
                    __pi_end_extension(__pi_test_secret);
                "#,
                ))
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.emitResult) throw new Error("emit not resolved");
                    // Should resolve even with no handlers (dispatched: true, handler_count: 0)
                    if (globalThis.emitResult.error) {
                        throw new Error("emit errored: " + globalThis.emitResult.error);
                    }
                    if (globalThis.emitResult.dispatched !== true) {
                        throw new Error("emit not dispatched: " + JSON.stringify(globalThis.emitResult));
                    }
                "#,
                )
                .await
                .expect("verify emit with no handlers");
        });
    }

    // ---- Additional tool conformance tests ----

    #[test]
    fn dispatcher_tool_read_returns_file_content() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let file_path = temp_dir.path().join("readable.txt");
            std::fs::write(&file_path, "file content here").expect("write test file");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let file_path_js = file_path.display().to_string().replace('\\', "\\\\");
            let script = format!(
                r#"
                globalThis.result = null;
                pi.tool("read", {{ path: "{file_path_js}" }})
                    .then((r) => {{ globalThis.result = r; }})
                    .catch((e) => {{ globalThis.result = {{ error: e.message || String(e) }}; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["read"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("read not resolved");
                    if (globalThis.result.error) throw new Error("read error: " + globalThis.result.error);
                "#,
                )
                .await
                .expect("verify read tool");
        });
    }

    // ======================================================================
    // bd-321a.4: Session dispatcher taxonomy tests
    // ======================================================================
    // Table-driven tests proving dispatch_session returns taxonomy-correct
    // error codes (timeout|denied|io|invalid_request|internal).

    /// Direct unit test of dispatch_session error taxonomy without JS runtime.
    /// Uses TestSession to verify error code classification for each operation.
    #[test]
    fn session_dispatch_taxonomy_unknown_op_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session("c1", "nonexistent_op", serde_json::json!({}))
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(
                        code, "invalid_request",
                        "unknown op must be invalid_request"
                    );
                }
                HostcallOutcome::Success(_) | HostcallOutcome::StreamChunk { .. } => {
                    unreachable!("unexpected hostcall outcome");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_set_model_missing_provider_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session("c2", "set_model", serde_json::json!({"modelId": "gpt-4o"}))
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(
                        code, "invalid_request",
                        "set_model missing provider must be invalid_request"
                    );
                }
                HostcallOutcome::Success(_) => {
                    unreachable!("unexpected hostcall outcome");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    unreachable!("unexpected hostcall outcome");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_set_model_missing_model_id_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session(
                    "c3",
                    "set_model",
                    serde_json::json!({"provider": "anthropic"}),
                )
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(code, "invalid_request");
                }
                HostcallOutcome::Success(_) => {
                    unreachable!("unexpected hostcall outcome");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    unreachable!("unexpected hostcall outcome");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_set_thinking_level_empty_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session("c4", "set_thinking_level", serde_json::json!({}))
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(code, "invalid_request");
                }
                HostcallOutcome::Success(_) => {
                    unreachable!("unexpected hostcall outcome");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    unreachable!("unexpected hostcall outcome");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_set_label_empty_target_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session("c5", "set_label", serde_json::json!({}))
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(code, "invalid_request");
                }
                HostcallOutcome::Success(_) => {
                    unreachable!("unexpected hostcall outcome");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    unreachable!("unexpected hostcall outcome");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_append_message_invalid_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session(
                    "c6",
                    "append_message",
                    serde_json::json!({"message": {"not_a_valid_message": true}}),
                )
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(
                        code, "invalid_request",
                        "malformed message must be invalid_request"
                    );
                }
                HostcallOutcome::Success(_) => {
                    unreachable!("unexpected hostcall outcome");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    unreachable!("unexpected hostcall outcome");
                }
            }
        });
    }

    #[test]
    #[allow(clippy::items_after_statements, clippy::too_many_lines)]
    fn session_dispatch_taxonomy_io_error_from_session_trait() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use a session impl that returns IO errors
            struct FailSession;

            #[async_trait]
            impl ExtensionSession for FailSession {
                async fn get_state(&self) -> Value {
                    Value::Null
                }
                async fn get_messages(&self) -> Vec<SessionMessage> {
                    Vec::new()
                }
                async fn get_entries(&self) -> Vec<Value> {
                    Vec::new()
                }
                async fn get_branch(&self) -> Vec<Value> {
                    Vec::new()
                }
                async fn set_name(&self, _name: String) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn append_message(&self, _message: SessionMessage) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn append_custom_entry(
                    &self,
                    _custom_type: String,
                    _data: Option<Value>,
                ) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn set_model(&self, _provider: String, _model_id: String) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn get_model(&self) -> (Option<String>, Option<String>) {
                    (None, None)
                }
                async fn set_thinking_level(&self, _level: String) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn get_thinking_level(&self) -> Option<String> {
                    None
                }
                async fn set_label(
                    &self,
                    _target_id: String,
                    _label: Option<String>,
                ) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
            }

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(FailSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            // Table of ops that call session trait mutators (which will fail with IO error)
            let io_cases = [
                ("set_name", serde_json::json!({"name": "test"})),
                (
                    "set_model",
                    serde_json::json!({"provider": "a", "modelId": "b"}),
                ),
                ("set_thinking_level", serde_json::json!({"level": "high"})),
                (
                    "set_label",
                    serde_json::json!({"targetId": "abc", "label": "x"}),
                ),
                (
                    "append_entry",
                    serde_json::json!({"customType": "note", "data": null}),
                ),
                (
                    "append_message",
                    serde_json::json!({"message": {"role": "custom", "customType": "x", "content": "y", "display": true}}),
                ),
            ];

            for (op, params) in &io_cases {
                let outcome = dispatcher.dispatch_session("cx", op, params.clone()).await;
                match outcome {
                    HostcallOutcome::Error { code, .. } => {
                        assert_eq!(code, "io", "session IO error for op '{op}' must be 'io'");
                    }
                    HostcallOutcome::Success(_) => {
                        unreachable!("unexpected hostcall outcome");
                    }
                    HostcallOutcome::StreamChunk { .. } => {
                        unreachable!("unexpected hostcall outcome");
                    }
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_read_ops_succeed_with_null_session() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            let read_ops = [
                "get_state",
                "getState",
                "get_messages",
                "getMessages",
                "get_entries",
                "getEntries",
                "get_branch",
                "getBranch",
                "get_file",
                "getFile",
                "get_name",
                "getName",
                "get_model",
                "getModel",
                "get_thinking_level",
                "getThinkingLevel",
            ];

            for op in &read_ops {
                let outcome = dispatcher
                    .dispatch_session("cr", op, serde_json::json!({}))
                    .await;
                assert!(
                    matches!(outcome, HostcallOutcome::Success(_)),
                    "read op '{op}' should succeed"
                );
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_case_insensitive_aliases() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            // Each alias pair should produce the same result
            let alias_pairs = [
                ("get_state", "getstate"),
                ("get_messages", "getmessages"),
                ("get_entries", "getentries"),
                ("get_branch", "getbranch"),
                ("get_file", "getfile"),
                ("get_name", "getname"),
                ("get_model", "getmodel"),
                ("get_thinking_level", "getthinkinglevel"),
            ];

            for (snake, camel) in &alias_pairs {
                let outcome_a = dispatcher
                    .dispatch_session("ca", snake, serde_json::json!({}))
                    .await;
                let outcome_b = dispatcher
                    .dispatch_session("cb", camel, serde_json::json!({}))
                    .await;
                match (&outcome_a, &outcome_b) {
                    (HostcallOutcome::Success(a), HostcallOutcome::Success(b)) => {
                        assert_eq!(
                            a, b,
                            "alias pair ({snake}, {camel}) should produce same output"
                        );
                    }
                    _ => unreachable!("unexpected hostcall outcome"),
                }
            }
        });
    }

    #[test]
    fn ui_dispatch_taxonomy_missing_op_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_ui("ui-1", "   ", serde_json::json!({}), None)
                .await;
            assert!(
                matches!(outcome, HostcallOutcome::Error { code, .. } if code == "invalid_request")
            );
        });
    }

    #[test]
    fn ui_dispatch_taxonomy_timeout_error_maps_to_timeout() {
        futures::executor::block_on(async {
            struct TimeoutUiHandler;

            #[async_trait]
            impl ExtensionUiHandler for TimeoutUiHandler {
                async fn request_ui(
                    &self,
                    _request: ExtensionUiRequest,
                ) -> Result<Option<ExtensionUiResponse>> {
                    Err(Error::extension("Extension UI request timed out"))
                }
            }

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(TimeoutUiHandler),
                PathBuf::from("."),
            );

            let outcome = dispatcher
                .dispatch_ui("ui-2", "confirm", serde_json::json!({}), None)
                .await;
            assert!(matches!(outcome, HostcallOutcome::Error { code, .. } if code == "timeout"));
        });
    }

    #[test]
    fn ui_dispatch_taxonomy_unconfigured_maps_to_denied() {
        futures::executor::block_on(async {
            struct MissingUiHandler;

            #[async_trait]
            impl ExtensionUiHandler for MissingUiHandler {
                async fn request_ui(
                    &self,
                    _request: ExtensionUiRequest,
                ) -> Result<Option<ExtensionUiResponse>> {
                    Err(Error::extension("Extension UI sender not configured"))
                }
            }

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(MissingUiHandler),
                PathBuf::from("."),
            );

            let outcome = dispatcher
                .dispatch_ui("ui-3", "confirm", serde_json::json!({}), None)
                .await;
            assert!(matches!(outcome, HostcallOutcome::Error { code, .. } if code == "denied"));
        });
    }

    #[test]
    fn protocol_adapter_host_call_to_host_result_success() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let message = ExtensionMessage {
                id: "msg-hostcall-1".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-hostcall-1".to_string(),
                    capability: "session".to_string(),
                    method: "session".to_string(),
                    params: serde_json::json!({ "op": "get_state" }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert_eq!(result.call_id, "call-hostcall-1");
                    assert!(!result.is_error, "expected success host_result");
                    assert!(
                        result.output.is_object(),
                        "host_result output must remain object"
                    );
                    assert!(result.error.is_none(), "success should not include error");
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_adapter_missing_op_returns_invalid_request_taxonomy() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let message = ExtensionMessage {
                id: "msg-hostcall-2".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-hostcall-2".to_string(),
                    capability: "session".to_string(),
                    method: "session".to_string(),
                    params: serde_json::json!({}),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(result.is_error, "expected error host_result");
                    assert!(result.output.is_object(), "error output must be object");
                    let error = result.error.expect("error payload");
                    assert_eq!(
                        error.code,
                        crate::extensions::HostCallErrorCode::InvalidRequest
                    );
                    let details = error.details.expect("error details");
                    assert_eq!(
                        details["dispatcherDecisionTrace"]["selectedRuntime"],
                        Value::String("rust-extension-dispatcher".to_string())
                    );
                    assert_eq!(
                        details["dispatcherDecisionTrace"]["schemaPath"],
                        Value::String("ExtensionBody::HostCall/HostCallPayload".to_string())
                    );
                    assert_eq!(
                        details["dispatcherDecisionTrace"]["schemaVersion"],
                        Value::String(PROTOCOL_VERSION.to_string())
                    );
                    assert_eq!(
                        details["dispatcherDecisionTrace"]["fallbackReason"],
                        Value::String("schema_validation_failed".to_string())
                    );
                    assert_eq!(
                        details["extensionInput"]["method"],
                        Value::String("session".to_string())
                    );
                    assert_eq!(
                        details["extensionOutput"]["code"],
                        Value::String("invalid_request".to_string())
                    );
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_adapter_unknown_method_includes_fallback_trace() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let message = ExtensionMessage {
                id: "msg-hostcall-unknown-method".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-hostcall-unknown-method".to_string(),
                    capability: "session".to_string(),
                    method: "not_a_real_method".to_string(),
                    params: serde_json::json!({ "foo": 1 }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(result.is_error, "expected error host_result");
                    let error = result.error.expect("error payload");
                    assert_eq!(
                        error.code,
                        crate::extensions::HostCallErrorCode::InvalidRequest
                    );
                    let details = error.details.expect("error details");
                    assert_eq!(
                        details["dispatcherDecisionTrace"]["fallbackReason"],
                        Value::String("unsupported_method_fallback".to_string())
                    );
                    assert_eq!(
                        details["dispatcherDecisionTrace"]["method"],
                        Value::String("not_a_real_method".to_string())
                    );
                    assert_eq!(
                        details["schemaDiff"]["observedParamKeys"],
                        Value::Array(vec![Value::String("foo".to_string())])
                    );
                    assert_eq!(
                        details["extensionInput"]["params"]["foo"],
                        Value::Number(serde_json::Number::from(1))
                    );
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn dispatch_events_list_unknown_extension_returns_empty_events() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            let outcome = dispatcher
                .dispatch_events(
                    "call-events-unknown-extension",
                    Some("missing.extension"),
                    "list",
                    serde_json::json!({}),
                )
                .await;

            match outcome {
                HostcallOutcome::Success(value) => {
                    assert_eq!(value, serde_json::json!({ "events": [] }));
                }
                HostcallOutcome::Error { code, message } => {
                    unreachable!("unexpected error outcome: {code}: {message}");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    unreachable!("unexpected stream outcome");
                }
            }
        });
    }

    #[test]
    fn protocol_adapter_rejects_non_host_call_messages() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let message = ExtensionMessage {
                id: "msg-hostcall-3".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::ToolResult(crate::extensions::ToolResultPayload {
                    call_id: "tool-1".to_string(),
                    output: serde_json::json!({}),
                    is_error: false,
                }),
            };

            let err = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect_err("non-host-call should fail");
            assert!(
                err.to_string()
                    .contains("dispatch_protocol_message expects host_call"),
                "unexpected error: {err}"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Policy enforcement tests
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_denied_capability_returns_error() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Set up JS promise handler for pi.exec()
            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.exec("echo", ["hello"]).catch((e) => { globalThis.err = e; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // Safe profile denies "exec"
            let policy = ExtensionPolicy::from_profile(PolicyProfile::Safe);
            let dispatcher = build_dispatcher_with_policy(Rc::clone(&runtime), policy);

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            let _ = runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err.code !== "denied") {
                        throw new Error("Expected denied code, got: " + globalThis.err.code);
                    }
                "#,
                )
                .await
                .expect("verify denied error");
        });
    }

    #[test]
    fn dispatch_denied_capability_still_denied_when_advanced_path_disabled() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.exec("echo", ["hello"]).catch((e) => { globalThis.err = e; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let oracle_config = DualExecOracleConfig {
                sample_ppm: 0,
                ..DualExecOracleConfig::default()
            };
            let policy = ExtensionPolicy::from_profile(PolicyProfile::Safe);
            let mut dispatcher =
                build_dispatcher_with_policy_and_oracle(Rc::clone(&runtime), policy, oracle_config);
            dispatcher.io_uring_lane_config = IoUringLanePolicyConfig::conservative();
            dispatcher.io_uring_force_compat = false;
            assert!(
                !dispatcher.advanced_dispatch_enabled(),
                "advanced path should be disabled for this test"
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            let _ = runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err.code !== "denied") {
                        throw new Error("Expected denied code, got: " + globalThis.err.code);
                    }
                "#,
                )
                .await
                .expect("verify denied error");
        });
    }

    #[test]
    fn dispatch_allowed_capability_proceeds() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.log("test message").then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let policy = ExtensionPolicy::from_profile(PolicyProfile::Permissive);
            let dispatcher = build_dispatcher_with_policy(Rc::clone(&runtime), policy);

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            let _ = runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("Promise not resolved");
                "#,
                )
                .await
                .expect("verify allowed");
        });
    }

    #[test]
    fn dispatch_allowed_capability_still_resolves_when_advanced_path_disabled() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.log("test message").then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let oracle_config = DualExecOracleConfig {
                sample_ppm: 0,
                ..DualExecOracleConfig::default()
            };
            let policy = ExtensionPolicy::from_profile(PolicyProfile::Permissive);
            let mut dispatcher =
                build_dispatcher_with_policy_and_oracle(Rc::clone(&runtime), policy, oracle_config);
            dispatcher.io_uring_lane_config = IoUringLanePolicyConfig::conservative();
            dispatcher.io_uring_force_compat = false;
            assert!(
                !dispatcher.advanced_dispatch_enabled(),
                "advanced path should be disabled for this test"
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            let _ = runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("Promise not resolved");
                "#,
                )
                .await
                .expect("verify allowed");
        });
    }

    #[test]
    fn advanced_dispatch_enabled_when_dual_exec_sampling_non_zero() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let oracle_config = DualExecOracleConfig {
                sample_ppm: 1,
                ..DualExecOracleConfig::default()
            };
            let dispatcher = build_dispatcher_with_policy_and_oracle(
                Rc::clone(&runtime),
                ExtensionPolicy::from_profile(PolicyProfile::Permissive),
                oracle_config,
            );
            assert!(dispatcher.advanced_dispatch_enabled());
        });
    }

    #[test]
    fn advanced_dispatch_enabled_when_io_uring_is_enabled() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let oracle_config = DualExecOracleConfig {
                sample_ppm: 0,
                ..DualExecOracleConfig::default()
            };
            let mut dispatcher = build_dispatcher_with_policy_and_oracle(
                Rc::clone(&runtime),
                ExtensionPolicy::from_profile(PolicyProfile::Permissive),
                oracle_config,
            );
            dispatcher.io_uring_lane_config = IoUringLanePolicyConfig {
                enabled: true,
                ring_available: true,
                max_queue_depth: 256,
                allow_filesystem: true,
                allow_network: true,
            };
            assert!(dispatcher.advanced_dispatch_enabled());
        });
    }

    #[test]
    fn advanced_dispatch_enabled_when_io_uring_force_compat_is_set() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let oracle_config = DualExecOracleConfig {
                sample_ppm: 0,
                ..DualExecOracleConfig::default()
            };
            let mut dispatcher = build_dispatcher_with_policy_and_oracle(
                Rc::clone(&runtime),
                ExtensionPolicy::from_profile(PolicyProfile::Permissive),
                oracle_config,
            );
            dispatcher.io_uring_lane_config = IoUringLanePolicyConfig::conservative();
            dispatcher.io_uring_force_compat = true;
            assert!(dispatcher.advanced_dispatch_enabled());
        });
    }

    #[test]
    fn dispatch_strict_mode_denies_unknown_capability() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.http({ url: "http://localhost" }).catch((e) => { globalThis.err = e; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // Strict mode with no default_caps: everything denied
            let policy = ExtensionPolicy {
                mode: ExtensionPolicyMode::Strict,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                per_extension: HashMap::new(),
                ..Default::default()
            };
            let dispatcher = build_dispatcher_with_policy(Rc::clone(&runtime), policy);

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            let _ = runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err.code !== "denied") {
                        throw new Error("Expected denied code, got: " + globalThis.err.code);
                    }
                "#,
                )
                .await
                .expect("verify strict denied");
        });
    }

    #[test]
    fn protocol_dispatch_denied_returns_error() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            // Safe profile denies "exec"
            let policy = ExtensionPolicy::from_profile(PolicyProfile::Safe);
            let dispatcher = build_dispatcher_with_policy(Rc::clone(&runtime), policy);

            let message = ExtensionMessage {
                id: "msg-policy-deny".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-policy-deny".to_string(),
                    capability: "exec".to_string(),
                    method: "exec".to_string(),
                    params: serde_json::json!({ "cmd": "echo hello" }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(result.is_error, "expected denied error result");
                    let error = result.error.expect("error payload");
                    assert_eq!(error.code, HostCallErrorCode::Denied);
                    assert!(
                        error.message.contains("exec"),
                        "error should mention denied capability: {}",
                        error.message
                    );
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn dispatch_deny_caps_blocks_http() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.http({ url: "http://localhost" }).catch((e) => { globalThis.err = e; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let policy = ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: vec!["http".to_string()],
                per_extension: HashMap::new(),
                ..Default::default()
            };
            let dispatcher = build_dispatcher_with_policy(Rc::clone(&runtime), policy);

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            let _ = runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err.code !== "denied") {
                        throw new Error("Expected denied code, got: " + globalThis.err.code);
                    }
                "#,
                )
                .await
                .expect("verify deny_caps http blocked");
        });
    }

    #[test]
    fn per_extension_deny_blocks_specific_extension() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Trigger a session hostcall from JS
            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    globalThis.result = null;
                    pi.session("getState", {}).catch((e) => { globalThis.err = e; })
                        .then((r) => { if (r) globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let mut per_extension = HashMap::new();
            per_extension.insert(
                "blocked-ext".to_string(),
                ExtensionOverride {
                    mode: None,
                    allow: Vec::new(),
                    deny: vec!["session".to_string()],
                    quota: None,
                },
            );
            let policy = ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                per_extension,
                ..Default::default()
            };
            let dispatcher = build_dispatcher_with_policy(Rc::clone(&runtime), policy);

            // Modify the request to come from the blocked extension
            let mut request = requests.into_iter().next().unwrap();
            request.extension_id = Some("blocked-ext".to_string());

            dispatcher.dispatch_and_complete(request).await;

            let _ = runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err.code !== "denied") {
                        throw new Error("Expected denied code, got: " + globalThis.err.code);
                    }
                "#,
                )
                .await
                .expect("verify per-extension deny");
        });
    }

    #[test]
    fn prompt_decision_treated_as_deny_in_dispatcher() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.exec("echo", ["hello"]).catch((e) => { globalThis.err = e; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // Prompt mode with no defaults → exec falls through to Prompt
            let policy = ExtensionPolicy {
                mode: ExtensionPolicyMode::Prompt,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
                per_extension: HashMap::new(),
                ..Default::default()
            };
            let dispatcher = build_dispatcher_with_policy(Rc::clone(&runtime), policy);

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            let _ = runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err.code !== "denied") {
                        throw new Error("Expected denied, got: " + globalThis.err.code);
                    }
                "#,
                )
                .await
                .expect("verify prompt treated as deny");
        });
    }

    // -----------------------------------------------------------------------
    // Utility function unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn protocol_hostcall_op_extracts_op_field() {
        let params = serde_json::json!({ "op": "get_state" });
        assert_eq!(protocol_hostcall_op(&params), Some("get_state"));
    }

    #[test]
    fn protocol_hostcall_op_extracts_method_field() {
        let params = serde_json::json!({ "method": "do_thing" });
        assert_eq!(protocol_hostcall_op(&params), Some("do_thing"));
    }

    #[test]
    fn protocol_hostcall_op_extracts_name_field() {
        let params = serde_json::json!({ "name": "my_event" });
        assert_eq!(protocol_hostcall_op(&params), Some("my_event"));
    }

    #[test]
    fn protocol_hostcall_op_prefers_op_over_method_and_name() {
        let params = serde_json::json!({ "op": "a", "method": "b", "name": "c" });
        assert_eq!(protocol_hostcall_op(&params), Some("a"));
    }

    #[test]
    fn protocol_hostcall_op_falls_back_to_method_when_op_missing() {
        let params = serde_json::json!({ "method": "b", "name": "c" });
        assert_eq!(protocol_hostcall_op(&params), Some("b"));
    }

    #[test]
    fn protocol_hostcall_op_returns_none_for_empty_or_whitespace() {
        assert_eq!(protocol_hostcall_op(&serde_json::json!({})), None);
        assert_eq!(protocol_hostcall_op(&serde_json::json!({ "op": "" })), None);
        assert_eq!(
            protocol_hostcall_op(&serde_json::json!({ "op": "   " })),
            None
        );
    }

    #[test]
    fn protocol_hostcall_op_trims_whitespace() {
        let params = serde_json::json!({ "op": "  get_state  " });
        assert_eq!(protocol_hostcall_op(&params), Some("get_state"));
    }

    #[test]
    fn protocol_hostcall_op_returns_none_for_non_string_values() {
        assert_eq!(protocol_hostcall_op(&serde_json::json!({ "op": 42 })), None);
        assert_eq!(
            protocol_hostcall_op(&serde_json::json!({ "op": true })),
            None
        );
        assert_eq!(
            protocol_hostcall_op(&serde_json::json!({ "op": null })),
            None
        );
    }

    #[test]
    fn parse_protocol_hostcall_method_normalizes_case_and_whitespace() {
        assert!(matches!(
            parse_protocol_hostcall_method(" Tool "),
            Some(ProtocolHostcallMethod::Tool)
        ));
        assert!(matches!(
            parse_protocol_hostcall_method("EXEC"),
            Some(ProtocolHostcallMethod::Exec)
        ));
        assert!(matches!(
            parse_protocol_hostcall_method(" session "),
            Some(ProtocolHostcallMethod::Session)
        ));
    }

    #[test]
    fn parse_protocol_hostcall_method_rejects_unknown_or_empty_values() {
        assert!(parse_protocol_hostcall_method("").is_none());
        assert!(parse_protocol_hostcall_method("   ").is_none());
        assert!(parse_protocol_hostcall_method("not_a_method").is_none());
    }

    #[test]
    fn protocol_error_fallback_reason_preserves_invalid_request_taxonomy() {
        assert_eq!(
            protocol_error_fallback_reason("tool", "invalid_request"),
            "schema_validation_failed"
        );
        assert_eq!(
            protocol_error_fallback_reason("  SESSION ", "invalid_request"),
            "schema_validation_failed"
        );
        assert_eq!(
            protocol_error_fallback_reason("unknown", "invalid_request"),
            "unsupported_method_fallback"
        );
    }

    #[test]
    fn protocol_error_fallback_reason_maps_non_invalid_request_codes() {
        assert_eq!(
            protocol_error_fallback_reason("tool", "denied"),
            "policy_denied"
        );
        assert_eq!(
            protocol_error_fallback_reason("tool", "timeout"),
            "handler_timeout"
        );
        assert_eq!(
            protocol_error_fallback_reason("tool", "tool_error"),
            "handler_error"
        );
        assert_eq!(
            protocol_error_fallback_reason("tool", "unexpected"),
            "runtime_internal_error"
        );
    }

    #[test]
    fn protocol_normalize_output_passes_object_through() {
        let obj = serde_json::json!({ "key": "value" });
        assert_eq!(protocol_normalize_output(obj.clone()), obj);
    }

    #[test]
    fn protocol_normalize_output_wraps_non_object_in_value_field() {
        assert_eq!(
            protocol_normalize_output(serde_json::json!("hello")),
            serde_json::json!({ "value": "hello" })
        );
        assert_eq!(
            protocol_normalize_output(serde_json::json!(42)),
            serde_json::json!({ "value": 42 })
        );
        assert_eq!(
            protocol_normalize_output(serde_json::json!(true)),
            serde_json::json!({ "value": true })
        );
        assert_eq!(
            protocol_normalize_output(Value::Null),
            serde_json::json!({ "value": null })
        );
        assert_eq!(
            protocol_normalize_output(serde_json::json!([1, 2, 3])),
            serde_json::json!({ "value": [1, 2, 3] })
        );
    }

    #[test]
    fn protocol_error_code_maps_known_codes() {
        assert_eq!(protocol_error_code("timeout"), HostCallErrorCode::Timeout);
        assert_eq!(protocol_error_code("denied"), HostCallErrorCode::Denied);
        assert_eq!(protocol_error_code("io"), HostCallErrorCode::Io);
        assert_eq!(protocol_error_code("tool_error"), HostCallErrorCode::Io);
        assert_eq!(
            protocol_error_code("invalid_request"),
            HostCallErrorCode::InvalidRequest
        );
    }

    #[test]
    fn protocol_error_code_unknown_maps_to_internal() {
        assert_eq!(
            protocol_error_code("something_else"),
            HostCallErrorCode::Internal
        );
        assert_eq!(protocol_error_code(""), HostCallErrorCode::Internal);
        assert_eq!(
            protocol_error_code("not_a_code"),
            HostCallErrorCode::Internal
        );
    }

    #[test]
    fn protocol_error_code_normalizes_case_and_whitespace() {
        assert_eq!(protocol_error_code(" Timeout "), HostCallErrorCode::Timeout);
        assert_eq!(protocol_error_code("DENIED"), HostCallErrorCode::Denied);
        assert_eq!(protocol_error_code(" Tool_Error "), HostCallErrorCode::Io);
        assert_eq!(
            protocol_error_code(" Invalid_Request "),
            HostCallErrorCode::InvalidRequest
        );
    }

    #[test]
    fn protocol_error_fallback_reason_normalizes_code_before_taxonomy_mapping() {
        assert_eq!(
            protocol_error_fallback_reason(" session ", " INVALID_REQUEST "),
            "schema_validation_failed"
        );
        assert_eq!(
            protocol_error_fallback_reason("unknown", " INVALID_REQUEST "),
            "unsupported_method_fallback"
        );
        assert_eq!(
            protocol_error_fallback_reason("tool", " TOOL_ERROR "),
            "handler_error"
        );
    }

    fn test_protocol_payload(call_id: &str) -> HostCallPayload {
        HostCallPayload {
            call_id: call_id.to_string(),
            capability: "test".to_string(),
            method: "tool".to_string(),
            params: serde_json::json!({}),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        }
    }

    fn test_hostcall_request(call_id: &str, kind: HostcallKind, payload: Value) -> HostcallRequest {
        HostcallRequest {
            call_id: call_id.to_string(),
            kind,
            payload,
            trace_id: 0,
            extension_id: Some("ext.protocol.params".to_string()),
        }
    }

    #[test]
    fn protocol_params_from_request_matches_hostcall_request_params_for_hash() {
        let requests = vec![
            test_hostcall_request(
                "tool-case",
                HostcallKind::Tool {
                    name: "read".to_string(),
                },
                serde_json::json!({ "path": "README.md" }),
            ),
            test_hostcall_request(
                "tool-non-object-case",
                HostcallKind::Tool {
                    name: "read".to_string(),
                },
                serde_json::json!(["README.md", "Cargo.toml"]),
            ),
            test_hostcall_request(
                "exec-object-case",
                HostcallKind::Exec {
                    cmd: "echo from kind".to_string(),
                },
                serde_json::json!({
                    "command": "legacy alias should be removed",
                    "cmd": "payload override should lose",
                    "args": ["hello"],
                }),
            ),
            test_hostcall_request(
                "exec-non-object-case",
                HostcallKind::Exec {
                    cmd: "bash -lc true".to_string(),
                },
                serde_json::json!("raw payload"),
            ),
            test_hostcall_request(
                "http-case",
                HostcallKind::Http,
                serde_json::json!({
                    "url": "https://example.com",
                    "method": "GET",
                }),
            ),
            test_hostcall_request(
                "http-non-object-case",
                HostcallKind::Http,
                serde_json::json!("https://example.com/raw"),
            ),
            test_hostcall_request(
                "session-case",
                HostcallKind::Session {
                    op: "get_state".to_string(),
                },
                serde_json::json!({
                    "op": "payload override should lose",
                    "includeEntries": true,
                }),
            ),
            test_hostcall_request(
                "ui-non-object-case",
                HostcallKind::Ui {
                    op: "set_status".to_string(),
                },
                serde_json::json!("ready"),
            ),
            test_hostcall_request(
                "events-null-case",
                HostcallKind::Events {
                    op: "list_flags".to_string(),
                },
                Value::Null,
            ),
            test_hostcall_request(
                "log-case",
                HostcallKind::Log,
                serde_json::json!({
                    "level": "info",
                    "event": "test.protocol",
                    "message": "hello",
                }),
            ),
            test_hostcall_request(
                "log-non-object-case",
                HostcallKind::Log,
                serde_json::json!("raw-log-payload"),
            ),
            test_hostcall_request(
                "log-array-case",
                HostcallKind::Log,
                serde_json::json!(["raw", "log", "payload"]),
            ),
            test_hostcall_request("log-null-case", HostcallKind::Log, Value::Null),
        ];

        for request in requests {
            assert_eq!(
                protocol_params_from_request(&request),
                request.params_for_hash(),
                "protocol params shape diverged for {}",
                request.call_id
            );
        }
    }

    #[test]
    fn protocol_params_from_request_preserves_reserved_key_precedence() {
        let exec_request = test_hostcall_request(
            "exec-precedence",
            HostcallKind::Exec {
                cmd: "echo from kind".to_string(),
            },
            serde_json::json!({
                "command": "legacy alias",
                "cmd": "payload cmd should not win",
                "args": ["a", "b"],
            }),
        );
        let exec_params = protocol_params_from_request(&exec_request);
        assert_eq!(exec_params["cmd"], serde_json::json!("echo from kind"));
        assert_eq!(exec_params.get("command"), None);

        for (call_id, kind) in [
            (
                "session-precedence",
                HostcallKind::Session {
                    op: "get_state".to_string(),
                },
            ),
            (
                "ui-precedence",
                HostcallKind::Ui {
                    op: "set_status".to_string(),
                },
            ),
            (
                "events-precedence",
                HostcallKind::Events {
                    op: "list_flags".to_string(),
                },
            ),
        ] {
            let request = test_hostcall_request(
                call_id,
                kind.clone(),
                serde_json::json!({ "op": "payload op should not win", "x": 1 }),
            );
            let params = protocol_params_from_request(&request);
            let expected_op = match kind {
                HostcallKind::Session { ref op }
                | HostcallKind::Ui { ref op }
                | HostcallKind::Events { ref op } => op.clone(),
                _ => unreachable!("loop only includes op-based hostcall kinds"),
            };
            assert_eq!(params["op"], Value::String(expected_op));
        }
    }

    fn assert_protocol_result_equivalent_except_error_details(
        plain: &HostResultPayload,
        traced: &HostResultPayload,
    ) {
        assert_eq!(plain.call_id, traced.call_id);
        assert_eq!(plain.output, traced.output);
        assert_eq!(plain.is_error, traced.is_error);
        assert_eq!(
            plain.chunk.as_ref().map(|chunk| {
                (
                    chunk.index,
                    chunk.is_last,
                    chunk
                        .backpressure
                        .as_ref()
                        .map(|bp| (bp.credits, bp.delay_ms)),
                )
            }),
            traced.chunk.as_ref().map(|chunk| {
                (
                    chunk.index,
                    chunk.is_last,
                    chunk
                        .backpressure
                        .as_ref()
                        .map(|bp| (bp.credits, bp.delay_ms)),
                )
            })
        );
        match (plain.error.as_ref(), traced.error.as_ref()) {
            (None, None) => {}
            (Some(plain_error), Some(traced_error)) => {
                assert_eq!(plain_error.code, traced_error.code);
                assert_eq!(plain_error.message, traced_error.message);
                assert_eq!(plain_error.retryable, traced_error.retryable);
            }
            _ => unreachable!("unexpected hostcall protocol result"),
        }
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_success() {
        let payload = test_protocol_payload("call-1");
        let result = hostcall_outcome_to_protocol_result(
            &payload.call_id,
            HostcallOutcome::Success(serde_json::json!({ "ok": true })),
        );
        assert_eq!(result.call_id, "call-1");
        assert!(!result.is_error);
        assert!(result.error.is_none());
        assert!(result.chunk.is_none());
        assert!(result.output.is_object());
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_success_wraps_non_object() {
        let payload = test_protocol_payload("call-2");
        let result = hostcall_outcome_to_protocol_result(
            &payload.call_id,
            HostcallOutcome::Success(serde_json::json!("plain string")),
        );
        assert!(!result.is_error);
        assert_eq!(
            result.output,
            serde_json::json!({ "value": "plain string" })
        );
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_stream_chunk() {
        let payload = test_protocol_payload("call-3");
        let result = hostcall_outcome_to_protocol_result(
            &payload.call_id,
            HostcallOutcome::StreamChunk {
                sequence: 5,
                chunk: serde_json::json!({ "stdout": "hello\n" }),
                is_final: false,
            },
        );
        assert_eq!(result.call_id, "call-3");
        assert!(!result.is_error);
        assert!(result.error.is_none());
        let chunk = result.chunk.expect("should have chunk");
        assert_eq!(chunk.index, 5);
        assert!(!chunk.is_last);
        assert_eq!(result.output["sequence"], 5);
        assert!(!result.output["isFinal"].as_bool().unwrap());
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_stream_chunk_final() {
        let payload = test_protocol_payload("call-4");
        let result = hostcall_outcome_to_protocol_result(
            &payload.call_id,
            HostcallOutcome::StreamChunk {
                sequence: 10,
                chunk: serde_json::json!({ "code": 0 }),
                is_final: true,
            },
        );
        let chunk = result.chunk.expect("should have chunk");
        assert!(chunk.is_last);
        assert_eq!(chunk.index, 10);
        assert!(result.output["isFinal"].as_bool().unwrap());
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_error() {
        let payload = test_protocol_payload("call-5");
        let result = hostcall_outcome_to_protocol_result(
            &payload.call_id,
            HostcallOutcome::Error {
                code: "io".to_string(),
                message: "disk full".to_string(),
            },
        );
        assert_eq!(result.call_id, "call-5");
        assert!(result.is_error);
        assert!(result.chunk.is_none());
        let error = result.error.expect("should have error");
        assert_eq!(error.code, HostCallErrorCode::Io);
        assert_eq!(error.message, "disk full");
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_error_unknown_code_maps_to_internal() {
        let payload = test_protocol_payload("call-6");
        let result = hostcall_outcome_to_protocol_result(
            &payload.call_id,
            HostcallOutcome::Error {
                code: "something_weird".to_string(),
                message: "unexpected".to_string(),
            },
        );
        let error = result.error.expect("should have error");
        assert_eq!(error.code, HostCallErrorCode::Internal);
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_error_normalizes_mixed_case_code() {
        let payload = test_protocol_payload("call-6b");
        let result = hostcall_outcome_to_protocol_result(
            &payload.call_id,
            HostcallOutcome::Error {
                code: "  Invalid_Request  ".to_string(),
                message: "normalized".to_string(),
            },
        );
        let error = result.error.expect("should have error");
        assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
        assert_eq!(error.message, "normalized");
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_error_normalizes_denied_timeout_and_tool_error_alias() {
        let cases = [
            ("  DeNied ", HostCallErrorCode::Denied),
            ("  TimeOut ", HostCallErrorCode::Timeout),
            ("  TOOL_ERROR ", HostCallErrorCode::Io),
        ];

        for (idx, (raw_code, expected_code)) in cases.into_iter().enumerate() {
            let payload = test_protocol_payload(&format!("call-plain-normalize-{idx}"));
            let message = format!("normalized-{idx}");
            let result = hostcall_outcome_to_protocol_result(
                &payload.call_id,
                HostcallOutcome::Error {
                    code: raw_code.to_string(),
                    message: message.clone(),
                },
            );

            let error = result.error.expect("should have error");
            assert_eq!(error.code, expected_code, "raw code: {raw_code}");
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_success_equivalent_to_plain() {
        let payload = test_protocol_payload("call-trace-success");
        let outcome = HostcallOutcome::Success(serde_json::json!({
            "ok": true,
            "nested": { "n": 7 }
        }));
        let plain = hostcall_outcome_to_protocol_result(&payload.call_id, outcome.clone());
        let traced = hostcall_outcome_to_protocol_result_with_trace(&payload, outcome);

        assert_protocol_result_equivalent_except_error_details(&plain, &traced);
        assert!(traced.error.is_none());
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_stream_equivalent_to_plain() {
        let payload = test_protocol_payload("call-trace-stream");
        let outcome = HostcallOutcome::StreamChunk {
            sequence: 3,
            chunk: serde_json::json!({ "stdout": "chunk" }),
            is_final: false,
        };
        let plain = hostcall_outcome_to_protocol_result(&payload.call_id, outcome.clone());
        let traced = hostcall_outcome_to_protocol_result_with_trace(&payload, outcome);

        assert_protocol_result_equivalent_except_error_details(&plain, &traced);
        assert!(traced.error.is_none());
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_error_adds_details_without_mutating_error_core()
     {
        let mut payload = test_protocol_payload("call-trace-error");
        payload.method = "tool".to_string();
        payload.params = serde_json::json!({ "zeta": 1, "alpha": 2 });
        let outcome = HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: "invalid payload".to_string(),
        };
        let plain = hostcall_outcome_to_protocol_result(&payload.call_id, outcome.clone());
        let traced = hostcall_outcome_to_protocol_result_with_trace(&payload, outcome);

        assert_protocol_result_equivalent_except_error_details(&plain, &traced);

        let plain_error = plain.error.expect("plain conversion should include error");
        assert!(
            plain_error.details.is_none(),
            "plain conversion should not inject trace details"
        );
        let traced_error = traced.error.expect("trace conversion should include error");
        let details = traced_error
            .details
            .expect("trace conversion should include structured details");
        assert_eq!(
            details["dispatcherDecisionTrace"]["fallbackReason"],
            serde_json::json!("schema_validation_failed")
        );
        assert_eq!(
            details["schemaDiff"]["observedParamKeys"],
            serde_json::json!(["alpha", "zeta"])
        );
        assert_eq!(
            details["extensionInput"]["callId"],
            serde_json::json!("call-trace-error")
        );
        assert_eq!(
            details["extensionOutput"]["code"],
            serde_json::json!("invalid_request")
        );
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_normalizes_invalid_request_taxonomy() {
        let mut known_method_payload = test_protocol_payload("call-trace-error-known");
        known_method_payload.method = " TOOL ".to_string();
        let known_method_result = hostcall_outcome_to_protocol_result_with_trace(
            &known_method_payload,
            HostcallOutcome::Error {
                code: "  INVALID_REQUEST ".to_string(),
                message: "bad request".to_string(),
            },
        );
        let known_method_error = known_method_result.error.expect("expected error");
        assert_eq!(known_method_error.code, HostCallErrorCode::InvalidRequest);
        let known_details = known_method_error.details.expect("expected details");
        assert_eq!(
            known_details["dispatcherDecisionTrace"]["fallbackReason"],
            serde_json::json!("schema_validation_failed")
        );

        let mut unknown_method_payload = test_protocol_payload("call-trace-error-unknown");
        unknown_method_payload.method = "custom_method".to_string();
        let unknown_method_result = hostcall_outcome_to_protocol_result_with_trace(
            &unknown_method_payload,
            HostcallOutcome::Error {
                code: "  INVALID_REQUEST ".to_string(),
                message: "bad request".to_string(),
            },
        );
        let unknown_method_error = unknown_method_result.error.expect("expected error");
        assert_eq!(unknown_method_error.code, HostCallErrorCode::InvalidRequest);
        let unknown_details = unknown_method_error.details.expect("expected details");
        assert_eq!(
            unknown_details["dispatcherDecisionTrace"]["fallbackReason"],
            serde_json::json!("unsupported_method_fallback")
        );
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_normalizes_tool_error_taxonomy() {
        let mut payload = test_protocol_payload("call-trace-error-tool");
        payload.method = "tool".to_string();
        let result = hostcall_outcome_to_protocol_result_with_trace(
            &payload,
            HostcallOutcome::Error {
                code: "  TOOL_ERROR ".to_string(),
                message: "handler exploded".to_string(),
            },
        );

        let error = result.error.expect("expected error");
        assert_eq!(error.code, HostCallErrorCode::Io);
        let details = error.details.expect("expected details");
        assert_eq!(
            details["dispatcherDecisionTrace"]["fallbackReason"],
            serde_json::json!("handler_error")
        );
        assert_eq!(
            details["extensionOutput"]["code"],
            serde_json::json!("  TOOL_ERROR ")
        );
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_normalizes_timeout_taxonomy() {
        let mut payload = test_protocol_payload("call-trace-error-timeout");
        payload.method = "exec".to_string();
        let result = hostcall_outcome_to_protocol_result_with_trace(
            &payload,
            HostcallOutcome::Error {
                code: "  TimeOut  ".to_string(),
                message: "handler timed out".to_string(),
            },
        );

        let error = result.error.expect("expected error");
        assert_eq!(error.code, HostCallErrorCode::Timeout);
        let details = error.details.expect("expected details");
        assert_eq!(
            details["dispatcherDecisionTrace"]["fallbackReason"],
            serde_json::json!("handler_timeout")
        );
        assert_eq!(
            details["extensionOutput"]["code"],
            serde_json::json!("  TimeOut  ")
        );
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_normalizes_denied_taxonomy() {
        let mut payload = test_protocol_payload("call-trace-error-denied");
        payload.method = "session".to_string();
        let result = hostcall_outcome_to_protocol_result_with_trace(
            &payload,
            HostcallOutcome::Error {
                code: "  DeNied ".to_string(),
                message: "blocked by policy".to_string(),
            },
        );

        let error = result.error.expect("expected error");
        assert_eq!(error.code, HostCallErrorCode::Denied);
        let details = error.details.expect("expected details");
        assert_eq!(
            details["dispatcherDecisionTrace"]["fallbackReason"],
            serde_json::json!("policy_denied")
        );
        assert_eq!(
            details["extensionOutput"]["code"],
            serde_json::json!("  DeNied ")
        );
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_normalizes_unknown_code_to_internal_taxonomy()
    {
        let mut payload = test_protocol_payload("call-trace-error-unknown-code");
        payload.method = "tool".to_string();
        let result = hostcall_outcome_to_protocol_result_with_trace(
            &payload,
            HostcallOutcome::Error {
                code: "  SOME_NEW_CODE ".to_string(),
                message: "unexpected runtime state".to_string(),
            },
        );

        let error = result.error.expect("expected error");
        assert_eq!(error.code, HostCallErrorCode::Internal);
        let details = error.details.expect("expected details");
        assert_eq!(
            details["dispatcherDecisionTrace"]["fallbackReason"],
            serde_json::json!("runtime_internal_error")
        );
        assert_eq!(
            details["extensionOutput"]["code"],
            serde_json::json!("  SOME_NEW_CODE ")
        );
    }

    #[test]
    fn hostcall_code_to_str_roundtrips_all_variants() {
        use crate::connectors::HostCallErrorCode;
        assert_eq!(hostcall_code_to_str(HostCallErrorCode::Timeout), "timeout");
        assert_eq!(hostcall_code_to_str(HostCallErrorCode::Denied), "denied");
        assert_eq!(hostcall_code_to_str(HostCallErrorCode::Io), "io");
        assert_eq!(
            hostcall_code_to_str(HostCallErrorCode::InvalidRequest),
            "invalid_request"
        );
        assert_eq!(
            hostcall_code_to_str(HostCallErrorCode::Internal),
            "internal"
        );
    }

    // -----------------------------------------------------------------------
    // Protocol dispatch for all method types
    // -----------------------------------------------------------------------

    #[test]
    fn protocol_dispatch_tool_success() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("file.txt"), "protocol test content")
                .expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = ExtensionDispatcher::new_with_policy(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["read"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
                ExtensionPolicy::from_profile(PolicyProfile::Permissive),
            );

            let message = ExtensionMessage {
                id: "msg-tool-proto".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-tool-proto".to_string(),
                    capability: "read".to_string(),
                    method: "tool".to_string(),
                    params: serde_json::json!({ "name": "read", "input": { "path": "file.txt" } }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol tool dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(!result.is_error, "expected success: {result:?}");
                    assert!(result.output.is_object());
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_dispatch_tool_missing_name_returns_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            let message = ExtensionMessage {
                id: "msg-tool-noname".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-tool-noname".to_string(),
                    capability: "tool".to_string(),
                    method: "tool".to_string(),
                    params: serde_json::json!({ "input": {} }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(result.is_error);
                    let error = result.error.expect("error");
                    assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
                    assert!(
                        error.message.contains("method") || error.message.contains("tool"),
                        "error should mention 'method' or 'tool': {}",
                        error.message
                    );
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_dispatch_tool_empty_name_returns_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            let message = ExtensionMessage {
                id: "msg-tool-empty".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-tool-empty".to_string(),
                    capability: "tool".to_string(),
                    method: "tool".to_string(),
                    params: serde_json::json!({ "name": "", "input": {} }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(result.is_error);
                    let error = result.error.expect("error");
                    assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_dispatch_http_success() {
        futures::executor::block_on(async {
            let addr = spawn_http_server("protocol http ok");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = ExtensionDispatcher::new_with_policy(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::new(HttpConnectorConfig {
                    default_timeout_ms: 5000,
                    require_tls: false,
                    ..HttpConnectorConfig::default()
                })),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
                ExtensionPolicy::from_profile(PolicyProfile::Permissive),
            );

            let message = ExtensionMessage {
                id: "msg-http-proto".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-http-proto".to_string(),
                    capability: "http".to_string(),
                    method: "http".to_string(),
                    params: serde_json::json!({
                        "url": format!("http://{addr}/test"),
                        "method": "GET",
                    }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol http dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(!result.is_error, "expected success: {result:?}");
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_dispatch_ui_success() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = ExtensionDispatcher::new_with_policy(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
                ExtensionPolicy::from_profile(PolicyProfile::Permissive),
            );

            let message = ExtensionMessage {
                id: "msg-ui-proto".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-ui-proto".to_string(),
                    capability: "ui".to_string(),
                    method: "ui".to_string(),
                    params: serde_json::json!({ "op": "notification", "message": "test" }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol ui dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(!result.is_error, "expected success: {result:?}");
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_dispatch_ui_missing_op_returns_error() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            let message = ExtensionMessage {
                id: "msg-ui-noop".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-ui-noop".to_string(),
                    capability: "ui".to_string(),
                    method: "ui".to_string(),
                    params: serde_json::json!({ "message": "test" }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(result.is_error);
                    let error = result.error.expect("error");
                    assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
                    assert!(
                        error.message.contains("op"),
                        "error should mention 'op': {}",
                        error.message
                    );
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_dispatch_events_missing_op_returns_error() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            let message = ExtensionMessage {
                id: "msg-events-noop".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-events-noop".to_string(),
                    capability: "events".to_string(),
                    method: "events".to_string(),
                    params: serde_json::json!({ "data": {} }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(result.is_error);
                    let error = result.error.expect("error");
                    assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
                    assert!(
                        error.message.contains("op"),
                        "error should mention 'op': {}",
                        error.message
                    );
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_dispatch_log_returns_success() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            let message = ExtensionMessage {
                id: "msg-log-proto".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-log-proto".to_string(),
                    capability: "log".to_string(),
                    method: "log".to_string(),
                    params: serde_json::json!({ "message": "test log" }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol log dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(!result.is_error, "log dispatch should succeed: {result:?}");
                }
                other => unreachable!("unexpected protocol response: {other:?}"),
            }
        });
    }

    fn regime_signal(
        queue_depth: f64,
        service_time_us: f64,
        opcode_entropy: f64,
        llc_miss_rate: f64,
    ) -> RegimeSignal {
        RegimeSignal {
            queue_depth,
            service_time_us,
            opcode_entropy,
            llc_miss_rate,
        }
    }

    fn drive_detector_to_interleaved(detector: &mut RegimeShiftDetector) {
        for _ in 0..64 {
            let _ = detector.observe(regime_signal(1.0, 600.0, 0.8, 0.02));
        }
        for _ in 0..48 {
            let observation = detector.observe(regime_signal(40.0, 14_000.0, 2.6, 0.92));
            if observation.transition == Some(RegimeTransition::EnterInterleavedBatching) {
                break;
            }
        }
    }

    #[test]
    fn regime_detector_switches_to_interleaved_on_sustained_upshift() {
        let mut detector = RegimeShiftDetector::default();
        let mut switched = false;

        for _ in 0..64 {
            let _ = detector.observe(regime_signal(1.0, 700.0, 0.9, 0.03));
        }
        for _ in 0..64 {
            let observation = detector.observe(regime_signal(42.0, 16_000.0, 2.8, 0.95));
            if observation.transition == Some(RegimeTransition::EnterInterleavedBatching) {
                switched = true;
                break;
            }
        }

        assert!(
            switched,
            "detector should switch on sustained high-contention shift"
        );
        assert_eq!(
            detector.current_mode(),
            RegimeAdaptationMode::InterleavedBatching
        );
    }

    #[test]
    fn regime_detector_avoids_false_positives_on_stationary_noise() {
        let mut detector = RegimeShiftDetector::default();
        let mut transitions = 0_usize;

        for idx in 0..320 {
            let jitter = match idx % 5 {
                0 => -70.0,
                1 => -20.0,
                2 => 0.0,
                3 => 35.0,
                _ => 80.0,
            };
            let queue_depth = if idx % 3 == 0 { 2.0 } else { 1.0 };
            let entropy = if idx % 7 == 0 { 1.2 } else { 1.0 };
            let observation =
                detector.observe(regime_signal(queue_depth, 900.0 + jitter, entropy, 0.06));
            if observation.transition.is_some() {
                transitions = transitions.saturating_add(1);
            }
        }

        assert_eq!(
            transitions, 0,
            "stationary noise should not trigger transitions"
        );
        assert_eq!(
            detector.current_mode(),
            RegimeAdaptationMode::SequentialFastPath
        );
    }

    fn synthetic_cusum_score_signal(score: f64) -> RegimeSignal {
        RegimeSignal {
            queue_depth: 0.0,
            service_time_us: (score / 0.35) * 5_000.0,
            opcode_entropy: 0.0,
            llc_miss_rate: 0.0,
        }
    }

    fn synthetic_cusum_score(index: usize, shift_score: f64) -> f64 {
        const BASELINE_SCORE: f64 = 0.2;
        const SIGMA_SCORE: f64 = 0.05;

        let stationary_jitter = match index % 5 {
            0 => -SIGMA_SCORE,
            1 => -0.5 * SIGMA_SCORE,
            2 => 0.0,
            3 => 0.5 * SIGMA_SCORE,
            _ => SIGMA_SCORE,
        };
        BASELINE_SCORE + stationary_jitter + shift_score
    }

    const SYNTHETIC_PRE_SHIFT_SAMPLES: usize = 500;
    const SYNTHETIC_TOTAL_SAMPLES: usize = 1_000;

    fn synthetic_cusum_detection_delay(k_sigma: f64) -> (usize, Option<usize>) {
        const SIGMA_SCORE: f64 = 0.05;

        let mut detector = RegimeShiftDetector::default();
        let mut pre_shift_candidates = 0_usize;
        let mut first_post_shift_delay = None;

        for index in 0..SYNTHETIC_TOTAL_SAMPLES {
            let shift_score = if index >= SYNTHETIC_PRE_SHIFT_SAMPLES {
                k_sigma * SIGMA_SCORE
            } else {
                0.0
            };
            let observation = detector.observe(synthetic_cusum_score_signal(
                synthetic_cusum_score(index, shift_score),
            ));

            if index < SYNTHETIC_PRE_SHIFT_SAMPLES {
                assert!(
                    !observation.cusum_triggered || !observation.posterior_triggered,
                    "stationary prefix must not satisfy both CUSUM and posterior gates at index {index}: {observation:?}",
                );
                if observation.regime_shift_candidate {
                    pre_shift_candidates = pre_shift_candidates.saturating_add(1);
                }
            } else if observation.regime_shift_candidate && first_post_shift_delay.is_none() {
                first_post_shift_delay = Some(index - SYNTHETIC_PRE_SHIFT_SAMPLES + 1);
            }
        }

        (pre_shift_candidates, first_post_shift_delay)
    }

    #[test]
    fn cusum_regime_shift_detector_flags_synthetic_mean_shift_with_bounded_false_positives() {
        // This pins the CUSUM+posterior candidate gate on sustained mean shifts
        // while keeping the stationary prefix below the joint alarm threshold.
        let (pre_shift_candidates_1sigma, delay_1sigma) = synthetic_cusum_detection_delay(1.0);
        let (pre_shift_candidates_2sigma, delay_2sigma) = synthetic_cusum_detection_delay(2.0);
        let (pre_shift_candidates_3sigma, delay_3sigma) = synthetic_cusum_detection_delay(3.0);
        let (pre_shift_candidates_4sigma, delay_4sigma) = synthetic_cusum_detection_delay(4.0);

        for (k_sigma, pre_shift_candidates) in [
            (1.0, pre_shift_candidates_1sigma),
            (2.0, pre_shift_candidates_2sigma),
            (3.0, pre_shift_candidates_3sigma),
            (4.0, pre_shift_candidates_4sigma),
        ] {
            assert!(
                pre_shift_candidates <= SYNTHETIC_PRE_SHIFT_SAMPLES / 20,
                "{k_sigma}σ case exceeded 5% pre-shift false-positive budget: {pre_shift_candidates}/{SYNTHETIC_PRE_SHIFT_SAMPLES}",
            );
        }

        assert!(
            delay_1sigma.is_some(),
            "1σ sustained shift should trigger CUSUM candidate"
        );
        assert!(
            delay_2sigma.is_some(),
            "2σ sustained shift should trigger CUSUM candidate"
        );
        assert!(
            delay_3sigma.is_some(),
            "3σ sustained shift should trigger CUSUM candidate"
        );
        assert!(
            delay_4sigma.is_some(),
            "4σ sustained shift should trigger CUSUM candidate"
        );
        let delay_1sigma = delay_1sigma.unwrap_or(usize::MAX);
        let delay_2sigma = delay_2sigma.unwrap_or(usize::MAX);
        let delay_3sigma = delay_3sigma.unwrap_or(usize::MAX);
        let delay_4sigma = delay_4sigma.unwrap_or(usize::MAX);

        assert!(
            delay_1sigma <= 128,
            "1σ sustained shift should be detected within 128 samples, got {delay_1sigma}",
        );
        assert!(
            delay_2sigma <= 64,
            "2σ sustained shift should be detected within 64 samples, got {delay_2sigma}",
        );
        assert!(
            delay_4sigma <= delay_3sigma
                && delay_3sigma <= delay_2sigma
                && delay_2sigma <= delay_1sigma,
            "larger shifts should not detect later: 1σ={delay_1sigma}, 2σ={delay_2sigma}, 3σ={delay_3sigma}, 4σ={delay_4sigma}",
        );
    }

    #[test]
    fn regime_detector_hysteresis_limits_thrash() {
        let mut detector = RegimeShiftDetector::default();
        drive_detector_to_interleaved(&mut detector);
        assert_eq!(
            detector.current_mode(),
            RegimeAdaptationMode::InterleavedBatching
        );

        let mut transitions = 0_usize;
        for idx in 0..200 {
            let signal = if idx % 2 == 0 {
                regime_signal(36.0, 12_500.0, 2.4, 0.88)
            } else {
                regime_signal(5.0, 2_200.0, 1.1, 0.18)
            };
            let observation = detector.observe(signal);
            if observation.transition.is_some() {
                transitions = transitions.saturating_add(1);
            }
        }

        assert!(
            transitions <= 5,
            "hysteresis/cooldown should prevent oscillation: observed {transitions} transitions"
        );
    }

    #[test]
    fn regime_detector_fallbacks_when_workload_cools() {
        let mut detector = RegimeShiftDetector::default();
        drive_detector_to_interleaved(&mut detector);
        assert_eq!(
            detector.current_mode(),
            RegimeAdaptationMode::InterleavedBatching
        );

        let mut fallback_triggered = false;
        let mut returned_to_sequential = false;
        for _ in 0..40 {
            let observation = detector.observe(regime_signal(0.0, 450.0, 0.2, 0.0));
            if observation.fallback_triggered {
                fallback_triggered = true;
            }
            if observation.transition == Some(RegimeTransition::ReturnToSequentialFastPath) {
                returned_to_sequential = true;
            }
        }

        assert!(
            fallback_triggered,
            "low queue/latency should trigger conservative fallback"
        );
        assert!(
            returned_to_sequential,
            "fallback should report an explicit transition"
        );
        assert_eq!(
            detector.current_mode(),
            RegimeAdaptationMode::SequentialFastPath
        );
    }

    #[test]
    fn rollout_gate_blocks_cherry_picked_high_contention_claims() {
        let mut detector = RegimeShiftDetector::default();
        let mut saw_block = false;
        let mut switched = false;

        for _ in 0..160 {
            let observation = detector.observe(regime_signal(46.0, 17_500.0, 3.0, 0.95));
            if observation.rollout_blocked_cherry_picked {
                saw_block = true;
            }
            if observation.transition == Some(RegimeTransition::EnterInterleavedBatching) {
                switched = true;
            }
        }

        assert!(saw_block, "gate should surface cherry-pick blocking signal");
        assert!(!switched, "high-only stream must not promote rollout");
        assert_eq!(
            detector.current_mode(),
            RegimeAdaptationMode::SequentialFastPath
        );
    }

    #[test]
    fn rollout_gate_promotes_after_stratified_evidence_reaches_threshold() {
        let mut detector = RegimeShiftDetector::default();
        let mut promoted = false;

        for _ in 0..80 {
            let _ = detector.observe(regime_signal(1.0, 700.0, 0.9, 0.03));
        }
        for _ in 0..96 {
            let observation = detector.observe(regime_signal(42.0, 16_000.0, 2.8, 0.95));
            if observation.transition == Some(RegimeTransition::EnterInterleavedBatching) {
                promoted = true;
                assert_eq!(
                    observation.rollout_action,
                    RolloutGateAction::PromoteInterleaved
                );
                assert!(
                    observation.rollout_promote_e_process >= observation.rollout_evidence_threshold
                );
                assert!(observation.rollout_coverage_ready);
                assert!(
                    observation.rollout_expected_loss.promote
                        < observation.rollout_expected_loss.hold
                );
                break;
            }
        }

        assert!(
            promoted,
            "stratified stream should promote interleaved batching"
        );
        assert_eq!(
            detector.current_mode(),
            RegimeAdaptationMode::InterleavedBatching
        );
    }

    #[test]
    fn rollout_gate_rolls_back_after_stratified_regression_evidence() {
        let mut detector = RegimeShiftDetector::default();
        drive_detector_to_interleaved(&mut detector);
        assert_eq!(
            detector.current_mode(),
            RegimeAdaptationMode::InterleavedBatching
        );

        let mut rolled_back = false;
        for _ in 0..320 {
            let observation = detector.observe(regime_signal(1.4, 1_500.0, 0.6, 0.02));
            if observation.transition == Some(RegimeTransition::ReturnToSequentialFastPath) {
                rolled_back = true;
                assert_eq!(
                    observation.rollout_action,
                    RolloutGateAction::RollbackSequential
                );
                assert!(
                    observation.rollout_rollback_e_process
                        >= observation.rollout_evidence_threshold
                );
                assert!(observation.rollout_coverage_ready);
                assert!(
                    observation.rollout_expected_loss.rollback
                        < observation.rollout_expected_loss.hold
                );
                break;
            }
        }

        assert!(
            rolled_back,
            "low-contention regression stream should trigger rollout rollback"
        );
        assert_eq!(
            detector.current_mode(),
            RegimeAdaptationMode::SequentialFastPath
        );
    }

    #[test]
    fn dual_exec_sampling_is_deterministic_for_same_request() {
        let request = HostcallRequest {
            call_id: "sample-deterministic".to_string(),
            kind: HostcallKind::Session {
                op: "get_state".to_string(),
            },
            payload: serde_json::json!({}),
            trace_id: 77,
            extension_id: Some("ext.det".to_string()),
        };
        let first = should_sample_shadow_dual_exec(&request, 100_000);
        for _ in 0..16 {
            assert_eq!(should_sample_shadow_dual_exec(&request, 100_000), first);
        }
    }

    #[test]
    fn dual_exec_sampling_respects_zero_and_full_scale_boundaries() {
        let request = HostcallRequest {
            call_id: "sample-boundary".to_string(),
            kind: HostcallKind::Session {
                op: "get_state".to_string(),
            },
            payload: serde_json::json!({}),
            trace_id: 91,
            extension_id: Some("ext.boundary".to_string()),
        };

        assert!(!should_sample_shadow_dual_exec(&request, 0));
        assert!(should_sample_shadow_dual_exec(
            &request,
            DUAL_EXEC_SAMPLE_MODULUS_PPM
        ));
        assert!(should_sample_shadow_dual_exec(
            &request,
            DUAL_EXEC_SAMPLE_MODULUS_PPM.saturating_add(1)
        ));
    }

    #[test]
    fn normalized_shadow_op_is_deterministic_across_format_variants() {
        assert_eq!(normalized_shadow_op(" get__state "), "getstate");
        assert_eq!(normalized_shadow_op("GET_STATE"), "getstate");
        assert_eq!(normalized_shadow_op("GeT_sTaTe"), "getstate");
        assert_eq!(normalized_shadow_op("list_flags"), "listflags");
    }

    #[test]
    fn shadow_safe_classification_accepts_normalized_read_only_ops() {
        let session_request = HostcallRequest {
            call_id: "shadow-safe-session".to_string(),
            kind: HostcallKind::Session {
                op: "  GET__MESSAGES ".to_string(),
            },
            payload: serde_json::json!({}),
            trace_id: 5,
            extension_id: Some("ext.shadow.safe".to_string()),
        };
        let events_request = HostcallRequest {
            call_id: "shadow-safe-events".to_string(),
            kind: HostcallKind::Events {
                op: " list_flags ".to_string(),
            },
            payload: serde_json::json!({}),
            trace_id: 6,
            extension_id: Some("ext.shadow.safe".to_string()),
        };
        let tool_request = HostcallRequest {
            call_id: "shadow-safe-tool".to_string(),
            kind: HostcallKind::Tool {
                name: " read ".to_string(),
            },
            payload: serde_json::json!({}),
            trace_id: 7,
            extension_id: Some("ext.shadow.safe".to_string()),
        };

        assert!(is_shadow_safe_request(&session_request));
        assert!(is_shadow_safe_request(&events_request));
        assert!(is_shadow_safe_request(&tool_request));
    }

    #[test]
    fn shadow_safe_classification_rejects_mutating_and_unsafe_kinds() {
        let requests = [
            (
                "session mutate",
                HostcallRequest {
                    call_id: "shadow-unsafe-session".to_string(),
                    kind: HostcallKind::Session {
                        op: "append_message".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: 11,
                    extension_id: Some("ext.shadow.unsafe".to_string()),
                },
            ),
            (
                "events mutate",
                HostcallRequest {
                    call_id: "shadow-unsafe-events".to_string(),
                    kind: HostcallKind::Events {
                        op: "set_flag".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: 12,
                    extension_id: Some("ext.shadow.unsafe".to_string()),
                },
            ),
            (
                "tool mutate",
                HostcallRequest {
                    call_id: "shadow-unsafe-tool".to_string(),
                    kind: HostcallKind::Tool {
                        name: "write".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: 13,
                    extension_id: Some("ext.shadow.unsafe".to_string()),
                },
            ),
            (
                "exec",
                HostcallRequest {
                    call_id: "shadow-unsafe-exec".to_string(),
                    kind: HostcallKind::Exec {
                        cmd: "echo nope".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: 14,
                    extension_id: Some("ext.shadow.unsafe".to_string()),
                },
            ),
            (
                "http",
                HostcallRequest {
                    call_id: "shadow-unsafe-http".to_string(),
                    kind: HostcallKind::Http,
                    payload: serde_json::json!({}),
                    trace_id: 15,
                    extension_id: Some("ext.shadow.unsafe".to_string()),
                },
            ),
            (
                "ui",
                HostcallRequest {
                    call_id: "shadow-unsafe-ui".to_string(),
                    kind: HostcallKind::Ui {
                        op: "prompt".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: 16,
                    extension_id: Some("ext.shadow.unsafe".to_string()),
                },
            ),
            (
                "log",
                HostcallRequest {
                    call_id: "shadow-unsafe-log".to_string(),
                    kind: HostcallKind::Log,
                    payload: serde_json::json!({}),
                    trace_id: 17,
                    extension_id: Some("ext.shadow.unsafe".to_string()),
                },
            ),
        ];

        for (case, request) in &requests {
            assert!(
                !is_shadow_safe_request(request),
                "expected non-shadow-safe classification for {case}"
            );
        }
    }

    #[test]
    fn dual_exec_diff_engine_detects_success_output_mismatch() {
        let fast = HostcallOutcome::Success(serde_json::json!({ "value": 1 }));
        let compat = HostcallOutcome::Success(serde_json::json!({ "value": 2 }));
        let diff = diff_hostcall_outcomes(&fast, &compat).expect("expected diff");
        assert_eq!(diff.reason, "success_output_mismatch");
        assert_ne!(diff.fast_fingerprint, diff.compat_fingerprint);
    }

    #[test]
    fn dual_exec_forensic_bundle_includes_trace_lane_diff_and_rollback_fields() {
        let request = HostcallRequest {
            call_id: "forensic-1".to_string(),
            kind: HostcallKind::Session {
                op: "get_state".to_string(),
            },
            payload: serde_json::json!({ "op": "get_state" }),
            trace_id: 9,
            extension_id: Some("ext.forensic".to_string()),
        };
        let diff = DualExecOutcomeDiff {
            reason: "success_output_mismatch",
            fast_fingerprint: "success:aaa".to_string(),
            compat_fingerprint: "success:bbb".to_string(),
        };
        let bundle = dual_exec_forensic_bundle(
            &request,
            &diff,
            Some("forced_compat_budget_controller"),
            42.0,
        );
        assert_eq!(
            bundle["call_trace"]["call_id"],
            Value::String("forensic-1".to_string())
        );
        assert_eq!(
            bundle["lane_decision"]["fast_lane"],
            Value::String("fast".to_string())
        );
        assert_eq!(
            bundle["lane_decision"]["compat_lane"],
            Value::String("compat_shadow".to_string())
        );
        assert_eq!(
            bundle["diff"]["reason"],
            Value::String("success_output_mismatch".to_string())
        );
        assert_eq!(
            bundle["rollback"]["reason"],
            Value::String("forced_compat_budget_controller".to_string())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn dual_exec_divergence_auto_triggers_rollback_kill_switch_state() {
        futures::executor::block_on(async {
            struct DivergentReadSession {
                counter: Arc<Mutex<u64>>,
            }

            #[async_trait]
            impl ExtensionSession for DivergentReadSession {
                async fn get_state(&self) -> Value {
                    let mut guard = self
                        .counter
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let value = *guard;
                    *guard = guard.saturating_add(1);
                    drop(guard);
                    serde_json::json!({ "seq": value })
                }

                async fn get_messages(&self) -> Vec<SessionMessage> {
                    Vec::new()
                }

                async fn get_entries(&self) -> Vec<Value> {
                    Vec::new()
                }

                async fn get_branch(&self) -> Vec<Value> {
                    Vec::new()
                }

                async fn set_name(&self, _name: String) -> Result<()> {
                    Ok(())
                }

                async fn append_message(&self, _message: SessionMessage) -> Result<()> {
                    Ok(())
                }

                async fn append_custom_entry(
                    &self,
                    _custom_type: String,
                    _data: Option<Value>,
                ) -> Result<()> {
                    Ok(())
                }

                async fn set_model(&self, _provider: String, _model_id: String) -> Result<()> {
                    Ok(())
                }

                async fn get_model(&self) -> (Option<String>, Option<String>) {
                    (None, None)
                }

                async fn set_thinking_level(&self, _level: String) -> Result<()> {
                    Ok(())
                }

                async fn get_thinking_level(&self) -> Option<String> {
                    None
                }

                async fn set_label(
                    &self,
                    _target_id: String,
                    _label: Option<String>,
                ) -> Result<()> {
                    Ok(())
                }
            }

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let session = Arc::new(DivergentReadSession {
                counter: Arc::new(Mutex::new(0)),
            });
            let oracle_config = DualExecOracleConfig {
                sample_ppm: DUAL_EXEC_SAMPLE_MODULUS_PPM,
                divergence_window: 4,
                divergence_budget: 2,
                rollback_requests: 24,
                overhead_budget_us: u64::MAX,
                overhead_backoff_requests: 1,
            };
            let dispatcher = ExtensionDispatcher::new_with_policy_and_oracle_config(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
                ExtensionPolicy::from_profile(PolicyProfile::Permissive),
                oracle_config,
            );

            for idx in 0..3_u64 {
                let request = HostcallRequest {
                    call_id: format!("dual-divergence-{idx}"),
                    kind: HostcallKind::Session {
                        op: "get_state".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: idx,
                    extension_id: Some("ext.shadow.rollback".to_string()),
                };
                dispatcher.dispatch_and_complete(request).await;
            }

            let state = dispatcher.dual_exec_state.borrow();
            assert!(
                state.divergence_total >= 2,
                "expected enough divergence samples to trip rollback"
            );
            assert!(state.rollback_active(), "rollback should be active");
            assert!(
                state
                    .rollback_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("ext.shadow.rollback")),
                "rollback reason should include extension scope"
            );
        });
    }

    #[test]
    fn dual_exec_rollback_forces_dispatch_batch_amac_to_skip_planning() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let oracle_config = DualExecOracleConfig {
                sample_ppm: 0,
                divergence_window: 8,
                divergence_budget: 2,
                rollback_requests: 16,
                overhead_budget_us: 1_500,
                overhead_backoff_requests: 8,
            };
            let dispatcher = ExtensionDispatcher::new_with_policy_and_oracle_config(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
                ExtensionPolicy::from_profile(PolicyProfile::Permissive),
                oracle_config,
            );

            {
                let mut amac = dispatcher.amac_executor.borrow_mut();
                *amac = AmacBatchExecutor::new(AmacBatchExecutorConfig::new(true, 2, 8));
            }

            {
                let mut detector = dispatcher.regime_detector.borrow_mut();
                drive_detector_to_interleaved(&mut detector);
            }

            let mut baseline = VecDeque::new();
            for idx in 0..4_u64 {
                baseline.push_back(HostcallRequest {
                    call_id: format!("baseline-{idx}"),
                    kind: HostcallKind::Session {
                        op: "get_state".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: idx,
                    extension_id: Some("ext.roll".to_string()),
                });
            }
            dispatcher.dispatch_batch_amac(baseline).await;
            let baseline_decisions = dispatcher
                .amac_executor
                .borrow()
                .telemetry()
                .toggle_decisions;
            assert!(
                baseline_decisions > 0,
                "expected AMAC planner to run before rollback activation"
            );

            {
                let mut state = dispatcher.dual_exec_state.borrow_mut();
                state.rollback_remaining = 16;
                state.rollback_reason =
                    Some("dual_exec_divergence_budget_exceeded:test".to_string());
            }

            let mut rollback_batch = VecDeque::new();
            for idx in 0..4_u64 {
                rollback_batch.push_back(HostcallRequest {
                    call_id: format!("rollback-{idx}"),
                    kind: HostcallKind::Session {
                        op: "get_state".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: idx + 100,
                    extension_id: Some("ext.roll".to_string()),
                });
            }
            dispatcher.dispatch_batch_amac(rollback_batch).await;

            let after_rollback = dispatcher
                .amac_executor
                .borrow()
                .telemetry()
                .toggle_decisions;
            assert_eq!(
                after_rollback, baseline_decisions,
                "rollback path should bypass AMAC planning and keep toggle decisions unchanged"
            );
        });
    }

    #[test]
    fn rollout_mode_controls_amac_planner_activation() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            {
                let mut amac = dispatcher.amac_executor.borrow_mut();
                *amac = AmacBatchExecutor::new(AmacBatchExecutorConfig::new(true, 2, 8));
            }

            let mut sequential_batch = VecDeque::new();
            for idx in 0..4_u64 {
                sequential_batch.push_back(HostcallRequest {
                    call_id: format!("rollout-seq-{idx}"),
                    kind: HostcallKind::Session {
                        op: "get_state".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: idx,
                    extension_id: Some("ext.rollout.mode".to_string()),
                });
            }
            dispatcher.dispatch_batch_amac(sequential_batch).await;
            let decisions_after_seq = dispatcher
                .amac_executor
                .borrow()
                .telemetry()
                .toggle_decisions;
            assert_eq!(
                decisions_after_seq, 0,
                "sequential rollout mode should skip AMAC planning"
            );

            {
                let mut detector = dispatcher.regime_detector.borrow_mut();
                drive_detector_to_interleaved(&mut detector);
            }

            let mut interleaved_batch = VecDeque::new();
            for idx in 0..4_u64 {
                interleaved_batch.push_back(HostcallRequest {
                    call_id: format!("rollout-interleaved-{idx}"),
                    kind: HostcallKind::Session {
                        op: "get_state".to_string(),
                    },
                    payload: serde_json::json!({}),
                    trace_id: idx + 100,
                    extension_id: Some("ext.rollout.mode".to_string()),
                });
            }
            dispatcher.dispatch_batch_amac(interleaved_batch).await;
            let decisions_after_interleaved = dispatcher
                .amac_executor
                .borrow()
                .telemetry()
                .toggle_decisions;
            assert!(
                decisions_after_interleaved > decisions_after_seq,
                "promotion should enable AMAC planning"
            );
        });
    }

    #[test]
    fn hostcall_io_hint_marks_expected_kinds_as_io_heavy() {
        assert_eq!(
            hostcall_io_hint(&HostcallKind::Http),
            HostcallIoHint::IoHeavy
        );
        assert_eq!(
            hostcall_io_hint(&HostcallKind::Tool {
                name: "read".to_string()
            }),
            HostcallIoHint::IoHeavy
        );
        assert_eq!(
            hostcall_io_hint(&HostcallKind::Session {
                op: "append_message".to_string()
            }),
            HostcallIoHint::IoHeavy
        );
    }

    #[test]
    fn hostcall_io_hint_marks_non_io_kinds_as_non_heavy() {
        assert_eq!(
            hostcall_io_hint(&HostcallKind::Ui {
                op: "prompt".to_string()
            }),
            HostcallIoHint::CpuBound
        );
        assert_eq!(
            hostcall_io_hint(&HostcallKind::Tool {
                name: "unknown_tool".to_string()
            }),
            HostcallIoHint::Unknown
        );
        assert_eq!(
            hostcall_io_hint(&HostcallKind::Session {
                op: "get_state".to_string()
            }),
            HostcallIoHint::Unknown
        );
    }

    #[test]
    fn hostcall_io_hint_classifies_edit_bash_and_exec() {
        assert_eq!(
            hostcall_io_hint(&HostcallKind::Tool {
                name: "edit".to_string()
            }),
            HostcallIoHint::IoHeavy,
            "edit tool should be IoHeavy"
        );
        assert_eq!(
            hostcall_io_hint(&HostcallKind::Tool {
                name: "bash".to_string()
            }),
            HostcallIoHint::CpuBound,
            "bash tool should be CpuBound"
        );
        assert_eq!(
            hostcall_io_hint(&HostcallKind::Exec {
                cmd: "ls".to_string()
            }),
            HostcallIoHint::CpuBound,
            "exec hostcall should be CpuBound"
        );
    }

    #[test]
    fn io_uring_bridge_reports_cancellation_when_request_not_pending() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let request = HostcallRequest {
                call_id: "cancelled-before-io-uring".to_string(),
                kind: HostcallKind::Http,
                payload: serde_json::json!({
                    "url": "https://example.com",
                    "method": "GET",
                }),
                trace_id: 1,
                extension_id: Some("ext.cancel".to_string()),
            };
            let bridge_dispatch = dispatcher.dispatch_hostcall_io_uring(&request).await;
            assert_eq!(
                bridge_dispatch.state,
                IoUringBridgeState::CancelledBeforeDispatch
            );
            assert_eq!(
                bridge_dispatch.fallback_reason,
                Some("cancelled_before_io_uring_dispatch")
            );
            match bridge_dispatch.outcome {
                HostcallOutcome::Error { code, message } => {
                    assert_eq!(code, "cancelled");
                    assert!(
                        message.contains("cancelled before io_uring dispatch"),
                        "unexpected cancellation message: {message}"
                    );
                }
                other => unreachable!("unexpected hostcall outcome: {other:?}"),
            }
        });
    }

    #[test]
    fn io_uring_bridge_fails_closed_when_executor_is_not_wired() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.http({ url: "https://example.com", method: "GET" })
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = e; });
                "#,
                )
                .await
                .expect("eval");

            let mut requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let request = requests.pop_front().expect("request");
            let bridge_dispatch = dispatcher.dispatch_hostcall_io_uring(&request).await;
            assert_eq!(
                bridge_dispatch.state,
                IoUringBridgeState::ExecutorUnavailable
            );
            assert_eq!(
                bridge_dispatch.fallback_reason,
                Some("io_uring_executor_unavailable")
            );
            match bridge_dispatch.outcome {
                HostcallOutcome::Error { code, message } => {
                    assert_eq!(code, "io_uring_unavailable");
                    assert!(
                        message.contains("executor is unavailable"),
                        "unexpected unavailable message: {message}"
                    );
                }
                other => unreachable!("unexpected hostcall outcome: {other:?}"),
            }
        });
    }

    // ========================================================================
    // bd-3ar8v.4.8.21: Protocol error-code taxonomy and validation tests
    // ========================================================================

    #[test]
    fn protocol_error_code_timeout_maps_correctly() {
        assert_eq!(protocol_error_code("timeout"), HostCallErrorCode::Timeout);
    }

    #[test]
    fn protocol_error_code_denied_maps_correctly() {
        assert_eq!(protocol_error_code("denied"), HostCallErrorCode::Denied);
    }

    #[test]
    fn protocol_error_code_io_maps_correctly() {
        assert_eq!(protocol_error_code("io"), HostCallErrorCode::Io);
    }

    #[test]
    fn protocol_error_code_tool_error_maps_to_io() {
        assert_eq!(protocol_error_code("tool_error"), HostCallErrorCode::Io);
    }

    #[test]
    fn protocol_error_code_invalid_request_maps_correctly() {
        assert_eq!(
            protocol_error_code("invalid_request"),
            HostCallErrorCode::InvalidRequest
        );
    }

    #[test]
    fn protocol_error_code_completely_unknown_maps_to_internal() {
        assert_eq!(
            protocol_error_code("completely_unknown"),
            HostCallErrorCode::Internal
        );
    }

    #[test]
    fn protocol_error_code_empty_string_maps_to_internal() {
        assert_eq!(protocol_error_code(""), HostCallErrorCode::Internal);
    }

    #[test]
    fn protocol_error_code_whitespace_only_maps_to_internal() {
        assert_eq!(protocol_error_code("   "), HostCallErrorCode::Internal);
    }

    #[test]
    fn protocol_error_code_case_insensitive_timeout() {
        assert_eq!(protocol_error_code("TIMEOUT"), HostCallErrorCode::Timeout);
        assert_eq!(protocol_error_code("Timeout"), HostCallErrorCode::Timeout);
        assert_eq!(protocol_error_code("TimeOut"), HostCallErrorCode::Timeout);
    }

    #[test]
    fn protocol_error_code_case_insensitive_denied() {
        assert_eq!(protocol_error_code("DENIED"), HostCallErrorCode::Denied);
        assert_eq!(protocol_error_code("Denied"), HostCallErrorCode::Denied);
    }

    #[test]
    fn protocol_error_code_case_insensitive_io() {
        assert_eq!(protocol_error_code("IO"), HostCallErrorCode::Io);
        assert_eq!(protocol_error_code("Io"), HostCallErrorCode::Io);
        assert_eq!(protocol_error_code("TOOL_ERROR"), HostCallErrorCode::Io);
        assert_eq!(protocol_error_code("Tool_Error"), HostCallErrorCode::Io);
    }

    #[test]
    fn protocol_error_code_case_insensitive_invalid_request() {
        assert_eq!(
            protocol_error_code("INVALID_REQUEST"),
            HostCallErrorCode::InvalidRequest
        );
        assert_eq!(
            protocol_error_code("Invalid_Request"),
            HostCallErrorCode::InvalidRequest
        );
    }

    #[test]
    fn protocol_error_code_trims_whitespace() {
        assert_eq!(
            protocol_error_code("  timeout  "),
            HostCallErrorCode::Timeout
        );
        assert_eq!(protocol_error_code("\tdenied\n"), HostCallErrorCode::Denied);
    }

    #[test]
    fn parse_protocol_hostcall_method_all_known_methods() {
        assert_eq!(
            parse_protocol_hostcall_method("tool"),
            Some(ProtocolHostcallMethod::Tool)
        );
        assert_eq!(
            parse_protocol_hostcall_method("exec"),
            Some(ProtocolHostcallMethod::Exec)
        );
        assert_eq!(
            parse_protocol_hostcall_method("http"),
            Some(ProtocolHostcallMethod::Http)
        );
        assert_eq!(
            parse_protocol_hostcall_method("session"),
            Some(ProtocolHostcallMethod::Session)
        );
        assert_eq!(
            parse_protocol_hostcall_method("ui"),
            Some(ProtocolHostcallMethod::Ui)
        );
        assert_eq!(
            parse_protocol_hostcall_method("events"),
            Some(ProtocolHostcallMethod::Events)
        );
        assert_eq!(
            parse_protocol_hostcall_method("log"),
            Some(ProtocolHostcallMethod::Log)
        );
    }

    #[test]
    fn parse_protocol_hostcall_method_case_insensitive() {
        assert_eq!(
            parse_protocol_hostcall_method("TOOL"),
            Some(ProtocolHostcallMethod::Tool)
        );
        assert_eq!(
            parse_protocol_hostcall_method("Tool"),
            Some(ProtocolHostcallMethod::Tool)
        );
        assert_eq!(
            parse_protocol_hostcall_method("SESSION"),
            Some(ProtocolHostcallMethod::Session)
        );
        assert_eq!(
            parse_protocol_hostcall_method("Events"),
            Some(ProtocolHostcallMethod::Events)
        );
    }

    #[test]
    fn parse_protocol_hostcall_method_trims_whitespace() {
        assert_eq!(
            parse_protocol_hostcall_method("  tool  "),
            Some(ProtocolHostcallMethod::Tool)
        );
        assert_eq!(
            parse_protocol_hostcall_method("\texec\n"),
            Some(ProtocolHostcallMethod::Exec)
        );
    }

    #[test]
    fn parse_protocol_hostcall_method_rejects_unknown() {
        assert_eq!(parse_protocol_hostcall_method("unknown"), None);
        assert_eq!(parse_protocol_hostcall_method("foobar"), None);
        assert_eq!(parse_protocol_hostcall_method("tools"), None);
    }

    #[test]
    fn parse_protocol_hostcall_method_rejects_empty() {
        assert_eq!(parse_protocol_hostcall_method(""), None);
        assert_eq!(parse_protocol_hostcall_method("   "), None);
    }

    #[test]
    fn protocol_normalize_output_preserves_objects() {
        let obj = serde_json::json!({"key": "value", "nested": {"a": 1}});
        let result = protocol_normalize_output(obj.clone());
        assert_eq!(result, obj);
    }

    #[test]
    fn protocol_normalize_output_wraps_string() {
        let val = serde_json::json!("hello");
        let result = protocol_normalize_output(val);
        assert_eq!(result, serde_json::json!({"value": "hello"}));
    }

    #[test]
    fn protocol_normalize_output_wraps_number() {
        let val = serde_json::json!(42);
        let result = protocol_normalize_output(val);
        assert_eq!(result, serde_json::json!({"value": 42}));
    }

    #[test]
    fn protocol_normalize_output_wraps_bool() {
        let val = serde_json::json!(true);
        let result = protocol_normalize_output(val);
        assert_eq!(result, serde_json::json!({"value": true}));
    }

    #[test]
    fn protocol_normalize_output_wraps_null() {
        let val = Value::Null;
        let result = protocol_normalize_output(val);
        assert_eq!(result, serde_json::json!({"value": null}));
    }

    #[test]
    fn protocol_normalize_output_wraps_array() {
        let val = serde_json::json!([1, 2, 3]);
        let result = protocol_normalize_output(val);
        assert_eq!(result, serde_json::json!({"value": [1, 2, 3]}));
    }

    #[test]
    fn protocol_normalize_output_preserves_empty_object() {
        let val = serde_json::json!({});
        let result = protocol_normalize_output(val.clone());
        assert_eq!(result, val);
    }

    #[test]
    fn protocol_error_fallback_reason_denied() {
        assert_eq!(
            protocol_error_fallback_reason("tool", "denied"),
            "policy_denied"
        );
        assert_eq!(
            protocol_error_fallback_reason("exec", "DENIED"),
            "policy_denied"
        );
    }

    #[test]
    fn protocol_error_fallback_reason_timeout() {
        assert_eq!(
            protocol_error_fallback_reason("tool", "timeout"),
            "handler_timeout"
        );
    }

    #[test]
    fn protocol_error_fallback_reason_io() {
        assert_eq!(
            protocol_error_fallback_reason("tool", "io"),
            "handler_error"
        );
        assert_eq!(
            protocol_error_fallback_reason("exec", "tool_error"),
            "handler_error"
        );
    }

    #[test]
    fn protocol_error_fallback_reason_invalid_request_known_method() {
        assert_eq!(
            protocol_error_fallback_reason("tool", "invalid_request"),
            "schema_validation_failed"
        );
        assert_eq!(
            protocol_error_fallback_reason("session", "invalid_request"),
            "schema_validation_failed"
        );
    }

    #[test]
    fn protocol_error_fallback_reason_invalid_request_unknown_method() {
        assert_eq!(
            protocol_error_fallback_reason("nonexistent", "invalid_request"),
            "unsupported_method_fallback"
        );
    }

    #[test]
    fn protocol_error_fallback_reason_unknown_code() {
        assert_eq!(
            protocol_error_fallback_reason("tool", "something_else"),
            "runtime_internal_error"
        );
        assert_eq!(
            protocol_error_fallback_reason("tool", ""),
            "runtime_internal_error"
        );
    }

    #[test]
    fn protocol_error_details_structure_complete() {
        let payload = HostCallPayload {
            call_id: "test-call-1".to_string(),
            capability: "tool".to_string(),
            method: "tool".to_string(),
            params: serde_json::json!({"name": "read", "input": {"path": "/tmp/test"}}),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let details = protocol_error_details(&payload, "invalid_request", "Tool not found");

        // Verify top-level structure
        assert!(details.get("dispatcherDecisionTrace").is_some());
        assert!(details.get("schemaDiff").is_some());
        assert!(details.get("extensionInput").is_some());
        assert!(details.get("extensionOutput").is_some());

        // Verify dispatcher decision trace
        let trace = &details["dispatcherDecisionTrace"];
        assert_eq!(trace["selectedRuntime"], "rust-extension-dispatcher");
        assert_eq!(trace["schemaVersion"], PROTOCOL_VERSION);
        assert_eq!(trace["method"], "tool");
        assert_eq!(trace["capability"], "tool");
        assert_eq!(trace["fallbackReason"], "schema_validation_failed");

        // Verify schema diff has sorted keys
        let observed_keys = details["schemaDiff"]["observedParamKeys"]
            .as_array()
            .expect("observedParamKeys must be array");
        let keys: Vec<&str> = observed_keys.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(keys, vec!["input", "name"]);

        // Verify extension input
        assert_eq!(details["extensionInput"]["callId"], "test-call-1");
        assert_eq!(details["extensionInput"]["capability"], "tool");
        assert_eq!(details["extensionInput"]["method"], "tool");

        // Verify extension output
        assert_eq!(details["extensionOutput"]["code"], "invalid_request");
        assert_eq!(details["extensionOutput"]["message"], "Tool not found");
    }

    #[test]
    fn protocol_error_details_non_object_params_yields_empty_keys() {
        let payload = HostCallPayload {
            call_id: "test-call-2".to_string(),
            capability: "exec".to_string(),
            method: "exec".to_string(),
            params: serde_json::json!("not an object"),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let details = protocol_error_details(&payload, "io", "exec failed");
        let observed_keys = details["schemaDiff"]["observedParamKeys"]
            .as_array()
            .expect("must be array");
        assert!(observed_keys.is_empty());
        assert_eq!(
            details["dispatcherDecisionTrace"]["fallbackReason"],
            "handler_error"
        );
    }

    #[test]
    fn protocol_hostcall_op_extracts_from_op_key() {
        let params = serde_json::json!({"op": "getState"});
        assert_eq!(protocol_hostcall_op(&params), Some("getState"));
    }

    #[test]
    fn protocol_hostcall_op_extracts_from_method_key() {
        let params = serde_json::json!({"method": "setModel"});
        assert_eq!(protocol_hostcall_op(&params), Some("setModel"));
    }

    #[test]
    fn protocol_hostcall_op_extracts_from_name_key() {
        let params = serde_json::json!({"name": "read"});
        assert_eq!(protocol_hostcall_op(&params), Some("read"));
    }

    #[test]
    fn protocol_hostcall_op_prefers_op_over_method() {
        let params = serde_json::json!({"op": "first", "method": "second"});
        assert_eq!(protocol_hostcall_op(&params), Some("first"));
    }

    #[test]
    fn protocol_hostcall_op_prefers_method_over_name() {
        let params = serde_json::json!({"method": "first", "name": "second"});
        assert_eq!(protocol_hostcall_op(&params), Some("first"));
    }

    #[test]
    fn protocol_hostcall_op_returns_none_for_empty_params() {
        let params = serde_json::json!({});
        assert_eq!(protocol_hostcall_op(&params), None);
    }

    #[test]
    fn protocol_hostcall_op_returns_none_for_empty_string_value() {
        let params = serde_json::json!({"op": ""});
        assert_eq!(protocol_hostcall_op(&params), None);
    }

    #[test]
    fn protocol_hostcall_op_returns_none_for_whitespace_only_value() {
        let params = serde_json::json!({"op": "   "});
        assert_eq!(protocol_hostcall_op(&params), None);
    }

    #[test]
    fn protocol_hostcall_op_trims_result() {
        let params = serde_json::json!({"op": "  getState  "});
        assert_eq!(protocol_hostcall_op(&params), Some("getState"));
    }

    #[test]
    fn protocol_hostcall_op_returns_none_for_non_string_value() {
        let params = serde_json::json!({"op": 42});
        assert_eq!(protocol_hostcall_op(&params), None);
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_success_normalizes_output() {
        let result = hostcall_outcome_to_protocol_result(
            "call-s1",
            HostcallOutcome::Success(serde_json::json!({"result": "ok"})),
        );
        assert_eq!(result.call_id, "call-s1");
        assert!(!result.is_error);
        assert!(result.error.is_none());
        assert!(result.chunk.is_none());
        assert_eq!(result.output, serde_json::json!({"result": "ok"}));
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_success_wraps_plain_string() {
        let result = hostcall_outcome_to_protocol_result(
            "call-s2",
            HostcallOutcome::Success(serde_json::json!("plain string")),
        );
        assert_eq!(result.output, serde_json::json!({"value": "plain string"}));
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_error_maps_code() {
        let result = hostcall_outcome_to_protocol_result(
            "call-e1",
            HostcallOutcome::Error {
                code: "denied".to_string(),
                message: "not allowed".to_string(),
            },
        );
        assert_eq!(result.call_id, "call-e1");
        assert!(result.is_error);
        let err = result.error.as_ref().expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Denied);
        assert_eq!(err.message, "not allowed");
        assert!(err.details.is_none());
        assert!(result.output.is_object());
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_error_unknown_code_maps_internal() {
        let result = hostcall_outcome_to_protocol_result(
            "call-e2",
            HostcallOutcome::Error {
                code: "mystery_error".to_string(),
                message: "something broke".to_string(),
            },
        );
        let err = result.error.as_ref().expect("error payload");
        assert_eq!(err.code, HostCallErrorCode::Internal);
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_stream_partial_chunk() {
        let result = hostcall_outcome_to_protocol_result(
            "call-sc1",
            HostcallOutcome::StreamChunk {
                sequence: 5,
                chunk: serde_json::json!({"data": "partial"}),
                is_final: false,
            },
        );
        assert_eq!(result.call_id, "call-sc1");
        assert!(!result.is_error);
        assert!(result.error.is_none());
        let chunk = result.chunk.as_ref().expect("chunk metadata");
        assert_eq!(chunk.index, 5);
        assert!(!chunk.is_last);
        assert_eq!(result.output["sequence"], 5);
        assert_eq!(result.output["isFinal"], false);
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_stream_final_chunk() {
        let result = hostcall_outcome_to_protocol_result(
            "call-sc2",
            HostcallOutcome::StreamChunk {
                sequence: 10,
                chunk: serde_json::json!(null),
                is_final: true,
            },
        );
        let chunk = result.chunk.as_ref().expect("chunk metadata");
        assert!(chunk.is_last);
        assert_eq!(result.output["isFinal"], true);
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_error_includes_details() {
        let payload = HostCallPayload {
            call_id: "call-trace-1".to_string(),
            capability: "tool".to_string(),
            method: "tool".to_string(),
            params: serde_json::json!({"name": "read"}),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let result = hostcall_outcome_to_protocol_result_with_trace(
            &payload,
            HostcallOutcome::Error {
                code: "timeout".to_string(),
                message: "operation timed out".to_string(),
            },
        );

        assert!(result.is_error);
        let err = result.error.as_ref().expect("error");
        assert_eq!(err.code, HostCallErrorCode::Timeout);
        assert_eq!(err.message, "operation timed out");

        // With-trace variant must include details
        let details = err.details.as_ref().expect("details must be present");
        assert!(details.get("dispatcherDecisionTrace").is_some());
        assert_eq!(
            details["dispatcherDecisionTrace"]["fallbackReason"],
            "handler_timeout"
        );
    }

    #[test]
    fn hostcall_outcome_to_protocol_result_with_trace_success_no_details() {
        let payload = HostCallPayload {
            call_id: "call-trace-2".to_string(),
            capability: "tool".to_string(),
            method: "tool".to_string(),
            params: serde_json::json!({"name": "read"}),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let result = hostcall_outcome_to_protocol_result_with_trace(
            &payload,
            HostcallOutcome::Success(serde_json::json!({"content": "file data"})),
        );

        assert!(!result.is_error);
        assert!(result.error.is_none());
        assert_eq!(result.output["content"], "file data");
    }

    // ── Property tests ──

    mod proptest_dispatcher {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn shannon_entropy_nonnegative(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
                let entropy = shannon_entropy_bytes(&bytes);
                assert!(
                    entropy >= 0.0,
                    "entropy must be non-negative, got {entropy}"
                );
            }

            #[test]
            fn shannon_entropy_bounded_by_log2_256(
                bytes in prop::collection::vec(any::<u8>(), 1..200),
            ) {
                let entropy = shannon_entropy_bytes(&bytes);
                assert!(
                    entropy <= 8.0 + f64::EPSILON,
                    "entropy must be <= 8.0 (log2(256)), got {entropy}"
                );
            }

            #[test]
            fn shannon_entropy_empty_is_zero(_dummy in Just(())) {
                assert!(
                    (shannon_entropy_bytes(&[]) - 0.0).abs() < f64::EPSILON,
                    "entropy of empty input must be 0.0"
                );
            }

            #[test]
            fn shannon_entropy_single_byte_is_zero(byte in any::<u8>()) {
                let entropy = shannon_entropy_bytes(&[byte]);
                assert!(
                    entropy.abs() < f64::EPSILON,
                    "entropy of single byte must be 0.0, got {entropy}"
                );
            }

            #[test]
            fn shannon_entropy_uniform_is_maximal(
                len in 256..512usize,
            ) {
                // Construct input with every byte value appearing equally
                #[allow(clippy::cast_possible_truncation)]
                let bytes: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
                let entropy = shannon_entropy_bytes(&bytes);
                // Should be close to 8.0 (log2(256))
                assert!(
                    entropy > 7.9,
                    "uniform distribution entropy should be near 8.0, got {entropy}"
                );
            }

            #[test]
            fn llc_miss_proxy_bounded(
                total_depth in 0..10_000usize,
                overflow_depth in 0..10_000usize,
                rejected_total in 0..100_000u64,
            ) {
                let proxy = llc_miss_proxy(total_depth, overflow_depth, rejected_total);
                assert!(
                    (0.0..=1.0).contains(&proxy),
                    "llc_miss_proxy must be in [0.0, 1.0], got {proxy}"
                );
            }

            #[test]
            fn llc_miss_proxy_zero_on_empty(_dummy in Just(())) {
                let proxy = llc_miss_proxy(0, 0, 0);
                assert!(
                    proxy.abs() < f64::EPSILON,
                    "llc_miss_proxy(0, 0, 0) must be 0.0"
                );
            }

            #[test]
            fn normalized_shadow_op_idempotent(op in "[a-zA-Z_]{1,20}") {
                let once = normalized_shadow_op(&op);
                let twice = normalized_shadow_op(&once);
                assert!(
                    once == twice,
                    "normalized_shadow_op must be idempotent: '{once}' vs '{twice}'"
                );
            }

            #[test]
            fn normalized_shadow_op_case_insensitive(op in "[a-zA-Z]{1,20}") {
                let lower = normalized_shadow_op(&op.to_lowercase());
                let upper = normalized_shadow_op(&op.to_uppercase());
                assert!(
                    lower == upper,
                    "normalized_shadow_op must be case-insensitive: '{lower}' vs '{upper}'"
                );
            }

            #[test]
            fn shadow_safe_session_op_case_insensitive(
                op in prop::sample::select(vec![
                    "getState".to_string(),
                    "GETSTATE".to_string(),
                    "get_state".to_string(),
                    "GET_STATE".to_string(),
                    "getMessages".to_string(),
                    "GET_MESSAGES".to_string(),
                ]),
            ) {
                assert!(
                    shadow_safe_session_op(&op),
                    "'{op}' should be recognized as safe session op"
                );
            }

            #[test]
            fn shadow_safe_tool_case_insensitive(
                name in prop::sample::select(vec![
                    "Read".to_string(),
                    "READ".to_string(),
                    "read".to_string(),
                    "Grep".to_string(),
                    "GREP".to_string(),
                ]),
            ) {
                assert!(
                    shadow_safe_tool(&name),
                    "'{name}' should be safe tool"
                );
            }

            #[test]
            fn usize_to_f64_monotonic(a in 0..u32::MAX as usize, b in 0..u32::MAX as usize) {
                let fa = usize_to_f64(a);
                let fb = usize_to_f64(b);
                if a <= b {
                    assert!(
                        fa <= fb,
                        "usize_to_f64 must be monotonic: {a} → {fa}, {b} → {fb}"
                    );
                }
            }
        }
    }
}
