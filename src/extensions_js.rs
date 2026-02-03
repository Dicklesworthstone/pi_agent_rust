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
use crate::scheduler::{
    Clock as SchedulerClock, DeterministicClock, HostcallOutcome, Macrotask as SchedulerMacrotask,
    MacrotaskKind as SchedulerMacrotaskKind, Scheduler, WallClock,
};
use rquickjs::function::Func;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Function, IntoJs, Object, Promise, Value};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

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

impl<'js> Default for PendingHostcalls<'js> {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a serde_json::Value to a rquickjs Value.
fn json_to_js<'js>(ctx: &Ctx<'js>, value: &serde_json::Value) -> rquickjs::Result<Value<'js>> {
    match value {
        serde_json::Value::Null => Ok(Value::new_null(ctx.clone())),
        serde_json::Value::Bool(b) => Ok(Value::new_bool(ctx.clone(), *b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::new_int(ctx.clone(), i as i32))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::new_float(ctx.clone(), f))
            } else {
                Ok(Value::new_null(ctx.clone()))
            }
        }
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
fn js_to_json<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> rquickjs::Result<serde_json::Value> {
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
            if let Some(v) = arr.get::<Value<'js>>(i)? {
                result.push(js_to_json(ctx, v)?);
            }
        }
        return Ok(serde_json::Value::Array(result));
    }
    if let Some(obj) = value.as_object() {
        let mut result = serde_json::Map::new();
        for item in obj.props::<String, Value<'js>>() {
            let (k, v) = item?;
            result.insert(k, js_to_json(ctx, v)?);
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
    /// Number of pending hostcalls.
    pub pending_hostcalls: usize,
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
    scheduler: std::cell::RefCell<Scheduler<C>>,
    hostcall_queue: HostcallQueue,
    trace_seq: AtomicU64,
}

impl PiJsRuntime<WallClock> {
    /// Create a new PiJS runtime with the default wall clock.
    pub async fn new() -> Result<Self> {
        Self::with_clock(WallClock).await
    }
}

