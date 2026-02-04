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
use rquickjs::function::Func;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Function, IntoJs, Object, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};
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
fn json_to_js<'js>(ctx: &Ctx<'js>, value: &serde_json::Value) -> rquickjs::Result<Value<'js>> {
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
fn js_to_json(value: &Value<'_>) -> rquickjs::Result<serde_json::Value> {
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
    Error::extension(format!("QuickJS: {err}"))
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

    /// Drain pending hostcall requests from the queue.
    ///
    /// Returns the requests that need to be processed by the host.
    /// After processing, call `complete_hostcall()` for each.
    pub fn drain_hostcall_requests(&self) -> VecDeque<HostcallRequest> {
        std::mem::take(&mut *self.hostcall_queue.borrow_mut())
    }

    /// Peek at pending hostcall requests without draining.
    pub fn pending_hostcall_count(&self) -> usize {
        self.hostcall_tracker.borrow().pending_count()
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
                        move |_ctx: Ctx<'_>,
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
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Tool { name },
                                payload,
                                trace_id,
                            };
                            queue.borrow_mut().push_back(request);
                            Ok(call_id)
                        }
                    }),
                )?;

                // __pi_exec_native(cmd, args) -> call_id
                global.set(
                    "__pi_exec_native",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        let tracker = hostcall_tracker.clone();
                        let scheduler = Rc::clone(&scheduler);
                        let hostcalls_total = Arc::clone(&hostcalls_total);
                        let trace_seq = Arc::clone(&trace_seq);
                        move |_ctx: Ctx<'_>,
                              cmd: String,
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
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Exec { cmd },
                                payload,
                                trace_id,
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
                        move |_ctx: Ctx<'_>, req: Value<'_>| -> rquickjs::Result<String> {
                            let payload = js_to_json(&req)?;
                            let call_id = format!("call-{}", generate_call_id());
                            hostcalls_total.fetch_add(1, AtomicOrdering::SeqCst);
                            let trace_id = trace_seq.fetch_add(1, AtomicOrdering::SeqCst);
                            let timeout_ms = default_hostcall_timeout_ms.filter(|ms| *ms > 0);
                            let timer_id =
                                timeout_ms.map(|ms| scheduler.borrow_mut().set_timeout(ms));
                            tracker.borrow_mut().register(call_id.clone(), timer_id);
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Http,
                                payload,
                                trace_id,
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
                        move |_ctx: Ctx<'_>,
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
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Session { op },
                                payload,
                                trace_id,
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
                        move |_ctx: Ctx<'_>,
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
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Ui { op },
                                payload,
                                trace_id,
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
                        move |_ctx: Ctx<'_>,
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
                            let request = HostcallRequest {
                                call_id: call_id.clone(),
                                kind: HostcallKind::Events { op },
                                payload,
                                trace_id,
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
                            tracing::debug!(event = "pijs.env.get", key = %key, "env get");
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

                // Install the JS bridge that creates Promises and wraps the native functions
                ctx.eval::<(), _>(PI_BRIDGE_JS)?;

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
const pi = {
    // pi.tool(name, input) - invoke a tool
    tool: __pi_make_hostcall(__pi_tool_native),

    // pi.exec(cmd, args) - execute a shell command
    exec: __pi_make_hostcall(__pi_exec_native),

    // pi.http(request) - make an HTTP request
    http: __pi_make_hostcall(__pi_http_native),

    // pi.session(op, args) - session operations
    session: __pi_make_hostcall(__pi_session_native),

    // pi.ui(op, args) - UI operations
    ui: __pi_make_hostcall(__pi_ui_native),

    // pi.events(op, args) - event operations
    events: __pi_make_hostcall(__pi_events_native),
};

pi.env = {
    get: __pi_env_get,
};

pi.process = {
    cwd: __pi_process_cwd_native(),
    args: __pi_process_args_native(),
};

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
        });
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
            assert_eq!(requests.len(), 3);

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
}
