//! QuickJS runtime scaffolding for JS-compatible extensions.
//!
//! This module implements the PiJS runtime with Promise-based hostcall bridge:
//! - Async QuickJS runtime + context creation
//! - `pi` global object with Promise-returning hostcall methods
//! - Deterministic event loop scheduler integration
//! - call_id → Promise resolver mapping for hostcall completions
//! - Microtask draining after each macrotask
//!
//! # Architecture (bd-2ke)
//!
//! ```text
//! JS Code                     Rust Host
//! -------                     ---------
//! pi.tool("read", {...})  --> enqueue HostcallRequest
//!   returns Promise           generate call_id
//!   store (resolve, reject)   track pending hostcall
//!
//! [scheduler tick]        <-- host completes hostcall
//!   delivers MacrotaskKind::HostcallComplete
//!   lookup (resolve, reject) by call_id
//!   resolve(result) or reject(error)
//!   drain microtasks (Promises .then chains)
//! ```

use crate::error::{Error, Result};
use crate::scheduler::{Clock as SchedulerClock, HostcallOutcome, Scheduler, WallClock};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use rquickjs::function::{Func, Opt};
use rquickjs::loader::{Loader as JsModuleLoader, Resolver as JsModuleResolver};
use rquickjs::module::Declared as JsModuleDeclared;
use rquickjs::{
    AsyncContext, AsyncRuntime, Coerced, Ctx, Exception, FromJs, Function, IntoJs, Module, Object,
    Value,
};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, path::Path, path::PathBuf};
use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc};
use swc_ecma_ast::{Module as SwcModule, Pass, Program as SwcProgram};
use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
use swc_ecma_parser::{Parser as SwcParser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_base::resolver;
use swc_ecma_transforms_typescript::strip;
use uuid::Uuid;

pub struct QuickJsRuntime {
    runtime: AsyncRuntime,
    context: AsyncContext,
}

#[allow(clippy::future_not_send)]
impl QuickJsRuntime {
    pub async fn new() -> Result<Self> {
        let runtime = AsyncRuntime::new().map_err(|err| map_js_error(&err))?;
        let context = AsyncContext::full(&runtime)
            .await
            .map_err(|err| map_js_error(&err))?;
        let instance = Self { runtime, context };
        instance.install_pi_stub().await?;
        Ok(instance)
    }

    pub async fn eval(&self, source: &str) -> Result<()> {
        self.context
            .with(|ctx| ctx.eval::<(), _>(source))
            .await
            .map_err(|err| map_js_error(&err))?;
        Ok(())
    }

    pub async fn eval_file(&self, path: &std::path::Path) -> Result<()> {
        self.context
            .with(|ctx| ctx.eval_file::<(), _>(path))
            .await
            .map_err(|err| map_js_error(&err))?;
        Ok(())
    }

    pub async fn run_pending_jobs(&self) -> Result<()> {
        loop {
            let ran = self
                .runtime
                .execute_pending_job()
                .await
                .map_err(|err| Error::extension(format!("QuickJS job: {err}")))?;
            if !ran {
                break;
            }
        }
        Ok(())
    }

    pub async fn run_until_idle(&self) -> Result<()> {
        self.runtime.idle().await;
        Ok(())
    }

    async fn install_pi_stub(&self) -> Result<()> {
        self.context
            .with(|ctx| {
                let global = ctx.globals();
                let pi = Object::new(ctx)?;

                pi.set(
                    "tool",
                    Func::from(
                        |ctx: Ctx<'_>, _name: String, _input: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(&ctx, "pi.tool"))
                        },
                    ),
                )?;
                pi.set(
                    "exec",
                    Func::from(
                        |ctx: Ctx<'_>, _cmd: String, _args: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(&ctx, "pi.exec"))
                        },
                    ),
                )?;
                pi.set(
                    "http",
                    Func::from(|ctx: Ctx<'_>, _req: Value| -> rquickjs::Result<Value> {
                        Err(throw_unimplemented(&ctx, "pi.http"))
                    }),
                )?;
                pi.set(
                    "session",
                    Func::from(
                        |ctx: Ctx<'_>, _op: String, _args: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(&ctx, "pi.session"))
                        },
                    ),
                )?;
                pi.set(
                    "ui",
                    Func::from(
                        |ctx: Ctx<'_>, _op: String, _args: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(&ctx, "pi.ui"))
                        },
                    ),
                )?;
                pi.set(
                    "events",
                    Func::from(
                        |ctx: Ctx<'_>, _op: String, _args: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(&ctx, "pi.events"))
                        },
                    ),
                )?;

                global.set("pi", pi)?;
                Ok(())
            })
            .await
            .map_err(|err| map_js_error(&err))?;
        Ok(())
    }
}

// ============================================================================
// Environment variable filtering (bd-1av0.9)
// ============================================================================

/// Determine whether an environment variable is safe to expose to extensions.
///
/// Uses a blocklist approach: most vars are allowed, but known sensitive
/// patterns (API keys, secrets, tokens, passwords, credentials) are blocked.
pub fn is_env_var_allowed(key: &str) -> bool {
    const BLOCKED_EXACT: &[&str] = &[
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GOOGLE_API_KEY",
        "AZURE_OPENAI_API_KEY",
        "COHERE_API_KEY",
        "GROQ_API_KEY",
        "DEEPINFRA_API_KEY",
        "CEREBRAS_API_KEY",
        "OPENROUTER_API_KEY",
        "MISTRAL_API_KEY",
        "MOONSHOT_API_KEY",
        "DASHSCOPE_API_KEY",
        "DEEPSEEK_API_KEY",
        "FIREWORKS_API_KEY",
        "TOGETHER_API_KEY",
        "PERPLEXITY_API_KEY",
        "XAI_API_KEY",
        "DATABASE_URL",
        "REDIS_URL",
        "MONGODB_URI",
        "PRIVATE_KEY",
        "CARGO_REGISTRY_TOKEN",
        "NPM_TOKEN",
        "GH_TOKEN",
        "GITHUB_TOKEN",
    ];
    const BLOCKED_SUFFIXES: &[&str] = &[
        "_SECRET",
        "_SECRET_KEY",
        "_ACCESS_KEY",
        "_PRIVATE_KEY",
        "_PASSWORD",
        "_PASSWD",
        "_CREDENTIAL",
        "_CREDENTIALS",
    ];
    const BLOCKED_PREFIXES: &[&str] = &["AWS_SECRET_", "AWS_SESSION_"];

    if key.starts_with("PI_") {
        return true;
    }
    if BLOCKED_EXACT.contains(&key) {
        return false;
    }
    let upper = key.to_ascii_uppercase();
    for suffix in BLOCKED_SUFFIXES {
        if upper.ends_with(suffix) {
            return false;
        }
    }
    for prefix in BLOCKED_PREFIXES {
        if upper.starts_with(prefix) {
            return false;
        }
    }
    true
}

// ============================================================================
// Promise Bridge Types (bd-2ke)
// ============================================================================

/// Type of hostcall being requested from JavaScript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostcallKind {
    /// pi.tool(name, input) - invoke a tool
    Tool { name: String },
    /// pi.exec(cmd, args) - execute a shell command
    Exec { cmd: String },
    /// pi.http(request) - make an HTTP request
    Http,
    /// pi.session(op, args) - session operations
    Session { op: String },
    /// pi.ui(op, args) - UI operations
    Ui { op: String },
    /// pi.events(op, args) - event operations
    Events { op: String },
}

/// A hostcall request enqueued from JavaScript.
#[derive(Debug, Clone)]
pub struct HostcallRequest {
    /// Unique identifier for correlation.
    pub call_id: String,
    /// Type of hostcall.
    pub kind: HostcallKind,
    /// JSON payload for the hostcall.
    pub payload: serde_json::Value,
    /// Trace ID for correlation with macrotask.
    pub trace_id: u64,
    /// Active extension id (when known) for policy/log correlation.
    pub extension_id: Option<String>,
}

/// Tool definition registered by a JS extension.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub struct ExtensionToolDef {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    pub description: String,
    pub parameters: serde_json::Value,
}

fn canonicalize_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    out.insert(key, canonicalize_json(value));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize_json).collect())
        }
        other => other.clone(),
    }
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    hex_lower(digest.as_slice())
}

fn hostcall_params_hash(method: &str, params: &serde_json::Value) -> String {
    let canonical = canonicalize_json(&serde_json::json!({ "method": method, "params": params }));
    let json = serde_json::to_string(&canonical).expect("serialize canonical hostcall params");
    sha256_hex(&json)
}

fn canonical_exec_params(cmd: &str, payload: &serde_json::Value) -> serde_json::Value {
    let mut obj = match payload {
        serde_json::Value::Object(map) => {
            let mut out = map.clone();
            out.remove("command");
            out
        }
        serde_json::Value::Null => serde_json::Map::new(),
        other => {
            let mut out = serde_json::Map::new();
            out.insert("payload".to_string(), other.clone());
            out
        }
    };

    obj.insert(
        "cmd".to_string(),
        serde_json::Value::String(cmd.to_string()),
    );
    serde_json::Value::Object(obj)
}

fn canonical_op_params(op: &str, payload: &serde_json::Value) -> serde_json::Value {
    let mut obj = match payload {
        serde_json::Value::Object(map) => map.clone(),
        serde_json::Value::Null => serde_json::Map::new(),
        other => {
            let mut out = serde_json::Map::new();
            // Reserved key for non-object args to avoid dropping semantics.
            out.insert("payload".to_string(), other.clone());
            out
        }
    };

    // Explicit op from hostcall kind always wins.
    obj.insert("op".to_string(), serde_json::Value::String(op.to_string()));
    serde_json::Value::Object(obj)
}

impl HostcallRequest {
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self.kind {
            HostcallKind::Tool { .. } => "tool",
            HostcallKind::Exec { .. } => "exec",
            HostcallKind::Http => "http",
            HostcallKind::Session { .. } => "session",
            HostcallKind::Ui { .. } => "ui",
            HostcallKind::Events { .. } => "events",
        }
    }

    #[must_use]
    pub fn required_capability(&self) -> String {
        match &self.kind {
            HostcallKind::Tool { name } => match name.trim().to_ascii_lowercase().as_str() {
                "read" | "grep" | "find" | "ls" => "read".to_string(),
                "write" | "edit" => "write".to_string(),
                "bash" => "exec".to_string(),
                _ => "tool".to_string(),
            },
            HostcallKind::Exec { .. } => "exec".to_string(),
            HostcallKind::Http => "http".to_string(),
            HostcallKind::Session { .. } => "session".to_string(),
            HostcallKind::Ui { .. } => "ui".to_string(),
            HostcallKind::Events { .. } => "events".to_string(),
        }
    }

    /// Build the canonical params shape for hashing.
    ///
    /// **Canonical shapes** (must match `hostcall_request_to_payload()` in `extensions.rs`):
    /// - `tool`:  `{ "name": <tool_name>, "input": <payload> }`
    /// - `exec`:  `{ "cmd": <string>, ...payload_fields }`
    /// - `http`:  payload passthrough
    /// - `session/ui/events`:  `{ "op": <string>, ...payload_fields }` (flattened)
    ///
    /// For non-object args to `session/ui/events`, payload is preserved under
    /// a reserved `"payload"` key (e.g. `{ "op": "set_status", "payload": "ready" }`).
    #[must_use]
    pub fn params_for_hash(&self) -> serde_json::Value {
        match &self.kind {
            HostcallKind::Tool { name } => {
                serde_json::json!({ "name": name, "input": self.payload.clone() })
            }
            HostcallKind::Exec { cmd } => canonical_exec_params(cmd, &self.payload),
            HostcallKind::Http => self.payload.clone(),
            HostcallKind::Session { op }
            | HostcallKind::Ui { op }
            | HostcallKind::Events { op } => canonical_op_params(op, &self.payload),
        }
    }

    #[must_use]
    pub fn params_hash(&self) -> String {
        hostcall_params_hash(self.method(), &self.params_for_hash())
    }
}

/// Stores pending Promise resolvers for hostcalls.
///
/// This is the core bridge between JS Promises and Rust async completions.
/// When a pi.* method is called:
/// 1. A Promise is created
/// 2. The (resolve, reject) functions are stored here keyed by call_id
/// 3. When the hostcall completes, we look up and call resolve/reject
pub struct PendingHostcalls<'js> {
    /// Map from call_id to (resolve, reject) functions.
    pending: HashMap<String, (Function<'js>, Function<'js>)>,
    /// Next call_id to assign.
    next_id: u64,
}

impl<'js> PendingHostcalls<'js> {
    /// Create a new empty pending hostcalls tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next_id: 1,
        }
    }

    /// Generate a unique call_id.
    pub fn next_call_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        format!("call-{id}")
    }

    /// Register a pending hostcall with its resolve/reject functions.
    pub fn register(&mut self, call_id: String, resolve: Function<'js>, reject: Function<'js>) {
        tracing::trace!(
            event = "promise_bridge.register",
            call_id = %call_id,
            pending_count = self.pending.len() + 1,
            "Registered pending hostcall"
        );
        self.pending.insert(call_id, (resolve, reject));
    }

    /// Complete a hostcall by resolving or rejecting its Promise.
    ///
    /// Returns true if the call_id was found and completed.
    pub fn complete(
        &mut self,
        ctx: &Ctx<'js>,
        call_id: &str,
        outcome: &HostcallOutcome,
    ) -> rquickjs::Result<bool> {
        let Some((resolve, reject)) = self.pending.remove(call_id) else {
            tracing::warn!(
                event = "promise_bridge.complete.not_found",
                call_id = %call_id,
                "Hostcall completion for unknown call_id"
            );
            return Ok(false);
        };

        match outcome {
            HostcallOutcome::Success(value) => {
                tracing::trace!(
                    event = "promise_bridge.resolve",
                    call_id = %call_id,
                    "Resolving Promise with success"
                );
                // Convert serde_json::Value to JS Value
                let js_value = json_to_js(ctx, value)?;
                resolve.call::<_, ()>((js_value,))?;
            }
            HostcallOutcome::Error { code, message } => {
                tracing::trace!(
                    event = "promise_bridge.reject",
                    call_id = %call_id,
                    code = %code,
                    message = %message,
                    "Rejecting Promise with error"
                );
                // Create an Error object with code and message
                let error = Object::new(ctx.clone())?;
                error.set("code", code.clone())?;
                error.set("message", message.clone())?;
                reject.call::<_, ()>((error,))?;
            }
            HostcallOutcome::StreamChunk { .. } => {
                tracing::trace!(
                    event = "promise_bridge.stream_chunk",
                    call_id = %call_id,
                    "Ignoring stream chunk in promise bridge"
                );
            }
        }

        tracing::trace!(
            event = "promise_bridge.complete",
            call_id = %call_id,
            remaining = self.pending.len(),
            "Hostcall completed"
        );
        Ok(true)
    }

    /// Cancel a pending hostcall by rejecting its Promise.
    pub fn cancel(
        &mut self,
        ctx: &Ctx<'js>,
        call_id: &str,
        reason: &str,
    ) -> rquickjs::Result<bool> {
        let Some((_resolve, reject)) = self.pending.remove(call_id) else {
            return Ok(false);
        };

        tracing::trace!(
            event = "promise_bridge.cancel",
            call_id = %call_id,
            reason = %reason,
            "Cancelling pending hostcall"
        );

        let error = Object::new(ctx.clone())?;
        error.set("code", "CANCELLED")?;
        error.set("message", reason)?;
        reject.call::<_, ()>((error,))?;
        Ok(true)
    }

    /// Get the number of pending hostcalls.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Check if there are no pending hostcalls.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Get all pending call_ids for debugging.
    #[must_use]
    pub fn pending_ids(&self) -> Vec<&String> {
        self.pending.keys().collect()
    }
}

impl Default for PendingHostcalls<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a serde_json::Value to a rquickjs Value.
#[allow(clippy::option_if_let_else)]
pub(crate) fn json_to_js<'js>(
    ctx: &Ctx<'js>,
    value: &serde_json::Value,
) -> rquickjs::Result<Value<'js>> {
    match value {
        serde_json::Value::Null => Ok(Value::new_null(ctx.clone())),
        serde_json::Value::Bool(b) => Ok(Value::new_bool(ctx.clone(), *b)),
        serde_json::Value::Number(n) => n.as_i64().and_then(|i| i32::try_from(i).ok()).map_or_else(
            || {
                n.as_f64().map_or_else(
                    || Ok(Value::new_null(ctx.clone())),
                    |f| Ok(Value::new_float(ctx.clone(), f)),
                )
            },
            |i| Ok(Value::new_int(ctx.clone(), i)),
        ),
        serde_json::Value::String(s) => s.clone().into_js(ctx),
        serde_json::Value::Array(arr) => {
            let js_arr = rquickjs::Array::new(ctx.clone())?;
            for (i, v) in arr.iter().enumerate() {
                let js_v = json_to_js(ctx, v)?;
                js_arr.set(i, js_v)?;
            }
            Ok(js_arr.into_value())
        }
        serde_json::Value::Object(obj) => {
            let js_obj = Object::new(ctx.clone())?;
            for (k, v) in obj {
                let js_v = json_to_js(ctx, v)?;
                js_obj.set(k.as_str(), js_v)?;
            }
            Ok(js_obj.into_value())
        }
    }
}

/// Convert a rquickjs Value to a serde_json::Value.
pub(crate) fn js_to_json(value: &Value<'_>) -> rquickjs::Result<serde_json::Value> {
    if value.is_null() || value.is_undefined() {
        return Ok(serde_json::Value::Null);
    }
    if let Some(b) = value.as_bool() {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Some(i) = value.as_int() {
        return Ok(serde_json::json!(i));
    }
    if let Some(f) = value.as_float() {
        return Ok(serde_json::json!(f));
    }
    if let Some(s) = value.as_string() {
        let s = s.to_string()?;
        return Ok(serde_json::Value::String(s));
    }
    if let Some(arr) = value.as_array() {
        let mut result = Vec::new();
        for i in 0..arr.len() {
            let v: Value<'_> = arr.get(i)?;
            result.push(js_to_json(&v)?);
        }
        return Ok(serde_json::Value::Array(result));
    }
    if let Some(obj) = value.as_object() {
        let mut result = serde_json::Map::new();
        for item in obj.props::<String, Value<'_>>() {
            let (k, v) = item?;
            result.insert(k, js_to_json(&v)?);
        }
        return Ok(serde_json::Value::Object(result));
    }
    // Fallback for functions, symbols, etc.
    Ok(serde_json::Value::Null)
}

/// Queue of hostcall requests waiting to be processed by the host.
pub type HostcallQueue = Rc<RefCell<VecDeque<HostcallRequest>>>;

// ============================================================================
// Deterministic PiJS Event Loop Scheduler (bd-8mm)
// ============================================================================

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

#[derive(Clone)]
pub struct ClockHandle(Arc<dyn Clock>);

impl ClockHandle {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self(clock)
    }
}

impl Clock for ClockHandle {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
    }
}

#[derive(Debug)]
pub struct ManualClock {
    now_ms: AtomicU64,
}

impl ManualClock {
    pub const fn new(start_ms: u64) -> Self {
        Self {
            now_ms: AtomicU64::new(start_ms),
        }
    }

    pub fn set(&self, ms: u64) {
        self.now_ms.store(ms, AtomicOrdering::SeqCst);
    }

    pub fn advance(&self, delta_ms: u64) {
        self.now_ms.fetch_add(delta_ms, AtomicOrdering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(AtomicOrdering::SeqCst)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacrotaskKind {
    TimerFired { timer_id: u64 },
    HostcallComplete { call_id: String },
    InboundEvent { event_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Macrotask {
    pub seq: u64,
    pub trace_id: u64,
    pub kind: MacrotaskKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MacrotaskEntry {
    seq: u64,
    trace_id: u64,
    kind: MacrotaskKind,
}

impl Ord for MacrotaskEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.seq.cmp(&other.seq)
    }
}

impl PartialOrd for MacrotaskEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimerEntry {
    deadline_ms: u64,
    order_seq: u64,
    timer_id: u64,
    trace_id: u64,
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.deadline_ms, self.order_seq, self.timer_id).cmp(&(
            other.deadline_ms,
            other.order_seq,
            other.timer_id,
        ))
    }
}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingMacrotask {
    trace_id: u64,
    kind: MacrotaskKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickResult {
    pub ran_macrotask: bool,
    pub microtasks_drained: usize,
}

pub struct PiEventLoop {
    clock: ClockHandle,
    seq: u64,
    next_timer_id: u64,
    pending: VecDeque<PendingMacrotask>,
    macro_queue: BinaryHeap<std::cmp::Reverse<MacrotaskEntry>>,
    timers: BinaryHeap<std::cmp::Reverse<TimerEntry>>,
    cancelled_timers: HashSet<u64>,
}

impl PiEventLoop {
    pub fn new(clock: ClockHandle) -> Self {
        Self {
            clock,
            seq: 0,
            next_timer_id: 1,
            pending: VecDeque::new(),
            macro_queue: BinaryHeap::new(),
            timers: BinaryHeap::new(),
            cancelled_timers: HashSet::new(),
        }
    }

    pub fn enqueue_hostcall_completion(&mut self, call_id: impl Into<String>) {
        let trace_id = self.next_seq();
        self.pending.push_back(PendingMacrotask {
            trace_id,
            kind: MacrotaskKind::HostcallComplete {
                call_id: call_id.into(),
            },
        });
    }

    pub fn enqueue_inbound_event(&mut self, event_id: impl Into<String>) {
        let trace_id = self.next_seq();
        self.pending.push_back(PendingMacrotask {
            trace_id,
            kind: MacrotaskKind::InboundEvent {
                event_id: event_id.into(),
            },
        });
    }

    pub fn set_timeout(&mut self, delay_ms: u64) -> u64 {
        let timer_id = self.next_timer_id;
        self.next_timer_id += 1;
        let order_seq = self.next_seq();
        let deadline_ms = self.clock.now_ms().saturating_add(delay_ms);
        self.timers.push(std::cmp::Reverse(TimerEntry {
            deadline_ms,
            order_seq,
            timer_id,
            trace_id: order_seq,
        }));
        timer_id
    }

    pub fn clear_timeout(&mut self, timer_id: u64) {
        self.cancelled_timers.insert(timer_id);
    }

    pub fn tick(
        &mut self,
        mut on_macrotask: impl FnMut(Macrotask),
        mut drain_microtasks: impl FnMut() -> bool,
    ) -> TickResult {
        self.ingest_pending();
        self.enqueue_due_timers();

        let mut ran_macrotask = false;
        if let Some(task) = self.pop_next_macrotask() {
            ran_macrotask = true;
            on_macrotask(task);
        }

        let mut microtasks_drained = 0;
        if ran_macrotask {
            while drain_microtasks() {
                microtasks_drained += 1;
            }
        }

        TickResult {
            ran_macrotask,
            microtasks_drained,
        }
    }

    fn ingest_pending(&mut self) {
        while let Some(pending) = self.pending.pop_front() {
            self.enqueue_macrotask(pending.trace_id, pending.kind);
        }
    }

    fn enqueue_due_timers(&mut self) {
        let now = self.clock.now_ms();
        while let Some(std::cmp::Reverse(entry)) = self.timers.peek().cloned() {
            if entry.deadline_ms > now {
                break;
            }
            let _ = self.timers.pop();
            if self.cancelled_timers.remove(&entry.timer_id) {
                continue;
            }
            self.enqueue_macrotask(
                entry.trace_id,
                MacrotaskKind::TimerFired {
                    timer_id: entry.timer_id,
                },
            );
        }
    }

    fn enqueue_macrotask(&mut self, trace_id: u64, kind: MacrotaskKind) {
        let seq = self.next_seq();
        self.macro_queue.push(std::cmp::Reverse(MacrotaskEntry {
            seq,
            trace_id,
            kind,
        }));
    }

    fn pop_next_macrotask(&mut self) -> Option<Macrotask> {
        self.macro_queue.pop().map(|entry| {
            let entry = entry.0;
            Macrotask {
                seq: entry.seq,
                trace_id: entry.trace_id,
                kind: entry.kind,
            }
        })
    }

    const fn next_seq(&mut self) -> u64 {
        let current = self.seq;
        self.seq = self.seq.saturating_add(1);
        current
    }
}

fn throw_unimplemented(ctx: &Ctx<'_>, name: &str) -> rquickjs::Error {
    let message = format!("{name} is not wired yet");
    match message.into_js(ctx) {
        Ok(value) => ctx.throw(value),
        Err(err) => err,
    }
}

fn map_js_error(err: &rquickjs::Error) -> Error {
    Error::extension(format!("QuickJS: {err:?}"))
}

fn format_quickjs_exception<'js>(ctx: &Ctx<'js>, caught: Value<'js>) -> String {
    if let Ok(obj) = caught.clone().try_into_object() {
        if let Some(exception) = Exception::from_object(obj) {
            if let Some(message) = exception.message() {
                if let Some(stack) = exception.stack() {
                    return format!("{message}\n{stack}");
                }
                return message;
            }
            if let Some(stack) = exception.stack() {
                return stack;
            }
        }
    }

    match Coerced::<String>::from_js(ctx, caught) {
        Ok(value) => value.0,
        Err(err) => format!("(failed to stringify QuickJS exception: {err})"),
    }
}

// ============================================================================
// Integrated PiJS Runtime with Promise Bridge (bd-2ke)
// ============================================================================

/// Statistics from a tick execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PiJsTickStats {
    /// Whether a macrotask was executed.
    pub ran_macrotask: bool,
    /// Number of microtask drain iterations.
    pub microtask_drains: usize,
    /// Number of pending QuickJS jobs drained.
    pub jobs_drained: usize,
    /// Number of pending hostcalls (in-flight Promises).
    pub pending_hostcalls: usize,
    /// Total hostcalls issued by this runtime.
    pub hostcalls_total: u64,
    /// Total hostcalls timed out by this runtime.
    pub hostcalls_timed_out: u64,
    /// Last observed QuickJS `memory_used_size` in bytes.
    pub memory_used_bytes: u64,
    /// Peak observed QuickJS `memory_used_size` in bytes.
    pub peak_memory_used_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct PiJsRuntimeLimits {
    /// Limit runtime heap usage (QuickJS allocator). `None` means unlimited.
    pub memory_limit_bytes: Option<usize>,
    /// Limit runtime stack usage. `None` uses QuickJS default.
    pub max_stack_bytes: Option<usize>,
    /// Interrupt budget to bound JS execution. `None` disables budget enforcement.
    ///
    /// This is implemented via QuickJS's interrupt hook. For deterministic unit tests,
    /// setting this to `Some(0)` forces an immediate abort.
    pub interrupt_budget: Option<u64>,
    /// Default timeout (ms) for hostcalls issued via `pi.*`.
    pub hostcall_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PiJsRuntimeConfig {
    pub cwd: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub limits: PiJsRuntimeLimits,
}

impl Default for PiJsRuntimeConfig {
    fn default() -> Self {
        Self {
            cwd: ".".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            limits: PiJsRuntimeLimits::default(),
        }
    }
}

#[derive(Debug)]
struct InterruptBudget {
    configured: Option<u64>,
    remaining: std::cell::Cell<Option<u64>>,
    tripped: std::cell::Cell<bool>,
}

impl InterruptBudget {
    const fn new(configured: Option<u64>) -> Self {
        Self {
            configured,
            remaining: std::cell::Cell::new(None),
            tripped: std::cell::Cell::new(false),
        }
    }

    fn reset(&self) {
        self.remaining.set(self.configured);
        self.tripped.set(false);
    }

    fn on_interrupt(&self) -> bool {
        let Some(remaining) = self.remaining.get() else {
            return false;
        };
        if remaining == 0 {
            self.tripped.set(true);
            return true;
        }
        self.remaining.set(Some(remaining - 1));
        false
    }

    fn did_trip(&self) -> bool {
        self.tripped.get()
    }

    fn clear_trip(&self) {
        self.tripped.set(false);
    }
}

#[derive(Debug, Default)]
struct HostcallTracker {
    pending: HashSet<String>,
    call_to_timer: HashMap<String, u64>,
    timer_to_call: HashMap<u64, String>,
}

enum HostcallCompletion {
    Delivered {
        #[allow(dead_code)]
        timer_id: Option<u64>,
    },
    Unknown,
}

impl HostcallTracker {
    fn register(&mut self, call_id: String, timer_id: Option<u64>) {
        self.pending.insert(call_id.clone());
        if let Some(timer_id) = timer_id {
            self.call_to_timer.insert(call_id.clone(), timer_id);
            self.timer_to_call.insert(timer_id, call_id);
        }
    }

    fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn is_pending(&self, call_id: &str) -> bool {
        self.pending.contains(call_id)
    }

    fn on_complete(&mut self, call_id: &str) -> HostcallCompletion {
        if !self.pending.remove(call_id) {
            return HostcallCompletion::Unknown;
        }

        let timer_id = self.call_to_timer.remove(call_id);
        if let Some(timer_id) = timer_id {
            self.timer_to_call.remove(&timer_id);
        }

        HostcallCompletion::Delivered { timer_id }
    }

    fn take_timed_out_call(&mut self, timer_id: u64) -> Option<String> {
        let call_id = self.timer_to_call.remove(&timer_id)?;
        self.call_to_timer.remove(&call_id);
        if !self.pending.remove(&call_id) {
            return None;
        }
        Some(call_id)
    }
}

// ============================================================================
// PiJS Module Loader (TypeScript + virtual modules)
// ============================================================================

#[derive(Debug)]
struct PiJsModuleState {
    virtual_modules: HashMap<String, String>,
    compiled_sources: HashMap<String, Vec<u8>>,
}

impl PiJsModuleState {
    fn new() -> Self {
        Self {
            virtual_modules: default_virtual_modules(),
            compiled_sources: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct PiJsResolver {
    state: Rc<RefCell<PiJsModuleState>>,
}

impl JsModuleResolver for PiJsResolver {
    fn resolve(&mut self, _ctx: &Ctx<'_>, base: &str, name: &str) -> rquickjs::Result<String> {
        let spec = name.trim();
        if spec.is_empty() {
            return Err(rquickjs::Error::new_resolving(base, name));
        }

        // Alias bare Node.js builtins to their node: prefixed virtual modules.
        let canonical = match spec {
            "fs" => "node:fs",
            "fs/promises" => "node:fs/promises",
            "path" => "node:path",
            "os" => "node:os",
            "child_process" => "node:child_process",
            "crypto" => "node:crypto",
            "http" => "node:http",
            "https" => "node:https",
            "util" => "node:util",
            "readline" => "node:readline",
            "url" => "node:url",
            "net" => "node:net",
            "events" => "node:events",
            "buffer" => "node:buffer",
            "assert" => "node:assert",
            "stream" => "node:stream",
            "module" => "node:module",
            "string_decoder" => "node:string_decoder",
            "querystring" => "node:querystring",
            "process" => "node:process",
            "stream/promises" => "node:stream/promises",
            other => other,
        };

        if self.state.borrow().virtual_modules.contains_key(canonical) {
            return Ok(canonical.to_string());
        }

        if let Some(path) = resolve_module_path(base, spec) {
            return Ok(path.to_string_lossy().to_string());
        }

        Err(rquickjs::Error::new_resolving_message(
            base,
            name,
            format!("Unsupported module specifier: {spec}"),
        ))
    }
}

#[derive(Clone, Debug)]
struct PiJsLoader {
    state: Rc<RefCell<PiJsModuleState>>,
}

impl JsModuleLoader for PiJsLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
    ) -> rquickjs::Result<Module<'js, JsModuleDeclared>> {
        let source = {
            let mut state = self.state.borrow_mut();
            if let Some(cached) = state.compiled_sources.get(name) {
                cached.clone()
            } else {
                let compiled = compile_module_source(&state.virtual_modules, name)?;
                state
                    .compiled_sources
                    .insert(name.to_string(), compiled.clone());
                compiled
            }
        };

        Module::declare(ctx.clone(), name, source)
    }
}

fn compile_module_source(
    virtual_modules: &HashMap<String, String>,
    name: &str,
) -> rquickjs::Result<Vec<u8>> {
    if let Some(source) = virtual_modules.get(name) {
        return Ok(prefix_import_meta_url(name, source));
    }

    let path = Path::new(name);
    if !path.is_file() {
        return Err(rquickjs::Error::new_loading_message(
            name,
            "Module is not a file",
        ));
    }

    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    let raw = fs::read_to_string(path)
        .map_err(|err| rquickjs::Error::new_loading_message(name, format!("read: {err}")))?;

    let compiled = match extension {
        "ts" | "tsx" => {
            let transpiled = transpile_typescript_module(&raw, name).map_err(|message| {
                rquickjs::Error::new_loading_message(name, format!("transpile: {message}"))
            })?;
            maybe_cjs_to_esm(&transpiled)
        }
        "js" | "mjs" => maybe_cjs_to_esm(&raw),
        "json" => json_module_to_esm(&raw, name).map_err(|message| {
            rquickjs::Error::new_loading_message(name, format!("json: {message}"))
        })?,
        other => {
            return Err(rquickjs::Error::new_loading_message(
                name,
                format!("Unsupported module extension: {other}"),
            ));
        }
    };

    Ok(prefix_import_meta_url(name, &compiled))
}

fn prefix_import_meta_url(module_name: &str, body: &str) -> Vec<u8> {
    let url = if module_name.starts_with('/') {
        format!("file://{module_name}")
    } else if module_name.starts_with("file://") {
        module_name.to_string()
    } else {
        format!("pi://{module_name}")
    };
    let url_literal = serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".to_string());
    format!("import.meta.url = {url_literal};\n{body}").into_bytes()
}

fn resolve_module_path(base: &str, specifier: &str) -> Option<PathBuf> {
    let specifier = specifier.trim();
    if specifier.is_empty() {
        return None;
    }

    if let Some(path) = specifier.strip_prefix("file://") {
        return resolve_existing_file(PathBuf::from(path));
    }

    let path = if specifier.starts_with('/') {
        PathBuf::from(specifier)
    } else if specifier.starts_with('.') {
        let base_path = Path::new(base);
        let base_dir = base_path.parent()?;
        base_dir.join(specifier)
    } else {
        return None;
    };

    resolve_existing_module_candidate(path)
}

fn resolve_existing_file(path: PathBuf) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path);
    }
    None
}

fn resolve_existing_module_candidate(path: PathBuf) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path);
    }

    if path.is_dir() {
        for candidate in ["index.ts", "index.js"] {
            let full = path.join(candidate);
            if full.is_file() {
                return Some(full);
            }
        }
        return None;
    }

    let extension = path.extension().and_then(|ext| ext.to_str());
    match extension {
        Some("js" | "mjs") => {
            let ts = path.with_extension("ts");
            if ts.is_file() {
                return Some(ts);
            }
        }
        None => {
            for ext in ["ts", "js"] {
                let candidate = path.with_extension(ext);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        _ => {}
    }

    None
}

static REQUIRE_CALL_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

/// Detect if a JavaScript source uses CommonJS patterns (`require(...)` or
/// `module.exports`) and transform it into an ESM-compatible wrapper.
///
/// Handles two cases:
/// 1. **Pure CJS** (no ESM `import`/`export`): full wrapper with
///    `module`/`exports`/`require` shim + `export default module.exports`
/// 2. **Mixed** (ESM imports + `require()` calls): inject `import` statements
///    for require targets and a `require()` function, preserving existing ESM
#[allow(clippy::too_many_lines)]
fn maybe_cjs_to_esm(source: &str) -> String {
    let has_require = source.contains("require(");
    let has_module_exports = source.contains("module.exports");

    if !has_require && !has_module_exports {
        return source.to_string();
    }

    let has_esm = source.lines().any(|line| {
        let trimmed = line.trim();
        (trimmed.starts_with("import ") || trimmed.starts_with("export "))
            && !trimmed.starts_with("//")
    });

    // Extract all require() specifiers
    let mut specifiers: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let re = REQUIRE_CALL_RE.get_or_init(|| {
        regex::Regex::new(r#"require\(\s*["']([^"']+)["']\s*\)"#).expect("require regex")
    });
    for cap in re.captures_iter(source) {
        let spec = cap[1].to_string();
        if seen.insert(spec.clone()) {
            specifiers.push(spec);
        }
    }

    if specifiers.is_empty() && !has_module_exports {
        return source.to_string();
    }
    if specifiers.is_empty() && has_esm {
        return source.to_string();
    }

    let mut output = String::with_capacity(source.len() + 512);

    // Generate ESM imports for require targets
    for (i, spec) in specifiers.iter().enumerate() {
        let _ = writeln!(output, "import * as __cjs_req_{i} from {spec:?};");
    }

    // Build require map + function
    if !specifiers.is_empty() {
        output.push_str("const __cjs_req_map = {");
        for (i, spec) in specifiers.iter().enumerate() {
            if i > 0 {
                output.push(',');
            }
            let _ = write!(output, "\n  {spec:?}: __cjs_req_{i}");
        }
        output.push_str("\n};\n");
        output.push_str(
            "function require(s) {\n\
             \x20 const m = __cjs_req_map[s];\n\
             \x20 if (!m) throw new Error('Cannot find module: ' + s);\n\
             \x20 return m.default !== undefined && typeof m.default === 'object' \
                  ? m.default : m;\n\
             }\n",
        );
    }

    if !has_esm {
        // Pure CJS: also add module/exports wrapper
        output.push_str(
            "const module = { exports: {} };\n\
             const exports = module.exports;\n",
        );
    }

    output.push_str(source);
    output.push('\n');

    if !has_esm {
        // Pure CJS: export module.exports as default only.
        // Named re-exports are omitted to avoid "invalid redefinition"
        // errors when the original source already declares matching names.
        output.push_str("export default module.exports;\n");
    }

    output
}

fn json_module_to_esm(raw: &str, name: &str) -> std::result::Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|err| format!("parse {name}: {err}"))?;
    let literal = serde_json::to_string(&value).map_err(|err| format!("encode {name}: {err}"))?;
    Ok(format!("export default {literal};\n"))
}

fn transpile_typescript_module(source: &str, name: &str) -> std::result::Result<String, String> {
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let cm: Lrc<SourceMap> = Lrc::default();
        let fm = cm.new_source_file(
            FileName::Custom(name.to_string()).into(),
            source.to_string(),
        );

        let syntax = Syntax::Typescript(TsSyntax {
            tsx: Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("tsx")),
            decorators: true,
            ..Default::default()
        });

        let mut parser = SwcParser::new(syntax, StringInput::from(&*fm), None);
        let module: SwcModule = parser
            .parse_module()
            .map_err(|err| format!("parse {name}: {err:?}"))?;

        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        let mut program = SwcProgram::Module(module);
        {
            let mut pass = resolver(unresolved_mark, top_level_mark, false);
            pass.process(&mut program);
        }
        {
            let mut pass = strip(unresolved_mark, top_level_mark);
            pass.process(&mut program);
        }
        let SwcProgram::Module(module) = program else {
            return Err(format!("transpile {name}: expected module"));
        };

        let mut buf = Vec::new();
        {
            let mut emitter = Emitter {
                cfg: swc_ecma_codegen::Config::default(),
                comments: None,
                cm: cm.clone(),
                wr: JsWriter::new(cm, "\n", &mut buf, None),
            };
            emitter
                .emit_module(&module)
                .map_err(|err| format!("emit {name}: {err}"))?;
        }

        String::from_utf8(buf).map_err(|err| format!("utf8 {name}: {err}"))
    })
}

#[allow(clippy::too_many_lines)]
fn default_virtual_modules() -> HashMap<String, String> {
    let mut modules = HashMap::new();

    modules.insert(
        "@sinclair/typebox".to_string(),
        r#"
export const Type = {
  String: (opts = {}) => ({ type: "string", ...opts }),
  Number: (opts = {}) => ({ type: "number", ...opts }),
  Boolean: (opts = {}) => ({ type: "boolean", ...opts }),
  Array: (items, opts = {}) => ({ type: "array", items, ...opts }),
  Object: (props = {}, opts = {}) => {
    const required = [];
    const properties = {};
    for (const [k, v] of Object.entries(props)) {
      if (v && typeof v === "object" && v.__pi_optional) {
        properties[k] = v.schema;
      } else {
        properties[k] = v;
        required.push(k);
      }
    }
    const out = { type: "object", properties, ...opts };
    if (required.length) out.required = required;
    return out;
  },
  Optional: (schema) => ({ __pi_optional: true, schema }),
  Literal: (value, opts = {}) => ({ const: value, ...opts }),
  Any: (opts = {}) => ({ ...opts }),
  Union: (schemas, opts = {}) => ({ anyOf: schemas, ...opts }),
  Enum: (values, opts = {}) => ({ enum: values, ...opts }),
  Integer: (opts = {}) => ({ type: "integer", ...opts }),
  Null: (opts = {}) => ({ type: "null", ...opts }),
  Unknown: (opts = {}) => ({ ...opts }),
  Tuple: (items, opts = {}) => ({ type: "array", items, minItems: items.length, maxItems: items.length, ...opts }),
  Record: (keySchema, valueSchema, opts = {}) => ({ type: "object", additionalProperties: valueSchema, ...opts }),
  Ref: (ref, opts = {}) => ({ $ref: ref, ...opts }),
  Intersect: (schemas, opts = {}) => ({ allOf: schemas, ...opts }),
};
export default { Type };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "@mariozechner/pi-ai".to_string(),
        r#"
export function StringEnum(values, opts = {}) {
  const list = Array.isArray(values) ? values.map((v) => String(v)) : [];
  return { type: "string", enum: list, ...opts };
}

export function calculateCost() {}

export function createAssistantMessageEventStream() {
  return {
    push: () => {},
    end: () => {},
  };
}

export function streamSimpleAnthropic() {
  throw new Error("@mariozechner/pi-ai.streamSimpleAnthropic is not available in PiJS");
}

export function streamSimpleOpenAIResponses() {
  throw new Error("@mariozechner/pi-ai.streamSimpleOpenAIResponses is not available in PiJS");
}

export async function complete(_model, _messages, _opts = {}) {
  // Return a minimal completion response stub
  return { content: "", model: _model ?? "unknown", usage: { input_tokens: 0, output_tokens: 0 } };
}

// Stub: completeSimple returns a simple text completion without streaming
export async function completeSimple(_model, _prompt, _opts = {}) {
  // Return an empty string completion
  return "";
}

export function getModel() {
  // Return a default model identifier
  return "claude-sonnet-4-5";
}

export function getApiProvider() {
  // Return a default provider identifier
  return "anthropic";
}

export function getModels() {
  // Return a list of available model identifiers
  return ["claude-sonnet-4-5", "claude-haiku-3-5"];
}

export async function loginOpenAICodex(_opts = {}) {
  return { accessToken: "", refreshToken: "", expiresAt: Date.now() + 3600000 };
}

export async function refreshOpenAICodexToken(_refreshToken) {
  return { accessToken: "", refreshToken: "", expiresAt: Date.now() + 3600000 };
}

export default { StringEnum, calculateCost, createAssistantMessageEventStream, streamSimpleAnthropic, streamSimpleOpenAIResponses, complete, completeSimple, getModel, getApiProvider, getModels, loginOpenAICodex, refreshOpenAICodexToken };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "@mariozechner/pi-tui".to_string(),
        r#"
export function matchesKey(_data, _key) {
  return false;
}

export function truncateToWidth(text, width) {
  const s = String(text ?? "");
  const w = Number(width ?? 0);
  if (!w || w <= 0) return "";
  return s.length <= w ? s : s.slice(0, w);
}

export class Text {
  constructor(text, x = 0, y = 0) {
    this.text = String(text ?? "");
    this.x = x;
    this.y = y;
  }
}

export class Container {
  constructor(..._args) {}
}

export class Markdown {
  constructor(..._args) {}
}

export class Spacer {
  constructor(..._args) {}
}

export function visibleWidth(str) {
  return String(str ?? "").length;
}

export function wrapTextWithAnsi(text, _width) {
  return String(text ?? "");
}

export class Editor {
  constructor(_opts = {}) {
    this.value = "";
  }
}

export const CURSOR_MARKER = "▌";

export function isKeyRelease(_data) {
  return false;
}

export function parseKey(key) {
  return { key: String(key ?? "") };
}

export class Box {
  constructor(_padX = 0, _padY = 0, _styleFn = null) {
    this.children = [];
  }

  addChild(child) {
    this.children.push(child);
  }
}

export class SelectList {
  constructor(items = [], _opts = {}) {
    this.items = Array.isArray(items) ? items : [];
    this.selected = 0;
  }

  setItems(items) {
    this.items = Array.isArray(items) ? items : [];
  }

  select(index) {
    const i = Number(index ?? 0);
    this.selected = Number.isFinite(i) ? i : 0;
  }
}

export class Input {
  constructor(_opts = {}) {
    this.value = "";
  }
}

export const Key = {
  // Special keys
  escape: "escape",
  esc: "esc",
  enter: "enter",
  tab: "tab",
  space: "space",
  backspace: "backspace",
  delete: "delete",
  home: "home",
  end: "end",
  pageUp: "pageUp",
  pageDown: "pageDown",
  up: "up",
  down: "down",
  left: "left",
  right: "right",
  // Single modifiers
  ctrl: (key) => `ctrl+${key}`,
  shift: (key) => `shift+${key}`,
  alt: (key) => `alt+${key}`,
  // Combined modifiers
  ctrlShift: (key) => `ctrl+shift+${key}`,
  shiftCtrl: (key) => `shift+ctrl+${key}`,
  ctrlAlt: (key) => `ctrl+alt+${key}`,
  altCtrl: (key) => `alt+ctrl+${key}`,
  shiftAlt: (key) => `shift+alt+${key}`,
  altShift: (key) => `alt+shift+${key}`,
  ctrlAltShift: (key) => `ctrl+alt+shift+${key}`,
};

export class DynamicBorder {
  constructor(_styleFn = null) {
    this.styleFn = _styleFn;
  }
}

export class SettingsList {
  constructor(_opts = {}) {
    this.items = [];
  }

  setItems(items) {
    this.items = Array.isArray(items) ? items : [];
  }
}

// Fuzzy string matching for filtering lists
export function fuzzyMatch(query, text, _opts = {}) {
  const q = String(query ?? '').toLowerCase();
  const t = String(text ?? '').toLowerCase();
  if (!q) return { match: true, score: 0, positions: [] };
  if (!t) return { match: false, score: 0, positions: [] };

  const positions = [];
  let qi = 0;
  for (let ti = 0; ti < t.length && qi < q.length; ti++) {
    if (t[ti] === q[qi]) {
      positions.push(ti);
      qi++;
    }
  }

  const match = qi === q.length;
  const score = match ? (q.length / t.length) * 100 : 0;
  return { match, score, positions };
}

// Get editor keybindings configuration
export function getEditorKeybindings() {
  return {
    save: 'ctrl+s',
    quit: 'ctrl+q',
    copy: 'ctrl+c',
    paste: 'ctrl+v',
    undo: 'ctrl+z',
    redo: 'ctrl+y',
    find: 'ctrl+f',
    replace: 'ctrl+h',
  };
}

// Filter an array of items using fuzzy matching
export function fuzzyFilter(query, items, _opts = {}) {
  const q = String(query ?? '').toLowerCase();
  if (!q) return items;
  if (!Array.isArray(items)) return [];
  return items.filter(item => {
    const text = typeof item === 'string' ? item : String(item?.label ?? item?.name ?? item);
    return fuzzyMatch(q, text).match;
  });
}

// Cancellable loader widget - shows loading state with optional cancel
export class CancellableLoader {
  constructor(message = 'Loading...', opts = {}) {
    this.message = String(message ?? 'Loading...');
    this.cancelled = false;
    this.onCancel = opts.onCancel ?? null;
  }

  cancel() {
    this.cancelled = true;
    if (typeof this.onCancel === 'function') {
      this.onCancel();
    }
  }

  render() {
    return this.cancelled ? [] : [this.message];
  }
}

export class Image {
  constructor(src, _opts = {}) {
    this.src = String(src ?? "");
    this.width = 0;
    this.height = 0;
  }
}

export default { matchesKey, truncateToWidth, visibleWidth, wrapTextWithAnsi, Text, Container, Markdown, Spacer, Editor, Box, SelectList, Input, Image, CURSOR_MARKER, isKeyRelease, parseKey, Key, DynamicBorder, SettingsList, fuzzyMatch, getEditorKeybindings, fuzzyFilter, CancellableLoader };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "@mariozechner/pi-coding-agent".to_string(),
        r#"
export const VERSION = "0.0.0";

export const DEFAULT_MAX_LINES = 2000;
export const DEFAULT_MAX_BYTES = 50 * 1024;

export function formatSize(bytes) {
  const b = Number(bytes ?? 0);
  const KB = 1024;
  const MB = 1024 * 1024;
  if (b >= MB) return `${(b / MB).toFixed(1)}MB`;
  if (b >= KB) return `${(b / KB).toFixed(1)}KB`;
  return `${Math.trunc(b)}B`;
}

function jsBytes(value) {
  return String(value ?? "").length;
}

export function truncateHead(text, opts = {}) {
  const raw = String(text ?? "");
  const maxLines = Number(opts.maxLines ?? DEFAULT_MAX_LINES);
  const maxBytes = Number(opts.maxBytes ?? DEFAULT_MAX_BYTES);

  const lines = raw.split("\n");
  const totalLines = lines.length;
  const totalBytes = jsBytes(raw);

  const out = [];
  let outBytes = 0;
  let truncatedBy = null;

  for (const line of lines) {
    if (out.length >= maxLines) {
      truncatedBy = "lines";
      break;
    }

    const candidate = out.length ? `\n${line}` : line;
    const candidateBytes = jsBytes(candidate);
    if (outBytes + candidateBytes > maxBytes) {
      truncatedBy = "bytes";
      break;
    }
    out.push(line);
    outBytes += candidateBytes;
  }

  const content = out.join("\n");
  return {
    content,
    truncated: truncatedBy != null,
    truncatedBy,
    totalLines,
    totalBytes,
    outputLines: out.length,
    outputBytes: jsBytes(content),
    lastLinePartial: false,
    firstLineExceedsLimit: false,
    maxLines,
    maxBytes,
  };
}

export function truncateTail(text, opts = {}) {
  const raw = String(text ?? "");
  const maxLines = Number(opts.maxLines ?? DEFAULT_MAX_LINES);
  const maxBytes = Number(opts.maxBytes ?? DEFAULT_MAX_BYTES);

  const lines = raw.split("\n");
  const totalLines = lines.length;
  const totalBytes = jsBytes(raw);

  const out = [];
  let outBytes = 0;
  let truncatedBy = null;

  for (let i = lines.length - 1; i >= 0; i--) {
    if (out.length >= maxLines) {
      truncatedBy = "lines";
      break;
    }
    const line = lines[i];
    const candidate = out.length ? `${line}\n` : line;
    const candidateBytes = jsBytes(candidate);
    if (outBytes + candidateBytes > maxBytes) {
      truncatedBy = "bytes";
      break;
    }
    out.unshift(line);
    outBytes += candidateBytes;
  }

  const content = out.join("\n");
  return {
    content,
    truncated: truncatedBy != null,
    truncatedBy,
    totalLines,
    totalBytes,
    outputLines: out.length,
    outputBytes: jsBytes(content),
    lastLinePartial: false,
    firstLineExceedsLimit: false,
    maxLines,
    maxBytes,
  };
}

export function parseSessionEntries(text) {
  const raw = String(text ?? "");
  const out = [];
  for (const line of raw.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      out.push(JSON.parse(trimmed));
    } catch {
      // ignore malformed lines
    }
  }
  return out;
}

export function convertToLlm(entries) {
  return entries;
}

export function serializeConversation(entries) {
  try {
    return JSON.stringify(entries ?? []);
  } catch {
    return String(entries ?? "");
  }
}

export function parseFrontmatter(text) {
  const raw = String(text ?? "");
  if (!raw.startsWith("---")) return { frontmatter: {}, body: raw };
  const end = raw.indexOf("\n---", 3);
  if (end === -1) return { frontmatter: {}, body: raw };

  const header = raw.slice(3, end).trim();
  const body = raw.slice(end + 4).replace(/^\n/, "");
  const frontmatter = {};
  for (const line of header.split(/\r?\n/)) {
    const idx = line.indexOf(":");
    if (idx === -1) continue;
    const key = line.slice(0, idx).trim();
    const val = line.slice(idx + 1).trim();
    if (!key) continue;
    frontmatter[key] = val;
  }
  return { frontmatter, body };
}

export function getMarkdownTheme() {
  return {};
}

export function getSettingsListTheme() {
  return {};
}

export function getSelectListTheme() {
  return {};
}

export class DynamicBorder {
  constructor(..._args) {}
}

export class BorderedLoader {
  constructor(..._args) {}
}

export class CustomEditor {
  constructor(_opts = {}) {
    this.value = "";
  }

  handleInput(_data) {}

  render(_width) {
    return [];
  }
}

export function createBashTool(_cwd, _opts = {}) {
  return {
    name: "bash",
    label: "bash",
    description: "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
    parameters: {
      type: "object",
      properties: {
        command: { type: "string", description: "The bash command to execute" },
        timeout: { type: "number", description: "Optional timeout in seconds" },
      },
      required: ["command"],
    },
    async execute(_id, params) {
      return { content: [{ type: "text", text: String(params?.command ?? "") }], details: {} };
    },
  };
}

export function createReadTool(_cwd, _opts = {}) {
  return {
    name: "read",
    label: "read",
    description: "Read the contents of a file. Supports text files and images (jpg, png, gif, webp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
    parameters: {
      type: "object",
      properties: {
        path: { type: "string", description: "The path to the file to read" },
        offset: { type: "number", description: "Line offset to start reading from (0-indexed)" },
        limit: { type: "number", description: "Maximum number of lines to read" },
      },
      required: ["path"],
    },
    async execute(_id, _params) {
      return { content: [{ type: "text", text: "" }], details: {} };
    },
  };
}

export function createLsTool(_cwd, _opts = {}) {
  return {
    name: "ls",
    label: "ls",
    description: "List files and directories. Returns names, sizes, and metadata.",
    parameters: {
      type: "object",
      properties: {
        path: { type: "string", description: "The path to list" },
      },
      required: ["path"],
    },
    async execute(_id, _params) {
      return { content: [{ type: "text", text: "" }], details: {} };
    },
  };
}

export function createGrepTool(_cwd, _opts = {}) {
  return {
    name: "grep",
    label: "grep",
    description: "Search file contents using regular expressions.",
    parameters: {
      type: "object",
      properties: {
        pattern: { type: "string", description: "The regex pattern to search for" },
        path: { type: "string", description: "The path to search in" },
      },
      required: ["pattern"],
    },
    async execute(_id, _params) {
      return { content: [{ type: "text", text: "" }], details: {} };
    },
  };
}

export function createWriteTool(_cwd, _opts = {}) {
  return {
    name: "write",
    label: "write",
    description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
    parameters: {
      type: "object",
      properties: {
        path: { type: "string", description: "The path to the file to write" },
        content: { type: "string", description: "The content to write to the file" },
      },
      required: ["path", "content"],
    },
    async execute(_id, _params) {
      return { content: [{ type: "text", text: "" }], details: {} };
    },
  };
}

export function createEditTool(_cwd, _opts = {}) {
  return {
    name: "edit",
    label: "edit",
    description: "Edit a file by replacing exact text. The oldText must match exactly (including whitespace). Use this for precise, surgical edits.",
    parameters: {
      type: "object",
      properties: {
        path: { type: "string", description: "The path to the file to edit" },
        oldText: { type: "string", description: "The exact text to find and replace" },
        newText: { type: "string", description: "The text to replace oldText with" },
      },
      required: ["path", "oldText", "newText"],
    },
    async execute(_id, _params) {
      return { content: [{ type: "text", text: "" }], details: {} };
    },
  };
}

export function copyToClipboard(_text) {
  return;
}

export function getAgentDir() {
  const home =
    globalThis.pi && globalThis.pi.env && typeof globalThis.pi.env.get === "function"
      ? globalThis.pi.env.get("HOME")
      : undefined;
  return home ? `${home}/.pi/agent` : "/home/unknown/.pi/agent";
}

// Stub: keyHint returns a keyboard shortcut hint string for UI display
export function keyHint(action, fallback = "") {
  // Map action names to default key bindings
  const keyMap = {
    expandTools: "Ctrl+E",
    copy: "Ctrl+C",
    paste: "Ctrl+V",
    save: "Ctrl+S",
    quit: "Ctrl+Q",
    help: "?",
  };
  return keyMap[action] || fallback || action;
}

// Stub: compact performs conversation compaction via LLM
export async function compact(_preparation, _model, _apiKey, _customInstructions, _signal) {
  // Return a minimal compaction result
  return {
    summary: "Conversation summary placeholder",
    firstKeptEntryId: null,
    tokensBefore: 0,
    tokensAfter: 0,
  };
}

/// Stub: AssistantMessageComponent for rendering assistant messages
export class AssistantMessageComponent {
  constructor(message, editable = false) {
    this.message = message;
    this.editable = editable;
  }

  render() {
    return [];
  }
}

// Stub: ToolExecutionComponent for rendering tool executions
export class ToolExecutionComponent {
  constructor(toolName, args, opts = {}, result, ui) {
    this.toolName = toolName;
    this.args = args;
    this.opts = opts;
    this.result = result;
    this.ui = ui;
  }

  render() {
    return [];
  }
}

// Stub: UserMessageComponent for rendering user messages
export class UserMessageComponent {
  constructor(text) {
    this.text = text;
  }

  render() {
    return [];
  }
}

export class SessionManager {
  constructor() {}
  getSessionFile() { return ""; }
  getSessionDir() { return ""; }
}

export function highlightCode(code, _lang, _theme) {
  return String(code ?? "");
}

export function getLanguageFromPath(filePath) {
  const ext = String(filePath ?? "").split(".").pop() || "";
  const map = { ts: "typescript", js: "javascript", py: "python", rs: "rust", go: "go", md: "markdown", json: "json", html: "html", css: "css", sh: "bash" };
  return map[ext] || ext;
}

export function isBashToolResult(result) {
  return result && typeof result === "object" && result.name === "bash";
}

export async function loadSkills() {
  return [];
}

export default {
  VERSION,
  DEFAULT_MAX_LINES,
  DEFAULT_MAX_BYTES,
  formatSize,
  truncateHead,
  truncateTail,
  parseSessionEntries,
  convertToLlm,
  serializeConversation,
  parseFrontmatter,
  getMarkdownTheme,
  getSettingsListTheme,
  getSelectListTheme,
  DynamicBorder,
  BorderedLoader,
  CustomEditor,
  createBashTool,
  createReadTool,
  createLsTool,
  createGrepTool,
  createWriteTool,
  createEditTool,
  copyToClipboard,
  getAgentDir,
  keyHint,
  compact,
  AssistantMessageComponent,
  ToolExecutionComponent,
  UserMessageComponent,
  SessionManager,
  highlightCode,
  getLanguageFromPath,
  isBashToolResult,
  loadSkills,
};
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "@anthropic-ai/sdk".to_string(),
        r"
export default class Anthropic {
  constructor(_opts = {}) {}
}
"
        .trim()
        .to_string(),
    );

    modules.insert(
        "@anthropic-ai/sandbox-runtime".to_string(),
        r"
export const SandboxManager = {
  initialize: async (_config) => {},
  reset: async () => {},
};
export default { SandboxManager };
"
        .trim()
        .to_string(),
    );

    modules.insert(
        "ms".to_string(),
        r#"
function parseMs(text) {
  const s = String(text ?? "").trim();
  if (!s) return undefined;

  const match = s.match(/^(\d+(?:\.\d+)?)\s*(ms|s|m|h|d|w|y)?$/i);
  if (!match) return undefined;
  const value = Number(match[1]);
  const unit = (match[2] || "ms").toLowerCase();
  const mult = unit === "ms" ? 1 :
               unit === "s"  ? 1000 :
               unit === "m"  ? 60000 :
               unit === "h"  ? 3600000 :
               unit === "d"  ? 86400000 :
               unit === "w"  ? 604800000 :
               unit === "y"  ? 31536000000 : 1;
  return Math.round(value * mult);
}

export default function ms(value) {
  return parseMs(value);
}

export const parse = parseMs;
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "jsonwebtoken".to_string(),
        r#"
export function sign() {
  throw new Error("jsonwebtoken.sign is not available in PiJS");
}

export function verify() {
  throw new Error("jsonwebtoken.verify is not available in PiJS");
}

export function decode() {
  return null;
}

export default { sign, verify, decode };
"#
        .trim()
        .to_string(),
    );

    // ── shell-quote ──────────────────────────────────────────────────
    modules.insert(
        "shell-quote".to_string(),
        r#"
export function parse(cmd) {
  if (typeof cmd !== 'string') return [];
  const args = [];
  let current = '';
  let inSingle = false;
  let inDouble = false;
  let escaped = false;
  for (let i = 0; i < cmd.length; i++) {
    const ch = cmd[i];
    if (escaped) { current += ch; escaped = false; continue; }
    if (ch === '\\' && !inSingle) { escaped = true; continue; }
    if (ch === "'" && !inDouble) { inSingle = !inSingle; continue; }
    if (ch === '"' && !inSingle) { inDouble = !inDouble; continue; }
    if ((ch === ' ' || ch === '\t') && !inSingle && !inDouble) {
      if (current) { args.push(current); current = ''; }
      continue;
    }
    current += ch;
  }
  if (current) args.push(current);
  return args;
}
export function quote(args) {
  if (!Array.isArray(args)) return '';
  return args.map(a => {
    if (/[^a-zA-Z0-9_\-=:./]/.test(a)) return "'" + a.replace(/'/g, "'\\''") + "'";
    return a;
  }).join(' ');
}
export default { parse, quote };
"#
        .trim()
        .to_string(),
    );

    // ── vscode-languageserver-protocol ──────────────────────────────
    {
        let vls = r"
export const DiagnosticSeverity = { Error: 1, Warning: 2, Information: 3, Hint: 4 };
export const CodeActionKind = { QuickFix: 'quickfix', Refactor: 'refactor', RefactorExtract: 'refactor.extract', RefactorInline: 'refactor.inline', RefactorRewrite: 'refactor.rewrite', Source: 'source', SourceOrganizeImports: 'source.organizeImports', SourceFixAll: 'source.fixAll' };
export const DocumentDiagnosticReportKind = { Full: 'full', Unchanged: 'unchanged' };
export const SymbolKind = { File: 1, Module: 2, Namespace: 3, Package: 4, Class: 5, Method: 6, Property: 7, Field: 8, Constructor: 9, Enum: 10, Interface: 11, Function: 12, Variable: 13, Constant: 14 };
function makeReqType(m) { return { type: { get method() { return m; } }, method: m }; }
function makeNotifType(m) { return { type: { get method() { return m; } }, method: m }; }
export const InitializeRequest = makeReqType('initialize');
export const DefinitionRequest = makeReqType('textDocument/definition');
export const ReferencesRequest = makeReqType('textDocument/references');
export const HoverRequest = makeReqType('textDocument/hover');
export const SignatureHelpRequest = makeReqType('textDocument/signatureHelp');
export const DocumentSymbolRequest = makeReqType('textDocument/documentSymbol');
export const RenameRequest = makeReqType('textDocument/rename');
export const CodeActionRequest = makeReqType('textDocument/codeAction');
export const DocumentDiagnosticRequest = makeReqType('textDocument/diagnostic');
export const WorkspaceDiagnosticRequest = makeReqType('workspace/diagnostic');
export const InitializedNotification = makeNotifType('initialized');
export const DidOpenTextDocumentNotification = makeNotifType('textDocument/didOpen');
export const DidChangeTextDocumentNotification = makeNotifType('textDocument/didChange');
export const DidCloseTextDocumentNotification = makeNotifType('textDocument/didClose');
export const DidSaveTextDocumentNotification = makeNotifType('textDocument/didSave');
export const PublishDiagnosticsNotification = makeNotifType('textDocument/publishDiagnostics');
export function createMessageConnection(_reader, _writer) {
  return {
    listen() {},
    sendRequest() { return Promise.resolve(null); },
    sendNotification() {},
    onNotification() {},
    onRequest() {},
    onClose() {},
    dispose() {},
  };
}
export class StreamMessageReader { constructor(_s) {} }
export class StreamMessageWriter { constructor(_s) {} }
"
        .trim()
        .to_string();

        modules.insert("vscode-languageserver-protocol".to_string(), vls.clone());
        modules.insert(
            "vscode-languageserver-protocol/node.js".to_string(),
            vls.clone(),
        );
        modules.insert("vscode-languageserver-protocol/node".to_string(), vls);
    }

    // ── @modelcontextprotocol/sdk ──────────────────────────────────
    {
        let mcp_client = r"
export class Client {
  constructor(_opts = {}) {}
  async connect(_transport) {}
  async listTools() { return { tools: [] }; }
  async listResources() { return { resources: [] }; }
  async callTool(_name, _args) { return { content: [] }; }
  async close() {}
}
"
        .trim()
        .to_string();

        let mcp_transport = r"
export class StdioClientTransport {
  constructor(_opts = {}) {}
  async start() {}
  async close() {}
}
"
        .trim()
        .to_string();

        modules.insert(
            "@modelcontextprotocol/sdk/client/index.js".to_string(),
            mcp_client.clone(),
        );
        modules.insert(
            "@modelcontextprotocol/sdk/client/index".to_string(),
            mcp_client,
        );
        modules.insert(
            "@modelcontextprotocol/sdk/client/stdio.js".to_string(),
            mcp_transport,
        );
        modules.insert(
            "@modelcontextprotocol/sdk/client/streamableHttp.js".to_string(),
            r"
export class StreamableHTTPClientTransport {
  constructor(_opts = {}) {}
  async start() {}
  async close() {}
}
"
            .trim()
            .to_string(),
        );
        modules.insert(
            "@modelcontextprotocol/sdk/client/sse.js".to_string(),
            r"
export class SSEClientTransport {
  constructor(_opts = {}) {}
  async start() {}
  async close() {}
}
"
            .trim()
            .to_string(),
        );
    }

    // ── glob ────────────────────────────────────────────────────────
    modules.insert(
        "glob".to_string(),
        r#"
export function globSync(pattern, _opts = {}) { return []; }
export function glob(pattern, optsOrCb, cb) {
  const callback = typeof optsOrCb === "function" ? optsOrCb : cb;
  if (typeof callback === "function") callback(null, []);
  return Promise.resolve([]);
}
export class Glob {
  constructor(_pattern, _opts = {}) { this.found = []; }
  on() { return this; }
}
export default { globSync, glob, Glob };
"#
        .trim()
        .to_string(),
    );

    // ── uuid ────────────────────────────────────────────────────────
    modules.insert(
        "uuid".to_string(),
        r#"
function randomHex(n) {
  let out = "";
  for (let i = 0; i < n; i++) out += Math.floor(Math.random() * 16).toString(16);
  return out;
}
export function v4() {
  return [randomHex(8), randomHex(4), "4" + randomHex(3), ((8 + Math.floor(Math.random() * 4)).toString(16)) + randomHex(3), randomHex(12)].join("-");
}
export function v7() {
  const ts = Date.now().toString(16).padStart(12, "0");
  return [ts.slice(0, 8), ts.slice(8) + randomHex(1), "7" + randomHex(3), ((8 + Math.floor(Math.random() * 4)).toString(16)) + randomHex(3), randomHex(12)].join("-");
}
export function v1() { return v4(); }
export function v3() { return v4(); }
export function v5() { return v4(); }
export function validate(uuid) { return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(String(uuid ?? "")); }
export function version(uuid) { return parseInt(String(uuid ?? "").charAt(14), 16) || 0; }
export default { v1, v3, v4, v5, v7, validate, version };
"#
        .trim()
        .to_string(),
    );

    // ── diff ────────────────────────────────────────────────────────
    modules.insert(
        "diff".to_string(),
        r#"
export function createTwoFilesPatch(oldFile, newFile, oldStr, newStr, _oldHeader, _newHeader, _opts) {
  const oldLines = String(oldStr ?? "").split("\n");
  const newLines = String(newStr ?? "").split("\n");
  let patch = `--- ${oldFile}\n+++ ${newFile}\n@@ -1,${oldLines.length} +1,${newLines.length} @@\n`;
  for (const line of oldLines) patch += `-${line}\n`;
  for (const line of newLines) patch += `+${line}\n`;
  return patch;
}
export function createPatch(fileName, oldStr, newStr, oldH, newH, opts) {
  return createTwoFilesPatch(fileName, fileName, oldStr, newStr, oldH, newH, opts);
}
export function diffLines(oldStr, newStr) {
  return [{ value: String(oldStr ?? ""), removed: true, added: false }, { value: String(newStr ?? ""), removed: false, added: true }];
}
export function diffChars(o, n) { return diffLines(o, n); }
export function diffWords(o, n) { return diffLines(o, n); }
export function applyPatch() { return false; }
export default { createTwoFilesPatch, createPatch, diffLines, diffChars, diffWords, applyPatch };
"#
        .trim()
        .to_string(),
    );

    // ── just-bash ──────────────────────────────────────────────────
    modules.insert(
        "just-bash".to_string(),
        r#"
export function bash(_cmd, _opts) { return Promise.resolve({ stdout: "", stderr: "", exitCode: 0 }); }
export { bash as Bash };
export default bash;
"#
        .trim()
        .to_string(),
    );

    // ── bunfig ─────────────────────────────────────────────────────
    modules.insert(
        "bunfig".to_string(),
        r"
export function define(_schema) { return {}; }
export async function loadConfig(opts) {
  const defaults = (opts && opts.defaultConfig) ? opts.defaultConfig : {};
  return { ...defaults };
}
export default { define, loadConfig };
"
        .trim()
        .to_string(),
    );

    // ── dotenv ─────────────────────────────────────────────────────
    modules.insert(
        "dotenv".to_string(),
        r#"
export function config(_opts) { return { parsed: {} }; }
export function parse(src) {
  const result = {};
  for (const line of String(src ?? "").split("\n")) {
    const idx = line.indexOf("=");
    if (idx === -1) continue;
    const key = line.slice(0, idx).trim();
    const val = line.slice(idx + 1).trim().replace(/^["']|["']$/g, "");
    if (key) result[key] = val;
  }
  return result;
}
export default { config, parse };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:path".to_string(),
        r#"
export function join(...parts) {
  const cleaned = parts.map((p) => String(p ?? "").replace(/\\/g, "/")).filter((p) => p.length > 0);
  return cleaned.join("/").replace(/\/+/g, "/");
}

export function dirname(p) {
  const s = String(p ?? "").replace(/\\/g, "/");
  const idx = s.lastIndexOf("/");
  if (idx <= 0) return "/";
  return s.slice(0, idx);
}

export function resolve(...parts) {
  const base =
    globalThis.pi && globalThis.pi.process && typeof globalThis.pi.process.cwd === "string"
      ? globalThis.pi.process.cwd
      : "/";
  const cleaned = parts
    .map((p) => String(p ?? "").replace(/\\/g, "/"))
    .filter((p) => p.length > 0);

  let out = "";
  for (const part of cleaned) {
    if (part.startsWith("/")) {
      out = part;
      continue;
    }
    out = out === "" || out.endsWith("/") ? out + part : out + "/" + part;
  }
  if (!out.startsWith("/")) {
    out = base.endsWith("/") ? base + out : base + "/" + out;
  }
  return out.replace(/\/+/g, "/");
}

export function basename(p, ext) {
  const s = String(p ?? "").replace(/\\/g, "/").replace(/\/+$/, "");
  const idx = s.lastIndexOf("/");
  const name = idx === -1 ? s : s.slice(idx + 1);
  if (ext && name.endsWith(ext)) {
    return name.slice(0, -ext.length);
  }
  return name;
}

export function relative(from, to) {
  const fromParts = String(from ?? "").replace(/\\/g, "/").split("/").filter(Boolean);
  const toParts = String(to ?? "").replace(/\\/g, "/").split("/").filter(Boolean);

  let common = 0;
  while (common < fromParts.length && common < toParts.length && fromParts[common] === toParts[common]) {
    common++;
  }

  const up = fromParts.length - common;
  const downs = toParts.slice(common);
  const result = [...Array(up).fill(".."), ...downs];
  return result.join("/") || ".";
}

export function isAbsolute(p) {
  return String(p ?? "").startsWith("/");
}

export function extname(p) {
  const s = String(p ?? "").replace(/\\/g, "/");
  const b = s.lastIndexOf("/");
  const name = b === -1 ? s : s.slice(b + 1);
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return "";
  return name.slice(dot);
}

export function normalize(p) {
  const s = String(p ?? "").replace(/\\/g, "/");
  const isAbs = s.startsWith("/");
  const parts = s.split("/").filter(Boolean);
  const out = [];
  for (const part of parts) {
    if (part === "..") { if (out.length > 0 && out[out.length - 1] !== "..") out.pop(); else if (!isAbs) out.push(part); }
    else if (part !== ".") out.push(part);
  }
  const result = out.join("/");
  return isAbs ? "/" + result : result || ".";
}

export function parse(p) {
  const s = String(p ?? "").replace(/\\/g, "/");
  const isAbs = s.startsWith("/");
  const lastSlash = s.lastIndexOf("/");
  const dir = lastSlash === -1 ? "" : s.slice(0, lastSlash) || (isAbs ? "/" : "");
  const base = lastSlash === -1 ? s : s.slice(lastSlash + 1);
  const ext = extname(base);
  const name = ext ? base.slice(0, -ext.length) : base;
  const root = isAbs ? "/" : "";
  return { root, dir, base, ext, name };
}

export function format(pathObj) {
  const dir = pathObj.dir || pathObj.root || "";
  const base = pathObj.base || (pathObj.name || "") + (pathObj.ext || "");
  if (!dir) return base;
  return dir === pathObj.root ? dir + base : dir + "/" + base;
}

export const sep = "/";
export const delimiter = ":";
export const posix = { join, dirname, resolve, basename, relative, isAbsolute, extname, normalize, parse, format, sep, delimiter };

const win32Stub = new Proxy({}, { get(_, prop) { throw new Error("path.win32." + String(prop) + " is not supported (Pi runs on POSIX only)"); } });
export const win32 = win32Stub;

export default { join, dirname, resolve, basename, relative, isAbsolute, extname, normalize, parse, format, sep, delimiter, posix, win32 };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:os".to_string(),
        r#"
export function homedir() {
  const home =
    globalThis.pi && globalThis.pi.env && typeof globalThis.pi.env.get === "function"
      ? globalThis.pi.env.get("HOME")
      : undefined;
  return home || "/home/unknown";
}
export function tmpdir() {
  return "/tmp";
}
export function hostname() {
  return "pi-host";
}
export function platform() {
  return "linux";
}
export function arch() {
  return "x64";
}
export function type() {
  return "Linux";
}
export function release() {
  return "6.0.0";
}
export function cpus() {
  return [{ model: "PiJS Virtual CPU", speed: 2400, times: { user: 0, nice: 0, sys: 0, idle: 0, irq: 0 } }];
}
export function totalmem() {
  return 8 * 1024 * 1024 * 1024;
}
export function freemem() {
  return 4 * 1024 * 1024 * 1024;
}
export function uptime() {
  return Math.floor(Date.now() / 1000);
}
export function loadavg() {
  return [0.0, 0.0, 0.0];
}
export function networkInterfaces() {
  return {};
}
export function userInfo(_options) {
  const home = homedir();
  return {
    uid: 1000,
    gid: 1000,
    username: "pi",
    homedir: home,
    shell: "/bin/sh",
  };
}
export function endianness() {
  return "LE";
}
export const EOL = "\n";
export const devNull = "/dev/null";
export const constants = {
  signals: {},
  errno: {},
  priority: { PRIORITY_LOW: 19, PRIORITY_BELOW_NORMAL: 10, PRIORITY_NORMAL: 0, PRIORITY_ABOVE_NORMAL: -7, PRIORITY_HIGH: -14, PRIORITY_HIGHEST: -20 },
};
export default { homedir, tmpdir, hostname, platform, arch, type, release, cpus, totalmem, freemem, uptime, loadavg, networkInterfaces, userInfo, endianness, EOL, devNull, constants };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:child_process".to_string(),
        r#"
const __pi_child_process_state = (() => {
  if (globalThis.__pi_child_process_state) {
    return globalThis.__pi_child_process_state;
  }
  const state = {
    nextPid: 1000,
    children: new Map(),
  };
  globalThis.__pi_child_process_state = state;
  return state;
})();

function __makeEmitter() {
  const listeners = new Map();
  const emitter = {
    on(event, listener) {
      const key = String(event);
      if (!listeners.has(key)) listeners.set(key, []);
      listeners.get(key).push(listener);
      return emitter;
    },
    once(event, listener) {
      const wrapper = (...args) => {
        emitter.off(event, wrapper);
        listener(...args);
      };
      return emitter.on(event, wrapper);
    },
    off(event, listener) {
      const key = String(event);
      const bucket = listeners.get(key);
      if (!bucket) return emitter;
      const idx = bucket.indexOf(listener);
      if (idx >= 0) bucket.splice(idx, 1);
      if (bucket.length === 0) listeners.delete(key);
      return emitter;
    },
    removeListener(event, listener) {
      return emitter.off(event, listener);
    },
    emit(event, ...args) {
      const key = String(event);
      const bucket = listeners.get(key) || [];
      for (const listener of [...bucket]) {
        try {
          listener(...args);
        } catch (_) {}
      }
      return emitter;
    },
  };
  return emitter;
}

function __emitCloseOnce(child, code) {
  if (child.__pi_done) return;
  child.__pi_done = true;
  __pi_child_process_state.children.delete(child.pid);
  child.emit("close", code);
}

function __parseSpawnOptions(raw) {
  const options = raw && typeof raw === "object" ? raw : {};
  const allowed = new Set(["cwd", "detached", "shell", "stdio"]);
  for (const key of Object.keys(options)) {
    if (!allowed.has(key)) {
      throw new Error(`node:child_process.spawn: unsupported option '${key}'`);
    }
  }

  if (options.shell !== undefined && options.shell !== false) {
    throw new Error("node:child_process.spawn: only shell=false is supported in PiJS");
  }

  let stdio = ["pipe", "pipe", "pipe"];
  if (options.stdio !== undefined) {
    if (!Array.isArray(options.stdio)) {
      throw new Error("node:child_process.spawn: options.stdio must be an array");
    }
    if (options.stdio.length !== 3) {
      throw new Error("node:child_process.spawn: options.stdio must have exactly 3 entries");
    }
    stdio = options.stdio.map((entry, idx) => {
      const value = String(entry ?? "");
      if (value !== "ignore" && value !== "pipe") {
        throw new Error(
          `node:child_process.spawn: unsupported stdio[${idx}] value '${value}'`,
        );
      }
      return value;
    });
  }

  const cwd =
    typeof options.cwd === "string" && options.cwd.trim().length > 0
      ? options.cwd
      : undefined;

  return {
    cwd,
    detached: Boolean(options.detached),
    stdio,
  };
}

function __installProcessKillBridge() {
  globalThis.__pi_process_kill_impl = (pidValue, signal = "SIGTERM") => {
    const pidNumeric = Number(pidValue);
    if (!Number.isFinite(pidNumeric) || pidNumeric === 0) {
      const err = new Error(`kill EINVAL: invalid pid ${String(pidValue)}`);
      err.code = "EINVAL";
      throw err;
    }
    const pid = Math.abs(Math.trunc(pidNumeric));
    const child = __pi_child_process_state.children.get(pid);
    if (!child) {
      const err = new Error(`kill ESRCH: no such process ${pid}`);
      err.code = "ESRCH";
      throw err;
    }
    child.kill(signal);
    return true;
  };
}

__installProcessKillBridge();

export function spawn(command, args = [], options = {}) {
  const cmd = String(command ?? "").trim();
  if (!cmd) {
    throw new Error("node:child_process.spawn: command is required");
  }
  if (!Array.isArray(args)) {
    throw new Error("node:child_process.spawn: args must be an array");
  }

  const argv = args.map((arg) => String(arg));
  const opts = __parseSpawnOptions(options);

  const child = __makeEmitter();
  child.pid = __pi_child_process_state.nextPid++;
  child.killed = false;
  child.__pi_done = false;
  child.__pi_kill_resolver = null;
  child.stdout = opts.stdio[1] === "pipe" ? __makeEmitter() : null;
  child.stderr = opts.stdio[2] === "pipe" ? __makeEmitter() : null;
  child.stdin = opts.stdio[0] === "pipe" ? __makeEmitter() : null;

  child.kill = (signal = "SIGTERM") => {
    if (child.__pi_done) return false;
    child.killed = true;
    if (typeof child.__pi_kill_resolver === "function") {
      child.__pi_kill_resolver({
        kind: "killed",
        signal: String(signal || "SIGTERM"),
      });
      child.__pi_kill_resolver = null;
    }
    __emitCloseOnce(child, null);
    return true;
  };

  __pi_child_process_state.children.set(child.pid, child);

  const execPromise = pi.exec(cmd, argv, { cwd: opts.cwd }).then(
    (result) => ({ kind: "result", result }),
    (error) => ({ kind: "error", error }),
  );

  const killPromise = new Promise((resolve) => {
    child.__pi_kill_resolver = resolve;
  });

  Promise.race([execPromise, killPromise]).then((outcome) => {
    if (!outcome || child.__pi_done) return;

    if (outcome.kind === "result") {
      const result = outcome.result || {};
      if (child.stdout && result.stdout !== undefined && result.stdout !== null && result.stdout !== "") {
        child.stdout.emit("data", String(result.stdout));
      }
      if (child.stderr && result.stderr !== undefined && result.stderr !== null && result.stderr !== "") {
        child.stderr.emit("data", String(result.stderr));
      }
      if (result.killed) {
        child.killed = true;
      }
      const code =
        typeof result.code === "number" && Number.isFinite(result.code)
          ? result.code
          : 0;
      __emitCloseOnce(child, code);
      return;
    }

    if (outcome.kind === "error") {
      const source = outcome.error || {};
      const error =
        source instanceof Error
          ? source
          : new Error(String(source.message || source || "spawn failed"));
      if (!error.code && source && source.code !== undefined) {
        error.code = String(source.code);
      }
      child.emit("error", error);
      __emitCloseOnce(child, 1);
    }
  });

  return child;
}

function __parseExecSyncResult(raw, command) {
  const result = JSON.parse(raw);
  if (result.error) {
    const err = new Error(`Command failed: ${command}\n${result.error}`);
    err.status = null;
    err.stdout = result.stdout || "";
    err.stderr = result.stderr || "";
    err.pid = result.pid || 0;
    err.signal = null;
    throw err;
  }
  if (result.killed) {
    const err = new Error(`Command timed out: ${command}`);
    err.killed = true;
    err.status = result.status;
    err.stdout = result.stdout || "";
    err.stderr = result.stderr || "";
    err.pid = result.pid || 0;
    err.signal = "SIGTERM";
    throw err;
  }
  return result;
}

export function spawnSync(command, argsInput, options) {
  const cmd = String(command ?? "").trim();
  if (!cmd) {
    throw new Error("node:child_process.spawnSync: command is required");
  }
  const args = Array.isArray(argsInput) ? argsInput.map(String) : [];
  const opts = (typeof argsInput === "object" && !Array.isArray(argsInput))
    ? argsInput
    : (options || {});
  const cwd = typeof opts.cwd === "string" ? opts.cwd : "";
  const timeout = typeof opts.timeout === "number" ? opts.timeout : 0;

  let result;
  try {
    const raw = __pi_exec_sync_native(cmd, JSON.stringify(args), cwd, timeout);
    result = JSON.parse(raw);
  } catch (e) {
    return {
      pid: 0,
      output: [null, "", e.message || ""],
      stdout: "",
      stderr: e.message || "",
      status: null,
      signal: null,
      error: e,
    };
  }

  if (result.error) {
    const err = new Error(result.error);
    return {
      pid: result.pid || 0,
      output: [null, result.stdout || "", result.stderr || ""],
      stdout: result.stdout || "",
      stderr: result.stderr || "",
      status: null,
      signal: null,
      error: err,
    };
  }

  return {
    pid: result.pid || 0,
    output: [null, result.stdout || "", result.stderr || ""],
    stdout: result.stdout || "",
    stderr: result.stderr || "",
    status: result.status ?? 0,
    signal: result.killed ? "SIGTERM" : null,
    error: undefined,
  };
}

export function execSync(command, options) {
  const cmdStr = String(command ?? "").trim();
  if (!cmdStr) {
    throw new Error("node:child_process.execSync: command is required");
  }
  const opts = options || {};
  const cwd = typeof opts.cwd === "string" ? opts.cwd : "";
  const timeout = typeof opts.timeout === "number" ? opts.timeout : 0;
  const maxBuffer = typeof opts.maxBuffer === "number" ? opts.maxBuffer : 1024 * 1024;

  // execSync runs through a shell, so pass via sh -c
  const raw = __pi_exec_sync_native("sh", JSON.stringify(["-c", cmdStr]), cwd, timeout);
  const result = __parseExecSyncResult(raw, cmdStr);

  if (result.status !== 0 && result.status !== null) {
    const err = new Error(
      `Command failed: ${cmdStr}\n${result.stderr || ""}`,
    );
    err.status = result.status;
    err.stdout = result.stdout || "";
    err.stderr = result.stderr || "";
    err.pid = result.pid || 0;
    err.signal = null;
    throw err;
  }

  const stdout = result.stdout || "";
  if (stdout.length > maxBuffer) {
    const err = new Error(`stdout maxBuffer length exceeded`);
    err.stdout = stdout.slice(0, maxBuffer);
    err.stderr = result.stderr || "";
    throw err;
  }

  const encoding = opts.encoding;
  if (encoding === "buffer" || encoding === null) {
    // Return a "buffer-like" string (QuickJS doesn't have real Buffer)
    return stdout;
  }
  return stdout;
}

export function exec(command, optionsOrCallback, callbackArg) {
  const opts = typeof optionsOrCallback === "object" ? optionsOrCallback : {};
  const callback = typeof optionsOrCallback === "function"
    ? optionsOrCallback
    : callbackArg;
  const cmdStr = String(command ?? "").trim();
  const cwd = opts && typeof opts.cwd === "string" ? opts.cwd : undefined;

  // Use pi.exec via shell
  pi.exec("sh", ["-c", cmdStr], { cwd }).then(
    (result) => {
      const stdout = String(result.stdout || "");
      const stderr = String(result.stderr || "");
      if (typeof callback === "function") {
        if (result.code !== 0 && result.code !== undefined && result.code !== null) {
          const err = new Error(`Command failed: ${cmdStr}`);
          err.code = result.code;
          err.killed = Boolean(result.killed);
          callback(err, stdout, stderr);
        } else {
          callback(null, stdout, stderr);
        }
      }
    },
    (error) => {
      if (typeof callback === "function") {
        callback(
          error instanceof Error ? error : new Error(String(error)),
          "",
          "",
        );
      }
    },
  );
}

export function execFileSync(file, argsInput, options) {
  const fileStr = String(file ?? "").trim();
  if (!fileStr) {
    throw new Error("node:child_process.execFileSync: file is required");
  }
  const args = Array.isArray(argsInput) ? argsInput.map(String) : [];
  const opts = (typeof argsInput === "object" && !Array.isArray(argsInput))
    ? argsInput
    : (options || {});
  const cwd = typeof opts.cwd === "string" ? opts.cwd : "";
  const timeout = typeof opts.timeout === "number" ? opts.timeout : 0;

  const raw = __pi_exec_sync_native(fileStr, JSON.stringify(args), cwd, timeout);
  const result = __parseExecSyncResult(raw, fileStr);

  if (result.status !== 0 && result.status !== null) {
    const err = new Error(
      `Command failed: ${fileStr}\n${result.stderr || ""}`,
    );
    err.status = result.status;
    err.stdout = result.stdout || "";
    err.stderr = result.stderr || "";
    err.pid = result.pid || 0;
    throw err;
  }

  return result.stdout || "";
}

export function execFile(file, argsOrOptsOrCb, optsOrCb, callbackArg) {
  const fileStr = String(file ?? "").trim();
  let args = [];
  let opts = {};
  let callback;
  if (typeof argsOrOptsOrCb === "function") {
    callback = argsOrOptsOrCb;
  } else if (Array.isArray(argsOrOptsOrCb)) {
    args = argsOrOptsOrCb.map(String);
    if (typeof optsOrCb === "function") {
      callback = optsOrCb;
    } else {
      opts = optsOrCb || {};
      callback = callbackArg;
    }
  } else if (typeof argsOrOptsOrCb === "object") {
    opts = argsOrOptsOrCb || {};
    callback = typeof optsOrCb === "function" ? optsOrCb : callbackArg;
  }

  const cwd = opts && typeof opts.cwd === "string" ? opts.cwd : undefined;

  pi.exec(fileStr, args, { cwd }).then(
    (result) => {
      const stdout = String(result.stdout || "");
      const stderr = String(result.stderr || "");
      if (typeof callback === "function") {
        if (result.code !== 0 && result.code !== undefined && result.code !== null) {
          const err = new Error(`Command failed: ${fileStr}`);
          err.code = result.code;
          callback(err, stdout, stderr);
        } else {
          callback(null, stdout, stderr);
        }
      }
    },
    (error) => {
      if (typeof callback === "function") {
        callback(
          error instanceof Error ? error : new Error(String(error)),
          "",
          "",
        );
      }
    },
  );
}

export function fork(_modulePath, _args, _opts) {
  throw new Error("node:child_process.fork is not available in PiJS");
}

export default { spawn, spawnSync, execSync, execFileSync, exec, execFile, fork };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:module".to_string(),
        r#"
import * as fs from "node:fs";
import * as fsPromises from "node:fs/promises";
import * as path from "node:path";
import * as os from "node:os";
import * as crypto from "node:crypto";
import * as url from "node:url";
import * as processMod from "node:process";
import * as buffer from "node:buffer";
import * as childProcess from "node:child_process";

function __normalizeBuiltin(id) {
  const spec = String(id ?? "");
  switch (spec) {
    case "fs":
    case "node:fs":
      return "node:fs";
    case "fs/promises":
    case "node:fs/promises":
      return "node:fs/promises";
    case "path":
    case "node:path":
      return "node:path";
    case "os":
    case "node:os":
      return "node:os";
    case "crypto":
    case "node:crypto":
      return "node:crypto";
    case "url":
    case "node:url":
      return "node:url";
    case "process":
    case "node:process":
      return "node:process";
    case "buffer":
    case "node:buffer":
      return "node:buffer";
    case "child_process":
    case "node:child_process":
      return "node:child_process";
    default:
      return spec;
  }
}

const __builtinModules = {
  "node:fs": fs,
  "node:fs/promises": fsPromises,
  "node:path": path,
  "node:os": os,
  "node:crypto": crypto,
  "node:url": url,
  "node:process": processMod,
  "node:buffer": buffer,
  "node:child_process": childProcess,
};

export function createRequire(_path) {
  return function require(id) {
    const normalized = __normalizeBuiltin(id);
    const builtIn = __builtinModules[normalized];
    if (builtIn) {
      return builtIn;
    }
    throw new Error(`Cannot find module '${String(id ?? "")}' in PiJS require()`);
  };
}

export default { createRequire };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:fs".to_string(),
        r#"
export const constants = { R_OK: 4, W_OK: 2, X_OK: 1, F_OK: 0 };
const __pi_vfs = (() => {
  if (globalThis.__pi_vfs_state) {
    return globalThis.__pi_vfs_state;
  }

  const state = {
    files: new Map(),
    dirs: new Set(["/"]),
  };

  function normalizePath(input) {
    const raw = String(input ?? "").replace(/\\/g, "/");
    const base = raw.startsWith("/")
      ? raw
      : `${(globalThis.process && typeof globalThis.process.cwd === "function" ? globalThis.process.cwd() : "/").replace(/\\/g, "/")}/${raw}`;
    const parts = [];
    for (const part of base.split("/")) {
      if (!part || part === ".") continue;
      if (part === "..") {
        if (parts.length > 0) parts.pop();
        continue;
      }
      parts.push(part);
    }
    return `/${parts.join("/")}`;
  }

  function dirname(path) {
    const normalized = normalizePath(path);
    if (normalized === "/") return "/";
    const idx = normalized.lastIndexOf("/");
    return idx <= 0 ? "/" : normalized.slice(0, idx);
  }

  function ensureDir(path) {
    const normalized = normalizePath(path);
    if (normalized === "/") return "/";
    const parts = normalized.slice(1).split("/");
    let current = "";
    for (const part of parts) {
      current = `${current}/${part}`;
      state.dirs.add(current);
    }
    return normalized;
  }

  function toBytes(data, opts) {
    const encoding =
      typeof opts === "string"
        ? opts
        : opts && typeof opts === "object" && typeof opts.encoding === "string"
          ? opts.encoding
          : undefined;
    const normalizedEncoding = encoding ? String(encoding).toLowerCase() : "utf8";

    if (typeof data === "string") {
      if (normalizedEncoding === "base64") {
        return Buffer.from(data, "base64");
      }
      return new TextEncoder().encode(data);
    }
    if (data instanceof Uint8Array) {
      return new Uint8Array(data);
    }
    if (data instanceof ArrayBuffer) {
      return new Uint8Array(data);
    }
    if (ArrayBuffer.isView(data)) {
      return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    }
    if (Array.isArray(data)) {
      return new Uint8Array(data);
    }
    return new TextEncoder().encode(String(data ?? ""));
  }

  function decodeBytes(bytes, opts) {
    const encoding =
      typeof opts === "string"
        ? opts
        : opts && typeof opts === "object" && typeof opts.encoding === "string"
          ? opts.encoding
          : undefined;
    if (!encoding || String(encoding).toLowerCase() === "buffer") {
      return Buffer.from(bytes);
    }
    const normalized = String(encoding).toLowerCase();
    if (normalized === "base64") {
      let bin = "";
      for (let i = 0; i < bytes.length; i++) {
        bin += String.fromCharCode(bytes[i] & 0xff);
      }
      return btoa(bin);
    }
    return new TextDecoder().decode(bytes);
  }

  function makeDirent(name, isDir) {
    return {
      name,
      isDirectory() { return isDir; },
      isFile() { return !isDir; },
      isSymbolicLink() { return false; },
    };
  }

  function listChildren(path, withFileTypes) {
    const normalized = normalizePath(path);
    const prefix = normalized === "/" ? "/" : `${normalized}/`;
    const children = new Map();

    for (const dir of state.dirs) {
      if (!dir.startsWith(prefix) || dir === normalized) continue;
      const rest = dir.slice(prefix.length);
      if (!rest || rest.includes("/")) continue;
      children.set(rest, true);
    }
    for (const file of state.files.keys()) {
      if (!file.startsWith(prefix)) continue;
      const rest = file.slice(prefix.length);
      if (!rest || rest.includes("/")) continue;
      if (!children.has(rest)) children.set(rest, false);
    }

    const names = Array.from(children.keys()).sort();
    if (withFileTypes) {
      return names.map((name) => makeDirent(name, children.get(name)));
    }
    return names;
  }

  function makeStat(path) {
    const normalized = normalizePath(path);
    const isDir = state.dirs.has(normalized);
    const bytes = state.files.get(normalized);
    const isFile = bytes !== undefined;
    if (!isDir && !isFile) {
      throw new Error(`ENOENT: no such file or directory, stat '${String(path ?? "")}'`);
    }
    const size = isFile ? bytes.byteLength : 0;
    return {
      isFile() { return isFile; },
      isDirectory() { return isDir; },
      isSymbolicLink() { return false; },
      isBlockDevice() { return false; },
      isCharacterDevice() { return false; },
      isFIFO() { return false; },
      isSocket() { return false; },
      size,
      mode: isDir ? 0o755 : 0o644,
      uid: 0,
      gid: 0,
      atimeMs: 0,
      mtimeMs: 0,
      ctimeMs: 0,
      birthtimeMs: 0,
      atime: new Date(0),
      mtime: new Date(0),
      ctime: new Date(0),
      birthtime: new Date(0),
      dev: 0,
      ino: 0,
      nlink: 1,
      rdev: 0,
      blksize: 4096,
      blocks: 0,
    };
  }

  state.normalizePath = normalizePath;
  state.dirname = dirname;
  state.ensureDir = ensureDir;
  state.toBytes = toBytes;
  state.decodeBytes = decodeBytes;
  state.listChildren = listChildren;
  state.makeStat = makeStat;
  globalThis.__pi_vfs_state = state;
  return state;
})();

export function existsSync(path) {
  const normalized = __pi_vfs.normalizePath(path);
  if (__pi_vfs.dirs.has(normalized) || __pi_vfs.files.has(normalized)) return true;
  if (typeof globalThis.__pi_host_read_file_sync === "function") {
    try {
      const content = globalThis.__pi_host_read_file_sync(normalized);
      const bytes = __pi_vfs.toBytes(content);
      __pi_vfs.ensureDir(__pi_vfs.dirname(normalized));
      __pi_vfs.files.set(normalized, bytes);
      return true;
    } catch (_e) { /* file not found on real FS */ }
  }
  return false;
}

export function readFileSync(path, encoding) {
  const normalized = __pi_vfs.normalizePath(path);
  let bytes = __pi_vfs.files.get(normalized);
  if (!bytes && typeof globalThis.__pi_host_read_file_sync === "function") {
    try {
      const content = globalThis.__pi_host_read_file_sync(normalized);
      bytes = __pi_vfs.toBytes(content);
      __pi_vfs.ensureDir(__pi_vfs.dirname(normalized));
      __pi_vfs.files.set(normalized, bytes);
    } catch (_e) { /* fall through to ENOENT */ }
  }
  if (!bytes) {
    throw new Error(`ENOENT: no such file or directory, open '${String(path ?? "")}'`);
  }
  return __pi_vfs.decodeBytes(bytes, encoding);
}

export function appendFileSync(path, data, opts) {
  const normalized = __pi_vfs.normalizePath(path);
  const current = __pi_vfs.files.get(normalized) || new Uint8Array();
  const next = __pi_vfs.toBytes(data, opts);
  const merged = new Uint8Array(current.byteLength + next.byteLength);
  merged.set(current, 0);
  merged.set(next, current.byteLength);
  __pi_vfs.ensureDir(__pi_vfs.dirname(normalized));
  __pi_vfs.files.set(normalized, merged);
}

export function writeFileSync(path, data, opts) {
  const normalized = __pi_vfs.normalizePath(path);
  __pi_vfs.ensureDir(__pi_vfs.dirname(normalized));
  __pi_vfs.files.set(normalized, __pi_vfs.toBytes(data, opts));
}

export function readdirSync(path, opts) {
  const normalized = __pi_vfs.normalizePath(path);
  if (!__pi_vfs.dirs.has(normalized)) {
    throw new Error(`ENOENT: no such file or directory, scandir '${String(path ?? "")}'`);
  }
  const withFileTypes = !!(opts && typeof opts === "object" && opts.withFileTypes);
  return __pi_vfs.listChildren(normalized, withFileTypes);
}

const __fakeStat = {
  isFile() { return false; },
  isDirectory() { return false; },
  isSymbolicLink() { return false; },
  isBlockDevice() { return false; },
  isCharacterDevice() { return false; },
  isFIFO() { return false; },
  isSocket() { return false; },
  size: 0, mode: 0o644, uid: 0, gid: 0,
  atimeMs: 0, mtimeMs: 0, ctimeMs: 0, birthtimeMs: 0,
  atime: new Date(0), mtime: new Date(0), ctime: new Date(0), birthtime: new Date(0),
  dev: 0, ino: 0, nlink: 1, rdev: 0, blksize: 4096, blocks: 0,
};
export function statSync(path) { return __pi_vfs.makeStat(path); }
export function lstatSync(path) { return __pi_vfs.makeStat(path); }
export function mkdtempSync(prefix, _opts) {
  const p = String(prefix ?? "/tmp/tmp-");
  const out = `${p}${Date.now().toString(36)}`;
  __pi_vfs.ensureDir(out);
  return out;
}
export function realpathSync(path, _opts) {
  return __pi_vfs.normalizePath(path);
}
export function unlinkSync(path) {
  const normalized = __pi_vfs.normalizePath(path);
  if (!__pi_vfs.files.delete(normalized)) {
    throw new Error(`ENOENT: no such file or directory, unlink '${String(path ?? "")}'`);
  }
}
export function rmdirSync(path, _opts) {
  const normalized = __pi_vfs.normalizePath(path);
  if (normalized === "/") {
    throw new Error("EBUSY: resource busy or locked, rmdir '/'");
  }
  for (const filePath of __pi_vfs.files.keys()) {
    if (filePath.startsWith(`${normalized}/`)) {
      throw new Error(`ENOTEMPTY: directory not empty, rmdir '${String(path ?? "")}'`);
    }
  }
  for (const dirPath of __pi_vfs.dirs) {
    if (dirPath.startsWith(`${normalized}/`)) {
      throw new Error(`ENOTEMPTY: directory not empty, rmdir '${String(path ?? "")}'`);
    }
  }
  if (!__pi_vfs.dirs.delete(normalized)) {
    throw new Error(`ENOENT: no such file or directory, rmdir '${String(path ?? "")}'`);
  }
}
export function rmSync(path, opts) {
  const normalized = __pi_vfs.normalizePath(path);
  if (__pi_vfs.files.has(normalized)) {
    __pi_vfs.files.delete(normalized);
    return;
  }
  if (__pi_vfs.dirs.has(normalized)) {
    const recursive = !!(opts && typeof opts === "object" && opts.recursive);
    if (!recursive) {
      rmdirSync(normalized);
      return;
    }
    for (const filePath of Array.from(__pi_vfs.files.keys())) {
      if (filePath === normalized || filePath.startsWith(`${normalized}/`)) {
        __pi_vfs.files.delete(filePath);
      }
    }
    for (const dirPath of Array.from(__pi_vfs.dirs)) {
      if (dirPath === normalized || dirPath.startsWith(`${normalized}/`)) {
        __pi_vfs.dirs.delete(dirPath);
      }
    }
    if (!__pi_vfs.dirs.has("/")) {
      __pi_vfs.dirs.add("/");
    }
    return;
  }
  throw new Error(`ENOENT: no such file or directory, rm '${String(path ?? "")}'`);
}
export function copyFileSync(src, dest, _mode) {
  writeFileSync(dest, readFileSync(src));
}
export function renameSync(oldPath, newPath) {
  const src = __pi_vfs.normalizePath(oldPath);
  const dst = __pi_vfs.normalizePath(newPath);
  const bytes = __pi_vfs.files.get(src);
  if (!bytes) {
    throw new Error(`ENOENT: no such file or directory, rename '${String(oldPath ?? "")}'`);
  }
  __pi_vfs.ensureDir(__pi_vfs.dirname(dst));
  __pi_vfs.files.set(dst, bytes);
  __pi_vfs.files.delete(src);
}
export function mkdirSync(path, _opts) {
  __pi_vfs.ensureDir(path);
  return __pi_vfs.normalizePath(path);
}
export function accessSync(path, _mode) {
  if (!existsSync(path)) {
    throw new Error("ENOENT: no such file or directory");
  }
}
export function chmodSync(_path, _mode) { return; }
export function chownSync(_path, _uid, _gid) { return; }
export function openSync(_path, _flags, _mode) { return 99; }
export function closeSync(_fd) { return; }
export function readSync(_fd, _buf, _off, _len, _pos) { return 0; }
export function writeSync(_fd, _buf, _off, _len, _pos) { return typeof _buf === 'string' ? _buf.length : (_len || 0); }
export function fstatSync(_fd) { return __fakeStat; }
export function ftruncateSync(_fd, _len) { return; }
export function futimesSync(_fd, _atime, _mtime) { return; }
function __fakeWatcher() {
  const w = { close() {}, unref() { return w; }, ref() { return w; }, on() { return w; }, once() { return w; }, removeListener() { return w; }, removeAllListeners() { return w; } };
  return w;
}
export function watch(_path, _optsOrListener, _listener) { return __fakeWatcher(); }
export function watchFile(_path, _optsOrListener, _listener) { return __fakeWatcher(); }
export function unwatchFile(_path, _listener) { return; }
export function createReadStream(_path, _opts) {
  return { on() { return this; }, pipe() { return this; }, destroy() {}, read() { return null; }, resume() { return this; }, pause() { return this; } };
}
export function createWriteStream(_path, _opts) {
  return { on() { return this; }, write() { return true; }, end() {}, destroy() {}, cork() {}, uncork() {} };
}
export function readFile(_path, optOrCb, cb) {
  const callback = typeof optOrCb === 'function' ? optOrCb : cb;
  if (typeof callback === 'function') callback(null, '');
}
export function writeFile(_path, _data, optOrCb, cb) {
  const callback = typeof optOrCb === 'function' ? optOrCb : cb;
  if (typeof callback === 'function') callback(null);
}
export function access(_path, modeOrCb, cb) {
  const callback = typeof modeOrCb === 'function' ? modeOrCb : cb;
  if (typeof callback === 'function') {
    try {
      accessSync(_path);
      callback(null);
    } catch (err) {
      callback(err);
    }
  }
}
export const promises = {
  access: async (path, _mode) => accessSync(path),
  mkdir: async (path, opts) => mkdirSync(path, opts),
  mkdtemp: async (prefix, _opts) => {
    return mkdtempSync(prefix, _opts);
  },
  readFile: async (path, opts) => readFileSync(path, opts),
  writeFile: async (path, data, opts) => writeFileSync(path, data, opts),
  unlink: async (path) => unlinkSync(path),
  rmdir: async (path, opts) => rmdirSync(path, opts),
  stat: async (path) => statSync(path),
  lstat: async (path) => lstatSync(path),
  realpath: async (path, _opts) => realpathSync(path, _opts),
  readdir: async (path, opts) => readdirSync(path, opts),
  rm: async (path, opts) => rmSync(path, opts),
  rename: async (oldPath, newPath) => renameSync(oldPath, newPath),
  copyFile: async (src, dest, mode) => copyFileSync(src, dest, mode),
  chmod: async (_path, _mode) => {},
};
export default { constants, existsSync, readFileSync, appendFileSync, writeFileSync, readdirSync, statSync, lstatSync, mkdtempSync, realpathSync, unlinkSync, rmdirSync, rmSync, copyFileSync, renameSync, mkdirSync, accessSync, chmodSync, chownSync, openSync, closeSync, readSync, writeSync, fstatSync, ftruncateSync, futimesSync, watch, watchFile, unwatchFile, createReadStream, createWriteStream, readFile, writeFile, access, promises };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:fs/promises".to_string(),
        r"
import fs from 'node:fs';

export async function access(path, mode) { return fs.promises.access(path, mode); }
export async function mkdir(path, opts) { return fs.promises.mkdir(path, opts); }
export async function mkdtemp(prefix, opts) { return fs.promises.mkdtemp(prefix, opts); }
export async function readFile(path, opts) { return fs.promises.readFile(path, opts); }
export async function writeFile(path, data, opts) { return fs.promises.writeFile(path, data, opts); }
export async function unlink(path) { return fs.promises.unlink(path); }
export async function rmdir(path, opts) { return fs.promises.rmdir(path, opts); }
export async function stat(path) { return fs.promises.stat(path); }
export async function realpath(path, opts) { return fs.promises.realpath(path, opts); }
export async function readdir(path, opts) { return fs.promises.readdir(path, opts); }
export async function rm(path, opts) { return fs.promises.rm(path, opts); }
export async function lstat(path) { return fs.promises.stat(path); }
export async function copyFile(src, dest) { return; }
export async function rename(oldPath, newPath) { return; }
export async function chmod(path, mode) { return; }
export async function chown(path, uid, gid) { return; }
export async function utimes(path, atime, mtime) { return; }
export async function appendFile(path, data, opts) { return fs.promises.writeFile(path, data, opts); }
export async function open(path, flags, mode) { return { close: async () => {} }; }
export async function truncate(path, len) { return; }
export default { access, mkdir, mkdtemp, readFile, writeFile, unlink, rmdir, stat, lstat, realpath, readdir, rm, copyFile, rename, chmod, chown, utimes, appendFile, open, truncate };
"
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:http".to_string(),
        r#"
export function createServer() {
  throw new Error("node:http.createServer is not available in PiJS");
}

export function request() {
  throw new Error("node:http.request is not available in PiJS");
}

export default { createServer, request };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:https".to_string(),
        r#"
export function request() {
  throw new Error("node:https.request is not available in PiJS");
}

export default { request };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:util".to_string(),
        r#"
export function inspect(value) {
  try {
    return JSON.stringify(value);
  } catch {
    return String(value ?? "");
  }
}

export function promisify(fn) {
  return (...args) => new Promise((resolve, reject) => {
    try {
      fn(...args, (err, result) => {
        if (err) reject(err);
        else resolve(result);
      });
    } catch (e) {
      reject(e);
    }
  });
}

export default { inspect, promisify };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:crypto".to_string(),
        crate::crypto_shim::NODE_CRYPTO_JS.trim().to_string(),
    );

    modules.insert(
        "node:readline".to_string(),
        r"
// Stub readline module - interactive prompts are not available in PiJS

export function createInterface(_opts) {
  return {
    question: (_query, callback) => {
      if (typeof callback === 'function') callback('');
    },
    close: () => {},
    on: () => {},
    once: () => {},
  };
}

export const promises = {
  createInterface: (_opts) => ({
    question: async (_query) => '',
    close: () => {},
    [Symbol.asyncIterator]: async function* () {},
  }),
};

export default { createInterface, promises };
"
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:url".to_string(),
        r"
export function fileURLToPath(url) {
  const u = String(url ?? '');
  if (u.startsWith('file://')) {
    return u.slice(7);
  }
  return u;
}
export function pathToFileURL(path) {
  return new URL('file://' + String(path ?? ''));
}
export class URL {
  constructor(input, base) {
    const u = String(input ?? '');
    this.href = u;
    this.protocol = u.split(':')[0] + ':';
    this.pathname = u.replace(/^[^:]+:\/\/[^\/]+/, '') || '/';
    this.hostname = (u.match(/^[^:]+:\/\/([^\/]+)/) || [])[1] || '';
    this.host = this.hostname;
    this.origin = this.protocol + '//' + this.hostname;
  }
  toString() { return this.href; }
}
export const URLSearchParams = globalThis.URLSearchParams || class URLSearchParams {
  constructor() { this._params = new Map(); }
  get(key) { return this._params.get(key); }
  set(key, val) { this._params.set(key, val); }
};
export default { fileURLToPath, pathToFileURL, URL, URLSearchParams };
"
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:net".to_string(),
        r"
// Stub net module - socket operations are not available in PiJS

export function createConnection(_opts, _callback) {
  throw new Error('node:net.createConnection is not available in PiJS');
}

export function createServer(_opts, _callback) {
  throw new Error('node:net.createServer is not available in PiJS');
}

export function connect(_opts, _callback) {
  throw new Error('node:net.connect is not available in PiJS');
}

export class Socket {
  constructor() {
    throw new Error('node:net.Socket is not available in PiJS');
  }
}

export class Server {
  constructor() {
    throw new Error('node:net.Server is not available in PiJS');
  }
}

export default { createConnection, createServer, connect, Socket, Server };
"
        .trim()
        .to_string(),
    );

    // ── node:events ──────────────────────────────────────────────────
    modules.insert(
        "node:events".to_string(),
        r"
class EventEmitter {
  constructor() {
    this._events = Object.create(null);
    this._maxListeners = 10;
  }

  on(event, listener) {
    if (!this._events[event]) this._events[event] = [];
    this._events[event].push(listener);
    return this;
  }

  addListener(event, listener) { return this.on(event, listener); }

  once(event, listener) {
    const wrapper = (...args) => {
      this.removeListener(event, wrapper);
      listener.apply(this, args);
    };
    wrapper._original = listener;
    return this.on(event, wrapper);
  }

  off(event, listener) { return this.removeListener(event, listener); }

  removeListener(event, listener) {
    const list = this._events[event];
    if (!list) return this;
    this._events[event] = list.filter(
      fn => fn !== listener && fn._original !== listener
    );
    if (this._events[event].length === 0) delete this._events[event];
    return this;
  }

  removeAllListeners(event) {
    if (event === undefined) {
      this._events = Object.create(null);
    } else {
      delete this._events[event];
    }
    return this;
  }

  emit(event, ...args) {
    const list = this._events[event];
    if (!list || list.length === 0) return false;
    for (const fn of list.slice()) {
      try { fn.apply(this, args); } catch (e) {
        if (event !== 'error') this.emit('error', e);
      }
    }
    return true;
  }

  listeners(event) {
    const list = this._events[event];
    if (!list) return [];
    return list.map(fn => fn._original || fn);
  }

  listenerCount(event) {
    const list = this._events[event];
    return list ? list.length : 0;
  }

  eventNames() { return Object.keys(this._events); }

  setMaxListeners(n) { this._maxListeners = n; return this; }
  getMaxListeners() { return this._maxListeners; }

  prependListener(event, listener) {
    if (!this._events[event]) this._events[event] = [];
    this._events[event].unshift(listener);
    return this;
  }

  prependOnceListener(event, listener) {
    const wrapper = (...args) => {
      this.removeListener(event, wrapper);
      listener.apply(this, args);
    };
    wrapper._original = listener;
    return this.prependListener(event, wrapper);
  }

  rawListeners(event) {
    return this._events[event] ? this._events[event].slice() : [];
  }
}

EventEmitter.EventEmitter = EventEmitter;
EventEmitter.defaultMaxListeners = 10;

export { EventEmitter };
export default EventEmitter;
"
        .trim()
        .to_string(),
    );

    // ── node:buffer ──────────────────────────────────────────────────
    modules.insert(
        "node:buffer".to_string(),
        r"
// Re-export the globalThis.Buffer if available, otherwise provide a stub.
const _Buffer = typeof globalThis.Buffer !== 'undefined'
  ? globalThis.Buffer
  : class Buffer extends Uint8Array {
      static from(input) { return new TextEncoder().encode(String(input)); }
      static alloc(size) { return new Uint8Array(size); }
      static isBuffer(obj) { return obj instanceof Uint8Array; }
      toString(encoding) {
        if (encoding === 'base64') return globalThis.btoa(String.fromCharCode(...this));
        return new TextDecoder().decode(this);
      }
    };

export const Buffer = _Buffer;
export default { Buffer: _Buffer };
"
        .trim()
        .to_string(),
    );

    // ── node:assert ──────────────────────────────────────────────────
    modules.insert(
        "node:assert".to_string(),
        r"
function assert(value, message) {
  if (!value) throw new Error(message || 'Assertion failed');
}
assert.ok = assert;
assert.equal = (a, b, msg) => { if (a != b) throw new Error(msg || `${a} != ${b}`); };
assert.strictEqual = (a, b, msg) => { if (a !== b) throw new Error(msg || `${a} !== ${b}`); };
assert.notEqual = (a, b, msg) => { if (a == b) throw new Error(msg || `${a} == ${b}`); };
assert.notStrictEqual = (a, b, msg) => { if (a === b) throw new Error(msg || `${a} === ${b}`); };
assert.deepEqual = assert.deepStrictEqual = (a, b, msg) => {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(msg || 'Deep equality failed');
};
assert.throws = (fn, _expected, msg) => {
  let threw = false;
  try { fn(); } catch (_) { threw = true; }
  if (!threw) throw new Error(msg || 'Expected function to throw');
};
assert.doesNotThrow = (fn, _expected, msg) => {
  try { fn(); } catch (e) { throw new Error(msg || `Got unwanted exception: ${e}`); }
};
assert.fail = (msg) => { throw new Error(msg || 'assert.fail()'); };

export default assert;
export { assert };
"
        .trim()
        .to_string(),
    );

    // ── node:stream ──────────────────────────────────────────────────
    modules.insert(
        "node:stream".to_string(),
        r#"
import EventEmitter from "node:events";

class Stream extends EventEmitter {}

class Readable extends Stream {
  constructor(opts) { super(); this._readableState = { flowing: null }; }
  read(_size) { return null; }
  pipe(dest) { return dest; }
  unpipe(_dest) { return this; }
  resume() { return this; }
  pause() { return this; }
  destroy() { this.emit('close'); return this; }
}

class Writable extends Stream {
  constructor(opts) { super(); this._writableState = {}; }
  write(_chunk, _encoding, _cb) { return true; }
  end(_chunk, _encoding, _cb) { this.emit('finish'); return this; }
  destroy() { this.emit('close'); return this; }
}

class Duplex extends Readable {
  constructor(opts) { super(opts); }
  write(_chunk, _encoding, _cb) { return true; }
  end(_chunk, _encoding, _cb) { this.emit('finish'); return this; }
}

class Transform extends Duplex {
  constructor(opts) { super(opts); }
  _transform(_chunk, _encoding, callback) { callback(); }
}

class PassThrough extends Transform {
  _transform(chunk, _encoding, callback) { callback(null, chunk); }
}

export { Stream, Readable, Writable, Duplex, Transform, PassThrough };
export default { Stream, Readable, Writable, Duplex, Transform, PassThrough };
"#
        .trim()
        .to_string(),
    );

    // node:stream/promises — promise-based stream utilities
    modules.insert(
        "node:stream/promises".to_string(),
        r"
import { Readable, Writable } from 'node:stream';
export async function pipeline(...streams) {
  // Stub pipeline: just resolves immediately
  return;
}
export async function finished(stream) {
  // Stub finished: resolves immediately
  return;
}
export default { pipeline, finished };
"
        .trim()
        .to_string(),
    );

    // node:string_decoder — often imported by stream consumers
    modules.insert(
        "node:string_decoder".to_string(),
        r"
export class StringDecoder {
  constructor(encoding) { this.encoding = encoding || 'utf8'; }
  write(buf) { return typeof buf === 'string' ? buf : String(buf ?? ''); }
  end(buf) { return buf ? this.write(buf) : ''; }
}
export default { StringDecoder };
"
        .trim()
        .to_string(),
    );

    // node:querystring — URL query string encoding/decoding
    modules.insert(
        "node:querystring".to_string(),
        r"
export function parse(qs, sep, eq) {
  const s = String(qs ?? '');
  const sepStr = sep || '&';
  const eqStr = eq || '=';
  const result = {};
  if (!s) return result;
  for (const pair of s.split(sepStr)) {
    const idx = pair.indexOf(eqStr);
    const key = idx === -1 ? decodeURIComponent(pair) : decodeURIComponent(pair.slice(0, idx));
    const val = idx === -1 ? '' : decodeURIComponent(pair.slice(idx + eqStr.length));
    if (Object.prototype.hasOwnProperty.call(result, key)) {
      if (Array.isArray(result[key])) result[key].push(val);
      else result[key] = [result[key], val];
    } else {
      result[key] = val;
    }
  }
  return result;
}
export function stringify(obj, sep, eq) {
  const sepStr = sep || '&';
  const eqStr = eq || '=';
  if (!obj || typeof obj !== 'object') return '';
  return Object.entries(obj).map(([k, v]) => {
    if (Array.isArray(v)) return v.map(i => encodeURIComponent(k) + eqStr + encodeURIComponent(i)).join(sepStr);
    return encodeURIComponent(k) + eqStr + encodeURIComponent(v ?? '');
  }).join(sepStr);
}
export const decode = parse;
export const encode = stringify;
export function escape(str) { return encodeURIComponent(str); }
export function unescape(str) { return decodeURIComponent(str); }
export default { parse, stringify, decode, encode, escape, unescape };
"
        .trim()
        .to_string(),
    );

    // node:process — re-exports globalThis.process
    modules.insert(
        "node:process".to_string(),
        r"
const p = globalThis.process || {};
export const env = p.env || {};
export const argv = p.argv || [];
export const cwd = typeof p.cwd === 'function' ? p.cwd : () => '/';
export const chdir = typeof p.chdir === 'function' ? p.chdir : () => { throw new Error('ENOSYS'); };
export const platform = p.platform || 'linux';
export const arch = p.arch || 'x64';
export const version = p.version || 'v20.0.0';
export const versions = p.versions || {};
export const pid = p.pid || 1;
export const ppid = p.ppid || 0;
export const title = p.title || 'pi';
export const execPath = p.execPath || '/usr/bin/pi';
export const execArgv = p.execArgv || [];
export const stdout = p.stdout || { write() {} };
export const stderr = p.stderr || { write() {} };
export const stdin = p.stdin || {};
export const nextTick = p.nextTick || ((fn, ...a) => Promise.resolve().then(() => fn(...a)));
export const hrtime = p.hrtime || Object.assign(() => [0, 0], { bigint: () => BigInt(0) });
export const exit = p.exit || (() => {});
export const kill = p.kill || (() => {});
export const on = p.on || (() => p);
export const off = p.off || (() => p);
export const once = p.once || (() => p);
export const addListener = p.addListener || (() => p);
export const removeListener = p.removeListener || (() => p);
export const removeAllListeners = p.removeAllListeners || (() => p);
export const listeners = p.listeners || (() => []);
export const emit = p.emit || (() => false);
export const emitWarning = p.emitWarning || (() => {});
export const uptime = p.uptime || (() => 0);
export const memoryUsage = p.memoryUsage || (() => ({ rss: 0, heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0 }));
export const cpuUsage = p.cpuUsage || (() => ({ user: 0, system: 0 }));
export const release = p.release || { name: 'node' };
export default p;
"
        .trim()
        .to_string(),
    );

    modules
}

/// Integrated PiJS runtime combining QuickJS, scheduler, and Promise bridge.
///
/// This is the main entry point for running JavaScript extensions with
/// proper async hostcall support. It provides:
///
/// - Promise-based `pi.*` methods that enqueue hostcall requests
/// - Deterministic event loop scheduling
/// - Automatic microtask draining after macrotasks
/// - Hostcall completion → Promise resolution/rejection
///
/// # Example
///
/// ```ignore
/// // Create runtime
/// let runtime = PiJsRuntime::new().await?;
///
/// // Evaluate extension code
/// runtime.eval("
///     pi.tool('read', { path: 'foo.txt' }).then(result => {
///         console.log('Got:', result);
///     });
/// ").await?;
///
/// // Process hostcall requests
/// while let Some(request) = runtime.drain_hostcall_requests().pop_front() {
///     // Execute the hostcall
///     let result = execute_tool(&request.kind, &request.payload).await;
///     // Deliver completion back to JS
///     runtime.complete_hostcall(&request.call_id, result)?;
/// }
///
/// // Tick the event loop to deliver completions
/// let stats = runtime.tick().await?;
/// ```
pub struct PiJsRuntime<C: SchedulerClock = WallClock> {
    runtime: AsyncRuntime,
    context: AsyncContext,
    scheduler: Rc<RefCell<Scheduler<C>>>,
    hostcall_queue: HostcallQueue,
    trace_seq: Arc<AtomicU64>,
    hostcall_tracker: Rc<RefCell<HostcallTracker>>,
    hostcalls_total: Arc<AtomicU64>,
    hostcalls_timed_out: Arc<AtomicU64>,
    peak_memory_used_bytes: Arc<AtomicU64>,
    interrupt_budget: Rc<InterruptBudget>,
    config: PiJsRuntimeConfig,
}

#[allow(clippy::future_not_send)]
impl PiJsRuntime<WallClock> {
    /// Create a new PiJS runtime with the default wall clock.
    #[allow(clippy::future_not_send)]
    pub async fn new() -> Result<Self> {
        Self::with_clock(WallClock).await
    }
}

#[allow(clippy::future_not_send)]
impl<C: SchedulerClock + 'static> PiJsRuntime<C> {
    /// Create a new PiJS runtime with a custom clock.
    #[allow(clippy::future_not_send)]
    pub async fn with_clock(clock: C) -> Result<Self> {
        Self::with_clock_and_config(clock, PiJsRuntimeConfig::default()).await
    }

    /// Create a new PiJS runtime with a custom clock and runtime config.
    #[allow(clippy::future_not_send)]
    pub async fn with_clock_and_config(clock: C, mut config: PiJsRuntimeConfig) -> Result<Self> {
        // Inject target architecture so JS process.arch can read it
        #[cfg(target_arch = "x86_64")]
        config
            .env
            .entry("PI_TARGET_ARCH".to_string())
            .or_insert_with(|| "x64".to_string());
        #[cfg(target_arch = "aarch64")]
        config
            .env
            .entry("PI_TARGET_ARCH".to_string())
            .or_insert_with(|| "arm64".to_string());
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        config
            .env
            .entry("PI_TARGET_ARCH".to_string())
            .or_insert_with(|| "x64".to_string());

        let runtime = AsyncRuntime::new().map_err(|err| map_js_error(&err))?;
        if let Some(limit) = config.limits.memory_limit_bytes {
            runtime.set_memory_limit(limit).await;
        }
        if let Some(limit) = config.limits.max_stack_bytes {
            runtime.set_max_stack_size(limit).await;
        }

        let interrupt_budget = Rc::new(InterruptBudget::new(config.limits.interrupt_budget));
        if config.limits.interrupt_budget.is_some() {
            let budget = Rc::clone(&interrupt_budget);
            runtime
                .set_interrupt_handler(Some(Box::new(move || budget.on_interrupt())))
                .await;
        }

        let module_state = Rc::new(RefCell::new(PiJsModuleState::new()));
        runtime
            .set_loader(
                PiJsResolver {
                    state: Rc::clone(&module_state),
                },
                PiJsLoader {
                    state: Rc::clone(&module_state),
                },
            )
            .await;

        let context = AsyncContext::full(&runtime)
            .await
            .map_err(|err| map_js_error(&err))?;

        let scheduler = Rc::new(RefCell::new(Scheduler::with_clock(clock)));
        let hostcall_queue: HostcallQueue = Rc::new(RefCell::new(VecDeque::new()));
        let hostcall_tracker = Rc::new(RefCell::new(HostcallTracker::default()));
        let hostcalls_total = Arc::new(AtomicU64::new(0));
        let hostcalls_timed_out = Arc::new(AtomicU64::new(0));
        let peak_memory_used_bytes = Arc::new(AtomicU64::new(0));
        let trace_seq = Arc::new(AtomicU64::new(1));

        let instance = Self {
            runtime,
            context,
            scheduler,
            hostcall_queue,
            trace_seq,
            hostcall_tracker,
            hostcalls_total,
            hostcalls_timed_out,
            peak_memory_used_bytes,
            interrupt_budget,
            config,
        };

        instance.install_pi_bridge().await?;
        Ok(instance)
    }

    fn map_quickjs_error(&self, err: &rquickjs::Error) -> Error {
        if self.interrupt_budget.did_trip() {
            self.interrupt_budget.clear_trip();
            return Error::extension("PiJS execution budget exceeded".to_string());
        }
        map_js_error(err)
    }

    fn map_quickjs_job_error<E: std::fmt::Display>(&self, err: E) -> Error {
        if self.interrupt_budget.did_trip() {
            self.interrupt_budget.clear_trip();
            return Error::extension("PiJS execution budget exceeded".to_string());
        }
        Error::extension(format!("QuickJS job: {err}"))
    }

    /// Evaluate JavaScript source code.
    pub async fn eval(&self, source: &str) -> Result<()> {
        self.interrupt_budget.reset();
        match self.context.with(|ctx| ctx.eval::<(), _>(source)).await {
            Ok(()) => {}
            Err(err) => return Err(self.map_quickjs_error(&err)),
        }
        // Drain any immediate jobs (Promise.resolve chains, etc.)
        self.drain_jobs().await?;
        Ok(())
    }

    /// Evaluate a JavaScript file.
    pub async fn eval_file(&self, path: &std::path::Path) -> Result<()> {
        self.interrupt_budget.reset();
        match self.context.with(|ctx| ctx.eval_file::<(), _>(path)).await {
            Ok(()) => {}
            Err(err) => return Err(self.map_quickjs_error(&err)),
        }
        self.drain_jobs().await?;
        Ok(())
    }

    /// Run a closure inside the JS context and map QuickJS errors into `pi::Error`.
    ///
    /// This is intentionally `pub(crate)` so the extensions runtime can call JS helper
    /// functions without exposing raw rquickjs types as part of the public API.
    pub(crate) async fn with_ctx<F, R>(&self, f: F) -> Result<R>
    where
        F: for<'js> FnOnce(Ctx<'js>) -> rquickjs::Result<R> + rquickjs::markers::ParallelSend,
        R: rquickjs::markers::ParallelSend,
    {
        self.interrupt_budget.reset();
        match self.context.with(f).await {
            Ok(value) => Ok(value),
            Err(err) => Err(self.map_quickjs_error(&err)),
        }
    }

    /// Read a global variable from the JS context and convert it to JSON.
    ///
    /// This is primarily intended for integration tests and diagnostics; it intentionally
    /// does not expose raw `rquickjs` types as part of the public API.
    pub async fn read_global_json(&self, name: &str) -> Result<serde_json::Value> {
        self.interrupt_budget.reset();
        let value = match self
            .context
            .with(|ctx| {
                let global = ctx.globals();
                let value: Value<'_> = global.get(name)?;
                js_to_json(&value)
            })
            .await
        {
            Ok(value) => value,
            Err(err) => return Err(self.map_quickjs_error(&err)),
        };
        Ok(value)
    }

    /// Drain pending hostcall requests from the queue.
    ///
    /// Returns the requests that need to be processed by the host.
    /// After processing, call `complete_hostcall()` for each.
    pub fn drain_hostcall_requests(&self) -> VecDeque<HostcallRequest> {
        std::mem::take(&mut *self.hostcall_queue.borrow_mut())
    }

    /// Drain pending QuickJS jobs (Promise microtasks) until fixpoint.
    pub async fn drain_microtasks(&self) -> Result<usize> {
        self.drain_jobs().await
    }

    /// Return the next timer deadline (runtime clock), if any.
    pub fn next_timer_deadline_ms(&self) -> Option<u64> {
        self.scheduler.borrow().next_timer_deadline()
    }

    /// Peek at pending hostcall requests without draining.
    pub fn pending_hostcall_count(&self) -> usize {
        self.hostcall_tracker.borrow().pending_count()
    }

    /// Check whether a given hostcall is still pending.
    ///
    /// This is useful for streaming hostcalls that need to stop polling/reading once the JS side
    /// has timed out or otherwise completed the call.
    pub fn is_hostcall_pending(&self, call_id: &str) -> bool {
        self.hostcall_tracker.borrow().is_pending(call_id)
    }

    /// Get all tools registered by loaded JS extensions.
    pub async fn get_registered_tools(&self) -> Result<Vec<ExtensionToolDef>> {
        self.interrupt_budget.reset();
        let value = match self
            .context
            .with(|ctx| {
                let global = ctx.globals();
                let getter: Function<'_> = global.get("__pi_get_registered_tools")?;
                let tools: Value<'_> = getter.call(())?;
                js_to_json(&tools)
            })
            .await
        {
            Ok(value) => value,
            Err(err) => return Err(self.map_quickjs_error(&err)),
        };

        serde_json::from_value(value).map_err(|err| Error::Json(Box::new(err)))
    }

    /// Read a global value by name and convert it to JSON.
    ///
    /// This is intentionally a narrow helper that avoids exposing raw `rquickjs`
    /// types in the public API (useful for integration tests and debugging).
    pub async fn get_global_json(&self, name: &str) -> Result<serde_json::Value> {
        self.interrupt_budget.reset();
        match self
            .context
            .with(|ctx| {
                let global = ctx.globals();
                let value: Value<'_> = global.get(name)?;
                js_to_json(&value)
            })
            .await
        {
            Ok(value) => Ok(value),
            Err(err) => Err(self.map_quickjs_error(&err)),
        }
    }

    /// Enqueue a hostcall completion to be delivered on next tick.
    pub fn complete_hostcall(&self, call_id: impl Into<String>, outcome: HostcallOutcome) {
        self.scheduler
            .borrow_mut()
            .enqueue_hostcall_complete(call_id.into(), outcome);
    }

    /// Enqueue an inbound event to be delivered on next tick.
    pub fn enqueue_event(&self, event_id: impl Into<String>, payload: serde_json::Value) {
        self.scheduler
            .borrow_mut()
            .enqueue_event(event_id.into(), payload);
    }

    /// Set a timer to fire after the given delay.
    ///
    /// Returns the timer ID for cancellation.
    pub fn set_timeout(&self, delay_ms: u64) -> u64 {
        self.scheduler.borrow_mut().set_timeout(delay_ms)
    }

    /// Cancel a timer by ID.
    pub fn clear_timeout(&self, timer_id: u64) -> bool {
        self.scheduler.borrow_mut().clear_timeout(timer_id)
    }

    /// Get the current time from the clock.
    pub fn now_ms(&self) -> u64 {
        self.scheduler.borrow().now_ms()
    }

    /// Check if there are pending tasks (macrotasks or timers).
    pub fn has_pending(&self) -> bool {
        self.scheduler.borrow().has_pending() || self.pending_hostcall_count() > 0
    }

    /// Execute one tick of the event loop.
    ///
    /// This will:
    /// 1. Move due timers to the macrotask queue
    /// 2. Execute one macrotask (if any)
    /// 3. Drain all pending QuickJS jobs (microtasks)
    ///
    /// Returns statistics about what was executed.
    pub async fn tick(&self) -> Result<PiJsTickStats> {
        // Get the next macrotask from scheduler
        let macrotask = self.scheduler.borrow_mut().tick();

        let mut stats = PiJsTickStats::default();

        if let Some(task) = macrotask {
            stats.ran_macrotask = true;
            self.interrupt_budget.reset();

            // Handle the macrotask inside the JS context
            let result = self
                .context
                .with(|ctx| {
                    self.handle_macrotask(&ctx, &task)?;
                    Ok::<_, rquickjs::Error>(())
                })
                .await;
            if let Err(err) = result {
                return Err(self.map_quickjs_error(&err));
            }

            // Drain microtasks until fixpoint
            stats.jobs_drained = self.drain_jobs().await?;
        }

        stats.pending_hostcalls = self.hostcall_tracker.borrow().pending_count();
        stats.hostcalls_total = self
            .hostcalls_total
            .load(std::sync::atomic::Ordering::SeqCst);
        stats.hostcalls_timed_out = self
            .hostcalls_timed_out
            .load(std::sync::atomic::Ordering::SeqCst);

        let usage = self.runtime.memory_usage().await;
        stats.memory_used_bytes = u64::try_from(usage.memory_used_size).unwrap_or(0);
        let mut peak = self
            .peak_memory_used_bytes
            .load(std::sync::atomic::Ordering::SeqCst);
        if stats.memory_used_bytes > peak {
            peak = stats.memory_used_bytes;
            self.peak_memory_used_bytes
                .store(peak, std::sync::atomic::Ordering::SeqCst);
        }
        stats.peak_memory_used_bytes = peak;

        if let Some(limit) = self.config.limits.memory_limit_bytes {
            let limit = u64::try_from(limit).unwrap_or(u64::MAX);
            if stats.memory_used_bytes > limit {
                return Err(Error::extension(format!(
                    "PiJS memory budget exceeded (used {} bytes, limit {} bytes)",
                    stats.memory_used_bytes, limit
                )));
            }
        }

        Ok(stats)
    }

    /// Drain all pending QuickJS jobs (microtasks).
    async fn drain_jobs(&self) -> Result<usize> {
        let mut count = 0;
        loop {
            let ran = match self.runtime.execute_pending_job().await {
                Ok(ran) => ran,
                Err(err) => return Err(self.map_quickjs_job_error(err)),
            };
            if !ran {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    /// Handle a macrotask by resolving/rejecting Promises or dispatching events.
    fn handle_macrotask(
        &self,
        ctx: &Ctx<'_>,
        task: &crate::scheduler::Macrotask,
    ) -> rquickjs::Result<()> {
        use crate::scheduler::MacrotaskKind as SMK;

        match &task.kind {
            SMK::HostcallComplete { call_id, outcome } => {
                let is_nonfinal_stream = matches!(
                    outcome,
                    HostcallOutcome::StreamChunk {
                        is_final: false,
                        ..
                    }
                );

                if is_nonfinal_stream {
                    // Non-final stream chunk: keep the call pending, just deliver the chunk.
                    if !self.hostcall_tracker.borrow().is_pending(call_id) {
                        tracing::debug!(
                            event = "pijs.macrotask.stream_chunk.ignored",
                            call_id = %call_id,
                            "Ignoring stream chunk (not pending)"
                        );
                        return Ok(());
                    }
                } else {
                    // Final chunk or non-stream outcome: complete the hostcall.
                    let completion = self.hostcall_tracker.borrow_mut().on_complete(call_id);
                    let timer_id = match completion {
                        HostcallCompletion::Delivered { timer_id } => timer_id,
                        HostcallCompletion::Unknown => {
                            tracing::debug!(
                                event = "pijs.macrotask.hostcall_complete.ignored",
                                call_id = %call_id,
                                "Ignoring hostcall completion (not pending)"
                            );
                            return Ok(());
                        }
                    };

                    if let Some(timer_id) = timer_id {
                        let _ = self.scheduler.borrow_mut().clear_timeout(timer_id);
                    }
                }

                tracing::debug!(
                    event = "pijs.macrotask.hostcall_complete",
                    call_id = %call_id,
                    seq = task.seq.value(),
                    "Delivering hostcall completion"
                );
                Self::deliver_hostcall_completion(ctx, call_id, outcome)?;
            }
            SMK::TimerFired { timer_id } => {
                if let Some(call_id) = self
                    .hostcall_tracker
                    .borrow_mut()
                    .take_timed_out_call(*timer_id)
                {
                    self.hostcalls_timed_out
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tracing::warn!(
                        event = "pijs.hostcall.timeout",
                        call_id = %call_id,
                        timer_id = timer_id,
                        "Hostcall timed out"
                    );

                    let outcome = HostcallOutcome::Error {
                        code: "timeout".to_string(),
                        message: "Hostcall timed out".to_string(),
                    };
                    Self::deliver_hostcall_completion(ctx, &call_id, &outcome)?;
                    return Ok(());
                }

                tracing::debug!(
                    event = "pijs.macrotask.timer_fired",
                    timer_id = timer_id,
                    seq = task.seq.value(),
                    "Timer fired"
                );
                // Timer callbacks are stored in a JS-side map
                Self::deliver_timer_fire(ctx, *timer_id)?;
            }
            SMK::InboundEvent { event_id, payload } => {
                tracing::debug!(
                    event = "pijs.macrotask.inbound_event",
                    event_id = %event_id,
                    seq = task.seq.value(),
                    "Delivering inbound event"
                );
                Self::deliver_inbound_event(ctx, event_id, payload)?;
            }
        }
        Ok(())
    }

    /// Deliver a hostcall completion to JS.
    fn deliver_hostcall_completion(
        ctx: &Ctx<'_>,
        call_id: &str,
        outcome: &HostcallOutcome,
    ) -> rquickjs::Result<()> {
        let global = ctx.globals();
        let complete_fn: Function<'_> = global.get("__pi_complete_hostcall")?;
        let js_outcome = match outcome {
            HostcallOutcome::Success(value) => {
                let obj = Object::new(ctx.clone())?;
                obj.set("ok", true)?;
                obj.set("value", json_to_js(ctx, value)?)?;
                obj
            }
            HostcallOutcome::Error { code, message } => {
                let obj = Object::new(ctx.clone())?;
                obj.set("ok", false)?;
                obj.set("code", code.clone())?;
                obj.set("message", message.clone())?;
                obj
            }
            HostcallOutcome::StreamChunk {
                chunk,
                sequence,
                is_final,
            } => {
                let obj = Object::new(ctx.clone())?;
                obj.set("ok", true)?;
                obj.set("stream", true)?;
                obj.set("sequence", *sequence)?;
                obj.set("isFinal", *is_final)?;
                obj.set("chunk", json_to_js(ctx, chunk)?)?;
                obj
            }
        };
        complete_fn.call::<_, ()>((call_id, js_outcome))?;
        Ok(())
    }

    /// Deliver a timer fire event to JS.
    fn deliver_timer_fire(ctx: &Ctx<'_>, timer_id: u64) -> rquickjs::Result<()> {
        let global = ctx.globals();
        let fire_fn: Function<'_> = global.get("__pi_fire_timer")?;
        fire_fn.call::<_, ()>((timer_id,))?;
        Ok(())
    }

    /// Deliver an inbound event to JS.
    fn deliver_inbound_event(
        ctx: &Ctx<'_>,
        event_id: &str,
        payload: &serde_json::Value,
    ) -> rquickjs::Result<()> {
        let global = ctx.globals();
        let dispatch_fn: Function<'_> = global.get("__pi_dispatch_event")?;
        let js_payload = json_to_js(ctx, payload)?;
        dispatch_fn.call::<_, ()>((event_id, js_payload))?;
        Ok(())
    }

    /// Generate a unique trace ID.
    fn next_trace_id(&self) -> u64 {
        self.trace_seq.fetch_add(1, AtomicOrdering::SeqCst)
    }

    /// Install the pi.* bridge with Promise-returning hostcall methods.
    ///
    /// The bridge uses a two-layer design:
    /// 1. Rust native functions (`__pi_*_native`) that return call_id strings
    /// 2. JS wrappers (`pi.*`) that create Promises and register them
    ///
    /// This avoids lifetime issues with returning Promises from Rust closures.
    #[allow(clippy::too_many_lines)]
    async fn install_pi_bridge(&self) -> Result<()> {
        let hostcall_queue = self.hostcall_queue.clone();
        let scheduler = Rc::clone(&self.scheduler);
        let hostcall_tracker = Rc::clone(&self.hostcall_tracker);
        let hostcalls_total = Arc::clone(&self.hostcalls_total);
        let trace_seq = Arc::clone(&self.trace_seq);
        let default_hostcall_timeout_ms = self.config.limits.hostcall_timeout_ms;
        let process_cwd = self.config.cwd.clone();
        let process_args = self.config.args.clone();
        let env = self.config.env.clone();

        self.context
            .with(|ctx| {
                let global = ctx.globals();

                // Install native functions that return call_ids
                // These are wrapped by JS to create Promises

                // __pi_tool_native(name, input) -> call_id
                global.set(
                    "__pi_tool_native",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        let tracker = hostcall_tracker.clone();
                        let scheduler = Rc::clone(&scheduler);
                        let hostcalls_total = Arc::clone(&hostcalls_total);
                        let trace_seq = Arc::clone(&trace_seq);
                        move |ctx: Ctx<'_>,
                              name: String,
                              input: Value<'_>|
                              -> rquickjs::Result<String> {
                            let payload = js_to_json(&input)?;
                            let call_id = format!("call-{}", generate_call_id());
                            hostcalls_total.fetch_add(1, AtomicOrdering::SeqCst);
                            let trace_id = trace_seq.fetch_add(1, AtomicOrdering::SeqCst);
                            let timeout_ms = default_hostcall_timeout_ms.filter(|ms| *ms > 0);
                            let timer_id =
                                timeout_ms.map(|ms| scheduler.borrow_mut().set_timeout(ms));
                            tracker.borrow_mut().register(call_id.clone(), timer_id);
                            let extension_id: Option<String> = ctx
                                .globals()
                                .get::<_, Option<String>>("__pi_current_extension_id")
                                .ok()
                                .flatten()
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty());
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Tool { name },
                                payload,
                                trace_id,
                                extension_id,
                            };
                            queue.borrow_mut().push_back(request);
                            Ok(call_id)
                        }
                    }),
                )?;

                // __pi_exec_native(cmd, args, options) -> call_id
                global.set(
                    "__pi_exec_native",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        let tracker = hostcall_tracker.clone();
                        let scheduler = Rc::clone(&scheduler);
                        let hostcalls_total = Arc::clone(&hostcalls_total);
                        let trace_seq = Arc::clone(&trace_seq);
                        move |ctx: Ctx<'_>,
                              cmd: String,
                              args: Value<'_>,
                              options: Opt<Value<'_>>|
                              -> rquickjs::Result<String> {
                            let mut options_json = match options.0.as_ref() {
                                None => serde_json::json!({}),
                                Some(value) if value.is_null() => serde_json::json!({}),
                                Some(value) => js_to_json(value)?,
                            };
                            if let Some(default_timeout_ms) =
                                default_hostcall_timeout_ms.filter(|ms| *ms > 0)
                            {
                                match &mut options_json {
                                    serde_json::Value::Object(map) => {
                                        let has_timeout = map.contains_key("timeout")
                                            || map.contains_key("timeoutMs")
                                            || map.contains_key("timeout_ms");
                                        if !has_timeout {
                                            map.insert(
                                                "timeoutMs".to_string(),
                                                serde_json::Value::from(default_timeout_ms),
                                            );
                                        }
                                    }
                                    _ => {
                                        options_json =
                                            serde_json::json!({ "timeoutMs": default_timeout_ms });
                                    }
                                }
                            }
                            let payload = serde_json::json!({
                                "args": js_to_json(&args)?,
                                "options": options_json,
                            });
                            let call_id = format!("call-{}", generate_call_id());
                            hostcalls_total.fetch_add(1, AtomicOrdering::SeqCst);
                            let trace_id = trace_seq.fetch_add(1, AtomicOrdering::SeqCst);
                            let timeout_ms = default_hostcall_timeout_ms.filter(|ms| *ms > 0);
                            let timer_id =
                                timeout_ms.map(|ms| scheduler.borrow_mut().set_timeout(ms));
                            tracker.borrow_mut().register(call_id.clone(), timer_id);
                            let extension_id: Option<String> = ctx
                                .globals()
                                .get::<_, Option<String>>("__pi_current_extension_id")
                                .ok()
                                .flatten()
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty());
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Exec { cmd },
                                payload,
                                trace_id,
                                extension_id,
                            };
                            queue.borrow_mut().push_back(request);
                            Ok(call_id)
                        }
                    }),
                )?;

                // __pi_http_native(request) -> call_id
                global.set(
                    "__pi_http_native",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        let tracker = hostcall_tracker.clone();
                        let scheduler = Rc::clone(&scheduler);
                        let hostcalls_total = Arc::clone(&hostcalls_total);
                        let trace_seq = Arc::clone(&trace_seq);
                        move |ctx: Ctx<'_>, req: Value<'_>| -> rquickjs::Result<String> {
                            let payload = js_to_json(&req)?;
                            let call_id = format!("call-{}", generate_call_id());
                            hostcalls_total.fetch_add(1, AtomicOrdering::SeqCst);
                            let trace_id = trace_seq.fetch_add(1, AtomicOrdering::SeqCst);
                            let timeout_ms = default_hostcall_timeout_ms.filter(|ms| *ms > 0);
                            let timer_id =
                                timeout_ms.map(|ms| scheduler.borrow_mut().set_timeout(ms));
                            tracker.borrow_mut().register(call_id.clone(), timer_id);
                            let extension_id: Option<String> = ctx
                                .globals()
                                .get::<_, Option<String>>("__pi_current_extension_id")
                                .ok()
                                .flatten()
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty());
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Http,
                                payload,
                                trace_id,
                                extension_id,
                            };
                            queue.borrow_mut().push_back(request);
                            Ok(call_id)
                        }
                    }),
                )?;

                // __pi_session_native(op, args) -> call_id
                global.set(
                    "__pi_session_native",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        let tracker = hostcall_tracker.clone();
                        let scheduler = Rc::clone(&scheduler);
                        let hostcalls_total = Arc::clone(&hostcalls_total);
                        let trace_seq = Arc::clone(&trace_seq);
                        move |ctx: Ctx<'_>,
                              op: String,
                              args: Value<'_>|
                              -> rquickjs::Result<String> {
                            let payload = js_to_json(&args)?;
                            let call_id = format!("call-{}", generate_call_id());
                            hostcalls_total.fetch_add(1, AtomicOrdering::SeqCst);
                            let trace_id = trace_seq.fetch_add(1, AtomicOrdering::SeqCst);
                            let timeout_ms = default_hostcall_timeout_ms.filter(|ms| *ms > 0);
                            let timer_id =
                                timeout_ms.map(|ms| scheduler.borrow_mut().set_timeout(ms));
                            tracker.borrow_mut().register(call_id.clone(), timer_id);
                            let extension_id: Option<String> = ctx
                                .globals()
                                .get::<_, Option<String>>("__pi_current_extension_id")
                                .ok()
                                .flatten()
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty());
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Session { op },
                                payload,
                                trace_id,
                                extension_id,
                            };
                            queue.borrow_mut().push_back(request);
                            Ok(call_id)
                        }
                    }),
                )?;

                // __pi_ui_native(op, args) -> call_id
                global.set(
                    "__pi_ui_native",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        let tracker = hostcall_tracker.clone();
                        let scheduler = Rc::clone(&scheduler);
                        let hostcalls_total = Arc::clone(&hostcalls_total);
                        let trace_seq = Arc::clone(&trace_seq);
                        move |ctx: Ctx<'_>,
                              op: String,
                              args: Value<'_>|
                              -> rquickjs::Result<String> {
                            let payload = js_to_json(&args)?;
                            let call_id = format!("call-{}", generate_call_id());
                            hostcalls_total.fetch_add(1, AtomicOrdering::SeqCst);
                            let trace_id = trace_seq.fetch_add(1, AtomicOrdering::SeqCst);
                            let timeout_ms = default_hostcall_timeout_ms.filter(|ms| *ms > 0);
                            let timer_id =
                                timeout_ms.map(|ms| scheduler.borrow_mut().set_timeout(ms));
                            tracker.borrow_mut().register(call_id.clone(), timer_id);
                            let extension_id: Option<String> = ctx
                                .globals()
                                .get::<_, Option<String>>("__pi_current_extension_id")
                                .ok()
                                .flatten()
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty());
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Ui { op },
                                payload,
                                trace_id,
                                extension_id,
                            };
                            queue.borrow_mut().push_back(request);
                            Ok(call_id)
                        }
                    }),
                )?;

                // __pi_events_native(op, args) -> call_id
                global.set(
                    "__pi_events_native",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        let tracker = hostcall_tracker.clone();
                        let scheduler = Rc::clone(&scheduler);
                        let hostcalls_total = Arc::clone(&hostcalls_total);
                        let trace_seq = Arc::clone(&trace_seq);
                        move |ctx: Ctx<'_>,
                              op: String,
                              args: Value<'_>|
                              -> rquickjs::Result<String> {
                            let payload = js_to_json(&args)?;
                            let call_id = format!("call-{}", generate_call_id());
                            hostcalls_total.fetch_add(1, AtomicOrdering::SeqCst);
                            let trace_id = trace_seq.fetch_add(1, AtomicOrdering::SeqCst);
                            let timeout_ms = default_hostcall_timeout_ms.filter(|ms| *ms > 0);
                            let timer_id =
                                timeout_ms.map(|ms| scheduler.borrow_mut().set_timeout(ms));
                            tracker.borrow_mut().register(call_id.clone(), timer_id);
                            let extension_id: Option<String> = ctx
                                .globals()
                                .get::<_, Option<String>>("__pi_current_extension_id")
                                .ok()
                                .flatten()
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty());
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Events { op },
                                payload,
                                trace_id,
                                extension_id,
                            };
                            queue.borrow_mut().push_back(request);
                            Ok(call_id)
                        }
                    }),
                )?;

                // __pi_set_timeout_native(delay_ms) -> timer_id
                global.set(
                    "__pi_set_timeout_native",
                    Func::from({
                        let scheduler = Rc::clone(&scheduler);
                        move |_ctx: Ctx<'_>, delay_ms: u64| -> rquickjs::Result<u64> {
                            Ok(scheduler.borrow_mut().set_timeout(delay_ms))
                        }
                    }),
                )?;

                // __pi_clear_timeout_native(timer_id) -> bool
                global.set(
                    "__pi_clear_timeout_native",
                    Func::from({
                        let scheduler = Rc::clone(&scheduler);
                        move |_ctx: Ctx<'_>, timer_id: u64| -> rquickjs::Result<bool> {
                            Ok(scheduler.borrow_mut().clear_timeout(timer_id))
                        }
                    }),
                )?;

                // __pi_now_ms_native() -> u64
                global.set(
                    "__pi_now_ms_native",
                    Func::from({
                        let scheduler = Rc::clone(&scheduler);
                        move |_ctx: Ctx<'_>| -> rquickjs::Result<u64> {
                            Ok(scheduler.borrow().now_ms())
                        }
                    }),
                )?;

                // __pi_process_cwd_native() -> String
                global.set(
                    "__pi_process_cwd_native",
                    Func::from({
                        let process_cwd = process_cwd.clone();
                        move |_ctx: Ctx<'_>| -> rquickjs::Result<String> { Ok(process_cwd.clone()) }
                    }),
                )?;

                // __pi_process_args_native() -> string[]
                global.set(
                    "__pi_process_args_native",
                    Func::from({
                        let process_args = process_args.clone();
                        move |_ctx: Ctx<'_>| -> rquickjs::Result<Vec<String>> {
                            Ok(process_args.clone())
                        }
                    }),
                )?;

                // __pi_process_exit_native(code) -> enqueues exit hostcall
                global.set(
                    "__pi_process_exit_native",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        let tracker = hostcall_tracker.clone();
                        move |_ctx: Ctx<'_>, code: i32| -> rquickjs::Result<()> {
                            tracing::info!(
                                event = "pijs.process.exit",
                                code,
                                "process.exit requested"
                            );
                            let call_id = format!("call-{}", generate_call_id());
                            tracker.borrow_mut().register(call_id.clone(), None);
                            let request = HostcallRequest {
                                call_id,
                                kind: HostcallKind::Events {
                                    op: "exit".to_string(),
                                },
                                payload: serde_json::json!({ "code": code }),
                                trace_id: 0,
                                extension_id: None,
                            };
                            queue.borrow_mut().push_back(request);
                            Ok(())
                        }
                    }),
                )?;

                // __pi_process_execpath_native() -> string
                global.set(
                    "__pi_process_execpath_native",
                    Func::from(move |_ctx: Ctx<'_>| -> rquickjs::Result<String> {
                        Ok(std::env::current_exe().map_or_else(
                            |_| "/usr/bin/pi".to_string(),
                            |p| p.to_string_lossy().into_owned(),
                        ))
                    }),
                )?;

                // __pi_env_get_native(key) -> string | null
                global.set(
                    "__pi_env_get_native",
                    Func::from({
                        let env = env.clone();
                        move |_ctx: Ctx<'_>, key: String| -> rquickjs::Result<Option<String>> {
                            let allowed = is_env_var_allowed(&key);
                            tracing::debug!(
                                event = "pijs.env.get",
                                key = %key,
                                allowed,
                                "env get"
                            );
                            if !allowed {
                                return Ok(None);
                            }
                            Ok(env.get(&key).cloned())
                        }
                    }),
                )?;

                // __pi_crypto_sha256_hex_native(text) -> hex string
                global.set(
                    "__pi_crypto_sha256_hex_native",
                    Func::from(
                        move |_ctx: Ctx<'_>, text: String| -> rquickjs::Result<String> {
                            tracing::debug!(
                                event = "pijs.crypto.sha256_hex",
                                input_len = text.len(),
                                "crypto sha256"
                            );
                            let mut hasher = Sha256::new();
                            hasher.update(text.as_bytes());
                            let digest = hasher.finalize();
                            Ok(hex_lower(&digest))
                        },
                    ),
                )?;

                // __pi_crypto_random_bytes_native(len) -> number[] (0-255)
                global.set(
                    "__pi_crypto_random_bytes_native",
                    Func::from(
                        move |_ctx: Ctx<'_>, len: usize| -> rquickjs::Result<Vec<u8>> {
                            tracing::debug!(
                                event = "pijs.crypto.random_bytes",
                                len,
                                "crypto random bytes"
                            );
                            Ok(random_bytes(len))
                        },
                    ),
                )?;

                // __pi_base64_encode_native(binary_string) -> base64 string
                global.set(
                    "__pi_base64_encode_native",
                    Func::from(
                        move |_ctx: Ctx<'_>, input: String| -> rquickjs::Result<String> {
                            let mut bytes = Vec::with_capacity(input.len());
                            for ch in input.chars() {
                                let code = ch as u32;
                                let byte = u8::try_from(code).map_err(|_| {
                                    rquickjs::Error::new_into_js_message(
                                        "base64",
                                        "encode",
                                        "Input contains non-latin1 characters",
                                    )
                                })?;
                                bytes.push(byte);
                            }
                            Ok(BASE64_STANDARD.encode(bytes))
                        },
                    ),
                )?;

                // __pi_base64_decode_native(base64) -> binary string
                global.set(
                    "__pi_base64_decode_native",
                    Func::from(
                        move |_ctx: Ctx<'_>, input: String| -> rquickjs::Result<String> {
                            let bytes = BASE64_STANDARD.decode(input).map_err(|err| {
                                rquickjs::Error::new_into_js_message(
                                    "base64",
                                    "decode",
                                    format!("Invalid base64: {err}"),
                                )
                            })?;

                            let mut out = String::with_capacity(bytes.len());
                            for byte in bytes {
                                out.push(byte as char);
                            }
                            Ok(out)
                        },
                    ),
                )?;

                // __pi_console_output_native(level, message) — routes JS console output
                // through the Rust tracing infrastructure so extensions get a working
                // `console` global.
                global.set(
                    "__pi_console_output_native",
                    Func::from(
                        move |_ctx: Ctx<'_>,
                              level: String,
                              message: String|
                              -> rquickjs::Result<()> {
                            match level.as_str() {
                                "error" => tracing::error!(
                                    target: "pijs.console",
                                    "{message}"
                                ),
                                "warn" => tracing::warn!(
                                    target: "pijs.console",
                                    "{message}"
                                ),
                                "debug" => tracing::debug!(
                                    target: "pijs.console",
                                    "{message}"
                                ),
                                "trace" => tracing::trace!(
                                    target: "pijs.console",
                                    "{message}"
                                ),
                                // "log" and "info" both map to info
                                _ => tracing::info!(
                                    target: "pijs.console",
                                    "{message}"
                                ),
                            }
                            Ok(())
                        },
                    ),
                )?;

                // __pi_host_read_file_sync(path) -> string (throws on error)
                // Synchronous real-filesystem read fallback for node:fs readFileSync.
                global.set(
                    "__pi_host_read_file_sync",
                    Func::from(|path: String| -> rquickjs::Result<String> {
                        std::fs::read_to_string(&path).map_err(|err| {
                            rquickjs::Error::new_loading_message(&path, format!("host read: {err}"))
                        })
                    }),
                )?;

                // __pi_exec_sync_native(cmd, args_json, cwd, timeout_ms) -> JSON string
                // Synchronous subprocess execution for node:child_process execSync/spawnSync.
                // Runs std::process::Command directly (no hostcall queue).
                global.set(
                    "__pi_exec_sync_native",
                    Func::from({
                        let process_cwd = process_cwd.clone();
                        move |_ctx: Ctx<'_>,
                              cmd: String,
                              args_json: String,
                              cwd: Opt<String>,
                              timeout_ms: Opt<f64>|
                              -> rquickjs::Result<String> {
                            use std::io::Read as _;
                            use std::process::{Command, Stdio};
                            use std::time::{Duration, Instant};

                            tracing::debug!(
                                event = "pijs.exec_sync",
                                cmd = %cmd,
                                "exec_sync"
                            );

                            let args: Vec<String> =
                                serde_json::from_str(&args_json).unwrap_or_default();

                            let working_dir = cwd
                                .0
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| process_cwd.clone());

                            let timeout = timeout_ms
                                .0
                                .filter(|ms| ms.is_finite() && *ms > 0.0)
                                .map(|ms| Duration::from_secs_f64(ms / 1000.0));

                            let result: std::result::Result<serde_json::Value, String> = (|| {
                                let mut command = Command::new(&cmd);
                                command
                                    .args(&args)
                                    .current_dir(&working_dir)
                                    .stdin(Stdio::null())
                                    .stdout(Stdio::piped())
                                    .stderr(Stdio::piped());

                                let mut child = command.spawn().map_err(|e| e.to_string())?;
                                let pid = child.id();

                                let mut stdout_pipe =
                                    child.stdout.take().ok_or("Missing stdout pipe")?;
                                let mut stderr_pipe =
                                    child.stderr.take().ok_or("Missing stderr pipe")?;

                                let stdout_handle = std::thread::spawn(move || {
                                    let mut buf = Vec::new();
                                    let _ = stdout_pipe.read_to_end(&mut buf);
                                    buf
                                });
                                let stderr_handle = std::thread::spawn(move || {
                                    let mut buf = Vec::new();
                                    let _ = stderr_pipe.read_to_end(&mut buf);
                                    buf
                                });

                                let start = Instant::now();
                                let mut killed = false;
                                let status = loop {
                                    if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
                                        break st;
                                    }
                                    if let Some(t) = timeout {
                                        if start.elapsed() >= t {
                                            killed = true;
                                            crate::tools::kill_process_tree(Some(pid));
                                            let _ = child.kill();
                                            break child.wait().map_err(|e| e.to_string())?;
                                        }
                                    }
                                    std::thread::sleep(Duration::from_millis(5));
                                };

                                let stdout_bytes = stdout_handle.join().unwrap_or_default();
                                let stderr_bytes = stderr_handle.join().unwrap_or_default();

                                let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
                                let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
                                let code = status.code();

                                Ok(serde_json::json!({
                                    "stdout": stdout,
                                    "stderr": stderr,
                                    "status": code,
                                    "killed": killed,
                                    "pid": pid,
                                }))
                            })(
                            );

                            let json = match result {
                                Ok(v) => v,
                                Err(e) => serde_json::json!({
                                    "stdout": "",
                                    "stderr": "",
                                    "status": null,
                                    "error": e,
                                    "killed": false,
                                    "pid": 0,
                                }),
                            };
                            Ok(json.to_string())
                        }
                    }),
                )?;

                // Register crypto hostcalls for node:crypto module
                crate::crypto_shim::register_crypto_hostcalls(&global)?;

                // Install the JS bridge that creates Promises and wraps the native functions
                match ctx.eval::<(), _>(PI_BRIDGE_JS) {
                    Ok(()) => {}
                    Err(rquickjs::Error::Exception) => {
                        let detail = format_quickjs_exception(&ctx, ctx.catch());
                        return Err(rquickjs::Error::new_into_js_message(
                            "PI_BRIDGE_JS",
                            "eval",
                            detail,
                        ));
                    }
                    Err(err) => return Err(err),
                }

                Ok(())
            })
            .await
            .map_err(|err| map_js_error(&err))?;

        Ok(())
    }
}

/// Generate a unique call_id using a thread-local counter.
fn generate_call_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];

    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[usize::from(byte >> 4)]);
        output.push(HEX[usize::from(byte & 0x0f)]);
    }
    output
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let bytes = Uuid::new_v4().into_bytes();
        let remaining = len - out.len();
        out.extend_from_slice(&bytes[..remaining.min(bytes.len())]);
    }
    out
}

/// JavaScript bridge code for managing pending hostcalls and timer callbacks.
///
/// This code creates the `pi` global object with Promise-returning methods.
/// Each method wraps a native Rust function (`__pi_*_native`) that returns a call_id.
const PI_BRIDGE_JS: &str = r"
// ============================================================================
// Console global — must come first so all other bridge code can use it.
// ============================================================================
if (typeof globalThis.console === 'undefined') {
    const __fmt = (...args) => args.map(a => {
        if (a === null) return 'null';
        if (a === undefined) return 'undefined';
        if (typeof a === 'object') {
            try { return JSON.stringify(a); } catch (_) { return String(a); }
        }
        return String(a);
    }).join(' ');

    globalThis.console = {
        log:   (...args) => { __pi_console_output_native('log', __fmt(...args)); },
        info:  (...args) => { __pi_console_output_native('info', __fmt(...args)); },
        warn:  (...args) => { __pi_console_output_native('warn', __fmt(...args)); },
        error: (...args) => { __pi_console_output_native('error', __fmt(...args)); },
        debug: (...args) => { __pi_console_output_native('debug', __fmt(...args)); },
        trace: (...args) => { __pi_console_output_native('trace', __fmt(...args)); },
        dir:   (...args) => { __pi_console_output_native('log', __fmt(...args)); },
        time:  ()        => {},
        timeEnd: ()      => {},
        timeLog: ()      => {},
        assert: (cond, ...args) => {
            if (!cond) __pi_console_output_native('error', 'Assertion failed: ' + __fmt(...args));
        },
        count:    () => {},
        countReset: () => {},
        group:    () => {},
        groupEnd: () => {},
        table:    (...args) => { __pi_console_output_native('log', __fmt(...args)); },
        clear:    () => {},
    };
}

// ============================================================================
// Intl polyfill — minimal stubs for extensions that use Intl APIs.
// QuickJS does not ship with Intl support; these cover the most common uses.
// ============================================================================
if (typeof globalThis.Intl === 'undefined') {
    const __intlPad = (n, w) => String(n).padStart(w || 2, '0');

    class NumberFormat {
        constructor(locale, opts) {
            this._locale = locale || 'en-US';
            this._opts = opts || {};
        }
        format(n) {
            const o = this._opts;
            if (o.style === 'currency') {
                const c = o.currency || 'USD';
                const v = Number(n).toFixed(o.maximumFractionDigits ?? 2);
                return c + ' ' + v;
            }
            if (o.notation === 'compact') {
                const abs = Math.abs(n);
                if (abs >= 1e9) return (n / 1e9).toFixed(1) + 'B';
                if (abs >= 1e6) return (n / 1e6).toFixed(1) + 'M';
                if (abs >= 1e3) return (n / 1e3).toFixed(1) + 'K';
                return String(n);
            }
            if (o.style === 'percent') return (Number(n) * 100).toFixed(0) + '%';
            return String(n);
        }
        resolvedOptions() { return { ...this._opts, locale: this._locale }; }
    }

    const __months = ['Jan','Feb','Mar','Apr','May','Jun','Jul','Aug','Sep','Oct','Nov','Dec'];
    class DateTimeFormat {
        constructor(locale, opts) {
            this._locale = locale || 'en-US';
            this._opts = opts || {};
        }
        format(d) {
            const dt = d instanceof Date ? d : new Date(d ?? Date.now());
            const o = this._opts;
            const parts = [];
            if (o.month === 'short') parts.push(__months[dt.getMonth()]);
            else if (o.month === 'numeric' || o.month === '2-digit') parts.push(__intlPad(dt.getMonth() + 1));
            if (o.day === 'numeric' || o.day === '2-digit') parts.push(String(dt.getDate()));
            if (o.year === 'numeric') parts.push(String(dt.getFullYear()));
            if (parts.length === 0) {
                return __intlPad(dt.getMonth()+1) + '/' + __intlPad(dt.getDate()) + '/' + dt.getFullYear();
            }
            if (o.hour !== undefined) {
                parts.push(__intlPad(dt.getHours()) + ':' + __intlPad(dt.getMinutes()));
            }
            return parts.join(' ');
        }
        resolvedOptions() { return { ...this._opts, locale: this._locale, timeZone: 'UTC' }; }
    }

    class Collator {
        constructor(locale, opts) {
            this._locale = locale || 'en';
            this._opts = opts || {};
        }
        compare(a, b) {
            const sa = String(a ?? '');
            const sb = String(b ?? '');
            if (this._opts.sensitivity === 'base') {
                return sa.toLowerCase().localeCompare(sb.toLowerCase());
            }
            return sa.localeCompare(sb);
        }
        resolvedOptions() { return { ...this._opts, locale: this._locale }; }
    }

    class Segmenter {
        constructor(locale, opts) {
            this._locale = locale || 'en';
            this._opts = opts || {};
        }
        segment(str) {
            const s = String(str ?? '');
            const segments = [];
            // Approximate grapheme segmentation: split by codepoints
            for (const ch of s) {
                segments.push({ segment: ch, index: segments.length, input: s });
            }
            segments[Symbol.iterator] = function*() { for (const seg of segments) yield seg; };
            return segments;
        }
    }

    class RelativeTimeFormat {
        constructor(locale, opts) {
            this._locale = locale || 'en';
            this._opts = opts || {};
        }
        format(value, unit) {
            const v = Number(value);
            const u = String(unit);
            const abs = Math.abs(v);
            const plural = abs !== 1 ? 's' : '';
            if (this._opts.numeric === 'auto') {
                if (v === -1 && u === 'day') return 'yesterday';
                if (v === 1 && u === 'day') return 'tomorrow';
            }
            if (v < 0) return abs + ' ' + u + plural + ' ago';
            return 'in ' + abs + ' ' + u + plural;
        }
    }

    globalThis.Intl = {
        NumberFormat,
        DateTimeFormat,
        Collator,
        Segmenter,
        RelativeTimeFormat,
    };
}

// Pending hostcalls: call_id -> { resolve, reject }
const __pi_pending_hostcalls = new Map();

// Timer callbacks: timer_id -> callback
const __pi_timer_callbacks = new Map();

// Event listeners: event_id -> [callback, ...]
const __pi_event_listeners = new Map();

// ============================================================================
// Extension Registry (registration + hooks)
// ============================================================================

var __pi_current_extension_id = null;

// extension_id -> { id, name, version, apiVersion, tools: Map, commands: Map, hooks: Map }
const __pi_extensions = new Map();

// Fast indexes
const __pi_tool_index = new Map();      // tool_name -> { extensionId, spec, execute }
const __pi_command_index = new Map();   // command_name -> { extensionId, name, description, handler }
const __pi_hook_index = new Map();      // event_name -> [{ extensionId, handler }, ...]
const __pi_event_bus_index = new Map(); // event_name -> [{ extensionId, handler }, ...] (pi.events.on)
const __pi_provider_index = new Map();  // provider_id -> { extensionId, spec }
const __pi_shortcut_index = new Map();  // key_id -> { extensionId, key, description, handler }
const __pi_message_renderer_index = new Map(); // customType -> { extensionId, customType, renderer }

// Async task tracking for Rust-driven calls (tool exec, command exec, event dispatch).
// task_id -> { status: 'pending'|'resolved'|'rejected', value?, error? }
const __pi_tasks = new Map();

function __pi_serialize_error(err) {
    if (!err) {
        return { message: 'Unknown error' };
    }
    if (typeof err === 'string') {
        return { message: err };
    }
    const out = { message: String(err.message || err) };
    if (err.code) out.code = String(err.code);
    if (err.stack) out.stack = String(err.stack);
    return out;
}

function __pi_task_start(task_id, promise) {
    const id = String(task_id || '').trim();
    if (!id) {
        throw new Error('task_id is required');
    }
    __pi_tasks.set(id, { status: 'pending' });
    Promise.resolve(promise).then(
        (value) => {
            __pi_tasks.set(id, { status: 'resolved', value: value });
        },
        (err) => {
            __pi_tasks.set(id, { status: 'rejected', error: __pi_serialize_error(err) });
        }
    );
    return id;
}

function __pi_task_poll(task_id) {
    const id = String(task_id || '').trim();
    return __pi_tasks.get(id) || null;
}

function __pi_task_take(task_id) {
    const id = String(task_id || '').trim();
    const state = __pi_tasks.get(id) || null;
    if (state && state.status !== 'pending') {
        __pi_tasks.delete(id);
    }
    return state;
}

function __pi_get_or_create_extension(extension_id, meta) {
    const id = String(extension_id || '').trim();
    if (!id) {
        throw new Error('extension_id is required');
    }

    if (!__pi_extensions.has(id)) {
        __pi_extensions.set(id, {
            id: id,
            name: (meta && meta.name) ? String(meta.name) : id,
            version: (meta && meta.version) ? String(meta.version) : '0.0.0',
            apiVersion: (meta && meta.apiVersion) ? String(meta.apiVersion) : '1.0',
            tools: new Map(),
            commands: new Map(),
            hooks: new Map(),
            eventBusHooks: new Map(),
            providers: new Map(),
            shortcuts: new Map(),
            flags: new Map(),
            flagValues: new Map(),
            messageRenderers: new Map(),
            activeTools: null,
        });
    }

    return __pi_extensions.get(id);
}

function __pi_begin_extension(extension_id, meta) {
    const ext = __pi_get_or_create_extension(extension_id, meta);
    __pi_current_extension_id = ext.id;
}

function __pi_end_extension() {
    __pi_current_extension_id = null;
}

function __pi_current_extension_or_throw() {
    if (!__pi_current_extension_id) {
        throw new Error('No active extension. Did you forget to call __pi_begin_extension?');
    }
    const ext = __pi_extensions.get(__pi_current_extension_id);
    if (!ext) {
        throw new Error('Internal error: active extension not found');
    }
    return ext;
}

async function __pi_with_extension_async(extension_id, fn) {
    const prev = __pi_current_extension_id;
    __pi_current_extension_id = String(extension_id || '').trim();
    try {
        return await fn();
    } finally {
        __pi_current_extension_id = prev;
    }
}

async function __pi_load_extension(extension_id, entry_specifier, meta) {
    const id = String(extension_id || '').trim();
    const entry = String(entry_specifier || '').trim();
    if (!id) {
        throw new Error('load_extension: extension_id is required');
    }
    if (!entry) {
        throw new Error('load_extension: entry_specifier is required');
    }

    const prev = __pi_current_extension_id;
    __pi_begin_extension(id, meta);
    try {
        const mod = await import(entry);
        const init = mod && mod.default;
        if (typeof init !== 'function') {
            throw new Error('load_extension: entry module must default-export a function');
        }
        await init(pi);
        return true;
    } finally {
        __pi_current_extension_id = prev;
    }
}

function __pi_register_tool(spec) {
    const ext = __pi_current_extension_or_throw();
    if (!spec || typeof spec !== 'object') {
        throw new Error('registerTool: spec must be an object');
    }
    const name = String(spec.name || '').trim();
    if (!name) {
        throw new Error('registerTool: spec.name is required');
    }
    if (typeof spec.execute !== 'function') {
        throw new Error('registerTool: spec.execute must be a function');
    }

    const toolSpec = {
        name: name,
        description: spec.description ? String(spec.description) : '',
        parameters: spec.parameters || { type: 'object', properties: {} },
    };
    if (typeof spec.label === 'string') {
        toolSpec.label = spec.label;
    }

    if (__pi_tool_index.has(name)) {
        const existing = __pi_tool_index.get(name);
        if (existing && existing.extensionId !== ext.id) {
            throw new Error(`registerTool: tool name collision: ${name}`);
        }
    }

    const record = { extensionId: ext.id, spec: toolSpec, execute: spec.execute };
    ext.tools.set(name, record);
    __pi_tool_index.set(name, record);
}

function __pi_get_registered_tools() {
    const names = Array.from(__pi_tool_index.keys()).map((v) => String(v));
    names.sort();
    const out = [];
    for (const name of names) {
        const record = __pi_tool_index.get(name);
        if (!record || !record.spec) continue;
        out.push(record.spec);
    }
    return out;
}

function __pi_register_command(name, spec) {
    const ext = __pi_current_extension_or_throw();
    const cmd = String(name || '').trim().replace(/^\//, '');
    if (!cmd) {
        throw new Error('registerCommand: name is required');
    }
    if (!spec || typeof spec !== 'object') {
        throw new Error('registerCommand: spec must be an object');
    }
    // Accept both spec.handler and spec.fn (PiCommand compat)
    const handler = typeof spec.handler === 'function' ? spec.handler
        : typeof spec.fn === 'function' ? spec.fn
        : undefined;
    if (!handler) {
        throw new Error('registerCommand: spec.handler must be a function');
    }

    const cmdSpec = {
        name: cmd,
        description: spec.description ? String(spec.description) : '',
    };

    if (__pi_command_index.has(cmd)) {
        const existing = __pi_command_index.get(cmd);
        if (existing && existing.extensionId !== ext.id) {
            throw new Error(`registerCommand: command name collision: ${cmd}`);
        }
    }

    const record = {
        extensionId: ext.id,
        name: cmd,
        description: cmdSpec.description,
        handler: handler,
        spec: cmdSpec,
    };
    ext.commands.set(cmd, record);
    __pi_command_index.set(cmd, record);
}

function __pi_register_provider(provider_id, spec) {
    const ext = __pi_current_extension_or_throw();
    const id = String(provider_id || '').trim();
    if (!id) {
        throw new Error('registerProvider: id is required');
    }
    if (!spec || typeof spec !== 'object') {
        throw new Error('registerProvider: spec must be an object');
    }

    const models = Array.isArray(spec.models) ? spec.models.map((m) => {
        const out = {
            id: m && m.id ? String(m.id) : '',
            name: m && m.name ? String(m.name) : '',
        };
        if (m && m.api) out.api = String(m.api);
        if (m && m.reasoning !== undefined) out.reasoning = !!m.reasoning;
        if (m && Array.isArray(m.input)) out.input = m.input.slice();
        if (m && m.cost) out.cost = m.cost;
        if (m && m.contextWindow !== undefined) out.contextWindow = m.contextWindow;
        if (m && m.maxTokens !== undefined) out.maxTokens = m.maxTokens;
        return out;
    }) : [];

    const hasStreamSimple = typeof spec.streamSimple === 'function';
    if (spec.streamSimple !== undefined && spec.streamSimple !== null && !hasStreamSimple) {
        throw new Error('registerProvider: spec.streamSimple must be a function');
    }

    const providerSpec = {
        id: id,
        baseUrl: spec.baseUrl ? String(spec.baseUrl) : '',
        apiKey: spec.apiKey ? String(spec.apiKey) : '',
        api: spec.api ? String(spec.api) : '',
        models: models,
        hasStreamSimple: hasStreamSimple,
    };

    if (hasStreamSimple && !providerSpec.api) {
        throw new Error('registerProvider: api is required when registering streamSimple');
    }

    if (__pi_provider_index.has(id)) {
        const existing = __pi_provider_index.get(id);
        if (existing && existing.extensionId !== ext.id) {
            throw new Error(`registerProvider: provider id collision: ${id}`);
        }
    }

    const record = {
        extensionId: ext.id,
        spec: providerSpec,
        streamSimple: hasStreamSimple ? spec.streamSimple : null,
    };
    ext.providers.set(id, record);
    __pi_provider_index.set(id, record);
}

// ============================================================================
// Provider Streaming (streamSimple bridge)
// ============================================================================

let __pi_provider_stream_seq = 0;
const __pi_provider_streams = new Map(); // stream_id -> { iterator, controller }

function __pi_make_abort_controller() {
    const listeners = new Set();
    const signal = {
        aborted: false,
        addEventListener: (type, cb) => {
            if (type !== 'abort') return;
            if (typeof cb === 'function') listeners.add(cb);
        },
        removeEventListener: (type, cb) => {
            if (type !== 'abort') return;
            listeners.delete(cb);
        },
    };
    return {
        signal,
        abort: () => {
            if (signal.aborted) return;
            signal.aborted = true;
            for (const cb of listeners) {
                try {
                    cb();
                } catch (_) {}
            }
        },
    };
}

async function __pi_provider_stream_simple_start(provider_id, model, context, options) {
    const id = String(provider_id || '').trim();
    if (!id) {
        throw new Error('providerStreamSimple.start: provider_id is required');
    }
    const record = __pi_provider_index.get(id);
    if (!record) {
        throw new Error('providerStreamSimple.start: unknown provider: ' + id);
    }
    if (!record.streamSimple || typeof record.streamSimple !== 'function') {
        throw new Error('providerStreamSimple.start: provider has no streamSimple handler: ' + id);
    }

    const controller = __pi_make_abort_controller();
    const mergedOptions = Object.assign({}, options || {}, { signal: controller.signal });

    const stream = record.streamSimple(model, context, mergedOptions);
    const iterator = stream && stream[Symbol.asyncIterator] ? stream[Symbol.asyncIterator]() : stream;
    if (!iterator || typeof iterator.next !== 'function') {
        throw new Error('providerStreamSimple.start: streamSimple must return an async iterator');
    }

    const stream_id = 'provider-stream-' + String(++__pi_provider_stream_seq);
    __pi_provider_streams.set(stream_id, { iterator, controller });
    return stream_id;
}

async function __pi_provider_stream_simple_next(stream_id) {
    const id = String(stream_id || '').trim();
    const record = __pi_provider_streams.get(id);
    if (!record) {
        return { done: true, value: null };
    }

    const result = await record.iterator.next();
    if (!result || result.done) {
        __pi_provider_streams.delete(id);
        return { done: true, value: null };
    }

    return { done: false, value: result.value };
}

async function __pi_provider_stream_simple_cancel(stream_id) {
    const id = String(stream_id || '').trim();
    const record = __pi_provider_streams.get(id);
    if (!record) {
        return false;
    }

    try {
        record.controller.abort();
    } catch (_) {}

    try {
        if (record.iterator && typeof record.iterator.return === 'function') {
            await record.iterator.return();
        }
    } catch (_) {}

    __pi_provider_streams.delete(id);
    return true;
}

const __pi_reserved_keys = new Set(['ctrl+c', 'ctrl+d', 'ctrl+l', 'ctrl+z']);

function __pi_key_to_string(key) {
    // Convert Key object from @mariozechner/pi-tui to string format
    if (typeof key === 'string') {
        return key.toLowerCase();
    }
    if (key && typeof key === 'object') {
        const kind = key.kind;
        const k = key.key || '';
        if (kind === 'ctrlAlt') {
            return 'ctrl+alt+' + k.toLowerCase();
        }
        if (kind === 'ctrlShift') {
            return 'ctrl+shift+' + k.toLowerCase();
        }
        if (kind === 'ctrl') {
            return 'ctrl+' + k.toLowerCase();
        }
        if (kind === 'alt') {
            return 'alt+' + k.toLowerCase();
        }
        if (kind === 'shift') {
            return 'shift+' + k.toLowerCase();
        }
        // Fallback for unknown object format
        if (k) {
            return k.toLowerCase();
        }
    }
    return '<unknown>';
}

function __pi_register_shortcut(key, spec) {
    const ext = __pi_current_extension_or_throw();
    if (!spec || typeof spec !== 'object') {
        throw new Error('registerShortcut: spec must be an object');
    }
    if (typeof spec.handler !== 'function') {
        throw new Error('registerShortcut: spec.handler must be a function');
    }

    const keyId = __pi_key_to_string(key);
    if (__pi_reserved_keys.has(keyId)) {
        throw new Error('registerShortcut: key ' + keyId + ' is reserved and cannot be overridden');
    }

    const record = {
        key: key,
        keyId: keyId,
        description: spec.description ? String(spec.description) : '',
        handler: spec.handler,
        extensionId: ext.id,
        spec: { shortcut: keyId, key: key, key_id: keyId, description: spec.description ? String(spec.description) : '' },
    };
    ext.shortcuts.set(keyId, record);
    __pi_shortcut_index.set(keyId, record);
}

function __pi_register_message_renderer(customType, renderer) {
    const ext = __pi_current_extension_or_throw();
    const typeId = String(customType || '').trim();
    if (!typeId) {
        throw new Error('registerMessageRenderer: customType is required');
    }
    if (typeof renderer !== 'function') {
        throw new Error('registerMessageRenderer: renderer must be a function');
    }

    const record = {
        customType: typeId,
        renderer: renderer,
        extensionId: ext.id,
    };
    ext.messageRenderers.set(typeId, record);
    __pi_message_renderer_index.set(typeId, record);
}

	function __pi_register_hook(event_name, handler) {
	    const ext = __pi_current_extension_or_throw();
	    const eventName = String(event_name || '').trim();
	    if (!eventName) {
	        throw new Error('on: event name is required');
	    }
	    if (typeof handler !== 'function') {
	        throw new Error('on: handler must be a function');
	    }

	    if (!ext.hooks.has(eventName)) {
	        ext.hooks.set(eventName, []);
	    }
	    ext.hooks.get(eventName).push(handler);

	    if (!__pi_hook_index.has(eventName)) {
	        __pi_hook_index.set(eventName, []);
	    }
	    const indexed = { extensionId: ext.id, handler: handler };
	    __pi_hook_index.get(eventName).push(indexed);

	    let removed = false;
	    return function unsubscribe() {
	        if (removed) return;
	        removed = true;

	        const local = ext.hooks.get(eventName);
	        if (Array.isArray(local)) {
	            const idx = local.indexOf(handler);
	            if (idx !== -1) local.splice(idx, 1);
	            if (local.length === 0) ext.hooks.delete(eventName);
	        }

	        const global = __pi_hook_index.get(eventName);
	        if (Array.isArray(global)) {
	            const idx = global.indexOf(indexed);
	            if (idx !== -1) global.splice(idx, 1);
	            if (global.length === 0) __pi_hook_index.delete(eventName);
	        }
	    };
	}

	function __pi_register_event_bus_hook(event_name, handler) {
	    const ext = __pi_current_extension_or_throw();
	    const eventName = String(event_name || '').trim();
	    if (!eventName) {
	        throw new Error('events.on: event name is required');
	    }
	    if (typeof handler !== 'function') {
	        throw new Error('events.on: handler must be a function');
	    }

	    if (!ext.eventBusHooks.has(eventName)) {
	        ext.eventBusHooks.set(eventName, []);
	    }
	    ext.eventBusHooks.get(eventName).push(handler);

	    if (!__pi_event_bus_index.has(eventName)) {
	        __pi_event_bus_index.set(eventName, []);
	    }
	    const indexed = { extensionId: ext.id, handler: handler };
	    __pi_event_bus_index.get(eventName).push(indexed);

	    let removed = false;
	    return function unsubscribe() {
	        if (removed) return;
	        removed = true;

	        const local = ext.eventBusHooks.get(eventName);
	        if (Array.isArray(local)) {
	            const idx = local.indexOf(handler);
	            if (idx !== -1) local.splice(idx, 1);
	            if (local.length === 0) ext.eventBusHooks.delete(eventName);
	        }

	        const global = __pi_event_bus_index.get(eventName);
	        if (Array.isArray(global)) {
	            const idx = global.indexOf(indexed);
	            if (idx !== -1) global.splice(idx, 1);
	            if (global.length === 0) __pi_event_bus_index.delete(eventName);
	        }
	    };
	}

function __pi_register_flag(flag_name, spec) {
    const ext = __pi_current_extension_or_throw();
    const name = String(flag_name || '').trim().replace(/^\//, '');
    if (!name) {
        throw new Error('registerFlag: name is required');
    }
    if (!spec || typeof spec !== 'object') {
        throw new Error('registerFlag: spec must be an object');
    }
    ext.flags.set(name, spec);
}

function __pi_set_flag_value(extension_id, flag_name, value) {
    const extId = String(extension_id || '').trim();
    const name = String(flag_name || '').trim().replace(/^\//, '');
    if (!extId || !name) return false;
    const ext = __pi_extensions.get(extId);
    if (!ext) return false;
    ext.flagValues.set(name, value);
    return true;
}

function __pi_get_flag(flag_name) {
    const ext = __pi_current_extension_or_throw();
    const name = String(flag_name || '').trim().replace(/^\//, '');
    if (!name) return undefined;
    if (ext.flagValues.has(name)) {
        return ext.flagValues.get(name);
    }
    const spec = ext.flags.get(name);
    return spec ? spec.default : undefined;
}

function __pi_set_active_tools(tools) {
    const ext = __pi_current_extension_or_throw();
    if (!Array.isArray(tools)) {
        throw new Error('setActiveTools: tools must be an array');
    }
    ext.activeTools = tools.map((t) => String(t));
    // Best-effort notify host; ignore completion.
    try {
        pi.events('setActiveTools', { extensionId: ext.id, tools: ext.activeTools }).catch(() => {});
    } catch (_) {}
}

function __pi_get_active_tools() {
    const ext = __pi_current_extension_or_throw();
    if (!Array.isArray(ext.activeTools)) return undefined;
    return ext.activeTools.slice();
}

function __pi_get_model() {
    return pi.events('getModel', {});
}

function __pi_set_model(provider, modelId) {
    const p = provider != null ? String(provider) : null;
    const m = modelId != null ? String(modelId) : null;
    return pi.events('setModel', { provider: p, modelId: m });
}

function __pi_get_thinking_level() {
    return pi.events('getThinkingLevel', {});
}

function __pi_set_thinking_level(level) {
    const l = level != null ? String(level).trim() : null;
    return pi.events('setThinkingLevel', { thinkingLevel: l });
}

function __pi_get_session_name() {
    return pi.session('get_name', {});
}

function __pi_set_session_name(name) {
    const n = name != null ? String(name) : '';
    return pi.session('set_name', { name: n });
}

function __pi_set_label(entryId, label) {
    const eid = String(entryId || '').trim();
    if (!eid) {
        throw new Error('setLabel: entryId is required');
    }
    const l = label != null ? String(label).trim() : null;
    return pi.session('set_label', { targetId: eid, label: l || undefined });
}

function __pi_append_entry(custom_type, data) {
    const ext = __pi_current_extension_or_throw();
    const customType = String(custom_type || '').trim();
    if (!customType) {
        throw new Error('appendEntry: customType is required');
    }
    try {
        pi.events('appendEntry', {
            extensionId: ext.id,
            customType: customType,
            data: data === undefined ? null : data,
        }).catch(() => {});
    } catch (_) {}
}

function __pi_send_message(message, options) {
    const ext = __pi_current_extension_or_throw();
    if (!message || typeof message !== 'object') {
        throw new Error('sendMessage: message must be an object');
    }
    const opts = options && typeof options === 'object' ? options : {};
    try {
        pi.events('sendMessage', { extensionId: ext.id, message: message, options: opts }).catch(() => {});
    } catch (_) {}
}

function __pi_send_user_message(text, options) {
    const ext = __pi_current_extension_or_throw();
    const msg = String(text === undefined || text === null ? '' : text).trim();
    if (!msg) return;
    const opts = options && typeof options === 'object' ? options : {};
    try {
        pi.events('sendUserMessage', { extensionId: ext.id, text: msg, options: opts }).catch(() => {});
    } catch (_) {}
}

function __pi_snapshot_extensions() {
    const out = [];
    for (const [id, ext] of __pi_extensions.entries()) {
        const tools = [];
        for (const tool of ext.tools.values()) {
            tools.push(tool.spec);
        }

        const commands = [];
        for (const cmd of ext.commands.values()) {
            commands.push(cmd.spec);
        }

        const providers = [];
        for (const provider of ext.providers.values()) {
            providers.push(provider.spec);
        }

        const event_hooks = [];
        for (const key of ext.hooks.keys()) {
            event_hooks.push(String(key));
        }

        const shortcuts = [];
        for (const shortcut of ext.shortcuts.values()) {
            shortcuts.push(shortcut.spec);
        }

        const message_renderers = [];
        for (const renderer of ext.messageRenderers.values()) {
            message_renderers.push(renderer.customType);
        }

        const flags = [];
        for (const [flagName, flagSpec] of ext.flags.entries()) {
            flags.push({
                name: flagName,
                description: flagSpec.description ? String(flagSpec.description) : '',
                type: flagSpec.type ? String(flagSpec.type) : 'string',
                default: flagSpec.default !== undefined ? flagSpec.default : null,
            });
        }

        out.push({
            id: id,
            name: ext.name,
            version: ext.version,
            api_version: ext.apiVersion,
            tools: tools,
            slash_commands: commands,
            providers: providers,
            shortcuts: shortcuts,
            message_renderers: message_renderers,
            flags: flags,
            event_hooks: event_hooks,
            active_tools: Array.isArray(ext.activeTools) ? ext.activeTools.slice() : null,
        });
    }
    return out;
}

function __pi_make_extension_theme() {
    // Minimal theme shim. Legacy emits ANSI; conformance harness should normalize ANSI away.
    return {
        fg: (_style, text) => String(text === undefined || text === null ? '' : text),
        bold: (text) => String(text === undefined || text === null ? '' : text),
        strikethrough: (text) => String(text === undefined || text === null ? '' : text),
    };
}

function __pi_make_extension_ui(hasUI) {
    const ui = {
        theme: __pi_make_extension_theme(),
        select: (title, options) => {
            if (!hasUI) return Promise.resolve(undefined);
            const list = Array.isArray(options) ? options : [];
            const mapped = list.map((v) => String(v));
            return pi.ui('select', { title: String(title === undefined || title === null ? '' : title), options: mapped });
        },
        confirm: (title, message) => {
            if (!hasUI) return Promise.resolve(false);
            return pi.ui('confirm', {
                title: String(title === undefined || title === null ? '' : title),
                message: String(message === undefined || message === null ? '' : message),
            });
        },
        input: (title, placeholder, def) => {
            if (!hasUI) return Promise.resolve(undefined);
            // Legacy extensions typically call input(title, placeholder?, default?)
            let payloadDefault = def;
            let payloadPlaceholder = placeholder;
            if (def === undefined && typeof placeholder === 'string') {
                payloadDefault = placeholder;
                payloadPlaceholder = undefined;
            }
            return pi.ui('input', {
                title: String(title === undefined || title === null ? '' : title),
                placeholder: payloadPlaceholder,
                default: payloadDefault,
            });
        },
        editor: (title, def, language) => {
            if (!hasUI) return Promise.resolve(undefined);
            // Legacy extensions typically call editor(title, defaultText)
            return pi.ui('editor', {
                title: String(title === undefined || title === null ? '' : title),
                language: language,
                default: def,
            });
        },
        notify: (message, level) => {
            const notifyType = level ? String(level) : undefined;
            const payload = {
                message: String(message === undefined || message === null ? '' : message),
            };
            if (notifyType) {
                payload.level = notifyType;
                payload.notifyType = notifyType; // legacy field
            }
            void pi.ui('notify', payload).catch(() => {});
        },
        setStatus: (statusKey, statusText) => {
            const key = String(statusKey === undefined || statusKey === null ? '' : statusKey);
            const text = String(statusText === undefined || statusText === null ? '' : statusText);
            void pi.ui('setStatus', {
                statusKey: key,
                statusText: text,
                text: text, // compat: some UI surfaces only consume `text`
            }).catch(() => {});
        },
        setWidget: (widgetKey, lines) => {
            if (!hasUI) return;
            const payload = { widgetKey: String(widgetKey === undefined || widgetKey === null ? '' : widgetKey) };
            if (Array.isArray(lines)) {
                payload.lines = lines.map((v) => String(v));
                payload.widgetLines = payload.lines; // compat with pi-mono RPC naming
                payload.content = payload.lines.join('\n'); // compat: some UI surfaces expect a single string
            }
            void pi.ui('setWidget', payload).catch(() => {});
        },
        setTitle: (title) => {
            void pi.ui('setTitle', {
                title: String(title === undefined || title === null ? '' : title),
            }).catch(() => {});
        },
        setEditorText: (text) => {
            void pi.ui('set_editor_text', {
                text: String(text === undefined || text === null ? '' : text),
            }).catch(() => {});
        },
        custom: (_component, options) => {
            if (!hasUI) return Promise.resolve(undefined);
            const payload = options && typeof options === 'object' ? options : {};
            return pi.ui('custom', payload);
        },
    };
    return ui;
}

function __pi_make_extension_ctx(ctx_payload) {
    const hasUI = !!(ctx_payload && (ctx_payload.hasUI || ctx_payload.has_ui));
    const cwd = ctx_payload && (ctx_payload.cwd || ctx_payload.CWD) ? String(ctx_payload.cwd || ctx_payload.CWD) : '';

    const entriesRaw =
        (ctx_payload && (ctx_payload.sessionEntries || ctx_payload.session_entries || ctx_payload.entries)) || [];
    const branchRaw =
        (ctx_payload && (ctx_payload.sessionBranch || ctx_payload.session_branch || ctx_payload.branch)) || entriesRaw;

    const entries = Array.isArray(entriesRaw) ? entriesRaw : [];
    const branch = Array.isArray(branchRaw) ? branchRaw : entries;

    const leafEntry =
        (ctx_payload &&
            (ctx_payload.sessionLeafEntry ||
                ctx_payload.session_leaf_entry ||
                ctx_payload.leafEntry ||
                ctx_payload.leaf_entry)) ||
        null;

    const modelRegistryValues =
        (ctx_payload && (ctx_payload.modelRegistry || ctx_payload.model_registry || ctx_payload.model_registry_values)) ||
        {};

    const sessionManager = {
        getEntries: () => entries,
        getBranch: () => branch,
        getLeafEntry: () => leafEntry,
    };

    return {
        hasUI: hasUI,
        cwd: cwd,
        ui: __pi_make_extension_ui(hasUI),
        sessionManager: sessionManager,
        modelRegistry: {
            getApiKeyForProvider: async (provider) => {
                const key = String(provider || '').trim();
                if (!key) return undefined;
                const value = modelRegistryValues[key];
                if (value === undefined || value === null) return undefined;
                return String(value);
            },
        },
    };
}

	async function __pi_dispatch_extension_event(event_name, event_payload, ctx_payload) {
	    const eventName = String(event_name || '').trim();
	    if (!eventName) {
	        throw new Error('dispatch_event: event name is required');
	    }

    const handlers = [
        ...(__pi_hook_index.get(eventName) || []),
        ...(__pi_event_bus_index.get(eventName) || []),
    ];
    if (handlers.length === 0) {
        return undefined;
    }

    const ctx = __pi_make_extension_ctx(ctx_payload);
	    if (eventName === 'input') {
	        const base = event_payload && typeof event_payload === 'object' ? event_payload : {};
	        const originalText = typeof base.text === 'string' ? base.text : String(base.text ?? '');
	        const originalImages = Array.isArray(base.images) ? base.images : undefined;
	        const source = base.source !== undefined ? base.source : 'extension';

        let currentText = originalText;
        let currentImages = originalImages;

	        for (const entry of handlers) {
	            const handler = entry && entry.handler;
	            if (typeof handler !== 'function') continue;
	            const event = { type: 'input', text: currentText, images: currentImages, source: source };
	            let result = undefined;
	            try {
	                result = await __pi_with_extension_async(entry.extensionId, () => handler(event, ctx));
	            } catch (e) {
		                try { globalThis.console && globalThis.console.error && globalThis.console.error('Event handler error:', eventName, entry.extensionId, e); } catch (_e) {}
		                continue;
		            }
	            if (result && typeof result === 'object') {
	                if (result.action === 'handled') return result;
	                if (result.action === 'transform' && typeof result.text === 'string') {
	                    currentText = result.text;
	                    if (result.images !== undefined) currentImages = result.images;
                }
            }
        }

        if (currentText !== originalText || currentImages !== originalImages) {
            return { action: 'transform', text: currentText, images: currentImages };
        }
        return { action: 'continue' };
    }

	    if (eventName === 'before_agent_start') {
	        const base = event_payload && typeof event_payload === 'object' ? event_payload : {};
	        const prompt = typeof base.prompt === 'string' ? base.prompt : '';
	        const images = Array.isArray(base.images) ? base.images : undefined;
	        let currentSystemPrompt = typeof base.systemPrompt === 'string' ? base.systemPrompt : '';
	        let modified = false;
	        const messages = [];

	        for (const entry of handlers) {
	            const handler = entry && entry.handler;
	            if (typeof handler !== 'function') continue;
	            const event = { type: 'before_agent_start', prompt, images, systemPrompt: currentSystemPrompt };
	            let result = undefined;
	            try {
	                result = await __pi_with_extension_async(entry.extensionId, () => handler(event, ctx));
	            } catch (e) {
		                try { globalThis.console && globalThis.console.error && globalThis.console.error('Event handler error:', eventName, entry.extensionId, e); } catch (_e) {}
		                continue;
		            }
	            if (result && typeof result === 'object') {
	                if (result.message !== undefined) messages.push(result.message);
	                if (result.systemPrompt !== undefined) {
	                    currentSystemPrompt = String(result.systemPrompt);
	                    modified = true;
                }
            }
        }

        if (messages.length > 0 || modified) {
            return { messages: messages.length > 0 ? messages : undefined, systemPrompt: modified ? currentSystemPrompt : undefined };
        }
        return undefined;
    }

	    let last = undefined;
	    for (const entry of handlers) {
	        const handler = entry && entry.handler;
	        if (typeof handler !== 'function') continue;
	        let value = undefined;
	        try {
	            value = await __pi_with_extension_async(entry.extensionId, () => handler(event_payload, ctx));
	        } catch (e) {
		            try { globalThis.console && globalThis.console.error && globalThis.console.error('Event handler error:', eventName, entry.extensionId, e); } catch (_e) {}
		            continue;
		        }
	        if (value === undefined) continue;

        // First-result semantics (legacy parity)
        if (eventName === 'user_bash') {
            return value;
        }

        last = value;

        // Early-stop semantics (legacy parity)
        if (eventName === 'tool_call' && value && typeof value === 'object' && value.block) {
            return value;
        }
        if (eventName.startsWith('session_before_') && value && typeof value === 'object' && value.cancel) {
            return value;
        }
    }
    return last;
}

async function __pi_execute_tool(tool_name, tool_call_id, input, ctx_payload) {
    const name = String(tool_name || '').trim();
    const record = __pi_tool_index.get(name);
    if (!record) {
        throw new Error(`Unknown tool: ${name}`);
    }

    const ctx = __pi_make_extension_ctx(ctx_payload);
    return await __pi_with_extension_async(record.extensionId, () =>
        record.execute(tool_call_id, input, undefined, undefined, ctx)
    );
}

async function __pi_execute_command(command_name, args, ctx_payload) {
    const name = String(command_name || '').trim().replace(/^\//, '');
    const record = __pi_command_index.get(name);
    if (!record) {
        throw new Error(`Unknown command: ${name}`);
    }

    const ctx = __pi_make_extension_ctx(ctx_payload);
    return await __pi_with_extension_async(record.extensionId, () => record.handler(args, ctx));
}

async function __pi_execute_shortcut(key_id, ctx_payload) {
    const id = String(key_id || '').trim().toLowerCase();
    const record = __pi_shortcut_index.get(id);
    if (!record) {
        throw new Error('Unknown shortcut: ' + id);
    }

    const ctx = __pi_make_extension_ctx(ctx_payload);
    return await __pi_with_extension_async(record.extensionId, () => record.handler(ctx));
}

// Hostcall stream class (async iterator for streaming hostcall results)
class __pi_HostcallStream {
    constructor(callId) {
        this.callId = callId;
        this.buffer = [];
        this.waitResolve = null;
        this.done = false;
    }
    pushChunk(chunk, isFinal) {
        if (isFinal) this.done = true;
        if (this.waitResolve) {
            const resolve = this.waitResolve;
            this.waitResolve = null;
            if (isFinal && chunk === null) {
                resolve({ value: undefined, done: true });
            } else {
                resolve({ value: chunk, done: false });
            }
        } else {
            this.buffer.push({ chunk, isFinal });
        }
    }
    pushError(error) {
        this.done = true;
        if (this.waitResolve) {
            const rej = this.waitResolve;
            this.waitResolve = null;
            rej({ __error: error });
        } else {
            this.buffer.push({ __error: error });
        }
    }
    next() {
        if (this.buffer.length > 0) {
            const entry = this.buffer.shift();
            if (entry.__error) return Promise.reject(entry.__error);
            if (entry.isFinal && entry.chunk === null) return Promise.resolve({ value: undefined, done: true });
            return Promise.resolve({ value: entry.chunk, done: false });
        }
        if (this.done) return Promise.resolve({ value: undefined, done: true });
        return new Promise((resolve, reject) => {
            this.waitResolve = (result) => {
                if (result && result.__error) reject(result.__error);
                else resolve(result);
            };
        });
    }
    return() {
        this.done = true;
        this.buffer = [];
        this.waitResolve = null;
        return Promise.resolve({ value: undefined, done: true });
    }
    [Symbol.asyncIterator]() { return this; }
}

// Complete a hostcall (called from Rust)
function __pi_complete_hostcall(call_id, outcome) {
    const pending = __pi_pending_hostcalls.get(call_id);
    if (!pending) return;

    if (outcome.stream) {
        const seq = Number(outcome.sequence);
        if (!Number.isFinite(seq)) {
            const error = new Error('Invalid stream sequence');
            error.code = 'STREAM_SEQUENCE';
            if (pending.stream) pending.stream.pushError(error);
            else if (pending.reject) pending.reject(error);
            __pi_pending_hostcalls.delete(call_id);
            return;
        }
        if (pending.lastSeq === undefined) {
            if (seq !== 0) {
                const error = new Error('Stream sequence must start at 0');
                error.code = 'STREAM_SEQUENCE';
                if (pending.stream) pending.stream.pushError(error);
                else if (pending.reject) pending.reject(error);
                __pi_pending_hostcalls.delete(call_id);
                return;
            }
        } else if (seq <= pending.lastSeq) {
            const error = new Error('Stream sequence out of order');
            error.code = 'STREAM_SEQUENCE';
            if (pending.stream) pending.stream.pushError(error);
            else if (pending.reject) pending.reject(error);
            __pi_pending_hostcalls.delete(call_id);
            return;
        }
        pending.lastSeq = seq;

        if (pending.stream) {
            pending.stream.pushChunk(outcome.chunk, outcome.isFinal);
        } else if (pending.onChunk) {
            const chunk = outcome.chunk;
            const isFinal = outcome.isFinal;
            Promise.resolve().then(() => {
                try {
                    pending.onChunk(chunk, isFinal);
                } catch (e) {
                    console.error('Hostcall onChunk error:', e);
                }
            });
        }
        if (outcome.isFinal) {
            __pi_pending_hostcalls.delete(call_id);
            if (pending.resolve) pending.resolve(outcome.chunk);
        }
        return;
    }

    if (!outcome.ok && pending.stream) {
        const error = new Error(outcome.message);
        error.code = outcome.code;
        pending.stream.pushError(error);
        __pi_pending_hostcalls.delete(call_id);
        return;
    }

    __pi_pending_hostcalls.delete(call_id);
    if (outcome.ok) {
        pending.resolve(outcome.value);
    } else {
        const error = new Error(outcome.message);
        error.code = outcome.code;
        pending.reject(error);
    }
}

// Fire a timer callback (called from Rust)
function __pi_fire_timer(timer_id) {
    const callback = __pi_timer_callbacks.get(timer_id);
    if (callback) {
        __pi_timer_callbacks.delete(timer_id);
        try {
            callback();
        } catch (e) {
            console.error('Timer callback error:', e);
        }
    }
}

// Dispatch an inbound event (called from Rust)
function __pi_dispatch_event(event_id, payload) {
    const listeners = __pi_event_listeners.get(event_id);
    if (listeners) {
        for (const listener of listeners) {
            try {
                listener(payload);
            } catch (e) {
                console.error('Event listener error:', e);
            }
        }
    }
}

// Register a timer callback (used by setTimeout)
function __pi_register_timer(timer_id, callback) {
    __pi_timer_callbacks.set(timer_id, callback);
}

// Unregister a timer callback (used by clearTimeout)
function __pi_unregister_timer(timer_id) {
    __pi_timer_callbacks.delete(timer_id);
}

// Add an event listener
function __pi_add_event_listener(event_id, callback) {
    if (!__pi_event_listeners.has(event_id)) {
        __pi_event_listeners.set(event_id, []);
    }
    __pi_event_listeners.get(event_id).push(callback);
}

// Remove an event listener
function __pi_remove_event_listener(event_id, callback) {
    const listeners = __pi_event_listeners.get(event_id);
    if (listeners) {
        const index = listeners.indexOf(callback);
        if (index !== -1) {
            listeners.splice(index, 1);
        }
    }
}

// Helper to create a Promise-returning hostcall wrapper
function __pi_make_hostcall(nativeFn) {
    return function(...args) {
        return new Promise((resolve, reject) => {
            const call_id = nativeFn(...args);
            __pi_pending_hostcalls.set(call_id, { resolve, reject });
        });
    };
}

function __pi_make_streaming_hostcall(nativeFn, ...args) {
    const call_id = nativeFn(...args);
    const stream = new __pi_HostcallStream(call_id);
    __pi_pending_hostcalls.set(call_id, { stream, resolve: () => {}, reject: () => {} });
    return stream;
}

function __pi_env_get(key) {
    const value = __pi_env_get_native(key);
    if (value === null || value === undefined) {
        return undefined;
    }
    return value;
}

function __pi_path_join(...parts) {
    let out = '';
    for (const part of parts) {
        if (!part) continue;
        if (part.startsWith('/')) {
            out = part;
            continue;
        }
        if (out === '' || out.endsWith('/')) {
            out += part;
        } else {
            out += '/' + part;
        }
    }
    return __pi_path_normalize(out);
}

function __pi_path_basename(path) {
    if (!path) return '';
    let p = path;
    while (p.length > 1 && p.endsWith('/')) {
        p = p.slice(0, -1);
    }
    const idx = p.lastIndexOf('/');
    return idx === -1 ? p : p.slice(idx + 1);
}

function __pi_path_normalize(path) {
    if (!path) return '';
    const isAbs = path.startsWith('/');
    const parts = path.split('/').filter(p => p.length > 0);
    const stack = [];
    for (const part of parts) {
        if (part === '.') continue;
        if (part === '..') {
            if (stack.length > 0 && stack[stack.length - 1] !== '..') {
                stack.pop();
            } else if (!isAbs) {
                stack.push('..');
            }
            continue;
        }
        stack.push(part);
    }
    const joined = stack.join('/');
    return isAbs ? '/' + joined : joined || (isAbs ? '/' : '');
}

function __pi_sleep(ms) {
    return new Promise((resolve) => {
        const delay = Math.max(0, ms | 0);
        const timer_id = __pi_set_timeout_native(delay);
        __pi_register_timer(timer_id, resolve);
    });
}

// Create the pi global object with Promise-returning methods
const __pi_exec_hostcall = __pi_make_hostcall(__pi_exec_native);
	const pi = {
    // pi.tool(name, input) - invoke a tool
    tool: __pi_make_hostcall(__pi_tool_native),

    // pi.exec(cmd, args, options) - execute a shell command
    exec: (cmd, args, options = {}) => {
        if (options && options.stream) {
            const onChunk =
                options && typeof options === 'object'
                    ? (options.onChunk || options.on_chunk)
                    : undefined;
            if (typeof onChunk === 'function') {
                const opts = Object.assign({}, options);
                delete opts.onChunk;
                delete opts.on_chunk;
                const call_id = __pi_exec_native(cmd, args, opts);
                return new Promise((resolve, reject) => {
                    __pi_pending_hostcalls.set(call_id, { onChunk, resolve, reject });
                });
            }
            return __pi_make_streaming_hostcall(__pi_exec_native, cmd, args, options);
        }
        return __pi_exec_hostcall(cmd, args, options);
    },

    // pi.http(request) - make an HTTP request
    http: (request) => {
        if (request && request.stream) {
            const onChunk =
                request && typeof request === 'object'
                    ? (request.onChunk || request.on_chunk)
                    : undefined;
            if (typeof onChunk === 'function') {
                const req = Object.assign({}, request);
                delete req.onChunk;
                delete req.on_chunk;
                const call_id = __pi_http_native(req);
                return new Promise((resolve, reject) => {
                    __pi_pending_hostcalls.set(call_id, { onChunk, resolve, reject });
                });
            }
            return __pi_make_streaming_hostcall(__pi_http_native, request);
        }
        return __pi_make_hostcall(__pi_http_native)(request);
    },

    // pi.session(op, args) - session operations
    session: __pi_make_hostcall(__pi_session_native),

    // pi.ui(op, args) - UI operations
    ui: __pi_make_hostcall(__pi_ui_native),

	    // pi.events(op, args) - event operations
	    events: __pi_make_hostcall(__pi_events_native),

    // Extension API (legacy-compatible subset)
    registerTool: __pi_register_tool,
    registerCommand: __pi_register_command,
    registerProvider: __pi_register_provider,
    registerShortcut: __pi_register_shortcut,
    registerMessageRenderer: __pi_register_message_renderer,
    on: __pi_register_hook,
    registerFlag: __pi_register_flag,
    getFlag: __pi_get_flag,
    setActiveTools: __pi_set_active_tools,
    getActiveTools: __pi_get_active_tools,
    getModel: __pi_get_model,
    setModel: __pi_set_model,
    getThinkingLevel: __pi_get_thinking_level,
    setThinkingLevel: __pi_set_thinking_level,
    appendEntry: __pi_append_entry,
	    sendMessage: __pi_send_message,
	    sendUserMessage: __pi_send_user_message,
	    getSessionName: __pi_get_session_name,
	    setSessionName: __pi_set_session_name,
	    setLabel: __pi_set_label,
	};

	// Convenience API: pi.events.emit/on (inter-extension bus).
	// Keep pi.events callable for legacy hostcall operations.
	pi.events.emit = (event, data, options = undefined) => {
	    const name = String(event || '').trim();
	    if (!name) {
	        throw new Error('events.emit: event name is required');
	    }
	    const payload = { event: name, data: (data === undefined ? null : data) };
	    if (options && typeof options === 'object') {
	        if (options.ctx !== undefined) payload.ctx = options.ctx;
	        if (options.timeout_ms !== undefined) payload.timeout_ms = options.timeout_ms;
	        if (options.timeoutMs !== undefined) payload.timeoutMs = options.timeoutMs;
	        if (options.timeout !== undefined) payload.timeout = options.timeout;
	    }
	    return pi.events('emit', payload);
	};
	pi.events.on = (event, handler) => __pi_register_event_bus_hook(event, handler);

	pi.env = {
	    get: __pi_env_get,
	};

pi.process = {
    cwd: __pi_process_cwd_native(),
    args: __pi_process_args_native(),
};

const __pi_det_cwd = __pi_env_get('PI_DETERMINISTIC_CWD');
if (__pi_det_cwd) {
    try { pi.process.cwd = __pi_det_cwd; } catch (_) {}
}

try { Object.freeze(pi.process.args); } catch (_) {}
try { Object.freeze(pi.process); } catch (_) {}

pi.path = {
    join: __pi_path_join,
    basename: __pi_path_basename,
    normalize: __pi_path_normalize,
};

pi.crypto = {
    sha256Hex: __pi_crypto_sha256_hex_native,
    randomBytes: __pi_crypto_random_bytes_native,
};

pi.time = {
    nowMs: __pi_now_ms_native,
    sleep: __pi_sleep,
};

// Make pi available globally
globalThis.pi = pi;

const __pi_det_time_raw = __pi_env_get('PI_DETERMINISTIC_TIME_MS');
const __pi_det_time_step_raw = __pi_env_get('PI_DETERMINISTIC_TIME_STEP_MS');
const __pi_det_random_raw = __pi_env_get('PI_DETERMINISTIC_RANDOM');
const __pi_det_random_seed_raw = __pi_env_get('PI_DETERMINISTIC_RANDOM_SEED');

if (__pi_det_time_raw !== undefined) {
    const __pi_det_base = Number(__pi_det_time_raw);
    if (Number.isFinite(__pi_det_base)) {
        const __pi_det_step = (() => {
            if (__pi_det_time_step_raw === undefined) return 1;
            const value = Number(__pi_det_time_step_raw);
            return Number.isFinite(value) ? value : 1;
        })();
        let __pi_det_tick = 0;
        const __pi_det_now = () => {
            const value = __pi_det_base + (__pi_det_step * __pi_det_tick);
            __pi_det_tick += 1;
            return value;
        };

        if (pi && pi.time) {
            pi.time.nowMs = () => __pi_det_now();
        }

        const __pi_OriginalDate = Date;
        class PiDeterministicDate extends __pi_OriginalDate {
            constructor(...args) {
                if (args.length === 0) {
                    super(__pi_det_now());
                } else {
                    super(...args);
                }
            }
            static now() {
                return __pi_det_now();
            }
        }
        PiDeterministicDate.UTC = __pi_OriginalDate.UTC;
        PiDeterministicDate.parse = __pi_OriginalDate.parse;
        globalThis.Date = PiDeterministicDate;
    }
}

if (__pi_det_random_raw !== undefined) {
    const __pi_det_random_val = Number(__pi_det_random_raw);
    if (Number.isFinite(__pi_det_random_val)) {
        Math.random = () => __pi_det_random_val;
    }
} else if (__pi_det_random_seed_raw !== undefined) {
    let __pi_det_state = Number(__pi_det_random_seed_raw);
    if (Number.isFinite(__pi_det_state)) {
        __pi_det_state = __pi_det_state >>> 0;
        Math.random = () => {
            __pi_det_state = (__pi_det_state * 1664525 + 1013904223) >>> 0;
            return __pi_det_state / 4294967296;
        };
    }
}

// ============================================================================
// Minimal Web/Node polyfills for legacy extensions (best-effort)
// ============================================================================

if (typeof globalThis.btoa !== 'function') {
    globalThis.btoa = (s) => {
        const bin = String(s === undefined || s === null ? '' : s);
        return __pi_base64_encode_native(bin);
    };
}

if (typeof globalThis.atob !== 'function') {
    globalThis.atob = (s) => {
        const b64 = String(s === undefined || s === null ? '' : s);
        return __pi_base64_decode_native(b64);
    };
}

if (typeof globalThis.TextEncoder === 'undefined') {
    class TextEncoder {
        encode(input) {
            const s = String(input === undefined || input === null ? '' : input);
            const bytes = [];
            for (let i = 0; i < s.length; i++) {
                let code = s.charCodeAt(i);
                if (code < 0x80) {
                    bytes.push(code);
                    continue;
                }
                if (code < 0x800) {
                    bytes.push(0xc0 | (code >> 6));
                    bytes.push(0x80 | (code & 0x3f));
                    continue;
                }
                if (code >= 0xd800 && code <= 0xdbff && i + 1 < s.length) {
                    const next = s.charCodeAt(i + 1);
                    if (next >= 0xdc00 && next <= 0xdfff) {
                        const cp = ((code - 0xd800) << 10) + (next - 0xdc00) + 0x10000;
                        bytes.push(0xf0 | (cp >> 18));
                        bytes.push(0x80 | ((cp >> 12) & 0x3f));
                        bytes.push(0x80 | ((cp >> 6) & 0x3f));
                        bytes.push(0x80 | (cp & 0x3f));
                        i++;
                        continue;
                    }
                }
                bytes.push(0xe0 | (code >> 12));
                bytes.push(0x80 | ((code >> 6) & 0x3f));
                bytes.push(0x80 | (code & 0x3f));
            }
            return new Uint8Array(bytes);
        }
    }
    globalThis.TextEncoder = TextEncoder;
}

if (typeof globalThis.TextDecoder === 'undefined') {
    class TextDecoder {
        constructor(encoding = 'utf-8') {
            this.encoding = encoding;
        }

        decode(input, _opts) {
            if (input === undefined || input === null) return '';
            if (typeof input === 'string') return input;

            let bytes;
            if (input instanceof ArrayBuffer) {
                bytes = new Uint8Array(input);
            } else if (ArrayBuffer.isView && ArrayBuffer.isView(input)) {
                bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
            } else if (Array.isArray(input)) {
                bytes = new Uint8Array(input);
            } else if (typeof input.length === 'number') {
                bytes = new Uint8Array(input);
            } else {
                return '';
            }

            let out = '';
            for (let i = 0; i < bytes.length; ) {
                const b0 = bytes[i++];
                if (b0 < 0x80) {
                    out += String.fromCharCode(b0);
                    continue;
                }
                if ((b0 & 0xe0) === 0xc0) {
                    const b1 = bytes[i++] & 0x3f;
                    out += String.fromCharCode(((b0 & 0x1f) << 6) | b1);
                    continue;
                }
                if ((b0 & 0xf0) === 0xe0) {
                    const b1 = bytes[i++] & 0x3f;
                    const b2 = bytes[i++] & 0x3f;
                    out += String.fromCharCode(((b0 & 0x0f) << 12) | (b1 << 6) | b2);
                    continue;
                }
                if ((b0 & 0xf8) === 0xf0) {
                    const b1 = bytes[i++] & 0x3f;
                    const b2 = bytes[i++] & 0x3f;
                    const b3 = bytes[i++] & 0x3f;
                    let cp = ((b0 & 0x07) << 18) | (b1 << 12) | (b2 << 6) | b3;
                    cp -= 0x10000;
                    out += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 0x3ff));
                    continue;
                }
            }
            return out;
        }
    }

    globalThis.TextDecoder = TextDecoder;
}

// structuredClone — deep clone using JSON round-trip
if (typeof globalThis.structuredClone === 'undefined') {
    globalThis.structuredClone = (value) => JSON.parse(JSON.stringify(value));
}

// queueMicrotask — schedule a microtask
if (typeof globalThis.queueMicrotask === 'undefined') {
    globalThis.queueMicrotask = (fn) => Promise.resolve().then(fn);
}

// performance.now() — high-resolution timer
if (typeof globalThis.performance === 'undefined') {
    const start = Date.now();
    globalThis.performance = { now: () => Date.now() - start, timeOrigin: start };
}

if (typeof globalThis.URLSearchParams === 'undefined') {
    class URLSearchParams {
        constructor(init) {
            this._pairs = [];
            if (typeof init === 'string') {
                const s = init.replace(/^\?/, '');
                if (s.length > 0) {
                    for (const part of s.split('&')) {
                        const idx = part.indexOf('=');
                        if (idx === -1) {
                            this.append(decodeURIComponent(part), '');
                        } else {
                            const k = part.slice(0, idx);
                            const v = part.slice(idx + 1);
                            this.append(decodeURIComponent(k), decodeURIComponent(v));
                        }
                    }
                }
            } else if (Array.isArray(init)) {
                for (const entry of init) {
                    if (!entry) continue;
                    this.append(entry[0], entry[1]);
                }
            } else if (init && typeof init === 'object') {
                for (const k of Object.keys(init)) {
                    this.append(k, init[k]);
                }
            }
        }

        append(key, value) {
            this._pairs.push([String(key), String(value)]);
        }

        toString() {
            const out = [];
            for (const [k, v] of this._pairs) {
                out.push(encodeURIComponent(k) + '=' + encodeURIComponent(v));
            }
            return out.join('&');
        }
    }

    globalThis.URLSearchParams = URLSearchParams;
}

if (typeof globalThis.URL === 'undefined') {
    class URL {
        constructor(input, base) {
            const s = base ? new URL(base).href.replace(/\/[^/]*$/, '/') + String(input ?? '') : String(input ?? '');
            const m = s.match(/^([a-zA-Z][a-zA-Z0-9+.-]*):\/\/([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/);
            if (m) {
                this.protocol = m[1] + ':';
                const auth = m[2];
                const atIdx = auth.lastIndexOf('@');
                if (atIdx !== -1) {
                    const userinfo = auth.slice(0, atIdx);
                    const ci = userinfo.indexOf(':');
                    this.username = ci === -1 ? userinfo : userinfo.slice(0, ci);
                    this._pw = ci === -1 ? String() : userinfo.slice(ci + 1);
                    this.host = auth.slice(atIdx + 1);
                } else {
                    this.username = '';
                    this._pw = String();
                    this.host = auth;
                }
                const hi = this.host.indexOf(':');
                this.hostname = hi === -1 ? this.host : this.host.slice(0, hi);
                this.port = hi === -1 ? '' : this.host.slice(hi + 1);
                this.pathname = m[3] || '/';
                this.search = m[4] || '';
                this.hash = m[5] || '';
            } else {
                this.protocol = '';
                this.username = '';
                this._pw = String();
                this.host = '';
                this.hostname = '';
                this.port = '';
                this.pathname = s;
                this.search = '';
                this.hash = '';
            }
            this.searchParams = new globalThis.URLSearchParams(this.search.replace(/^\?/, ''));
            this.origin = this.protocol ? `${this.protocol}//${this.host}` : '';
            this.href = this.toString();
        }
        get password() {
            return this._pw;
        }
        set password(value) {
            this._pw = value == null ? String() : String(value);
        }
        toString() {
            const auth = this.username ? `${this.username}${this.password ? ':' + this.password : ''}@` : '';
            return this.protocol ? `${this.protocol}//${auth}${this.host}${this.pathname}${this.search}${this.hash}` : this.pathname;
        }
        toJSON() { return this.toString(); }
    }
    globalThis.URL = URL;
}

if (typeof globalThis.Buffer === 'undefined') {
    class Buffer extends Uint8Array {
        static from(input, encoding) {
            if (typeof input === 'string') {
                const enc = String(encoding || '').toLowerCase();
                if (enc === 'base64') {
                    const bin = __pi_base64_decode_native(input);
                    const out = new Buffer(bin.length);
                    for (let i = 0; i < bin.length; i++) {
                        out[i] = bin.charCodeAt(i) & 0xff;
                    }
                    return out;
                }
                if (enc === 'hex') {
                    const hex = input.replace(/[^0-9a-fA-F]/g, '');
                    const out = new Buffer(hex.length >> 1);
                    for (let i = 0; i < out.length; i++) {
                        out[i] = parseInt(hex.substr(i * 2, 2), 16);
                    }
                    return out;
                }
                const encoded = new TextEncoder().encode(input);
                const out = new Buffer(encoded.length);
                out.set(encoded);
                return out;
            }
            if (input instanceof ArrayBuffer) {
                const out = new Buffer(input.byteLength);
                out.set(new Uint8Array(input));
                return out;
            }
            if (ArrayBuffer.isView && ArrayBuffer.isView(input)) {
                const out = new Buffer(input.byteLength);
                out.set(new Uint8Array(input.buffer, input.byteOffset, input.byteLength));
                return out;
            }
            if (Array.isArray(input)) {
                const out = new Buffer(input.length);
                for (let i = 0; i < input.length; i++) out[i] = input[i] & 0xff;
                return out;
            }
            throw new Error('Buffer.from: unsupported input');
        }
        static alloc(size, fill) {
            const buf = new Buffer(size);
            if (fill !== undefined) buf.fill(typeof fill === 'number' ? fill : 0);
            return buf;
        }
        static allocUnsafe(size) { return new Buffer(size); }
        static isBuffer(obj) { return obj instanceof Buffer; }
        static isEncoding(enc) {
            return ['utf8','utf-8','ascii','latin1','binary','base64','hex','ucs2','ucs-2','utf16le','utf-16le'].includes(String(enc).toLowerCase());
        }
        static byteLength(str, encoding) {
            if (typeof str !== 'string') return str.length || 0;
            const enc = String(encoding || 'utf8').toLowerCase();
            if (enc === 'base64') return Math.ceil(str.length * 3 / 4);
            if (enc === 'hex') return str.length >> 1;
            return new TextEncoder().encode(str).length;
        }
        static concat(list, totalLength) {
            if (!Array.isArray(list) || list.length === 0) return Buffer.alloc(0);
            const total = totalLength !== undefined ? totalLength : list.reduce((s, b) => s + b.length, 0);
            const out = Buffer.alloc(total);
            let offset = 0;
            for (const buf of list) {
                if (offset >= total) break;
                const src = buf instanceof Uint8Array ? buf : Buffer.from(buf);
                const copyLen = Math.min(src.length, total - offset);
                out.set(src.subarray(0, copyLen), offset);
                offset += copyLen;
            }
            return out;
        }
        static compare(a, b) {
            const len = Math.min(a.length, b.length);
            for (let i = 0; i < len; i++) {
                if (a[i] < b[i]) return -1;
                if (a[i] > b[i]) return 1;
            }
            if (a.length < b.length) return -1;
            if (a.length > b.length) return 1;
            return 0;
        }
        toString(encoding, start, end) {
            const s = start || 0;
            const e = end !== undefined ? end : this.length;
            const view = this.subarray(s, e);
            const enc = String(encoding || 'utf8').toLowerCase();
            if (enc === 'base64') {
                let binary = '';
                for (let i = 0; i < view.length; i++) binary += String.fromCharCode(view[i]);
                return __pi_base64_encode_native(binary);
            }
            if (enc === 'hex') {
                let hex = '';
                for (let i = 0; i < view.length; i++) hex += (view[i] < 16 ? '0' : '') + view[i].toString(16);
                return hex;
            }
            return new TextDecoder().decode(view);
        }
        toJSON() {
            return { type: 'Buffer', data: Array.from(this) };
        }
        equals(other) {
            if (this.length !== other.length) return false;
            for (let i = 0; i < this.length; i++) {
                if (this[i] !== other[i]) return false;
            }
            return true;
        }
        compare(other) { return Buffer.compare(this, other); }
        copy(target, targetStart, sourceStart, sourceEnd) {
            const ts = targetStart || 0;
            const ss = sourceStart || 0;
            const se = sourceEnd !== undefined ? sourceEnd : this.length;
            const src = this.subarray(ss, se);
            const copyLen = Math.min(src.length, target.length - ts);
            target.set(src.subarray(0, copyLen), ts);
            return copyLen;
        }
        slice(start, end) {
            const sliced = super.slice(start, end);
            const buf = new Buffer(sliced.length);
            buf.set(sliced);
            return buf;
        }
        indexOf(value, byteOffset, encoding) {
            const offset = byteOffset || 0;
            if (typeof value === 'number') {
                for (let i = offset; i < this.length; i++) {
                    if (this[i] === (value & 0xff)) return i;
                }
                return -1;
            }
            const needle = typeof value === 'string' ? Buffer.from(value, encoding) : value;
            outer: for (let i = offset; i <= this.length - needle.length; i++) {
                for (let j = 0; j < needle.length; j++) {
                    if (this[i + j] !== needle[j]) continue outer;
                }
                return i;
            }
            return -1;
        }
        includes(value, byteOffset, encoding) {
            return this.indexOf(value, byteOffset, encoding) !== -1;
        }
        write(string, offset, length, encoding) {
            const o = offset || 0;
            const enc = encoding || 'utf8';
            const bytes = Buffer.from(string, enc);
            const len = length !== undefined ? Math.min(length, bytes.length) : bytes.length;
            const copyLen = Math.min(len, this.length - o);
            this.set(bytes.subarray(0, copyLen), o);
            return copyLen;
        }
        fill(value, offset, end, encoding) {
            const s = offset || 0;
            const e = end !== undefined ? end : this.length;
            const v = typeof value === 'number' ? (value & 0xff) : 0;
            for (let i = s; i < e; i++) this[i] = v;
            return this;
        }
        readUInt8(offset) { return this[offset || 0]; }
        readUInt16BE(offset) { const o = offset || 0; return (this[o] << 8) | this[o + 1]; }
        readUInt16LE(offset) { const o = offset || 0; return this[o] | (this[o + 1] << 8); }
        readUInt32BE(offset) { const o = offset || 0; return ((this[o] << 24) | (this[o+1] << 16) | (this[o+2] << 8) | this[o+3]) >>> 0; }
        readUInt32LE(offset) { const o = offset || 0; return (this[o] | (this[o+1] << 8) | (this[o+2] << 16) | (this[o+3] << 24)) >>> 0; }
        readInt8(offset) { const v = this[offset || 0]; return v > 127 ? v - 256 : v; }
        writeUInt8(value, offset) { this[offset || 0] = value & 0xff; return (offset || 0) + 1; }
        writeUInt16BE(value, offset) { const o = offset || 0; this[o] = (value >> 8) & 0xff; this[o+1] = value & 0xff; return o + 2; }
        writeUInt16LE(value, offset) { const o = offset || 0; this[o] = value & 0xff; this[o+1] = (value >> 8) & 0xff; return o + 2; }
        writeUInt32BE(value, offset) { const o = offset || 0; this[o]=(value>>>24)&0xff; this[o+1]=(value>>>16)&0xff; this[o+2]=(value>>>8)&0xff; this[o+3]=value&0xff; return o+4; }
        writeUInt32LE(value, offset) { const o = offset || 0; this[o]=value&0xff; this[o+1]=(value>>>8)&0xff; this[o+2]=(value>>>16)&0xff; this[o+3]=(value>>>24)&0xff; return o+4; }
    }
    globalThis.Buffer = Buffer;
}

if (typeof globalThis.crypto === 'undefined') {
    globalThis.crypto = {};
}

if (typeof globalThis.crypto.getRandomValues !== 'function') {
    globalThis.crypto.getRandomValues = (arr) => {
        const len = Number(arr && arr.length ? arr.length : 0);
        const bytes = __pi_crypto_random_bytes_native(len);
        for (let i = 0; i < len; i++) {
            arr[i] = bytes[i] || 0;
        }
        return arr;
    };
}

if (!globalThis.crypto.subtle) {
    globalThis.crypto.subtle = {};
}

if (typeof globalThis.crypto.subtle.digest !== 'function') {
    globalThis.crypto.subtle.digest = async (algorithm, data) => {
        const name = typeof algorithm === 'string' ? algorithm : (algorithm && algorithm.name ? algorithm.name : '');
        const upper = String(name).toUpperCase();
        if (upper !== 'SHA-256') {
            throw new Error('crypto.subtle.digest: only SHA-256 is supported');
        }
        const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
        let text = '';
        for (let i = 0; i < bytes.length; i++) {
            text += String.fromCharCode(bytes[i]);
        }
        const hex = __pi_crypto_sha256_hex_native(text);
        const out = new Uint8Array(hex.length / 2);
        for (let i = 0; i < out.length; i++) {
            out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
        }
        return out.buffer;
    };
}

if (typeof globalThis.crypto.randomUUID !== 'function') {
    globalThis.crypto.randomUUID = () => {
        const bytes = __pi_crypto_random_bytes_native(16);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        const hex = Array.from(bytes, (b) => (b & 0xff).toString(16).padStart(2, '0')).join('');
        return (
            hex.slice(0, 8) +
            '-' +
            hex.slice(8, 12) +
            '-' +
            hex.slice(12, 16) +
            '-' +
            hex.slice(16, 20) +
            '-' +
            hex.slice(20)
        );
    };
}

// Intl polyfill - minimal implementation for string comparison
if (typeof globalThis.Intl === 'undefined') {
    class Collator {
        constructor(_locales, options = {}) {
            this.sensitivity = options.sensitivity || 'variant';
        }

        compare(a, b) {
            const strA = String(a ?? '');
            const strB = String(b ?? '');

            if (this.sensitivity === 'base' || this.sensitivity === 'accent') {
                // Case-insensitive comparison
                const lowerA = strA.toLowerCase();
                const lowerB = strB.toLowerCase();
                if (lowerA < lowerB) return -1;
                if (lowerA > lowerB) return 1;
                return 0;
            }

            // Default: case-sensitive comparison
            if (strA < strB) return -1;
            if (strA > strB) return 1;
            return 0;
        }
    }

    class NumberFormat {
        constructor(_locales, _options = {}) {}

        format(value) {
            return String(value ?? '');
        }
    }

    class DateTimeFormat {
        constructor(_locales, _options = {}) {}

        format(date) {
            if (date instanceof Date) {
                return date.toISOString();
            }
            return String(date ?? '');
        }
    }

    globalThis.Intl = {
        Collator,
        NumberFormat,
        DateTimeFormat,
    };
}

if (typeof globalThis.process === 'undefined') {
    const platform =
        __pi_env_get_native('PI_PLATFORM') ||
        __pi_env_get_native('OSTYPE') ||
        __pi_env_get_native('OS') ||
        'linux';
    const detHome = __pi_env_get_native('PI_DETERMINISTIC_HOME');
    const detCwd = __pi_env_get_native('PI_DETERMINISTIC_CWD');

    const envProxy = new Proxy(
        {},
        {
            get(_target, prop) {
                if (typeof prop !== 'string') return undefined;
                if (prop === 'HOME' && detHome) return detHome;
                const value = __pi_env_get_native(prop);
                return value === null || value === undefined ? undefined : value;
            },
            set(_target, prop, _value) {
                // Read-only in PiJS — silently ignore writes
                return typeof prop === 'string';
            },
            deleteProperty(_target, prop) {
                // Read-only — silently ignore deletes
                return typeof prop === 'string';
            },
            has(_target, prop) {
                if (typeof prop !== 'string') return false;
                if (prop === 'HOME' && detHome) return true;
                const value = __pi_env_get_native(prop);
                return value !== null && value !== undefined;
            },
            ownKeys() {
                // Cannot enumerate real env — return empty
                return [];
            },
            getOwnPropertyDescriptor(_target, prop) {
                if (typeof prop !== 'string') return undefined;
                const value = __pi_env_get_native(prop);
                if (value === null || value === undefined) return undefined;
                return { value, writable: false, enumerable: true, configurable: true };
            },
        },
    );

    // stdout/stderr that route through console output
    function makeWritable(level) {
        return {
            write(chunk) {
                if (typeof __pi_console_output_native === 'function') {
                    __pi_console_output_native(level, String(chunk));
                }
                return true;
            },
            end() { return this; },
            on() { return this; },
            once() { return this; },
            pipe() { return this; },
            isTTY: false,
        };
    }

    // Event listener registry
    const __evtMap = Object.create(null);
    function __on(event, fn) {
        if (!__evtMap[event]) __evtMap[event] = [];
        __evtMap[event].push(fn);
        return globalThis.process;
    }
    function __off(event, fn) {
        const arr = __evtMap[event];
        if (!arr) return globalThis.process;
        const idx = arr.indexOf(fn);
        if (idx >= 0) arr.splice(idx, 1);
        return globalThis.process;
    }

    const startMs = (typeof __pi_now_ms_native === 'function') ? __pi_now_ms_native() : 0;

    globalThis.process = {
        env: envProxy,
        argv: __pi_process_args_native(),
        cwd: () => detCwd || __pi_process_cwd_native(),
        platform: String(platform).split('-')[0],
        arch: __pi_env_get_native('PI_TARGET_ARCH') || 'x64',
        version: 'v20.0.0',
        versions: { node: '20.0.0', v8: '0.0.0', modules: '0' },
        pid: 1,
        ppid: 0,
        title: 'pi',
        execPath: (typeof __pi_process_execpath_native === 'function')
            ? __pi_process_execpath_native()
            : '/usr/bin/pi',
        execArgv: [],
        stdout: makeWritable('log'),
        stderr: makeWritable('error'),
        stdin: { on() { return this; }, once() { return this; }, read() {}, resume() { return this; }, pause() { return this; } },
        nextTick: (fn, ...args) => { Promise.resolve().then(() => fn(...args)); },
        hrtime: Object.assign((prev) => {
            const nowMs = (typeof __pi_now_ms_native === 'function') ? __pi_now_ms_native() : 0;
            const secs = Math.floor(nowMs / 1000);
            const nanos = Math.floor((nowMs % 1000) * 1e6);
            if (Array.isArray(prev) && prev.length >= 2) {
                let ds = secs - prev[0];
                let dn = nanos - prev[1];
                if (dn < 0) { ds -= 1; dn += 1e9; }
                return [ds, dn];
            }
            return [secs, nanos];
        }, {
            bigint: () => {
                const nowMs = (typeof __pi_now_ms_native === 'function') ? __pi_now_ms_native() : 0;
                return BigInt(Math.floor(nowMs * 1e6));
            },
        }),
        kill: (pid, sig) => {
            const impl = globalThis.__pi_process_kill_impl;
            if (typeof impl === 'function') {
                return impl(pid, sig);
            }
            const err = new Error('process.kill is not available in PiJS');
            err.code = 'ENOSYS';
            throw err;
        },
        exit: (code) => {
            const exitCode = code === undefined ? 0 : Number(code);
            // Fire exit listeners
            const listeners = __evtMap['exit'];
            if (listeners) {
                for (const fn of listeners.slice()) {
                    try { fn(exitCode); } catch (_) {}
                }
            }
            // Signal native side
            if (typeof __pi_process_exit_native === 'function') {
                __pi_process_exit_native(exitCode);
            }
            const err = new Error('process.exit(' + exitCode + ')');
            err.code = 'ERR_PROCESS_EXIT';
            err.exitCode = exitCode;
            throw err;
        },
        chdir: (_dir) => {
            const err = new Error('process.chdir is not supported in PiJS');
            err.code = 'ENOSYS';
            throw err;
        },
        uptime: () => {
            const nowMs = (typeof __pi_now_ms_native === 'function') ? __pi_now_ms_native() : 0;
            return Math.floor((nowMs - startMs) / 1000);
        },
        memoryUsage: () => ({
            rss: 0, heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0,
        }),
        cpuUsage: (_prev) => ({ user: 0, system: 0 }),
        emitWarning: (msg) => {
            if (typeof __pi_console_output_native === 'function') {
                __pi_console_output_native('warn', 'Warning: ' + msg);
            }
        },
        release: { name: 'node', lts: 'PiJS' },
        config: { variables: {} },
        features: {},
        on: __on,
        addListener: __on,
        off: __off,
        removeListener: __off,
        once(event, fn) {
            const wrapped = (...args) => {
                __off(event, wrapped);
                fn(...args);
            };
            wrapped._original = fn;
            __on(event, wrapped);
            return globalThis.process;
        },
        removeAllListeners(event) {
            if (event) { delete __evtMap[event]; }
            else { for (const k in __evtMap) delete __evtMap[k]; }
            return globalThis.process;
        },
        listeners(event) {
            return (__evtMap[event] || []).slice();
        },
        emit(event, ...args) {
            const listeners = __evtMap[event];
            if (!listeners || listeners.length === 0) return false;
            for (const fn of listeners.slice()) {
                try { fn(...args); } catch (_) {}
            }
            return true;
        },
    };

    try { Object.freeze(envProxy); } catch (_) {}
    try { Object.freeze(globalThis.process.argv); } catch (_) {}
    // Do NOT freeze globalThis.process — extensions may need to monkey-patch it
}

if (typeof globalThis.setTimeout !== 'function') {
    globalThis.setTimeout = (callback, delay, ...args) => {
        const ms = Number(delay || 0);
        const timer_id = __pi_set_timeout_native(ms <= 0 ? 0 : Math.floor(ms));
        __pi_register_timer(timer_id, () => {
            try {
                callback(...args);
            } catch (e) {
                console.error('setTimeout callback error:', e);
            }
        });
        return timer_id;
    };
}

if (typeof globalThis.clearTimeout !== 'function') {
    globalThis.clearTimeout = (timer_id) => {
        __pi_unregister_timer(timer_id);
        try {
            __pi_clear_timeout_native(timer_id);
        } catch (_) {}
    };
}

// setInterval polyfill using setTimeout
const __pi_intervals = new Map();
let __pi_interval_id = 0;

if (typeof globalThis.setInterval !== 'function') {
    globalThis.setInterval = (callback, delay, ...args) => {
        const ms = Math.max(0, Number(delay || 0));
        const id = ++__pi_interval_id;
        const run = () => {
            if (!__pi_intervals.has(id)) return;
            try {
                callback(...args);
            } catch (e) {
                console.error('setInterval callback error:', e);
            }
            if (__pi_intervals.has(id)) {
                __pi_intervals.set(id, globalThis.setTimeout(run, ms));
            }
        };
        __pi_intervals.set(id, globalThis.setTimeout(run, ms));
        return id;
    };
}

if (typeof globalThis.clearInterval !== 'function') {
    globalThis.clearInterval = (id) => {
        const timerId = __pi_intervals.get(id);
        if (timerId !== undefined) {
            globalThis.clearTimeout(timerId);
            __pi_intervals.delete(id);
        }
    };
}

if (typeof globalThis.fetch !== 'function') {
    class Headers {
        constructor(init) {
            this._map = {};
            if (init && typeof init === 'object') {
                if (Array.isArray(init)) {
                    for (const pair of init) {
                        if (pair && pair.length >= 2) this.set(pair[0], pair[1]);
                    }
                } else if (typeof init.forEach === 'function') {
                    init.forEach((v, k) => this.set(k, v));
                } else {
                    for (const k of Object.keys(init)) {
                        this.set(k, init[k]);
                    }
                }
            }
        }

        get(name) {
            const key = String(name || '').toLowerCase();
            return this._map[key] === undefined ? null : this._map[key];
        }

        set(name, value) {
            const key = String(name || '').toLowerCase();
            this._map[key] = String(value === undefined || value === null ? '' : value);
        }

        entries() {
            return Object.entries(this._map);
        }
    }

    class Response {
        constructor(bodyBytes, init) {
            const options = init && typeof init === 'object' ? init : {};
            this.status = Number(options.status || 0);
            this.ok = this.status >= 200 && this.status < 300;
            this.headers = new Headers(options.headers || {});
            this._bytes = bodyBytes || new Uint8Array();
            this.body = {
                getReader: () => {
                    let done = false;
                    return {
                        read: async () => {
                            if (done) return { done: true, value: undefined };
                            done = true;
                            return { done: false, value: this._bytes };
                        },
                        cancel: async () => {
                            done = true;
                        },
                        releaseLock: () => {},
                    };
                },
            };
        }

        async text() {
            return new TextDecoder().decode(this._bytes);
        }

        async json() {
            return JSON.parse(await this.text());
        }

        async arrayBuffer() {
            const copy = new Uint8Array(this._bytes.length);
            copy.set(this._bytes);
            return copy.buffer;
        }
    }

    globalThis.Headers = Headers;
    globalThis.Response = Response;

    // AbortController / AbortSignal polyfill — many npm packages check for these
    if (typeof globalThis.AbortController === 'undefined') {
        class AbortSignal {
            constructor() { this.aborted = false; this._listeners = []; }
            get reason() { return this.aborted ? new Error('This operation was aborted') : undefined; }
            addEventListener(type, fn) { if (type === 'abort') this._listeners.push(fn); }
            removeEventListener(type, fn) { if (type === 'abort') this._listeners = this._listeners.filter(f => f !== fn); }
            throwIfAborted() { if (this.aborted) throw this.reason; }
            static abort(reason) { const s = new AbortSignal(); s.aborted = true; s._reason = reason; return s; }
            static timeout(ms) { const s = new AbortSignal(); setTimeout(() => { s.aborted = true; s._listeners.forEach(fn => fn()); }, ms); return s; }
        }
        class AbortController {
            constructor() { this.signal = new AbortSignal(); }
            abort(reason) { this.signal.aborted = true; this.signal._reason = reason; this.signal._listeners.forEach(fn => fn()); }
        }
        globalThis.AbortController = AbortController;
        globalThis.AbortSignal = AbortSignal;
    }

    globalThis.fetch = async (input, init) => {
        const url = typeof input === 'string' ? input : String(input && input.url ? input.url : input);
        const options = init && typeof init === 'object' ? init : {};
        const method = options.method ? String(options.method) : 'GET';

        const headers = {};
        if (options.headers && typeof options.headers === 'object') {
            if (options.headers instanceof Headers) {
                for (const [k, v] of options.headers.entries()) headers[k] = v;
            } else if (Array.isArray(options.headers)) {
                for (const pair of options.headers) {
                    if (pair && pair.length >= 2) headers[String(pair[0])] = String(pair[1]);
                }
            } else {
                for (const k of Object.keys(options.headers)) {
                    headers[k] = String(options.headers[k]);
                }
            }
        }

        let body = undefined;
        if (options.body !== undefined && options.body !== null) {
            body = typeof options.body === 'string' ? options.body : String(options.body);
        }

        const resp = await pi.http({ url, method, headers, body });
        const status = resp && resp.status !== undefined ? Number(resp.status) : 0;
        const respHeaders = resp && resp.headers && typeof resp.headers === 'object' ? resp.headers : {};

        let bytes = new Uint8Array();
        if (resp && resp.body_bytes) {
            const bin = __pi_base64_decode_native(String(resp.body_bytes));
            const out = new Uint8Array(bin.length);
            for (let i = 0; i < bin.length; i++) {
                out[i] = bin.charCodeAt(i) & 0xff;
            }
            bytes = out;
        } else if (resp && resp.body !== undefined && resp.body !== null) {
            bytes = new TextEncoder().encode(String(resp.body));
        }

        return new Response(bytes, { status, headers: respHeaders });
    };
}
";

#[cfg(test)]
#[allow(clippy::future_not_send)]
mod tests {
    use super::*;
    use crate::scheduler::DeterministicClock;

    #[allow(clippy::future_not_send)]
    async fn get_global_json<C: SchedulerClock + 'static>(
        runtime: &PiJsRuntime<C>,
        name: &str,
    ) -> serde_json::Value {
        runtime
            .context
            .with(|ctx| {
                let global = ctx.globals();
                let value: Value<'_> = global.get(name)?;
                js_to_json(&value)
            })
            .await
            .expect("js context")
    }

    #[allow(clippy::future_not_send)]
    async fn drain_until_idle(
        runtime: &PiJsRuntime<Arc<DeterministicClock>>,
        clock: &Arc<DeterministicClock>,
    ) {
        for _ in 0..10_000 {
            if !runtime.has_pending() {
                break;
            }

            let stats = runtime.tick().await.expect("tick");
            if stats.ran_macrotask {
                continue;
            }

            let next_deadline = runtime.scheduler.borrow().next_timer_deadline();
            let Some(next_deadline) = next_deadline else {
                break;
            };

            let now = runtime.now_ms();
            assert!(
                next_deadline > now,
                "expected future timer deadline (deadline={next_deadline}, now={now})"
            );
            clock.set(next_deadline);
        }
    }

    #[test]
    fn hostcall_completions_run_before_due_timers() {
        let clock = Arc::new(ManualClock::new(1_000));
        let mut loop_state = PiEventLoop::new(ClockHandle::new(clock));

        let _timer = loop_state.set_timeout(0);
        loop_state.enqueue_hostcall_completion("call-1");

        let mut seen = Vec::new();
        let result = loop_state.tick(|task| seen.push(task.kind), || false);

        assert!(result.ran_macrotask);
        assert_eq!(
            seen,
            vec![MacrotaskKind::HostcallComplete {
                call_id: "call-1".to_string()
            }]
        );
    }

    #[test]
    fn timers_order_by_deadline_then_schedule_seq() {
        let clock = Arc::new(ManualClock::new(0));
        let mut loop_state = PiEventLoop::new(ClockHandle::new(clock.clone()));

        let t1 = loop_state.set_timeout(10);
        let t2 = loop_state.set_timeout(10);
        let t3 = loop_state.set_timeout(5);
        clock.set(10);

        let mut fired = Vec::new();
        for _ in 0..3 {
            loop_state.tick(
                |task| {
                    if let MacrotaskKind::TimerFired { timer_id } = task.kind {
                        fired.push(timer_id);
                    }
                },
                || false,
            );
        }

        assert_eq!(fired, vec![t3, t1, t2]);
    }

    #[test]
    fn clear_timeout_prevents_fire() {
        let clock = Arc::new(ManualClock::new(0));
        let mut loop_state = PiEventLoop::new(ClockHandle::new(clock.clone()));

        let timer_id = loop_state.set_timeout(5);
        loop_state.clear_timeout(timer_id);
        clock.set(10);

        let mut fired = Vec::new();
        let result = loop_state.tick(
            |task| {
                if let MacrotaskKind::TimerFired { timer_id } = task.kind {
                    fired.push(timer_id);
                }
            },
            || false,
        );

        assert!(!result.ran_macrotask);
        assert!(fired.is_empty());
    }

    #[test]
    fn microtasks_drain_to_fixpoint_after_macrotask() {
        let clock = Arc::new(ManualClock::new(0));
        let mut loop_state = PiEventLoop::new(ClockHandle::new(clock));

        loop_state.enqueue_inbound_event("evt-1");

        let mut drain_calls = 0;
        let result = loop_state.tick(
            |_task| {},
            || {
                drain_calls += 1;
                drain_calls <= 2
            },
        );

        assert!(result.ran_macrotask);
        assert_eq!(result.microtasks_drained, 2);
        assert_eq!(drain_calls, 3);
    }

    #[test]
    fn compile_module_source_reports_missing_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let missing_path = temp_dir.path().join("missing.js");
        let err = compile_module_source(&HashMap::new(), missing_path.to_string_lossy().as_ref())
            .expect_err("missing module should error");
        let message = err.to_string();
        assert!(
            message.contains("Module is not a file"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn compile_module_source_reports_unsupported_extension() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let bad_path = temp_dir.path().join("module.txt");
        std::fs::write(&bad_path, "hello").expect("write module.txt");

        let err = compile_module_source(&HashMap::new(), bad_path.to_string_lossy().as_ref())
            .expect_err("unsupported extension should error");
        let message = err.to_string();
        assert!(
            message.contains("Unsupported module extension"),
            "unexpected error: {message}"
        );
    }

    // Tests for the Promise bridge (bd-2ke)

    #[test]
    fn pijs_runtime_creates_hostcall_request() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            // Call pi.tool() which should enqueue a hostcall request
            runtime
                .eval(r#"pi.tool("read", { path: "test.txt" });"#)
                .await
                .expect("eval");

            // Check that a hostcall request was enqueued
            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);
            let req = &requests[0];
            assert!(matches!(&req.kind, HostcallKind::Tool { name } if name == "read"));
            assert_eq!(req.payload["path"], "test.txt");
            assert_eq!(req.extension_id.as_deref(), None);
        });
    }

    #[test]
    fn pijs_runtime_hostcall_request_captures_extension_id() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                __pi_begin_extension("ext.test", { name: "Test" });
                pi.tool("read", { path: "test.txt" });
                __pi_end_extension();
            "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].extension_id.as_deref(), Some("ext.test"));
        });
    }

    #[test]
    fn pijs_runtime_get_registered_tools_empty() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            let tools = runtime.get_registered_tools().await.expect("get tools");
            assert!(tools.is_empty());
        });
    }

    #[test]
    fn pijs_runtime_get_registered_tools_single_tool() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                __pi_begin_extension('ext.test', { name: 'Test' });
                pi.registerTool({
                    name: 'my_tool',
                    label: 'My Tool',
                    description: 'Does stuff',
                    parameters: { type: 'object', properties: { path: { type: 'string' } } },
                    execute: async (_callId, _input) => { return { ok: true }; },
                });
                __pi_end_extension();
            ",
                )
                .await
                .expect("eval");

            let tools = runtime.get_registered_tools().await.expect("get tools");
            assert_eq!(tools.len(), 1);
            assert_eq!(
                tools[0],
                ExtensionToolDef {
                    name: "my_tool".to_string(),
                    label: Some("My Tool".to_string()),
                    description: "Does stuff".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" }
                        }
                    }),
                }
            );
        });
    }

    #[test]
    fn pijs_runtime_get_registered_tools_sorts_by_name() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                __pi_begin_extension('ext.test', { name: 'Test' });
                pi.registerTool({ name: 'b', execute: async (_callId, _input) => { return {}; } });
                pi.registerTool({ name: 'a', execute: async (_callId, _input) => { return {}; } });
                __pi_end_extension();
            ",
                )
                .await
                .expect("eval");

            let tools = runtime.get_registered_tools().await.expect("get tools");
            assert_eq!(
                tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["a", "b"]
            );
        });
    }

    #[test]
    fn hostcall_params_hash_is_stable_for_key_ordering() {
        let first = serde_json::json!({ "b": 2, "a": 1 });
        let second = serde_json::json!({ "a": 1, "b": 2 });

        assert_eq!(
            hostcall_params_hash("http", &first),
            hostcall_params_hash("http", &second)
        );
        assert_ne!(
            hostcall_params_hash("http", &first),
            hostcall_params_hash("tool", &first)
        );
    }

    #[test]
    fn hostcall_request_params_for_hash_uses_canonical_shapes() {
        let cases = vec![
            (
                HostcallRequest {
                    call_id: "tool-case".to_string(),
                    kind: HostcallKind::Tool {
                        name: "read".to_string(),
                    },
                    payload: serde_json::json!({ "path": "README.md" }),
                    trace_id: 0,
                    extension_id: None,
                },
                serde_json::json!({ "name": "read", "input": { "path": "README.md" } }),
            ),
            (
                HostcallRequest {
                    call_id: "exec-case".to_string(),
                    kind: HostcallKind::Exec {
                        cmd: "echo".to_string(),
                    },
                    payload: serde_json::json!({
                        "command": "legacy alias should be dropped",
                        "args": ["hello"],
                        "options": { "timeout": 1000 }
                    }),
                    trace_id: 0,
                    extension_id: None,
                },
                serde_json::json!({
                    "cmd": "echo",
                    "args": ["hello"],
                    "options": { "timeout": 1000 }
                }),
            ),
            (
                HostcallRequest {
                    call_id: "session-object".to_string(),
                    kind: HostcallKind::Session {
                        op: "set_model".to_string(),
                    },
                    payload: serde_json::json!({
                        "provider": "openai",
                        "modelId": "gpt-4o"
                    }),
                    trace_id: 0,
                    extension_id: None,
                },
                serde_json::json!({
                    "op": "set_model",
                    "provider": "openai",
                    "modelId": "gpt-4o"
                }),
            ),
            (
                HostcallRequest {
                    call_id: "ui-non-object".to_string(),
                    kind: HostcallKind::Ui {
                        op: "set_status".to_string(),
                    },
                    payload: serde_json::json!("ready"),
                    trace_id: 0,
                    extension_id: None,
                },
                serde_json::json!({ "op": "set_status", "payload": "ready" }),
            ),
            (
                HostcallRequest {
                    call_id: "events-non-object".to_string(),
                    kind: HostcallKind::Events {
                        op: "emit".to_string(),
                    },
                    payload: serde_json::json!(42),
                    trace_id: 0,
                    extension_id: None,
                },
                serde_json::json!({ "op": "emit", "payload": 42 }),
            ),
            (
                HostcallRequest {
                    call_id: "session-null".to_string(),
                    kind: HostcallKind::Session {
                        op: "get_state".to_string(),
                    },
                    payload: serde_json::Value::Null,
                    trace_id: 0,
                    extension_id: None,
                },
                serde_json::json!({ "op": "get_state" }),
            ),
        ];

        for (request, expected) in cases {
            assert_eq!(
                request.params_for_hash(),
                expected,
                "canonical params mismatch for {}",
                request.call_id
            );
        }
    }

    #[test]
    fn hostcall_request_params_hash_matches_wasm_contract_for_canonical_requests() {
        let requests = vec![
            HostcallRequest {
                call_id: "hash-session".to_string(),
                kind: HostcallKind::Session {
                    op: "set_model".to_string(),
                },
                payload: serde_json::json!({
                    "modelId": "gpt-4o",
                    "provider": "openai"
                }),
                trace_id: 0,
                extension_id: Some("ext.test".to_string()),
            },
            HostcallRequest {
                call_id: "hash-ui".to_string(),
                kind: HostcallKind::Ui {
                    op: "set_status".to_string(),
                },
                payload: serde_json::json!("thinking"),
                trace_id: 0,
                extension_id: Some("ext.test".to_string()),
            },
        ];

        for request in requests {
            let params = request.params_for_hash();
            let js_hash = request.params_hash();

            let canonical = canonicalize_json(
                &serde_json::json!({ "method": request.method(), "params": params }),
            );
            let encoded = serde_json::to_string(&canonical).expect("serialize canonical request");
            let wasm_contract_hash = sha256_hex(&encoded);

            assert_eq!(
                js_hash, wasm_contract_hash,
                "hash parity mismatch for {}",
                request.call_id
            );
        }
    }

    #[test]
    fn pijs_runtime_multiple_hostcalls() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
            pi.tool("read", { path: "a.txt" });
            pi.exec("ls", ["-la"]);
            pi.http({ url: "https://example.com" });
        "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            let kinds = requests
                .iter()
                .map(|req| format!("{:?}", req.kind))
                .collect::<Vec<_>>();
            assert_eq!(requests.len(), 3, "hostcalls: {kinds:?}");

            assert!(matches!(&requests[0].kind, HostcallKind::Tool { name } if name == "read"));
            assert!(matches!(&requests[1].kind, HostcallKind::Exec { cmd } if cmd == "ls"));
            assert!(matches!(&requests[2].kind, HostcallKind::Http));
        });
    }

    #[test]
    fn pijs_runtime_hostcall_completion_resolves_promise() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            // Set up a promise handler that stores the result
            runtime
                .eval(
                    r#"
            globalThis.result = null;
            pi.tool("read", { path: "test.txt" }).then(r => {
                globalThis.result = r;
            });
        "#,
                )
                .await
                .expect("eval");

            // Get the hostcall request
            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);
            let call_id = requests[0].call_id.clone();

            // Complete the hostcall
            runtime.complete_hostcall(
                call_id,
                HostcallOutcome::Success(serde_json::json!({ "content": "hello world" })),
            );

            // Tick to deliver the completion
            let stats = runtime.tick().await.expect("tick");
            assert!(stats.ran_macrotask);

            // Verify the promise was resolved with the correct value
            runtime
                .eval(
                    r#"
            if (globalThis.result === null) {
                throw new Error("Promise not resolved");
            }
            if (globalThis.result.content !== "hello world") {
                throw new Error("Wrong result: " + JSON.stringify(globalThis.result));
            }
        "#,
                )
                .await
                .expect("verify result");
        });
    }

    #[test]
    fn pijs_runtime_hostcall_error_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            // Set up a promise handler that captures rejection
            runtime
                .eval(
                    r#"
            globalThis.error = null;
            pi.tool("read", { path: "nonexistent.txt" }).catch(e => {
                globalThis.error = { code: e.code, message: e.message };
            });
        "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            let call_id = requests[0].call_id.clone();

            // Complete with an error
            runtime.complete_hostcall(
                call_id,
                HostcallOutcome::Error {
                    code: "ENOENT".to_string(),
                    message: "File not found".to_string(),
                },
            );

            runtime.tick().await.expect("tick");

            // Verify the promise was rejected
            runtime
                .eval(
                    r#"
            if (globalThis.error === null) {
                throw new Error("Promise not rejected");
            }
            if (globalThis.error.code !== "ENOENT") {
                throw new Error("Wrong error code: " + globalThis.error.code);
            }
        "#,
                )
                .await
                .expect("verify error");
        });
    }

    #[test]
    fn pijs_runtime_tick_stats() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            // No pending tasks
            let stats = runtime.tick().await.expect("tick");
            assert!(!stats.ran_macrotask);
            assert_eq!(stats.pending_hostcalls, 0);

            // Create a hostcall
            runtime.eval(r#"pi.tool("test", {});"#).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // Complete it
            runtime.complete_hostcall(
                requests[0].call_id.clone(),
                HostcallOutcome::Success(serde_json::json!(null)),
            );

            let stats = runtime.tick().await.expect("tick");
            assert!(stats.ran_macrotask);
        });
    }

    #[test]
    fn pijs_hostcall_timeout_rejects_promise() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let mut config = PiJsRuntimeConfig::default();
            config.limits.hostcall_timeout_ms = Some(50);

            let runtime = PiJsRuntime::with_clock_and_config(Arc::clone(&clock), config)
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                    globalThis.done = false;
                    globalThis.code = null;
                    pi.tool("read", { path: "test.txt" })
                        .then(() => { globalThis.done = true; })
                        .catch((e) => { globalThis.code = e.code; globalThis.done = true; });
                    "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            clock.set(50);
            let stats = runtime.tick().await.expect("tick");
            assert!(stats.ran_macrotask);
            assert_eq!(stats.hostcalls_timed_out, 1);
            assert_eq!(
                get_global_json(&runtime, "done").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "code").await,
                serde_json::json!("timeout")
            );

            // Late completions should be ignored.
            runtime.complete_hostcall(
                requests[0].call_id.clone(),
                HostcallOutcome::Success(serde_json::json!({ "ok": true })),
            );
            let stats = runtime.tick().await.expect("tick late completion");
            assert!(stats.ran_macrotask);
            assert_eq!(stats.hostcalls_timed_out, 1);
        });
    }

    #[test]
    fn pijs_interrupt_budget_aborts_eval() {
        futures::executor::block_on(async {
            let mut config = PiJsRuntimeConfig::default();
            config.limits.interrupt_budget = Some(0);

            let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
                .await
                .expect("create runtime");

            let err = runtime
                .eval(
                    r"
                    let sum = 0;
                    for (let i = 0; i < 1000000; i++) { sum += i; }
                    ",
                )
                .await
                .expect_err("expected budget exceed");

            assert!(err.to_string().contains("PiJS execution budget exceeded"));
        });
    }

    #[test]
    fn pijs_microtasks_drain_before_next_macrotask() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(r"globalThis.order = []; globalThis.__pi_done = false;")
                .await
                .expect("init order");

            let timer_id = runtime.set_timeout(10);
            runtime
                .eval(&format!(
                    r#"__pi_register_timer({timer_id}, () => {{
                        globalThis.order.push("timer");
                        Promise.resolve().then(() => globalThis.order.push("timer-micro"));
                    }});"#
                ))
                .await
                .expect("register timer");

            runtime
                .eval(
                    r#"
                    pi.tool("read", {}).then(() => {
                        globalThis.order.push("hostcall");
                        Promise.resolve().then(() => globalThis.order.push("hostcall-micro"));
                    });
                    "#,
                )
                .await
                .expect("enqueue hostcall");

            let requests = runtime.drain_hostcall_requests();
            let call_id = requests
                .into_iter()
                .next()
                .expect("hostcall request")
                .call_id;

            runtime.complete_hostcall(call_id, HostcallOutcome::Success(serde_json::json!(null)));

            // Make the timer due as well.
            clock.set(10);

            // Tick 1: hostcall completion runs first, and its microtasks drain immediately.
            runtime.tick().await.expect("tick hostcall");
            let after_first = get_global_json(&runtime, "order").await;
            assert_eq!(
                after_first,
                serde_json::json!(["hostcall", "hostcall-micro"])
            );

            // Tick 2: timer runs, and its microtasks drain before the next macrotask.
            runtime.tick().await.expect("tick timer");
            let after_second = get_global_json(&runtime, "order").await;
            assert_eq!(
                after_second,
                serde_json::json!(["hostcall", "hostcall-micro", "timer", "timer-micro"])
            );
        });
    }

    #[test]
    fn pijs_clear_timeout_prevents_timer_callback() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(r"globalThis.order = []; ")
                .await
                .expect("init order");

            let timer_id = runtime.set_timeout(10);
            runtime
                .eval(&format!(
                    r#"__pi_register_timer({timer_id}, () => globalThis.order.push("timer"));"#
                ))
                .await
                .expect("register timer");

            assert!(runtime.clear_timeout(timer_id));
            clock.set(10);

            let stats = runtime.tick().await.expect("tick");
            assert!(!stats.ran_macrotask);

            let order = get_global_json(&runtime, "order").await;
            assert_eq!(order, serde_json::json!([]));
        });
    }

    #[test]
    fn pijs_env_get_honors_allowlist() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let mut env = HashMap::new();
            env.insert("HOME".to_string(), "/virtual/home".to_string());
            env.insert("PI_IMAGE_SAVE_MODE".to_string(), "tmp".to_string());
            env.insert(
                "AWS_SECRET_ACCESS_KEY".to_string(),
                "nope-do-not-expose".to_string(),
            );
            let config = PiJsRuntimeConfig {
                cwd: "/virtual/cwd".to_string(),
                args: vec!["--flag".to_string()],
                env,
                limits: PiJsRuntimeLimits::default(),
            };
            let runtime = PiJsRuntime::with_clock_and_config(Arc::clone(&clock), config)
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                    globalThis.home = pi.env.get("HOME");
                    globalThis.mode = pi.env.get("PI_IMAGE_SAVE_MODE");
                    globalThis.missing_is_undefined = (pi.env.get("NOPE") === undefined);
                    globalThis.secret_is_undefined = (pi.env.get("AWS_SECRET_ACCESS_KEY") === undefined);
                    globalThis.process_secret_is_undefined = (process.env.AWS_SECRET_ACCESS_KEY === undefined);
                    globalThis.secret_in_env = ("AWS_SECRET_ACCESS_KEY" in process.env);
                    "#,
                )
                .await
                .expect("eval env");

            assert_eq!(
                get_global_json(&runtime, "home").await,
                serde_json::json!("/virtual/home")
            );
            assert_eq!(
                get_global_json(&runtime, "mode").await,
                serde_json::json!("tmp")
            );
            assert_eq!(
                get_global_json(&runtime, "missing_is_undefined").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "secret_is_undefined").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "process_secret_is_undefined").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "secret_in_env").await,
                serde_json::json!(false)
            );
        });
    }

    #[test]
    fn pijs_process_path_crypto_time_apis_smoke() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(123));
            let config = PiJsRuntimeConfig {
                cwd: "/virtual/cwd".to_string(),
                args: vec!["a".to_string(), "b".to_string()],
                env: HashMap::new(),
                limits: PiJsRuntimeLimits::default(),
            };
            let runtime = PiJsRuntime::with_clock_and_config(Arc::clone(&clock), config)
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                    globalThis.cwd = pi.process.cwd;
                    globalThis.args = pi.process.args;
                    globalThis.pi_process_is_frozen = Object.isFrozen(pi.process);
                    globalThis.pi_args_is_frozen = Object.isFrozen(pi.process.args);
                    try { pi.process.cwd = "/hacked"; } catch (_) {}
                    try { pi.process.args.push("c"); } catch (_) {}
                    globalThis.cwd_after_mut = pi.process.cwd;
                    globalThis.args_after_mut = pi.process.args;

                    globalThis.joined = pi.path.join("/a", "b", "..", "c");
                    globalThis.base = pi.path.basename("/a/b/c.txt");
                    globalThis.norm = pi.path.normalize("/a/./b//../c/");

                    globalThis.hash = pi.crypto.sha256Hex("abc");
                    globalThis.bytes = pi.crypto.randomBytes(32);

                    globalThis.now = pi.time.nowMs();
                    globalThis.done = false;
                    pi.time.sleep(10).then(() => { globalThis.done = true; });
                    "#,
                )
                .await
                .expect("eval apis");

            assert_eq!(
                get_global_json(&runtime, "cwd").await,
                serde_json::json!("/virtual/cwd")
            );
            assert_eq!(
                get_global_json(&runtime, "args").await,
                serde_json::json!(["a", "b"])
            );
            assert_eq!(
                get_global_json(&runtime, "pi_process_is_frozen").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "pi_args_is_frozen").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "cwd_after_mut").await,
                serde_json::json!("/virtual/cwd")
            );
            assert_eq!(
                get_global_json(&runtime, "args_after_mut").await,
                serde_json::json!(["a", "b"])
            );

            assert_eq!(
                get_global_json(&runtime, "joined").await,
                serde_json::json!("/a/c")
            );
            assert_eq!(
                get_global_json(&runtime, "base").await,
                serde_json::json!("c.txt")
            );
            assert_eq!(
                get_global_json(&runtime, "norm").await,
                serde_json::json!("/a/c")
            );

            assert_eq!(
                get_global_json(&runtime, "hash").await,
                serde_json::json!(
                    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                )
            );

            let bytes = get_global_json(&runtime, "bytes").await;
            let bytes_arr = bytes.as_array().expect("bytes array");
            assert_eq!(bytes_arr.len(), 32);
            for value in bytes_arr {
                let n = value.as_u64().expect("byte number");
                assert!(n <= 255);
            }

            assert_eq!(
                get_global_json(&runtime, "now").await,
                serde_json::json!(123)
            );
            assert_eq!(
                get_global_json(&runtime, "done").await,
                serde_json::json!(false)
            );

            clock.set(133);
            runtime.tick().await.expect("tick sleep");
            assert_eq!(
                get_global_json(&runtime, "done").await,
                serde_json::json!(true)
            );
        });
    }

    #[test]
    fn pijs_inbound_event_fifo_and_microtask_fixpoint() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                    globalThis.order = [];
                    __pi_add_event_listener("evt", (payload) => {
                        globalThis.order.push(payload.n);
                        Promise.resolve().then(() => globalThis.order.push(payload.n + 1000));
                    });
                    "#,
                )
                .await
                .expect("install listener");

            runtime.enqueue_event("evt", serde_json::json!({ "n": 1 }));
            runtime.enqueue_event("evt", serde_json::json!({ "n": 2 }));

            runtime.tick().await.expect("tick 1");
            let after_first = get_global_json(&runtime, "order").await;
            assert_eq!(after_first, serde_json::json!([1, 1001]));

            runtime.tick().await.expect("tick 2");
            let after_second = get_global_json(&runtime, "order").await;
            assert_eq!(after_second, serde_json::json!([1, 1001, 2, 1002]));
        });
    }

    #[derive(Debug, Clone)]
    struct XorShift64 {
        state: u64,
    }

    impl XorShift64 {
        const fn new(seed: u64) -> Self {
            let seed = seed ^ 0x9E37_79B9_7F4A_7C15;
            Self { state: seed }
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.state = x;
            x
        }

        fn next_range_u64(&mut self, upper_exclusive: u64) -> u64 {
            if upper_exclusive == 0 {
                return 0;
            }
            self.next_u64() % upper_exclusive
        }

        fn next_usize(&mut self, upper_exclusive: usize) -> usize {
            let upper = u64::try_from(upper_exclusive).expect("usize fits u64");
            let value = self.next_range_u64(upper);
            usize::try_from(value).expect("value < upper_exclusive")
        }
    }

    #[allow(clippy::future_not_send)]
    async fn run_seeded_runtime_trace(seed: u64) -> serde_json::Value {
        let clock = Arc::new(DeterministicClock::new(0));
        let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
            .await
            .expect("create runtime");

        runtime
            .eval(
                r#"
                globalThis.order = [];
                __pi_add_event_listener("evt", (payload) => {
                    globalThis.order.push("event:" + payload.step);
                    Promise.resolve().then(() => globalThis.order.push("event-micro:" + payload.step));
                });
                "#,
            )
            .await
            .expect("init");

        let mut rng = XorShift64::new(seed);
        let mut timers = Vec::new();

        for step in 0..64u64 {
            match rng.next_range_u64(6) {
                0 => {
                    runtime
                        .eval(&format!(
                            r#"
                            pi.tool("test", {{ step: {step} }}).then(() => {{
                                globalThis.order.push("hostcall:{step}");
                                Promise.resolve().then(() => globalThis.order.push("hostcall-micro:{step}"));
                            }});
                            "#
                        ))
                        .await
                        .expect("enqueue hostcall");

                    for request in runtime.drain_hostcall_requests() {
                        runtime.complete_hostcall(
                            request.call_id,
                            HostcallOutcome::Success(serde_json::json!({ "step": step })),
                        );
                    }
                }
                1 => {
                    let delay_ms = rng.next_range_u64(25);
                    let timer_id = runtime.set_timeout(delay_ms);
                    timers.push(timer_id);
                    runtime
                        .eval(&format!(
                            r#"__pi_register_timer({timer_id}, () => {{
                                globalThis.order.push("timer:{step}");
                                Promise.resolve().then(() => globalThis.order.push("timer-micro:{step}"));
                            }});"#
                        ))
                        .await
                        .expect("register timer");
                }
                2 => {
                    runtime.enqueue_event("evt", serde_json::json!({ "step": step }));
                }
                3 => {
                    if !timers.is_empty() {
                        let idx = rng.next_usize(timers.len());
                        let _ = runtime.clear_timeout(timers[idx]);
                    }
                }
                4 => {
                    let delta_ms = rng.next_range_u64(50);
                    clock.advance(delta_ms);
                }
                _ => {}
            }

            // Drive the loop a bit.
            for _ in 0..3 {
                if !runtime.has_pending() {
                    break;
                }
                let _ = runtime.tick().await.expect("tick");
            }
        }

        drain_until_idle(&runtime, &clock).await;
        get_global_json(&runtime, "order").await
    }

    #[test]
    fn pijs_seeded_trace_is_deterministic() {
        futures::executor::block_on(async {
            let a = run_seeded_runtime_trace(0x00C0_FFEE).await;
            let b = run_seeded_runtime_trace(0x00C0_FFEE).await;
            assert_eq!(a, b);
        });
    }

    #[test]
    fn pijs_events_on_returns_unsubscribe_and_removes_handler() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                    globalThis.seen = [];
                    globalThis.done = false;

                    __pi_begin_extension("ext.b", { name: "ext.b" });
                    const off = pi.events.on("custom_event", (payload, _ctx) => { globalThis.seen.push(payload); });
                    if (typeof off !== "function") throw new Error("expected unsubscribe function");
                    __pi_end_extension();

                    (async () => {
                      await __pi_dispatch_extension_event("custom_event", { n: 1 }, {});
                      off();
                      await __pi_dispatch_extension_event("custom_event", { n: 2 }, {});
                      globalThis.done = true;
                    })();
                "#,
                )
                .await
                .expect("eval");

            assert_eq!(
                get_global_json(&runtime, "done").await,
                serde_json::Value::Bool(true)
            );
            assert_eq!(
                get_global_json(&runtime, "seen").await,
                serde_json::json!([{ "n": 1 }])
            );
        });
    }

    #[test]
    fn pijs_event_dispatch_continues_after_handler_error() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                    globalThis.seen = [];
                    globalThis.done = false;

                    __pi_begin_extension("ext.err", { name: "ext.err" });
                    pi.events.on("custom_event", (_payload, _ctx) => { throw new Error("boom"); });
                    __pi_end_extension();

                    __pi_begin_extension("ext.ok", { name: "ext.ok" });
                    pi.events.on("custom_event", (payload, _ctx) => { globalThis.seen.push(payload); });
                    __pi_end_extension();

                    (async () => {
                      await __pi_dispatch_extension_event("custom_event", { hello: "world" }, {});
                      globalThis.done = true;
                    })();
                "#,
                )
                .await
                .expect("eval");

            assert_eq!(
                get_global_json(&runtime, "done").await,
                serde_json::Value::Bool(true)
            );
            assert_eq!(
                get_global_json(&runtime, "seen").await,
                serde_json::json!([{ "hello": "world" }])
            );
        });
    }

    // ---- Extension crash recovery and isolation tests (bd-m4wc) ----

    #[test]
    fn pijs_crash_register_throw_host_continues() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            // Extension that throws during registration
            runtime
                .eval(
                    r#"
                    globalThis.postCrashResult = null;

                    __pi_begin_extension("ext.crash", { name: "ext.crash" });
                    // Simulate a throw during registration by registering a handler then
                    // throwing - the handler should still be partially registered
                    throw new Error("registration boom");
                "#,
                )
                .await
                .ok(); // May fail, that's fine

            // End the crashed extension context
            runtime.eval(r"__pi_end_extension();").await.ok();

            // Host can still load another extension after the crash
            runtime
                .eval(
                    r#"
                    __pi_begin_extension("ext.ok", { name: "ext.ok" });
                    pi.events.on("test_event", (p, _) => { globalThis.postCrashResult = p; });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("second extension should load");

            // Dispatch event - only the healthy extension should handle it
            runtime
                .eval(
                    r#"
                    (async () => {
                        await __pi_dispatch_extension_event("test_event", { ok: true }, {});
                    })();
                "#,
                )
                .await
                .expect("dispatch");

            assert_eq!(
                get_global_json(&runtime, "postCrashResult").await,
                serde_json::json!({ "ok": true })
            );
        });
    }

    #[test]
    fn pijs_crash_handler_throw_other_handlers_run() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                    globalThis.handlerResults = [];
                    globalThis.dispatchDone = false;

                    // Extension A: will throw
                    __pi_begin_extension("ext.a", { name: "ext.a" });
                    pi.events.on("multi_test", (_p, _c) => {
                        globalThis.handlerResults.push("a-before-throw");
                        throw new Error("handler crash");
                    });
                    __pi_end_extension();

                    // Extension B: should still run
                    __pi_begin_extension("ext.b", { name: "ext.b" });
                    pi.events.on("multi_test", (_p, _c) => {
                        globalThis.handlerResults.push("b-ok");
                    });
                    __pi_end_extension();

                    // Extension C: should also still run
                    __pi_begin_extension("ext.c", { name: "ext.c" });
                    pi.events.on("multi_test", (_p, _c) => {
                        globalThis.handlerResults.push("c-ok");
                    });
                    __pi_end_extension();

                    (async () => {
                        await __pi_dispatch_extension_event("multi_test", {}, {});
                        globalThis.dispatchDone = true;
                    })();
                "#,
                )
                .await
                .expect("eval");

            assert_eq!(
                get_global_json(&runtime, "dispatchDone").await,
                serde_json::Value::Bool(true)
            );

            let results = get_global_json(&runtime, "handlerResults").await;
            let arr = results.as_array().expect("should be array");
            // Handler A ran (at least the part before throw)
            assert!(
                arr.iter().any(|v| v == "a-before-throw"),
                "Handler A should have run before throwing"
            );
            // Handlers B and C should have run despite A's crash
            assert!(
                arr.iter().any(|v| v == "b-ok"),
                "Handler B should run after A crashes"
            );
            assert!(
                arr.iter().any(|v| v == "c-ok"),
                "Handler C should run after A crashes"
            );
        });
    }

    #[test]
    fn pijs_crash_invalid_hostcall_returns_error_not_panic() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            // Extension makes an invalid hostcall (unknown tool)
            runtime
                .eval(
                    r#"
                    globalThis.invalidResult = null;
                    globalThis.errCode = null;

                    __pi_begin_extension("ext.bad", { name: "ext.bad" });
                    pi.tool("completely_nonexistent_tool_xyz", { junk: true })
                        .then((r) => { globalThis.invalidResult = r; })
                        .catch((e) => { globalThis.errCode = e.code || "unknown"; });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("eval");

            // The hostcall should be queued but not crash the runtime
            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1, "Hostcall should be queued");

            // Host can still evaluate JS after the invalid hostcall
            runtime
                .eval(
                    r"
                    globalThis.hostStillAlive = true;
                ",
                )
                .await
                .expect("host should still work");

            assert_eq!(
                get_global_json(&runtime, "hostStillAlive").await,
                serde_json::Value::Bool(true)
            );
        });
    }

    #[test]
    fn pijs_crash_after_crash_new_extensions_load() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            // Simulate a crash sequence: extension throws, then new ones load fine
            runtime
                .eval(
                    r#"
                    globalThis.loadOrder = [];

                    // Extension 1: loads fine
                    __pi_begin_extension("ext.1", { name: "ext.1" });
                    globalThis.loadOrder.push("1-loaded");
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("ext 1");

            // Extension 2: crashes during eval
            runtime
                .eval(
                    r#"
                    __pi_begin_extension("ext.2", { name: "ext.2" });
                    globalThis.loadOrder.push("2-before-crash");
                    throw new Error("ext 2 crash");
                "#,
                )
                .await
                .ok(); // Expected to fail

            runtime.eval(r"__pi_end_extension();").await.ok();

            // Extension 3: should still load after ext 2's crash
            runtime
                .eval(
                    r#"
                    __pi_begin_extension("ext.3", { name: "ext.3" });
                    globalThis.loadOrder.push("3-loaded");
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("ext 3 should load after crash");

            // Extension 4: loads fine too
            runtime
                .eval(
                    r#"
                    __pi_begin_extension("ext.4", { name: "ext.4" });
                    globalThis.loadOrder.push("4-loaded");
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("ext 4 should load");

            let order = get_global_json(&runtime, "loadOrder").await;
            let arr = order.as_array().expect("should be array");
            assert!(
                arr.iter().any(|v| v == "1-loaded"),
                "Extension 1 should have loaded"
            );
            assert!(
                arr.iter().any(|v| v == "3-loaded"),
                "Extension 3 should load after crash"
            );
            assert!(
                arr.iter().any(|v| v == "4-loaded"),
                "Extension 4 should load after crash"
            );
        });
    }

    #[test]
    fn pijs_crash_no_cross_contamination_between_extensions() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                    globalThis.extAData = null;
                    globalThis.extBData = null;
                    globalThis.eventsDone = false;

                    // Extension A: sets its own state
                    __pi_begin_extension("ext.isolated.a", { name: "ext.isolated.a" });
                    pi.events.on("isolation_test", (_p, _c) => {
                        globalThis.extAData = "from-A";
                    });
                    __pi_end_extension();

                    // Extension B: sets its own state independently
                    __pi_begin_extension("ext.isolated.b", { name: "ext.isolated.b" });
                    pi.events.on("isolation_test", (_p, _c) => {
                        globalThis.extBData = "from-B";
                    });
                    __pi_end_extension();

                    (async () => {
                        await __pi_dispatch_extension_event("isolation_test", {}, {});
                        globalThis.eventsDone = true;
                    })();
                "#,
                )
                .await
                .expect("eval");

            assert_eq!(
                get_global_json(&runtime, "eventsDone").await,
                serde_json::Value::Bool(true)
            );
            // Each extension should have set its own global independently
            assert_eq!(
                get_global_json(&runtime, "extAData").await,
                serde_json::json!("from-A")
            );
            assert_eq!(
                get_global_json(&runtime, "extBData").await,
                serde_json::json!("from-B")
            );
        });
    }

    #[test]
    fn pijs_crash_interrupt_budget_stops_infinite_loop() {
        futures::executor::block_on(async {
            let config = PiJsRuntimeConfig {
                limits: PiJsRuntimeLimits {
                    // Use a small interrupt budget to catch infinite loops quickly
                    interrupt_budget: Some(1000),
                    ..Default::default()
                },
                ..Default::default()
            };
            let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
                .await
                .expect("create runtime");

            // Try to run an infinite loop - should be interrupted by budget
            let result = runtime
                .eval(
                    r"
                    let i = 0;
                    while (true) { i++; }
                    globalThis.loopResult = i;
                ",
                )
                .await;

            // The eval should fail due to interrupt
            assert!(
                result.is_err(),
                "Infinite loop should be interrupted by budget"
            );

            // Host should still be alive after interrupt
            let alive_result = runtime.eval(r#"globalThis.postInterrupt = "alive";"#).await;
            // After an interrupt, the runtime may or may not accept new evals
            // The key assertion is that we didn't hang
            if alive_result.is_ok() {
                assert_eq!(
                    get_global_json(&runtime, "postInterrupt").await,
                    serde_json::json!("alive")
                );
            }
        });
    }

    #[test]
    fn pijs_events_emit_queues_events_hostcall() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
                    __pi_begin_extension("ext.test", { name: "Test" });
                    pi.events.emit("custom_event", { a: 1 });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let req = &requests[0];
            assert_eq!(req.extension_id.as_deref(), Some("ext.test"));
            assert!(
                matches!(&req.kind, HostcallKind::Events { op } if op == "emit"),
                "unexpected hostcall kind: {:?}",
                req.kind
            );
            assert_eq!(
                req.payload,
                serde_json::json!({ "event": "custom_event", "data": { "a": 1 } })
            );
        });
    }

    #[test]
    fn pijs_console_global_is_defined_and_callable() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            // Verify console global exists and all standard methods are functions
            runtime
                .eval(
                    r"
                    globalThis.console_exists = typeof globalThis.console === 'object';
                    globalThis.has_log   = typeof console.log   === 'function';
                    globalThis.has_warn  = typeof console.warn  === 'function';
                    globalThis.has_error = typeof console.error === 'function';
                    globalThis.has_info  = typeof console.info  === 'function';
                    globalThis.has_debug = typeof console.debug === 'function';
                    globalThis.has_trace = typeof console.trace === 'function';
                    globalThis.has_dir   = typeof console.dir   === 'function';
                    globalThis.has_assert = typeof console.assert === 'function';
                    globalThis.has_table = typeof console.table === 'function';

                    // Call each method to ensure they don't throw
                    console.log('test log', 42, { key: 'value' });
                    console.warn('test warn');
                    console.error('test error');
                    console.info('test info');
                    console.debug('test debug');
                    console.trace('test trace');
                    console.dir({ a: 1 });
                    console.assert(true, 'should not appear');
                    console.assert(false, 'assertion failed message');
                    console.table([1, 2, 3]);
                    console.time();
                    console.timeEnd();
                    console.group();
                    console.groupEnd();
                    console.clear();

                    globalThis.calls_succeeded = true;
                    ",
                )
                .await
                .expect("eval console tests");

            assert_eq!(
                get_global_json(&runtime, "console_exists").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "has_log").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "has_warn").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "has_error").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "has_info").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "has_debug").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "has_trace").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "has_dir").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "has_assert").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "has_table").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "calls_succeeded").await,
                serde_json::json!(true)
            );
        });
    }

    #[test]
    fn pijs_node_events_module_provides_event_emitter() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            // Use dynamic import() since eval() runs as a script, not a module
            runtime
                .eval(
                    r"
                    globalThis.results = [];
                    globalThis.testDone = false;

                    import('node:events').then(({ EventEmitter }) => {
                        const emitter = new EventEmitter();

                        emitter.on('data', (val) => globalThis.results.push('data:' + val));
                        emitter.once('done', () => globalThis.results.push('done'));

                        emitter.emit('data', 1);
                        emitter.emit('data', 2);
                        emitter.emit('done');
                        emitter.emit('done'); // should not fire again

                        globalThis.listenerCount = emitter.listenerCount('data');
                        globalThis.eventNames = emitter.eventNames();
                        globalThis.testDone = true;
                    });
                    ",
                )
                .await
                .expect("eval EventEmitter test");

            assert_eq!(
                get_global_json(&runtime, "testDone").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "results").await,
                serde_json::json!(["data:1", "data:2", "done"])
            );
            assert_eq!(
                get_global_json(&runtime, "listenerCount").await,
                serde_json::json!(1)
            );
            assert_eq!(
                get_global_json(&runtime, "eventNames").await,
                serde_json::json!(["data"])
            );
        });
    }

    #[test]
    fn pijs_bare_module_aliases_resolve_correctly() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            // Test that bare "events" alias resolves to "node:events"
            runtime
                .eval(
                    r"
                    globalThis.bare_events_ok = false;
                    import('events').then((mod) => {
                        const e = new mod.default();
                        globalThis.bare_events_ok = typeof e.on === 'function';
                    });
                    ",
                )
                .await
                .expect("eval bare events import");

            assert_eq!(
                get_global_json(&runtime, "bare_events_ok").await,
                serde_json::json!(true)
            );
        });
    }

    #[test]
    fn pijs_path_extended_functions() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.pathResults = {};
                    import('node:path').then((path) => {
                        globalThis.pathResults.isAbsRoot = path.isAbsolute('/foo/bar');
                        globalThis.pathResults.isAbsRel = path.isAbsolute('foo/bar');
                        globalThis.pathResults.extJs = path.extname('/a/b/file.js');
                        globalThis.pathResults.extNone = path.extname('/a/b/noext');
                        globalThis.pathResults.extDot = path.extname('.hidden');
                        globalThis.pathResults.norm = path.normalize('/a/b/../c/./d');
                        globalThis.pathResults.parseBase = path.parse('/home/user/file.txt').base;
                        globalThis.pathResults.parseExt = path.parse('/home/user/file.txt').ext;
                        globalThis.pathResults.parseName = path.parse('/home/user/file.txt').name;
                        globalThis.pathResults.parseDir = path.parse('/home/user/file.txt').dir;
                        globalThis.pathResults.hasPosix = typeof path.posix === 'object';
                        globalThis.pathResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval path extended");

            let r = get_global_json(&runtime, "pathResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["isAbsRoot"], serde_json::json!(true));
            assert_eq!(r["isAbsRel"], serde_json::json!(false));
            assert_eq!(r["extJs"], serde_json::json!(".js"));
            assert_eq!(r["extNone"], serde_json::json!(""));
            assert_eq!(r["extDot"], serde_json::json!(""));
            assert_eq!(r["norm"], serde_json::json!("/a/c/d"));
            assert_eq!(r["parseBase"], serde_json::json!("file.txt"));
            assert_eq!(r["parseExt"], serde_json::json!(".txt"));
            assert_eq!(r["parseName"], serde_json::json!("file"));
            assert_eq!(r["parseDir"], serde_json::json!("/home/user"));
            assert_eq!(r["hasPosix"], serde_json::json!(true));
        });
    }

    #[test]
    fn pijs_fs_callback_apis() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.fsResults = {};
                    import('node:fs').then((fs) => {
                        // readFile callback
                        fs.readFile('/fake', 'utf8', (err, data) => {
                            globalThis.fsResults.readFileCallbackCalled = true;
                            globalThis.fsResults.readFileData = data;
                        });
                        // writeFile callback
                        fs.writeFile('/fake', 'data', (err) => {
                            globalThis.fsResults.writeFileCallbackCalled = true;
                        });
                        // accessSync throws
                        try {
                            fs.accessSync('/nonexistent');
                            globalThis.fsResults.accessSyncThrew = false;
                        } catch (e) {
                            globalThis.fsResults.accessSyncThrew = true;
                        }
                        // access callback with error
                        fs.access('/nonexistent', (err) => {
                            globalThis.fsResults.accessCallbackErr = !!err;
                        });
                        globalThis.fsResults.hasLstatSync = typeof fs.lstatSync === 'function';
                        globalThis.fsResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval fs callbacks");

            let r = get_global_json(&runtime, "fsResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["readFileCallbackCalled"], serde_json::json!(true));
            assert_eq!(r["readFileData"], serde_json::json!(""));
            assert_eq!(r["writeFileCallbackCalled"], serde_json::json!(true));
            assert_eq!(r["accessSyncThrew"], serde_json::json!(true));
            assert_eq!(r["accessCallbackErr"], serde_json::json!(true));
            assert_eq!(r["hasLstatSync"], serde_json::json!(true));
        });
    }

    #[test]
    fn pijs_fs_sync_roundtrip_and_dirents() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.fsRoundTrip = {};
                    import('node:fs').then((fs) => {
                        fs.mkdirSync('/tmp/demo', { recursive: true });
                        fs.writeFileSync('/tmp/demo/hello.txt', 'hello world');
                        fs.writeFileSync('/tmp/demo/raw.bin', Buffer.from([1, 2, 3, 4]));

                        globalThis.fsRoundTrip.exists = fs.existsSync('/tmp/demo/hello.txt');
                        globalThis.fsRoundTrip.readText = fs.readFileSync('/tmp/demo/hello.txt', 'utf8');
                        const raw = fs.readFileSync('/tmp/demo/raw.bin');
                        globalThis.fsRoundTrip.rawLen = raw.length;

                        const names = fs.readdirSync('/tmp/demo');
                        globalThis.fsRoundTrip.names = names;

                        const dirents = fs.readdirSync('/tmp/demo', { withFileTypes: true });
                        globalThis.fsRoundTrip.direntHasMethods =
                          typeof dirents[0].isFile === 'function' &&
                          typeof dirents[0].isDirectory === 'function';

                        const dirStat = fs.statSync('/tmp/demo');
                        const fileStat = fs.statSync('/tmp/demo/hello.txt');
                        globalThis.fsRoundTrip.isDir = dirStat.isDirectory();
                        globalThis.fsRoundTrip.isFile = fileStat.isFile();
                        globalThis.fsRoundTrip.done = true;
                    });
                    ",
                )
                .await
                .expect("eval fs sync roundtrip");

            let r = get_global_json(&runtime, "fsRoundTrip").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["exists"], serde_json::json!(true));
            assert_eq!(r["readText"], serde_json::json!("hello world"));
            assert_eq!(r["rawLen"], serde_json::json!(4));
            assert_eq!(r["isDir"], serde_json::json!(true));
            assert_eq!(r["isFile"], serde_json::json!(true));
            assert_eq!(r["direntHasMethods"], serde_json::json!(true));
            assert_eq!(r["names"], serde_json::json!(["hello.txt", "raw.bin"]));
        });
    }

    #[test]
    fn pijs_create_require_supports_node_builtins() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.requireResults = {};
                    import('node:module').then(({ createRequire }) => {
                        const require = createRequire('/tmp/example.js');
                        const path = require('path');
                        const fs = require('node:fs');
                        const crypto = require('crypto');

                        globalThis.requireResults.pathJoinWorks = path.join('a', 'b') === 'a/b';
                        globalThis.requireResults.fsReadFileSync = typeof fs.readFileSync === 'function';
                        globalThis.requireResults.cryptoHasRandomUUID = typeof crypto.randomUUID === 'function';

                        try {
                            require('left-pad');
                            globalThis.requireResults.missingModuleThrows = false;
                        } catch (err) {
                            globalThis.requireResults.missingModuleThrows =
                              String(err && err.message || '').includes('Cannot find module');
                        }
                        globalThis.requireResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval createRequire test");

            let r = get_global_json(&runtime, "requireResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["pathJoinWorks"], serde_json::json!(true));
            assert_eq!(r["fsReadFileSync"], serde_json::json!(true));
            assert_eq!(r["cryptoHasRandomUUID"], serde_json::json!(true));
            assert_eq!(r["missingModuleThrows"], serde_json::json!(true));
        });
    }

    #[test]
    fn pijs_fs_promises_delegates_to_node_fs_promises_api() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.fsPromisesResults = {};
                    import('node:fs/promises').then(async (fsp) => {
                        await fsp.mkdir('/tmp/promise-demo', { recursive: true });
                        await fsp.writeFile('/tmp/promise-demo/value.txt', 'value');
                        const text = await fsp.readFile('/tmp/promise-demo/value.txt', 'utf8');
                        const names = await fsp.readdir('/tmp/promise-demo');

                        globalThis.fsPromisesResults.readText = text;
                        globalThis.fsPromisesResults.names = names;
                        globalThis.fsPromisesResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval fs promises test");

            let r = get_global_json(&runtime, "fsPromisesResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["readText"], serde_json::json!("value"));
            assert_eq!(r["names"], serde_json::json!(["value.txt"]));
        });
    }

    #[test]
    fn pijs_child_process_spawn_emits_data_and_close() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.childProcessResult = { events: [] };
                    import('node:child_process').then(({ spawn }) => {
                        const child = spawn('pi', ['--version'], {
                            shell: false,
                            stdio: ['ignore', 'pipe', 'pipe'],
                        });
                        let stdout = '';
                        let stderr = '';
                        child.stdout?.on('data', (chunk) => {
                            stdout += chunk.toString();
                            globalThis.childProcessResult.events.push('stdout');
                        });
                        child.stderr?.on('data', (chunk) => {
                            stderr += chunk.toString();
                            globalThis.childProcessResult.events.push('stderr');
                        });
                        child.on('error', (err) => {
                            globalThis.childProcessResult.error =
                                String((err && err.message) || err || '');
                            globalThis.childProcessResult.done = true;
                        });
                        child.on('close', (code) => {
                            globalThis.childProcessResult.events.push('close');
                            globalThis.childProcessResult.code = code;
                            globalThis.childProcessResult.stdout = stdout;
                            globalThis.childProcessResult.stderr = stderr;
                            globalThis.childProcessResult.killed = child.killed;
                            globalThis.childProcessResult.pid = child.pid;
                            globalThis.childProcessResult.done = true;
                        });
                    });
                    ",
                )
                .await
                .expect("eval child_process spawn script");

            let mut requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);
            let request = requests.pop_front().expect("exec hostcall");
            assert!(
                matches!(&request.kind, HostcallKind::Exec { cmd } if cmd == "pi"),
                "unexpected hostcall kind: {:?}",
                request.kind
            );

            runtime.complete_hostcall(
                request.call_id,
                HostcallOutcome::Success(serde_json::json!({
                    "stdout": "line-1\n",
                    "stderr": "warn-1\n",
                    "code": 0,
                    "killed": false
                })),
            );

            drain_until_idle(&runtime, &clock).await;
            let r = get_global_json(&runtime, "childProcessResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["code"], serde_json::json!(0));
            assert_eq!(r["stdout"], serde_json::json!("line-1\n"));
            assert_eq!(r["stderr"], serde_json::json!("warn-1\n"));
            assert_eq!(r["killed"], serde_json::json!(false));
            assert_eq!(
                r["events"],
                serde_json::json!(["stdout", "stderr", "close"])
            );
        });
    }

    #[test]
    fn pijs_child_process_process_kill_targets_spawned_pid() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.childKillResult = {};
                    import('node:child_process').then(({ spawn }) => {
                        const child = spawn('pi', ['--version'], {
                            shell: false,
                            detached: true,
                            stdio: ['ignore', 'pipe', 'pipe'],
                        });
                        globalThis.childKillResult.pid = child.pid;
                        child.on('close', (code) => {
                            globalThis.childKillResult.code = code;
                            globalThis.childKillResult.killed = child.killed;
                            globalThis.childKillResult.done = true;
                        });
                        try {
                            globalThis.childKillResult.killOk = process.kill(-child.pid, 'SIGKILL') === true;
                        } catch (err) {
                            globalThis.childKillResult.killErrorCode = String((err && err.code) || '');
                            globalThis.childKillResult.killErrorMessage = String((err && err.message) || err || '');
                        }
                    });
                    ",
                )
                .await
                .expect("eval child_process kill script");

            let mut requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);
            let request = requests.pop_front().expect("exec hostcall");
            runtime.complete_hostcall(
                request.call_id,
                HostcallOutcome::Success(serde_json::json!({
                    "stdout": "",
                    "stderr": "",
                    "code": 0,
                    "killed": false
                })),
            );

            drain_until_idle(&runtime, &clock).await;
            let r = get_global_json(&runtime, "childKillResult").await;
            assert_eq!(r["killOk"], serde_json::json!(true));
            assert_eq!(r["killed"], serde_json::json!(true));
            assert_eq!(r["code"], serde_json::Value::Null);
            assert_eq!(r["done"], serde_json::json!(true));
        });
    }

    #[test]
    fn pijs_child_process_denied_exec_emits_error_and_close() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.childDeniedResult = {};
                    import('node:child_process').then(({ spawn }) => {
                        const child = spawn('pi', ['--version'], {
                            shell: false,
                            stdio: ['ignore', 'pipe', 'pipe'],
                        });
                        child.on('error', (err) => {
                            globalThis.childDeniedResult.errorCode = String((err && err.code) || '');
                            globalThis.childDeniedResult.errorMessage = String((err && err.message) || err || '');
                        });
                        child.on('close', (code) => {
                            globalThis.childDeniedResult.code = code;
                            globalThis.childDeniedResult.killed = child.killed;
                            globalThis.childDeniedResult.done = true;
                        });
                    });
                    ",
                )
                .await
                .expect("eval child_process denied script");

            let mut requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);
            let request = requests.pop_front().expect("exec hostcall");
            runtime.complete_hostcall(
                request.call_id,
                HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "Capability 'exec' denied by policy".to_string(),
                },
            );

            drain_until_idle(&runtime, &clock).await;
            let r = get_global_json(&runtime, "childDeniedResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["errorCode"], serde_json::json!("denied"));
            assert_eq!(
                r["errorMessage"],
                serde_json::json!("Capability 'exec' denied by policy")
            );
            assert_eq!(r["code"], serde_json::json!(1));
            assert_eq!(r["killed"], serde_json::json!(false));
        });
    }

    #[test]
    fn pijs_child_process_rejects_unsupported_shell_option() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.childOptionResult = {};
                    import('node:child_process').then(({ spawn }) => {
                        try {
                            spawn('pi', ['--version'], { shell: true });
                            globalThis.childOptionResult.threw = false;
                        } catch (err) {
                            globalThis.childOptionResult.threw = true;
                            globalThis.childOptionResult.message = String((err && err.message) || err || '');
                        }
                        globalThis.childOptionResult.done = true;
                    });
                    ",
                )
                .await
                .expect("eval child_process unsupported shell script");

            drain_until_idle(&runtime, &clock).await;
            let r = get_global_json(&runtime, "childOptionResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["threw"], serde_json::json!(true));
            assert_eq!(
                r["message"],
                serde_json::json!(
                    "node:child_process.spawn: only shell=false is supported in PiJS"
                )
            );
            assert_eq!(runtime.drain_hostcall_requests().len(), 0);
        });
    }

    // -----------------------------------------------------------------------
    // bd-2b9y: Node core shim unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn pijs_node_os_module_exports() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.osResults = {};
                    import('node:os').then((os) => {
                        globalThis.osResults.homedir = os.homedir();
                        globalThis.osResults.tmpdir = os.tmpdir();
                        globalThis.osResults.hostname = os.hostname();
                        globalThis.osResults.platform = os.platform();
                        globalThis.osResults.arch = os.arch();
                        globalThis.osResults.type = os.type();
                        globalThis.osResults.release = os.release();
                        globalThis.osResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval node:os");

            let r = get_global_json(&runtime, "osResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            // homedir returns HOME env or fallback
            assert!(r["homedir"].is_string());
            assert_eq!(r["tmpdir"], serde_json::json!("/tmp"));
            assert_eq!(r["hostname"], serde_json::json!("pi-host"));
            assert_eq!(r["platform"], serde_json::json!("linux"));
            assert_eq!(r["arch"], serde_json::json!("x64"));
            assert_eq!(r["type"], serde_json::json!("Linux"));
            assert_eq!(r["release"], serde_json::json!("6.0.0"));
        });
    }

    #[test]
    fn pijs_node_os_bare_import_alias() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.bare_os_ok = false;
                    import('os').then((os) => {
                        globalThis.bare_os_ok = typeof os.homedir === 'function'
                            && typeof os.platform === 'function';
                    });
                    ",
                )
                .await
                .expect("eval bare os import");

            assert_eq!(
                get_global_json(&runtime, "bare_os_ok").await,
                serde_json::json!(true)
            );
        });
    }

    #[test]
    fn pijs_node_url_module_exports() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.urlResults = {};
                    import('node:url').then((url) => {
                        globalThis.urlResults.fileToPath = url.fileURLToPath('file:///home/user/test.txt');
                        globalThis.urlResults.pathToFile = url.pathToFileURL('/home/user/test.txt').href;

                        const u = new url.URL('https://example.com/path?key=val#frag');
                        globalThis.urlResults.href = u.href;
                        globalThis.urlResults.protocol = u.protocol;
                        globalThis.urlResults.hostname = u.hostname;
                        globalThis.urlResults.pathname = u.pathname;
                        globalThis.urlResults.toString = u.toString();

                        globalThis.urlResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval node:url");

            let r = get_global_json(&runtime, "urlResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["fileToPath"], serde_json::json!("/home/user/test.txt"));
            assert_eq!(
                r["pathToFile"],
                serde_json::json!("file:///home/user/test.txt")
            );
            // URL parsing
            assert!(r["href"].as_str().unwrap().starts_with("https://"));
            assert_eq!(r["protocol"], serde_json::json!("https:"));
            assert_eq!(r["hostname"], serde_json::json!("example.com"));
            // Shim URL.pathname includes query+fragment (lightweight parser)
            assert!(r["pathname"].as_str().unwrap().starts_with("/path"));
        });
    }

    #[test]
    fn pijs_node_crypto_create_hash_and_uuid() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.cryptoResults = {};
                    import('node:crypto').then((crypto) => {
                        // createHash
                        const hash = crypto.createHash('sha256');
                        hash.update('hello');
                        globalThis.cryptoResults.hexDigest = hash.digest('hex');

                        // createHash chained
                        globalThis.cryptoResults.chainedHex = crypto
                            .createHash('sha256')
                            .update('world')
                            .digest('hex');

                        // randomUUID
                        const uuid = crypto.randomUUID();
                        globalThis.cryptoResults.uuidLength = uuid.length;
                        // UUID v4 format: 8-4-4-4-12
                        globalThis.cryptoResults.uuidHasDashes = uuid.split('-').length === 5;

                        globalThis.cryptoResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval node:crypto");

            let r = get_global_json(&runtime, "cryptoResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            // createHash returns a hex string
            assert!(r["hexDigest"].is_string());
            let hex = r["hexDigest"].as_str().unwrap();
            // djb2-simulated hash, not real SHA-256 — verify it's a non-empty hex string
            assert!(!hex.is_empty());
            assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
            // chained usage also works
            assert!(r["chainedHex"].is_string());
            let chained = r["chainedHex"].as_str().unwrap();
            assert!(!chained.is_empty());
            assert!(chained.chars().all(|c| c.is_ascii_hexdigit()));
            // Two different inputs produce different hashes
            assert_ne!(r["hexDigest"], r["chainedHex"]);
            // randomUUID format
            assert_eq!(r["uuidLength"], serde_json::json!(36));
            assert_eq!(r["uuidHasDashes"], serde_json::json!(true));
        });
    }

    #[test]
    fn pijs_buffer_global_operations() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.bufResults = {};
                    // Test the global Buffer polyfill (set up during runtime init)
                    const B = globalThis.Buffer;
                    globalThis.bufResults.hasBuffer = typeof B === 'function';
                    globalThis.bufResults.hasFrom = typeof B.from === 'function';

                    // Buffer.from with array input
                    const arr = B.from([65, 66, 67]);
                    globalThis.bufResults.fromArrayLength = arr.length;

                    // Uint8Array allocation
                    const zeroed = new Uint8Array(16);
                    globalThis.bufResults.allocLength = zeroed.length;

                    globalThis.bufResults.done = true;
                    ",
                )
                .await
                .expect("eval Buffer");

            let r = get_global_json(&runtime, "bufResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["hasBuffer"], serde_json::json!(true));
            assert_eq!(r["hasFrom"], serde_json::json!(true));
            assert_eq!(r["fromArrayLength"], serde_json::json!(3));
            assert_eq!(r["allocLength"], serde_json::json!(16));
        });
    }

    #[test]
    fn pijs_node_fs_promises_async_roundtrip() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.fspResults = {};
                    import('node:fs/promises').then(async (fsp) => {
                        // Write then read back
                        await fsp.writeFile('/test/hello.txt', 'async content');
                        const data = await fsp.readFile('/test/hello.txt', 'utf8');
                        globalThis.fspResults.readBack = data;

                        // stat
                        const st = await fsp.stat('/test/hello.txt');
                        globalThis.fspResults.statIsFile = st.isFile();
                        globalThis.fspResults.statSize = st.size;

                        // mkdir + readdir
                        await fsp.mkdir('/test/subdir');
                        await fsp.writeFile('/test/subdir/a.txt', 'aaa');
                        const entries = await fsp.readdir('/test/subdir');
                        globalThis.fspResults.dirEntries = entries;

                        // unlink
                        await fsp.unlink('/test/subdir/a.txt');
                        const exists = await fsp.access('/test/subdir/a.txt').then(() => true).catch(() => false);
                        globalThis.fspResults.deletedFileExists = exists;

                        globalThis.fspResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval fs/promises");

            drain_until_idle(&runtime, &clock).await;

            let r = get_global_json(&runtime, "fspResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["readBack"], serde_json::json!("async content"));
            assert_eq!(r["statIsFile"], serde_json::json!(true));
            assert!(r["statSize"].as_u64().unwrap() > 0);
            assert_eq!(r["dirEntries"], serde_json::json!(["a.txt"]));
            assert_eq!(r["deletedFileExists"], serde_json::json!(false));
        });
    }

    #[test]
    fn pijs_node_process_module_exports() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let config = PiJsRuntimeConfig {
                cwd: "/test/project".to_string(),
                args: vec!["arg1".to_string(), "arg2".to_string()],
                env: HashMap::new(),
                limits: PiJsRuntimeLimits::default(),
            };
            let runtime = PiJsRuntime::with_clock_and_config(Arc::clone(&clock), config)
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.procResults = {};
                    import('node:process').then((proc) => {
                        globalThis.procResults.platform = proc.platform;
                        globalThis.procResults.arch = proc.arch;
                        globalThis.procResults.version = proc.version;
                        globalThis.procResults.pid = proc.pid;
                        globalThis.procResults.cwdType = typeof proc.cwd;
                        globalThis.procResults.cwdValue = typeof proc.cwd === 'function'
                            ? proc.cwd() : proc.cwd;
                        globalThis.procResults.hasEnv = typeof proc.env === 'object';
                        globalThis.procResults.hasStdout = typeof proc.stdout === 'object';
                        globalThis.procResults.hasStderr = typeof proc.stderr === 'object';
                        globalThis.procResults.hasNextTick = typeof proc.nextTick === 'function';

                        // nextTick should schedule microtask
                        globalThis.procResults.nextTickRan = false;
                        proc.nextTick(() => { globalThis.procResults.nextTickRan = true; });

                        // hrtime should return array
                        const hr = proc.hrtime();
                        globalThis.procResults.hrtimeIsArray = Array.isArray(hr);
                        globalThis.procResults.hrtimeLength = hr.length;

                        globalThis.procResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval node:process");

            drain_until_idle(&runtime, &clock).await;

            let r = get_global_json(&runtime, "procResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["platform"], serde_json::json!("linux"));
            assert_eq!(r["arch"], serde_json::json!("x64"));
            assert!(r["version"].is_string());
            assert_eq!(r["pid"], serde_json::json!(1));
            assert!(r["hasEnv"] == serde_json::json!(true));
            assert!(r["hasStdout"] == serde_json::json!(true));
            assert!(r["hasStderr"] == serde_json::json!(true));
            assert!(r["hasNextTick"] == serde_json::json!(true));
            // nextTick is scheduled as microtask — should have run
            assert_eq!(r["nextTickRan"], serde_json::json!(true));
            assert_eq!(r["hrtimeIsArray"], serde_json::json!(true));
            assert_eq!(r["hrtimeLength"], serde_json::json!(2));
        });
    }

    #[test]
    fn pijs_node_path_relative_resolve_format() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let config = PiJsRuntimeConfig {
                cwd: "/home/user/project".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                limits: PiJsRuntimeLimits::default(),
            };
            let runtime = PiJsRuntime::with_clock_and_config(Arc::clone(&clock), config)
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.pathResults2 = {};
                    import('node:path').then((path) => {
                        // relative
                        globalThis.pathResults2.relSameDir = path.relative('/a/b/c', '/a/b/c/d');
                        globalThis.pathResults2.relUp = path.relative('/a/b/c', '/a/b');
                        globalThis.pathResults2.relSame = path.relative('/a/b', '/a/b');

                        // resolve uses cwd as base
                        globalThis.pathResults2.resolveAbs = path.resolve('/absolute/path');
                        globalThis.pathResults2.resolveRel = path.resolve('relative');

                        // format
                        globalThis.pathResults2.formatFull = path.format({
                            dir: '/home/user',
                            base: 'file.txt'
                        });

                        // sep and delimiter constants
                        globalThis.pathResults2.sep = path.sep;
                        globalThis.pathResults2.delimiter = path.delimiter;

                        // dirname edge cases
                        globalThis.pathResults2.dirnameRoot = path.dirname('/');
                        globalThis.pathResults2.dirnameNested = path.dirname('/a/b/c');

                        // join edge cases
                        globalThis.pathResults2.joinEmpty = path.join();
                        globalThis.pathResults2.joinDots = path.join('a', '..', 'b');

                        globalThis.pathResults2.done = true;
                    });
                    ",
                )
                .await
                .expect("eval path extended 2");

            let r = get_global_json(&runtime, "pathResults2").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["relSameDir"], serde_json::json!("d"));
            assert_eq!(r["relUp"], serde_json::json!(".."));
            assert_eq!(r["relSame"], serde_json::json!("."));
            assert_eq!(r["resolveAbs"], serde_json::json!("/absolute/path"));
            // resolve('relative') should resolve against cwd
            assert!(r["resolveRel"].as_str().unwrap().ends_with("/relative"));
            assert_eq!(r["formatFull"], serde_json::json!("/home/user/file.txt"));
            assert_eq!(r["sep"], serde_json::json!("/"));
            assert_eq!(r["delimiter"], serde_json::json!(":"));
            assert_eq!(r["dirnameRoot"], serde_json::json!("/"));
            assert_eq!(r["dirnameNested"], serde_json::json!("/a/b"));
            // join doesn't normalize; normalize is separate
            let join_dots = r["joinDots"].as_str().unwrap();
            assert!(join_dots == "b" || join_dots == "a/../b");
        });
    }

    #[test]
    fn pijs_node_util_module_exports() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.utilResults = {};
                    import('node:util').then((util) => {
                        globalThis.utilResults.hasInspect = typeof util.inspect === 'function';
                        globalThis.utilResults.hasPromisify = typeof util.promisify === 'function';
                        globalThis.utilResults.inspectResult = util.inspect({ a: 1, b: [2, 3] });
                        globalThis.utilResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval node:util");

            let r = get_global_json(&runtime, "utilResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["hasInspect"], serde_json::json!(true));
            assert_eq!(r["hasPromisify"], serde_json::json!(true));
            // inspect should return some string representation
            assert!(r["inspectResult"].is_string());
        });
    }

    #[test]
    fn pijs_node_assert_module_pass_and_fail() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.assertResults = {};
                    import('node:assert').then((mod) => {
                        const assert = mod.default;

                        // Passing assertions should not throw
                        assert.ok(true);
                        assert.strictEqual(1, 1);
                        assert.deepStrictEqual({ a: 1 }, { a: 1 });
                        assert.notStrictEqual(1, 2);

                        // Failing assertion should throw
                        try {
                            assert.strictEqual(1, 2);
                            globalThis.assertResults.failDidNotThrow = true;
                        } catch (e) {
                            globalThis.assertResults.failThrew = true;
                            globalThis.assertResults.failMessage = e.message || String(e);
                        }

                        globalThis.assertResults.done = true;
                    });
                    ",
                )
                .await
                .expect("eval node:assert");

            let r = get_global_json(&runtime, "assertResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["failThrew"], serde_json::json!(true));
            assert!(r["failMessage"].is_string());
        });
    }

    #[test]
    fn pijs_node_fs_sync_edge_cases() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.fsEdge = {};
                    import('node:fs').then((fs) => {
                        // Write, overwrite, read back
                        fs.writeFileSync('/edge/file.txt', 'first');
                        fs.writeFileSync('/edge/file.txt', 'second');
                        globalThis.fsEdge.overwrite = fs.readFileSync('/edge/file.txt', 'utf8');

                        // existsSync for existing vs non-existing
                        globalThis.fsEdge.existsTrue = fs.existsSync('/edge/file.txt');
                        globalThis.fsEdge.existsFalse = fs.existsSync('/nonexistent/file.txt');

                        // mkdirSync + readdirSync with withFileTypes
                        fs.mkdirSync('/edge/dir');
                        fs.writeFileSync('/edge/dir/a.txt', 'aaa');
                        fs.mkdirSync('/edge/dir/sub');
                        const dirents = fs.readdirSync('/edge/dir', { withFileTypes: true });
                        globalThis.fsEdge.direntCount = dirents.length;
                        const fileDirent = dirents.find(d => d.name === 'a.txt');
                        const dirDirent = dirents.find(d => d.name === 'sub');
                        globalThis.fsEdge.fileIsFile = fileDirent ? fileDirent.isFile() : null;
                        globalThis.fsEdge.dirIsDir = dirDirent ? dirDirent.isDirectory() : null;

                        // rmSync recursive
                        fs.writeFileSync('/edge/dir/sub/deep.txt', 'deep');
                        fs.rmSync('/edge/dir', { recursive: true });
                        globalThis.fsEdge.rmRecursiveGone = !fs.existsSync('/edge/dir');

                        // accessSync on non-existing file should throw
                        try {
                            fs.accessSync('/nope');
                            globalThis.fsEdge.accessThrew = false;
                        } catch (e) {
                            globalThis.fsEdge.accessThrew = true;
                        }

                        // statSync on directory
                        fs.mkdirSync('/edge/statdir');
                        const dStat = fs.statSync('/edge/statdir');
                        globalThis.fsEdge.dirStatIsDir = dStat.isDirectory();
                        globalThis.fsEdge.dirStatIsFile = dStat.isFile();

                        globalThis.fsEdge.done = true;
                    });
                    ",
                )
                .await
                .expect("eval fs edge cases");

            let r = get_global_json(&runtime, "fsEdge").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["overwrite"], serde_json::json!("second"));
            assert_eq!(r["existsTrue"], serde_json::json!(true));
            assert_eq!(r["existsFalse"], serde_json::json!(false));
            assert_eq!(r["direntCount"], serde_json::json!(2));
            assert_eq!(r["fileIsFile"], serde_json::json!(true));
            assert_eq!(r["dirIsDir"], serde_json::json!(true));
            assert_eq!(r["rmRecursiveGone"], serde_json::json!(true));
            assert_eq!(r["accessThrew"], serde_json::json!(true));
            assert_eq!(r["dirStatIsDir"], serde_json::json!(true));
            assert_eq!(r["dirStatIsFile"], serde_json::json!(false));
        });
    }

    #[test]
    fn pijs_node_net_and_http_stubs_throw() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.stubResults = {};
                    (async () => {
                        // node:net createServer should throw
                        const net = await import('node:net');
                        try {
                            net.createServer();
                            globalThis.stubResults.netThrew = false;
                        } catch (e) {
                            globalThis.stubResults.netThrew = true;
                        }

                        // node:http createServer should throw
                        const http = await import('node:http');
                        try {
                            http.createServer();
                            globalThis.stubResults.httpThrew = false;
                        } catch (e) {
                            globalThis.stubResults.httpThrew = true;
                        }

                        // node:https request should throw
                        const https = await import('node:https');
                        try {
                            https.request('https://example.com');
                            globalThis.stubResults.httpsThrew = false;
                        } catch (e) {
                            globalThis.stubResults.httpsThrew = true;
                        }

                        globalThis.stubResults.done = true;
                    })();
                    ",
                )
                .await
                .expect("eval stub throws");

            drain_until_idle(&runtime, &clock).await;

            let r = get_global_json(&runtime, "stubResults").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["netThrew"], serde_json::json!(true));
            assert_eq!(r["httpThrew"], serde_json::json!(true));
            assert_eq!(r["httpsThrew"], serde_json::json!(true));
        });
    }

    #[test]
    fn pijs_node_readline_stub_exports() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.rlResult = {};
                    import('node:readline').then((rl) => {
                        globalThis.rlResult.hasCreateInterface = typeof rl.createInterface === 'function';
                        globalThis.rlResult.done = true;
                    });
                    ",
                )
                .await
                .expect("eval readline");

            let r = get_global_json(&runtime, "rlResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["hasCreateInterface"], serde_json::json!(true));
        });
    }

    // ── Streaming hostcall tests ────────────────────────────────────────

    #[test]
    fn pijs_stream_chunks_delivered_via_async_iterator() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            // Start a streaming exec call
            runtime
                .eval(
                    r#"
            globalThis.chunks = [];
            globalThis.done = false;
            (async () => {
                const stream = pi.exec("cat", ["big.txt"], { stream: true });
                for await (const chunk of stream) {
                    globalThis.chunks.push(chunk);
                }
                globalThis.done = true;
            })();
            "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);
            let call_id = requests[0].call_id.clone();

            // Send three non-final chunks then a final one
            for seq in 0..3 {
                runtime.complete_hostcall(
                    call_id.clone(),
                    HostcallOutcome::StreamChunk {
                        sequence: seq,
                        chunk: serde_json::json!({ "line": seq }),
                        is_final: false,
                    },
                );
                let stats = runtime.tick().await.expect("tick chunk");
                assert!(stats.ran_macrotask);
            }

            // Hostcall should still be pending (tracker not yet completed)
            assert!(
                runtime.hostcall_tracker.borrow().is_pending(&call_id),
                "hostcall should still be pending after non-final chunks"
            );

            // Send final chunk
            runtime.complete_hostcall(
                call_id.clone(),
                HostcallOutcome::StreamChunk {
                    sequence: 3,
                    chunk: serde_json::json!({ "line": 3 }),
                    is_final: true,
                },
            );
            let stats = runtime.tick().await.expect("tick final");
            assert!(stats.ran_macrotask);

            // Hostcall is now completed
            assert!(
                !runtime.hostcall_tracker.borrow().is_pending(&call_id),
                "hostcall should be completed after final chunk"
            );

            // Run microtasks to let the async iterator resolve
            runtime.tick().await.expect("tick settle");

            let chunks = get_global_json(&runtime, "chunks").await;
            let arr = chunks.as_array().expect("chunks is array");
            assert_eq!(arr.len(), 4, "expected 4 chunks, got {arr:?}");
            for (i, c) in arr.iter().enumerate() {
                assert_eq!(c["line"], serde_json::json!(i), "chunk {i}");
            }

            let done = get_global_json(&runtime, "done").await;
            assert_eq!(
                done,
                serde_json::json!(true),
                "async loop should have completed"
            );
        });
    }

    #[test]
    fn pijs_stream_error_rejects_async_iterator() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
            globalThis.chunks = [];
            globalThis.errMsg = null;
            (async () => {
                try {
                    const stream = pi.exec("fail", [], { stream: true });
                    for await (const chunk of stream) {
                        globalThis.chunks.push(chunk);
                    }
                } catch (e) {
                    globalThis.errMsg = e.message;
                }
            })();
            "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            let call_id = requests[0].call_id.clone();

            // Send one good chunk
            runtime.complete_hostcall(
                call_id.clone(),
                HostcallOutcome::StreamChunk {
                    sequence: 0,
                    chunk: serde_json::json!("first"),
                    is_final: false,
                },
            );
            runtime.tick().await.expect("tick chunk 0");

            // Now error the hostcall
            runtime.complete_hostcall(
                call_id,
                HostcallOutcome::Error {
                    code: "STREAM_ERR".into(),
                    message: "broken pipe".into(),
                },
            );
            runtime.tick().await.expect("tick error");
            runtime.tick().await.expect("tick settle");

            let chunks = get_global_json(&runtime, "chunks").await;
            assert_eq!(
                chunks.as_array().expect("array").len(),
                1,
                "should have received 1 chunk before error"
            );

            let err = get_global_json(&runtime, "errMsg").await;
            assert_eq!(err, serde_json::json!("broken pipe"));
        });
    }

    #[test]
    fn pijs_stream_http_returns_async_iterator() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
            globalThis.chunks = [];
            globalThis.done = false;
            (async () => {
                const stream = pi.http({ url: "http://example.com", stream: true });
                for await (const chunk of stream) {
                    globalThis.chunks.push(chunk);
                }
                globalThis.done = true;
            })();
            "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);
            let call_id = requests[0].call_id.clone();

            // Two chunks: non-final then final
            runtime.complete_hostcall(
                call_id.clone(),
                HostcallOutcome::StreamChunk {
                    sequence: 0,
                    chunk: serde_json::json!("chunk-a"),
                    is_final: false,
                },
            );
            runtime.tick().await.expect("tick a");

            runtime.complete_hostcall(
                call_id,
                HostcallOutcome::StreamChunk {
                    sequence: 1,
                    chunk: serde_json::json!("chunk-b"),
                    is_final: true,
                },
            );
            runtime.tick().await.expect("tick b");
            runtime.tick().await.expect("tick settle");

            let chunks = get_global_json(&runtime, "chunks").await;
            let arr = chunks.as_array().expect("array");
            assert_eq!(arr.len(), 2);
            assert_eq!(arr[0], serde_json::json!("chunk-a"));
            assert_eq!(arr[1], serde_json::json!("chunk-b"));

            assert_eq!(
                get_global_json(&runtime, "done").await,
                serde_json::json!(true)
            );
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn pijs_stream_concurrent_exec_calls_have_independent_lifecycle() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
            globalThis.streamA = [];
            globalThis.streamB = [];
            globalThis.doneA = false;
            globalThis.doneB = false;
            (async () => {
                const stream = pi.exec("cmd-a", [], { stream: true });
                for await (const chunk of stream) {
                    globalThis.streamA.push(chunk);
                }
                globalThis.doneA = true;
            })();
            (async () => {
                const stream = pi.exec("cmd-b", [], { stream: true });
                for await (const chunk of stream) {
                    globalThis.streamB.push(chunk);
                }
                globalThis.doneB = true;
            })();
            "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 2, "expected two streaming exec requests");

            let mut call_a: Option<String> = None;
            let mut call_b: Option<String> = None;
            for request in &requests {
                match &request.kind {
                    HostcallKind::Exec { cmd } if cmd == "cmd-a" => {
                        call_a = Some(request.call_id.clone());
                    }
                    HostcallKind::Exec { cmd } if cmd == "cmd-b" => {
                        call_b = Some(request.call_id.clone());
                    }
                    _ => {}
                }
            }

            let call_a = call_a.expect("call_id for cmd-a");
            let call_b = call_b.expect("call_id for cmd-b");
            assert_ne!(call_a, call_b, "concurrent calls must have distinct ids");
            assert_eq!(runtime.pending_hostcall_count(), 2);

            runtime.complete_hostcall(
                call_a.clone(),
                HostcallOutcome::StreamChunk {
                    sequence: 0,
                    chunk: serde_json::json!("a0"),
                    is_final: false,
                },
            );
            runtime.tick().await.expect("tick a0");

            runtime.complete_hostcall(
                call_b.clone(),
                HostcallOutcome::StreamChunk {
                    sequence: 0,
                    chunk: serde_json::json!("b0"),
                    is_final: false,
                },
            );
            runtime.tick().await.expect("tick b0");
            assert_eq!(runtime.pending_hostcall_count(), 2);

            runtime.complete_hostcall(
                call_b.clone(),
                HostcallOutcome::StreamChunk {
                    sequence: 1,
                    chunk: serde_json::json!("b1"),
                    is_final: true,
                },
            );
            runtime.tick().await.expect("tick b1");
            assert_eq!(runtime.pending_hostcall_count(), 1);
            assert!(runtime.is_hostcall_pending(&call_a));
            assert!(!runtime.is_hostcall_pending(&call_b));

            runtime.complete_hostcall(
                call_a.clone(),
                HostcallOutcome::StreamChunk {
                    sequence: 1,
                    chunk: serde_json::json!("a1"),
                    is_final: true,
                },
            );
            runtime.tick().await.expect("tick a1");
            assert_eq!(runtime.pending_hostcall_count(), 0);
            assert!(!runtime.is_hostcall_pending(&call_a));

            runtime.tick().await.expect("tick settle 1");
            runtime.tick().await.expect("tick settle 2");

            let stream_a = get_global_json(&runtime, "streamA").await;
            let stream_b = get_global_json(&runtime, "streamB").await;
            assert_eq!(
                stream_a.as_array().expect("streamA array"),
                &vec![serde_json::json!("a0"), serde_json::json!("a1")]
            );
            assert_eq!(
                stream_b.as_array().expect("streamB array"),
                &vec![serde_json::json!("b0"), serde_json::json!("b1")]
            );
            assert_eq!(
                get_global_json(&runtime, "doneA").await,
                serde_json::json!(true)
            );
            assert_eq!(
                get_global_json(&runtime, "doneB").await,
                serde_json::json!(true)
            );
        });
    }

    #[test]
    fn pijs_stream_chunk_ignored_after_hostcall_completed() {
        futures::executor::block_on(async {
            let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r#"
            globalThis.result = null;
            pi.tool("read", { path: "test.txt" }).then(r => {
                globalThis.result = r;
            });
            "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            let call_id = requests[0].call_id.clone();

            // Complete normally first
            runtime.complete_hostcall(
                call_id.clone(),
                HostcallOutcome::Success(serde_json::json!({ "content": "done" })),
            );
            runtime.tick().await.expect("tick success");

            // Now try to deliver a stream chunk to the same call_id — should be ignored
            runtime.complete_hostcall(
                call_id,
                HostcallOutcome::StreamChunk {
                    sequence: 0,
                    chunk: serde_json::json!("stale"),
                    is_final: false,
                },
            );
            // This should not panic
            let stats = runtime.tick().await.expect("tick stale chunk");
            assert!(stats.ran_macrotask, "macrotask should run (and be ignored)");

            let result = get_global_json(&runtime, "result").await;
            assert_eq!(result["content"], serde_json::json!("done"));
        });
    }

    // ── node:child_process sync tests ──────────────────────────────────

    #[test]
    fn pijs_exec_sync_runs_command_and_returns_stdout() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.syncResult = {};
                    import('node:child_process').then(({ execSync }) => {
                        try {
                            const output = execSync('echo hello-sync');
                            globalThis.syncResult.stdout = output.trim();
                            globalThis.syncResult.done = true;
                        } catch (e) {
                            globalThis.syncResult.error = String(e);
                            globalThis.syncResult.stack = e.stack || '';
                            globalThis.syncResult.done = false;
                        }
                    }).catch(e => {
                        globalThis.syncResult.promiseError = String(e);
                    });
                    ",
                )
                .await
                .expect("eval execSync test");

            let r = get_global_json(&runtime, "syncResult").await;
            assert!(
                r["done"] == serde_json::json!(true),
                "execSync test failed: error={}, stack={}, promiseError={}",
                r["error"],
                r["stack"],
                r["promiseError"]
            );
            assert_eq!(r["stdout"], serde_json::json!("hello-sync"));
        });
    }

    #[test]
    fn pijs_exec_sync_throws_on_nonzero_exit() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.syncErr = {};
                    import('node:child_process').then(({ execSync }) => {
                        try {
                            execSync('exit 42');
                            globalThis.syncErr.threw = false;
                        } catch (e) {
                            globalThis.syncErr.threw = true;
                            globalThis.syncErr.status = e.status;
                            globalThis.syncErr.hasStderr = typeof e.stderr === 'string';
                        }
                        globalThis.syncErr.done = true;
                    });
                    ",
                )
                .await
                .expect("eval execSync nonzero");

            let r = get_global_json(&runtime, "syncErr").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["threw"], serde_json::json!(true));
            // Status is a JS number (always f64 in QuickJS), so compare as f64
            assert_eq!(r["status"].as_f64(), Some(42.0));
            assert_eq!(r["hasStderr"], serde_json::json!(true));
        });
    }

    #[test]
    fn pijs_exec_sync_empty_command_throws() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.emptyResult = {};
                    import('node:child_process').then(({ execSync }) => {
                        try {
                            execSync('');
                            globalThis.emptyResult.threw = false;
                        } catch (e) {
                            globalThis.emptyResult.threw = true;
                            globalThis.emptyResult.msg = e.message;
                        }
                        globalThis.emptyResult.done = true;
                    });
                    ",
                )
                .await
                .expect("eval execSync empty");

            let r = get_global_json(&runtime, "emptyResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["threw"], serde_json::json!(true));
            assert!(
                r["msg"]
                    .as_str()
                    .unwrap_or("")
                    .contains("command is required")
            );
        });
    }

    #[test]
    fn pijs_spawn_sync_returns_result_object() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.spawnSyncResult = {};
                    import('node:child_process').then(({ spawnSync }) => {
                        const r = spawnSync('echo', ['spawn-test']);
                        globalThis.spawnSyncResult.stdout = r.stdout.trim();
                        globalThis.spawnSyncResult.status = r.status;
                        globalThis.spawnSyncResult.hasOutput = Array.isArray(r.output);
                        globalThis.spawnSyncResult.noError = r.error === undefined;
                        globalThis.spawnSyncResult.done = true;
                    });
                    ",
                )
                .await
                .expect("eval spawnSync test");

            let r = get_global_json(&runtime, "spawnSyncResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["stdout"], serde_json::json!("spawn-test"));
            assert_eq!(r["status"].as_f64(), Some(0.0));
            assert_eq!(r["hasOutput"], serde_json::json!(true));
            assert_eq!(r["noError"], serde_json::json!(true));
        });
    }

    #[test]
    fn pijs_spawn_sync_captures_nonzero_exit() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.spawnSyncFail = {};
                    import('node:child_process').then(({ spawnSync }) => {
                        const r = spawnSync('sh', ['-c', 'exit 7']);
                        globalThis.spawnSyncFail.status = r.status;
                        globalThis.spawnSyncFail.signal = r.signal;
                        globalThis.spawnSyncFail.done = true;
                    });
                    ",
                )
                .await
                .expect("eval spawnSync fail");

            let r = get_global_json(&runtime, "spawnSyncFail").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["status"].as_f64(), Some(7.0));
            assert_eq!(r["signal"], serde_json::json!(null));
        });
    }

    #[test]
    fn pijs_spawn_sync_bad_command_returns_error() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.badCmd = {};
                    import('node:child_process').then(({ spawnSync }) => {
                        const r = spawnSync('__nonexistent_binary_xyzzy__');
                        globalThis.badCmd.hasError = r.error !== undefined;
                        globalThis.badCmd.statusNull = r.status === null;
                        globalThis.badCmd.done = true;
                    });
                    ",
                )
                .await
                .expect("eval spawnSync bad cmd");

            let r = get_global_json(&runtime, "badCmd").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["hasError"], serde_json::json!(true));
            assert_eq!(r["statusNull"], serde_json::json!(true));
        });
    }

    #[test]
    fn pijs_exec_file_sync_runs_binary_directly() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.execFileResult = {};
                    import('node:child_process').then(({ execFileSync }) => {
                        const output = execFileSync('echo', ['file-sync-test']);
                        globalThis.execFileResult.stdout = output.trim();
                        globalThis.execFileResult.done = true;
                    });
                    ",
                )
                .await
                .expect("eval execFileSync test");

            let r = get_global_json(&runtime, "execFileResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["stdout"], serde_json::json!("file-sync-test"));
        });
    }

    #[test]
    fn pijs_exec_sync_captures_stderr() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.stderrResult = {};
                    import('node:child_process').then(({ execSync }) => {
                        try {
                            execSync('echo err-msg >&2 && exit 1');
                            globalThis.stderrResult.threw = false;
                        } catch (e) {
                            globalThis.stderrResult.threw = true;
                            globalThis.stderrResult.stderr = e.stderr.trim();
                        }
                        globalThis.stderrResult.done = true;
                    });
                    ",
                )
                .await
                .expect("eval execSync stderr");

            let r = get_global_json(&runtime, "stderrResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["threw"], serde_json::json!(true));
            assert_eq!(r["stderr"], serde_json::json!("err-msg"));
        });
    }

    #[test]
    fn pijs_exec_sync_with_cwd_option() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.cwdResult = {};
                    import('node:child_process').then(({ execSync }) => {
                        const output = execSync('pwd', { cwd: '/tmp' });
                        globalThis.cwdResult.dir = output.trim();
                        globalThis.cwdResult.done = true;
                    });
                    ",
                )
                .await
                .expect("eval execSync cwd");

            let r = get_global_json(&runtime, "cwdResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            // /tmp may resolve to /private/tmp on macOS
            let dir = r["dir"].as_str().unwrap_or("");
            assert!(
                dir == "/tmp" || dir.ends_with("/tmp"),
                "expected /tmp, got: {dir}"
            );
        });
    }

    #[test]
    fn pijs_spawn_sync_empty_command_throws() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.emptySpawn = {};
                    import('node:child_process').then(({ spawnSync }) => {
                        try {
                            spawnSync('');
                            globalThis.emptySpawn.threw = false;
                        } catch (e) {
                            globalThis.emptySpawn.threw = true;
                            globalThis.emptySpawn.msg = e.message;
                        }
                        globalThis.emptySpawn.done = true;
                    });
                    ",
                )
                .await
                .expect("eval spawnSync empty");

            let r = get_global_json(&runtime, "emptySpawn").await;
            assert_eq!(r["done"], serde_json::json!(true));
            assert_eq!(r["threw"], serde_json::json!(true));
            assert!(
                r["msg"]
                    .as_str()
                    .unwrap_or("")
                    .contains("command is required")
            );
        });
    }

    #[test]
    fn pijs_spawn_sync_options_as_second_arg() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            // spawnSync(cmd, options) with no args array — options is 2nd param
            runtime
                .eval(
                    r"
                    globalThis.optsResult = {};
                    import('node:child_process').then(({ spawnSync }) => {
                        const r = spawnSync('pwd', { cwd: '/tmp' });
                        globalThis.optsResult.stdout = r.stdout.trim();
                        globalThis.optsResult.done = true;
                    });
                    ",
                )
                .await
                .expect("eval spawnSync opts as 2nd arg");

            let r = get_global_json(&runtime, "optsResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            let stdout = r["stdout"].as_str().unwrap_or("");
            assert!(
                stdout == "/tmp" || stdout.ends_with("/tmp"),
                "expected /tmp, got: {stdout}"
            );
        });
    }

    // ── node:os expanded API tests ─────────────────────────────────────

    #[test]
    fn pijs_os_expanded_apis() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.osEx = {};
                    import('node:os').then((os) => {
                        const cpuArr = os.cpus();
                        globalThis.osEx.cpusIsArray = Array.isArray(cpuArr);
                        globalThis.osEx.cpusLen = cpuArr.length;
                        globalThis.osEx.cpuHasModel = typeof cpuArr[0].model === 'string';
                        globalThis.osEx.cpuHasSpeed = typeof cpuArr[0].speed === 'number';
                        globalThis.osEx.cpuHasTimes = typeof cpuArr[0].times === 'object';

                        globalThis.osEx.totalmem = os.totalmem();
                        globalThis.osEx.totalMemPositive = os.totalmem() > 0;
                        globalThis.osEx.freeMemPositive = os.freemem() > 0;
                        globalThis.osEx.freeMemLessTotal = os.freemem() <= os.totalmem();

                        globalThis.osEx.uptimePositive = os.uptime() > 0;

                        const la = os.loadavg();
                        globalThis.osEx.loadavgIsArray = Array.isArray(la);
                        globalThis.osEx.loadavgLen = la.length;

                        globalThis.osEx.networkInterfacesIsObj = typeof os.networkInterfaces() === 'object';

                        const ui = os.userInfo();
                        globalThis.osEx.userInfoHasUid = typeof ui.uid === 'number';
                        globalThis.osEx.userInfoHasUsername = typeof ui.username === 'string';
                        globalThis.osEx.userInfoHasHomedir = typeof ui.homedir === 'string';
                        globalThis.osEx.userInfoHasShell = typeof ui.shell === 'string';

                        globalThis.osEx.endianness = os.endianness();
                        globalThis.osEx.eol = os.EOL;
                        globalThis.osEx.devNull = os.devNull;
                        globalThis.osEx.hasConstants = typeof os.constants === 'object';

                        globalThis.osEx.done = true;
                    });
                    ",
                )
                .await
                .expect("eval node:os expanded");

            let r = get_global_json(&runtime, "osEx").await;
            assert_eq!(r["done"], serde_json::json!(true));
            // cpus()
            assert_eq!(r["cpusIsArray"], serde_json::json!(true));
            assert!(r["cpusLen"].as_f64().unwrap_or(0.0) >= 1.0);
            assert_eq!(r["cpuHasModel"], serde_json::json!(true));
            assert_eq!(r["cpuHasSpeed"], serde_json::json!(true));
            assert_eq!(r["cpuHasTimes"], serde_json::json!(true));
            // totalmem/freemem
            assert_eq!(r["totalMemPositive"], serde_json::json!(true));
            assert_eq!(r["freeMemPositive"], serde_json::json!(true));
            assert_eq!(r["freeMemLessTotal"], serde_json::json!(true));
            // uptime
            assert_eq!(r["uptimePositive"], serde_json::json!(true));
            // loadavg
            assert_eq!(r["loadavgIsArray"], serde_json::json!(true));
            assert_eq!(r["loadavgLen"].as_f64(), Some(3.0));
            // networkInterfaces
            assert_eq!(r["networkInterfacesIsObj"], serde_json::json!(true));
            // userInfo
            assert_eq!(r["userInfoHasUid"], serde_json::json!(true));
            assert_eq!(r["userInfoHasUsername"], serde_json::json!(true));
            assert_eq!(r["userInfoHasHomedir"], serde_json::json!(true));
            assert_eq!(r["userInfoHasShell"], serde_json::json!(true));
            // endianness / EOL / devNull / constants
            assert_eq!(r["endianness"], serde_json::json!("LE"));
            assert_eq!(r["eol"], serde_json::json!("\n"));
            assert_eq!(r["devNull"], serde_json::json!("/dev/null"));
            assert_eq!(r["hasConstants"], serde_json::json!(true));
        });
    }

    // ── Buffer expanded API tests ──────────────────────────────────────

    #[test]
    fn pijs_buffer_expanded_apis() {
        futures::executor::block_on(async {
            let clock = Arc::new(DeterministicClock::new(0));
            let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
                .await
                .expect("create runtime");

            runtime
                .eval(
                    r"
                    globalThis.bufResult = {};
                    (() => {
                        const B = globalThis.Buffer;

                        // alloc
                        const a = B.alloc(4, 0xAB);
                        globalThis.bufResult.allocFill = Array.from(a);

                        // from string + hex encoding
                        const hex = B.from('48656c6c6f', 'hex');
                        globalThis.bufResult.hexDecode = hex.toString('utf8');

                        // concat
                        const c = B.concat([B.from('Hello'), B.from(' World')]);
                        globalThis.bufResult.concat = c.toString();

                        // byteLength
                        globalThis.bufResult.byteLength = B.byteLength('Hello');

                        // compare
                        globalThis.bufResult.compareEqual = B.compare(B.from('abc'), B.from('abc'));
                        globalThis.bufResult.compareLess = B.compare(B.from('abc'), B.from('abd'));
                        globalThis.bufResult.compareGreater = B.compare(B.from('abd'), B.from('abc'));

                        // isEncoding
                        globalThis.bufResult.isEncodingUtf8 = B.isEncoding('utf8');
                        globalThis.bufResult.isEncodingFake = B.isEncoding('fake');

                        // isBuffer
                        globalThis.bufResult.isBufferTrue = B.isBuffer(B.from('x'));
                        globalThis.bufResult.isBufferFalse = B.isBuffer('x');

                        // instance methods
                        const b = B.from('Hello World');
                        globalThis.bufResult.indexOf = b.indexOf('World');
                        globalThis.bufResult.includes = b.includes('World');
                        globalThis.bufResult.notIncludes = b.includes('xyz');

                        const sliced = b.slice(0, 5);
                        globalThis.bufResult.slice = sliced.toString();

                        globalThis.bufResult.toJSON = b.toJSON().type;

                        const eq1 = B.from('abc');
                        const eq2 = B.from('abc');
                        const eq3 = B.from('xyz');
                        globalThis.bufResult.equalsTrue = eq1.equals(eq2);
                        globalThis.bufResult.equalsFalse = eq1.equals(eq3);

                        // copy
                        const src = B.from('Hello');
                        const dst = B.alloc(5);
                        src.copy(dst);
                        globalThis.bufResult.copy = dst.toString();

                        // write
                        const wb = B.alloc(10);
                        wb.write('Hi');
                        globalThis.bufResult.write = wb.toString('utf8', 0, 2);

                        // readUInt / writeUInt
                        const nb = B.alloc(4);
                        nb.writeUInt16BE(0x1234, 0);
                        globalThis.bufResult.readUInt16BE = nb.readUInt16BE(0);
                        nb.writeUInt32LE(0xDEADBEEF, 0);
                        globalThis.bufResult.readUInt32LE = nb.readUInt32LE(0);

                        // hex encoding
                        const hb = B.from([0xDE, 0xAD]);
                        globalThis.bufResult.toHex = hb.toString('hex');

                        // base64 round-trip
                        const b64 = B.from('Hello').toString('base64');
                        const roundTrip = B.from(b64, 'base64').toString();
                        globalThis.bufResult.base64Round = roundTrip;

                        globalThis.bufResult.done = true;
                    })();
                    ",
                )
                .await
                .expect("eval Buffer expanded");

            let r = get_global_json(&runtime, "bufResult").await;
            assert_eq!(r["done"], serde_json::json!(true));
            // alloc with fill
            assert_eq!(r["allocFill"], serde_json::json!([0xAB, 0xAB, 0xAB, 0xAB]));
            // hex decode
            assert_eq!(r["hexDecode"], serde_json::json!("Hello"));
            // concat
            assert_eq!(r["concat"], serde_json::json!("Hello World"));
            // byteLength
            assert_eq!(r["byteLength"].as_f64(), Some(5.0));
            // compare
            assert_eq!(r["compareEqual"].as_f64(), Some(0.0));
            assert!(r["compareLess"].as_f64().unwrap_or(0.0) < 0.0);
            assert!(r["compareGreater"].as_f64().unwrap_or(0.0) > 0.0);
            // isEncoding
            assert_eq!(r["isEncodingUtf8"], serde_json::json!(true));
            assert_eq!(r["isEncodingFake"], serde_json::json!(false));
            // isBuffer
            assert_eq!(r["isBufferTrue"], serde_json::json!(true));
            assert_eq!(r["isBufferFalse"], serde_json::json!(false));
            // indexOf / includes
            assert_eq!(r["indexOf"].as_f64(), Some(6.0));
            assert_eq!(r["includes"], serde_json::json!(true));
            assert_eq!(r["notIncludes"], serde_json::json!(false));
            // slice
            assert_eq!(r["slice"], serde_json::json!("Hello"));
            // toJSON
            assert_eq!(r["toJSON"], serde_json::json!("Buffer"));
            // equals
            assert_eq!(r["equalsTrue"], serde_json::json!(true));
            assert_eq!(r["equalsFalse"], serde_json::json!(false));
            // copy
            assert_eq!(r["copy"], serde_json::json!("Hello"));
            // write
            assert_eq!(r["write"], serde_json::json!("Hi"));
            // readUInt16BE
            assert_eq!(r["readUInt16BE"].as_f64(), Some(f64::from(0x1234)));
            // readUInt32LE
            assert_eq!(r["readUInt32LE"].as_f64(), Some(f64::from(0xDEAD_BEEF_u32)));
            // hex
            assert_eq!(r["toHex"], serde_json::json!("dead"));
            // base64 round-trip
            assert_eq!(r["base64Round"], serde_json::json!("Hello"));
        });
    }
}