#[allow(clippy::future_not_send)]
impl<C: SchedulerClock + 'static> PiJsRuntime<C> {
    /// Create a new PiJS runtime with a custom clock.
    pub async fn with_clock(clock: C) -> Result<Self> {
        let runtime = AsyncRuntime::new().map_err(|err| map_js_error(&err))?;
        let context = AsyncContext::full(&runtime)
            .await
            .map_err(|err| map_js_error(&err))?;

        let scheduler = std::cell::RefCell::new(Scheduler::with_clock(clock));
        let hostcall_queue: HostcallQueue = Rc::new(RefCell::new(VecDeque::new()));

        let instance = Self {
            runtime,
            context,
            scheduler,
            hostcall_queue,
            trace_seq: AtomicU64::new(1),
        };

        instance.install_pi_bridge().await?;
        Ok(instance)
    }

    /// Evaluate JavaScript source code.
    pub async fn eval(&self, source: &str) -> Result<()> {
        self.context
            .with(|ctx| ctx.eval::<(), _>(source))
            .await
            .map_err(|err| map_js_error(&err))?;
        // Drain any immediate jobs (Promise.resolve chains, etc.)
        self.drain_jobs().await?;
        Ok(())
    }

    /// Evaluate a JavaScript file.
    pub async fn eval_file(&self, path: &std::path::Path) -> Result<()> {
        self.context
            .with(|ctx| ctx.eval_file::<(), _>(path))
            .await
            .map_err(|err| map_js_error(&err))?;
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
        self.hostcall_queue.borrow().len()
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
        self.scheduler.borrow().has_pending() || !self.hostcall_queue.borrow().is_empty()
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

            // Handle the macrotask inside the JS context
            self.context
                .with(|ctx| {
                    self.handle_macrotask(&ctx, &task)?;
                    Ok::<_, rquickjs::Error>(())
                })
                .await
                .map_err(|err| map_js_error(&err))?;

            // Drain microtasks until fixpoint
            stats.jobs_drained = self.drain_jobs().await?;
        }

        stats.pending_hostcalls = self.hostcall_queue.borrow().len();
        Ok(stats)
    }

    /// Drain all pending QuickJS jobs (microtasks).
    async fn drain_jobs(&self) -> Result<usize> {
        let mut count = 0;
        loop {
            let ran = self
                .runtime
                .execute_pending_job()
                .await
                .map_err(|err| Error::extension(format!("QuickJS job: {err}")))?;
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
                tracing::debug!(
                    event = "pijs.macrotask.hostcall_complete",
                    call_id = %call_id,
                    seq = task.seq.value(),
                    "Delivering hostcall completion"
                );
                // The actual Promise resolution is handled by the global
                // __pi_complete_hostcall function installed in JS
                self.deliver_hostcall_completion(ctx, call_id, outcome)?;
            }
            SMK::TimerFired { timer_id } => {
                tracing::debug!(
                    event = "pijs.macrotask.timer_fired",
                    timer_id = timer_id,
                    seq = task.seq.value(),
                    "Timer fired"
                );
                // Timer callbacks are stored in a JS-side map
                self.deliver_timer_fire(ctx, *timer_id)?;
            }
            SMK::InboundEvent { event_id, payload } => {
                tracing::debug!(
                    event = "pijs.macrotask.inbound_event",
                    event_id = %event_id,
                    seq = task.seq.value(),
                    "Delivering inbound event"
                );
                self.deliver_inbound_event(ctx, event_id, payload)?;
            }
        }
        Ok(())
    }

    /// Deliver a hostcall completion to JS.
    fn deliver_hostcall_completion(
        &self,
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
    fn deliver_timer_fire(&self, ctx: &Ctx<'_>, timer_id: u64) -> rquickjs::Result<()> {
        let global = ctx.globals();
        let fire_fn: Function<'_> = global.get("__pi_fire_timer")?;
        fire_fn.call::<_, ()>((timer_id,))?;
        Ok(())
    }

    /// Deliver an inbound event to JS.
    fn deliver_inbound_event(
        &self,
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
    async fn install_pi_bridge(&self) -> Result<()> {
        let hostcall_queue = self.hostcall_queue.clone();

        self.context
            .with(|ctx| {
                let global = ctx.globals();

                // Install the pending hostcalls storage and helper functions
                ctx.eval::<(), _>(PI_BRIDGE_JS)?;

                // Create the pi object
                let pi = Object::new(ctx.clone())?;

                // Helper to create a hostcall function
                let queue = hostcall_queue.clone();
                let make_hostcall =
                    |kind_factory: fn(String, serde_json::Value) -> HostcallKind| {
                        let queue = queue.clone();
                        move |ctx: Ctx<'_>,
                              name: String,
                              input: Value<'_>|
                              -> rquickjs::Result<Promise<'_>> {
                            let payload = js_to_json(&ctx, input)?;
                            let (promise, resolve, reject) = Promise::new(&ctx)?;

                            // Get call_id from JS bridge
                            let global = ctx.globals();
                            let register_fn: Function<'_> = global.get("__pi_register_hostcall")?;
                            let call_id: String = register_fn.call((resolve, reject))?;

                            // Enqueue the hostcall request
                            let request = HostcallRequest {
                                call_id,
                                kind: kind_factory(name, payload.clone()),
                                payload,
                                trace_id: 0, // Will be set by scheduler
                            };
                            queue.borrow_mut().push_back(request);

                            Ok(promise)
                        }
                    };

                // pi.tool(name, input)
                pi.set(
                    "tool",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        move |ctx: Ctx<'_>,
                              name: String,
                              input: Value<'_>|
                              -> rquickjs::Result<Promise<'_>> {
                            let payload = js_to_json(&ctx, input)?;
                            let (promise, resolve, reject) = Promise::new(&ctx)?;

                            let global = ctx.globals();
                            let register_fn: Function<'_> = global.get("__pi_register_hostcall")?;
                            let call_id: String = register_fn.call((resolve, reject))?;

                            let request = HostcallRequest {
                                call_id,
                                kind: HostcallKind::Tool { name },
                                payload,
                                trace_id: 0,
                            };
                            queue.borrow_mut().push_back(request);

                            Ok(promise)
                        }
                    }),
                )?;

                // pi.exec(cmd, args)
                pi.set(
                    "exec",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        move |ctx: Ctx<'_>,
                              cmd: String,
                              args: Value<'_>|
                              -> rquickjs::Result<Promise<'_>> {
                            let payload = js_to_json(&ctx, args)?;
                            let (promise, resolve, reject) = Promise::new(&ctx)?;

                            let global = ctx.globals();
                            let register_fn: Function<'_> = global.get("__pi_register_hostcall")?;
                            let call_id: String = register_fn.call((resolve, reject))?;

                            let request = HostcallRequest {
                                call_id,
                                kind: HostcallKind::Exec { cmd },
                                payload,
                                trace_id: 0,
                            };
                            queue.borrow_mut().push_back(request);

                            Ok(promise)
                        }
                    }),
                )?;

                // pi.http(request)
                pi.set(
                    "http",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        move |ctx: Ctx<'_>, req: Value<'_>| -> rquickjs::Result<Promise<'_>> {
                            let payload = js_to_json(&ctx, req)?;
                            let (promise, resolve, reject) = Promise::new(&ctx)?;

                            let global = ctx.globals();
                            let register_fn: Function<'_> = global.get("__pi_register_hostcall")?;
                            let call_id: String = register_fn.call((resolve, reject))?;

                            let request = HostcallRequest {
                                call_id,
                                kind: HostcallKind::Http,
                                payload,
                                trace_id: 0,
                            };
                            queue.borrow_mut().push_back(request);

                            Ok(promise)
                        }
                    }),
                )?;

                // pi.session(op, args)
                pi.set(
                    "session",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        move |ctx: Ctx<'_>,
                              op: String,
                              args: Value<'_>|
                              -> rquickjs::Result<Promise<'_>> {
                            let payload = js_to_json(&ctx, args)?;
                            let (promise, resolve, reject) = Promise::new(&ctx)?;

                            let global = ctx.globals();
                            let register_fn: Function<'_> = global.get("__pi_register_hostcall")?;
                            let call_id: String = register_fn.call((resolve, reject))?;

                            let request = HostcallRequest {
                                call_id,
                                kind: HostcallKind::Session { op },
                                payload,
                                trace_id: 0,
                            };
                            queue.borrow_mut().push_back(request);

                            Ok(promise)
                        }
                    }),
                )?;

                // pi.ui(op, args)
                pi.set(
                    "ui",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        move |ctx: Ctx<'_>,
                              op: String,
                              args: Value<'_>|
                              -> rquickjs::Result<Promise<'_>> {
                            let payload = js_to_json(&ctx, args)?;
                            let (promise, resolve, reject) = Promise::new(&ctx)?;

                            let global = ctx.globals();
                            let register_fn: Function<'_> = global.get("__pi_register_hostcall")?;
                            let call_id: String = register_fn.call((resolve, reject))?;

                            let request = HostcallRequest {
                                call_id,
                                kind: HostcallKind::Ui { op },
                                payload,
                                trace_id: 0,
                            };
                            queue.borrow_mut().push_back(request);

                            Ok(promise)
                        }
                    }),
                )?;

                // pi.events(op, args)
                pi.set(
                    "events",
                    Func::from({
                        let queue = hostcall_queue.clone();
                        move |ctx: Ctx<'_>,
                              op: String,
                              args: Value<'_>|
                              -> rquickjs::Result<Promise<'_>> {
                            let payload = js_to_json(&ctx, args)?;
                            let (promise, resolve, reject) = Promise::new(&ctx)?;

                            let global = ctx.globals();
                            let register_fn: Function<'_> = global.get("__pi_register_hostcall")?;
                            let call_id: String = register_fn.call((resolve, reject))?;

                            let request = HostcallRequest {
                                call_id,
                                kind: HostcallKind::Events { op },
                                payload,
                                trace_id: 0,
                            };
                            queue.borrow_mut().push_back(request);

                            Ok(promise)
                        }
                    }),
                )?;

                global.set("pi", pi)?;
                Ok(())
            })
            .await
            .map_err(|err| map_js_error(&err))?;

        Ok(())
    }
}

/// JavaScript bridge code for managing pending hostcalls and timer callbacks.
const PI_BRIDGE_JS: &str = r#"
// Pending hostcalls: call_id -> { resolve, reject }
const __pi_pending_hostcalls = new Map();
let __pi_next_call_id = 1;

// Timer callbacks: timer_id -> callback
const __pi_timer_callbacks = new Map();

// Event listeners: event_id -> [callback, ...]
const __pi_event_listeners = new Map();

// Register a new pending hostcall, returns the call_id
function __pi_register_hostcall(resolve, reject) {
    const call_id = `call-${__pi_next_call_id++}`;
    __pi_pending_hostcalls.set(call_id, { resolve, reject });
    return call_id;
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
"#;

#[cfg(test)]
mod tests {
    use super::*;

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
}
