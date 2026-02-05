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
    pub label: String,
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

    #[must_use]
    pub fn params_for_hash(&self) -> serde_json::Value {
        match &self.kind {
            HostcallKind::Tool { name } => {
                serde_json::json!({ "name": name, "input": self.payload.clone() })
            }
            HostcallKind::Exec { cmd } => {
                let mut map = serde_json::Map::new();
                map.insert("cmd".to_string(), serde_json::Value::String(cmd.clone()));
                match &self.payload {
                    serde_json::Value::Object(obj) => {
                        for (key, value) in obj {
                            if key == "cmd" {
                                continue;
                            }
                            map.insert(key.clone(), value.clone());
                        }
                    }
                    other => {
                        map.insert("payload".to_string(), other.clone());
                    }
                }
                serde_json::Value::Object(map)
            }
            HostcallKind::Http => self.payload.clone(),
            HostcallKind::Session { op }
            | HostcallKind::Ui { op }
            | HostcallKind::Events { op } => {
                serde_json::json!({ "op": op, "args": self.payload.clone() })
            }
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

        if self.state.borrow().virtual_modules.contains_key(spec) {
            return Ok(spec.to_string());
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
        "ts" | "tsx" => transpile_typescript_module(&raw, name).map_err(|message| {
            rquickjs::Error::new_loading_message(name, format!("transpile: {message}"))
        })?,
        "js" | "mjs" => raw,
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

export default { StringEnum, calculateCost, createAssistantMessageEventStream };
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

export const Key = {
  ctrlAlt: (key) => ({ kind: "ctrlAlt", key: String(key) }),
};

export default { matchesKey, truncateToWidth, Text, Container, Markdown, Spacer, Key };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "@mariozechner/pi-coding-agent".to_string(),
        r#"
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

export function createBashTool(_cwd, _opts = {}) {
  return {
    name: "bash",
    label: "bash",
    description: "Execute a shell command",
    parameters: { type: "object", properties: { command: { type: "string" } }, required: ["command"] },
    async execute(_id, params) {
      return { content: [{ type: "text", text: String(params?.command ?? "") }], details: {} };
    },
  };
}

export default { parseFrontmatter, getMarkdownTheme, createBashTool };
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

export default { join, dirname };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:url".to_string(),
        r#"
export function fileURLToPath(url) {
  const s = String(url ?? "");
  if (s.startsWith("file://")) return s.slice("file://".length);
  return s;
}
export default { fileURLToPath };
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
export default { homedir };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:child_process".to_string(),
        r#"
export function spawn() {
  throw new Error("node:child_process.spawn is not available in PiJS");
}
export default { spawn };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:module".to_string(),
        r#"
export function createRequire(_path) {
  return function require(_id) {
    throw new Error("require() is not available in PiJS");
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
export function existsSync(_path) { return false; }
export function readFileSync(_path, _encoding) { return ""; }
export function writeFileSync(_path, _data, _opts) { return; }
export function readdirSync(_path, _opts) { return []; }
export function statSync(_path) { throw new Error("statSync unavailable"); }
export default { existsSync, readFileSync, writeFileSync, readdirSync, statSync };
"#
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:fs/promises".to_string(),
        r"
export async function mkdir(_path, _opts) { return; }
export async function writeFile(_path, _data, _opts) { return; }
export default { mkdir, writeFile };
"
        .trim()
        .to_string(),
    );

    modules.insert(
        "node:crypto".to_string(),
        r#"
export function randomUUID() {
  // Not cryptographically secure; sufficient for deterministic tests.
  const r = Math.random().toString(16).slice(2);
  return `00000000-0000-4000-8000-${r.padEnd(12, "0").slice(0, 12)}`;
}
export default { randomUUID };
"#
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
    pub async fn with_clock_and_config(clock: C, config: PiJsRuntimeConfig) -> Result<Self> {
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

                tracing::debug!(
                    event = "pijs.macrotask.hostcall_complete",
                    call_id = %call_id,
                    seq = task.seq.value(),
                    "Delivering hostcall completion"
                );
                // The actual Promise resolution is handled by the global
                // __pi_complete_hostcall function installed in JS
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

                // __pi_env_get_native(key) -> string | null
                global.set(
                    "__pi_env_get_native",
                    Func::from({
                        let env = env.clone();
                        move |_ctx: Ctx<'_>, key: String| -> rquickjs::Result<Option<String>> {
                            let allowed = key == "HOME"
                                || key == "OSTYPE"
                                || key == "OS"
                                || key.starts_with("PI_");
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
const __pi_provider_index = new Map();  // provider_id -> { extensionId, spec }
const __pi_shortcut_index = new Map();  // key_id -> { extensionId, key, description, handler }

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
            providers: new Map(),
            shortcuts: new Map(),
            flags: new Map(),
            flagValues: new Map(),
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
        label: spec.label ? String(spec.label) : name,
        description: spec.description ? String(spec.description) : '',
        parameters: spec.parameters || { type: 'object', properties: {} },
    };

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
    if (typeof spec.handler !== 'function') {
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
        handler: spec.handler,
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

    const providerSpec = {
        id: id,
        baseUrl: spec.baseUrl ? String(spec.baseUrl) : '',
        apiKey: spec.apiKey ? String(spec.apiKey) : '',
        api: spec.api ? String(spec.api) : '',
        models: models,
    };

    if (__pi_provider_index.has(id)) {
        const existing = __pi_provider_index.get(id);
        if (existing && existing.extensionId !== ext.id) {
            throw new Error(`registerProvider: provider id collision: ${id}`);
        }
    }

    const record = { extensionId: ext.id, spec: providerSpec };
    ext.providers.set(id, record);
    __pi_provider_index.set(id, record);
}

const __pi_reserved_keys = new Set(['ctrl+c', 'ctrl+d', 'ctrl+l', 'ctrl+z']);

function __pi_register_shortcut(key, spec) {
    const ext = __pi_current_extension_or_throw();
    if (!spec || typeof spec !== 'object') {
        throw new Error('registerShortcut: spec must be an object');
    }
    if (typeof spec.handler !== 'function') {
        throw new Error('registerShortcut: spec.handler must be a function');
    }

    const keyId = typeof key === 'string' ? key.toLowerCase() : JSON.stringify(key ?? null);
    if (__pi_reserved_keys.has(keyId)) {
        throw new Error('registerShortcut: key ' + keyId + ' is reserved and cannot be overridden');
    }

    const record = {
        key: key,
        keyId: keyId,
        description: spec.description ? String(spec.description) : '',
        handler: spec.handler,
        extensionId: ext.id,
        spec: { key: key, key_id: keyId, description: spec.description ? String(spec.description) : '' },
    };
    ext.shortcuts.set(keyId, record);
    __pi_shortcut_index.set(keyId, record);
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

        out.push({
            id: id,
            name: ext.name,
            version: ext.version,
            api_version: ext.apiVersion,
            tools: tools,
            slash_commands: commands,
            providers: providers,
            shortcuts: shortcuts,
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

    const handlers = __pi_hook_index.get(eventName) || [];
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
	                console.error('Event handler error:', eventName, entry.extensionId, e);
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
	                console.error('Event handler error:', eventName, entry.extensionId, e);
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
	            console.error('Event handler error:', eventName, entry.extensionId, e);
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

// Complete a hostcall (called from Rust)
function __pi_complete_hostcall(call_id, outcome) {
    const pending = __pi_pending_hostcalls.get(call_id);
    if (!pending) {
        console.warn('Unknown hostcall completion:', call_id);
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

    // pi.exec(cmd, args) - execute a shell command
    exec: (cmd, args, options = {}) => __pi_exec_hostcall(cmd, args, options),

    // pi.http(request) - make an HTTP request
    http: __pi_make_hostcall(__pi_http_native),

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
	pi.events.on = (event, handler) => __pi_register_hook(event, handler);

	pi.env = {
	    get: __pi_env_get,
	};

pi.process = {
    cwd: __pi_process_cwd_native(),
    args: __pi_process_args_native(),
};

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

if (typeof globalThis.Buffer === 'undefined') {
    class Buffer extends Uint8Array {
        static from(input, encoding) {
            if (typeof input === 'string') {
                const enc = String(encoding || '').toLowerCase();
                if (enc === 'base64') {
                    const bin = __pi_base64_decode_native(input);
                    const out = new Uint8Array(bin.length);
                    for (let i = 0; i < bin.length; i++) {
                        out[i] = bin.charCodeAt(i) & 0xff;
                    }
                    return out;
                }
                return new TextEncoder().encode(input);
            }
            if (input instanceof ArrayBuffer) {
                return new Uint8Array(input);
            }
            if (ArrayBuffer.isView && ArrayBuffer.isView(input)) {
                return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
            }
            if (Array.isArray(input)) {
                return new Uint8Array(input);
            }
            throw new Error('Buffer.from: unsupported input');
        }
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

if (typeof globalThis.process === 'undefined') {
    const platform =
        __pi_env_get_native('PI_PLATFORM') ||
        __pi_env_get_native('OSTYPE') ||
        __pi_env_get_native('OS') ||
        'linux';

    const envProxy = new Proxy(
        {},
        {
            get(_target, prop) {
                if (typeof prop !== 'string') return undefined;
                const value = __pi_env_get_native(prop);
                return value === null || value === undefined ? undefined : value;
            },
            set(_target, prop, value) {
                // Read-only in PiJS
                return typeof prop === 'string';
            },
            has(_target, prop) {
                if (typeof prop !== 'string') return false;
                const value = __pi_env_get_native(prop);
                return value !== null && value !== undefined;
            },
        },
    );

    globalThis.process = {
        env: envProxy,
        argv: __pi_process_args_native(),
        cwd: () => __pi_process_cwd_native(),
        platform: String(platform).split('-')[0],
        kill: (_pid, _sig) => {
            throw new Error('process.kill is not available in PiJS');
        },
        exit: (_code) => {
            throw new Error('process.exit is not available in PiJS');
        },
    };

    try { Object.freeze(envProxy); } catch (_) {}
    try { Object.freeze(globalThis.process.argv); } catch (_) {}
    try { Object.freeze(globalThis.process); } catch (_) {}
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
                    label: "My Tool".to_string(),
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
}
