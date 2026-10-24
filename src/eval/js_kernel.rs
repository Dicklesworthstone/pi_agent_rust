//! Persistent JavaScript kernel for the eval tool (bd-cv653.1.4).
//!
//! A dedicated OS thread owns a **sync** QuickJS `Runtime` + `Context` — a
//! separate trust domain from the extension realm (`extensions_js`), with a
//! workspace-only ES module loader and no extension hostcalls. Cells evaluate as scripts (the
//! completion value of the last statement returns, REPL-style), `console.*`
//! output streams through an invocation-owned bounded channel, globals persist
//! across cells, and top-level promises settle by pumping pending jobs.
//! Per-cell timeouts use the QuickJS interrupt handler. A timeout discards the
//! realm with an explicit state-loss diagnostic so stale promise continuations
//! cannot run in a later cell.
//!
//! The tool re-entry bridge mirrors the Python design: `tool.read(...)` etc.
//! block the kernel thread on a channel round-trip that the host resolves
//! through the invocation's host policy. `tool.call` is awaitable;
//! timers and promise replies advance while the cell awaits its result.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::time::{Duration, Instant};

mod async_host;
mod console;
mod display;
mod evaluate;
#[cfg(test)]
mod evaluation_tests;
mod modules;

/// A cell request for the kernel thread.
pub struct JsCellRequest {
    pub code: String,
    pub deadline: Instant,
    pub reply: Sender<JsCellResponse>,
    pub output: Option<SyncSender<Value>>,
    pub filename: Option<String>,
    pub owner: crate::agent_cx::AgentCx,
}

/// Cell outcome from the kernel thread.
pub struct JsCellResponse {
    pub ok: bool,
    pub console: String,
    pub result: Option<String>,
    pub error: Option<String>,
    /// Detached JSON snapshots, independently validated by the Tool host.
    pub displays: Vec<Value>,
}

/// A tool re-entry request from inside a JS cell.
pub struct JsBridgeRequest {
    pub tool: String,
    pub input: Value,
    pub reply: Sender<Result<String, String>>,
}

pub struct JsKernel {
    requests: Sender<JsCellRequest>,
    pub bridge: Receiver<JsBridgeRequest>,
    pub cells_run: u64,
    stopped: Arc<AtomicBool>,
}

impl JsKernel {
    #[cfg(test)]
    pub fn spawn() -> std::io::Result<Self> {
        Self::spawn_in(&std::env::current_dir()?)
    }

    pub fn spawn_in(cwd: &Path) -> std::io::Result<Self> {
        // Freeze relative workspace spelling now, not on a later worker poll.
        // Canonicalization and all actual file reads require a cell owner.
        let workspace = std::path::absolute(cwd)?;
        let (req_tx, req_rx) = std::sync::mpsc::channel::<JsCellRequest>();
        let (bridge_tx, bridge_rx) = std::sync::mpsc::channel::<JsBridgeRequest>();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        std::thread::Builder::new()
            .name("eval-js-kernel".into())
            .spawn(move || kernel_thread(&req_rx, &bridge_tx, &worker_stopped, &workspace))?;
        Ok(Self {
            requests: req_tx,
            bridge: bridge_rx,
            cells_run: 0,
            stopped,
        })
    }

    /// Raw kernel tests may collect bounded console output without a host.
    #[cfg(test)]
    pub fn submit(
        &self,
        code: String,
        deadline: Instant,
    ) -> Result<Receiver<JsCellResponse>, String> {
        self.submit_with_output(code, deadline, None)
    }

    /// The output sender belongs to this request alone and closes before its
    /// final reply. Revoking the kernel also interrupts blocked output delivery.
    #[cfg(test)]
    pub fn submit_with_output(
        &self,
        code: String,
        deadline: Instant,
        output: Option<SyncSender<Value>>,
    ) -> Result<Receiver<JsCellResponse>, String> {
        self.submit_source_with_output(code, None, deadline, output)
    }

    pub fn submit_source_with_output(
        &self,
        code: String,
        filename: Option<String>,
        deadline: Instant,
        output: Option<SyncSender<Value>>,
    ) -> Result<Receiver<JsCellResponse>, String> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.requests
            .send(JsCellRequest {
                code,
                deadline,
                reply: reply_tx,
                output,
                filename,
                owner: crate::agent_cx::AgentCx::for_current_or_request(),
            })
            .map_err(|_| String::from("kernel thread gone"))?;
        Ok(reply_rx)
    }
}

impl Drop for JsKernel {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// Deadline cell shared with the interrupt handler.
#[derive(Clone)]
struct DeadlineCell(Arc<AtomicU64>, Arc<AtomicBool>);

impl DeadlineCell {
    fn new(stopped: Arc<AtomicBool>) -> Self {
        Self(Arc::new(AtomicU64::new(u64::MAX)), stopped)
    }
    fn set(&self, deadline: Instant, origin: Instant) {
        let millis = u64::try_from(deadline.saturating_duration_since(origin).as_millis())
            .unwrap_or(u64::MAX);
        self.0.store(millis, Ordering::SeqCst);
    }
    fn clear(&self) {
        self.0.store(u64::MAX, Ordering::SeqCst);
    }
    fn expired(&self, origin: Instant) -> bool {
        let budget = self.0.load(Ordering::SeqCst);
        self.1.load(Ordering::SeqCst)
            || (budget != u64::MAX
                && u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX) >= budget)
    }
}

#[allow(clippy::too_many_lines)] // one realm lifecycle: build, serve cells, rebuild on taint
fn kernel_thread(
    requests: &Receiver<JsCellRequest>,
    bridge_tx: &Sender<JsBridgeRequest>,
    stopped: &Arc<AtomicBool>,
    workspace: &Path,
) {
    // A timed-out realm may still contain promise continuations. Rebuild it
    // before another cell rather than running old work under a new deadline.
    loop {
        if stopped.load(Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = rquickjs::Runtime::new() else {
            return;
        };
        let modules = modules::ModuleFiles::new(workspace.to_path_buf(), Arc::clone(stopped));
        runtime.set_loader(modules.clone(), modules.clone());
        let Ok(context) = rquickjs::Context::full(&runtime) else {
            return;
        };
        let origin = Instant::now();
        let deadline = DeadlineCell::new(Arc::clone(stopped));
        {
            let deadline = deadline.clone();
            runtime.set_interrupt_handler(Some(Box::new(move || deadline.expired(origin))));
        }
        let console = Rc::new(RefCell::new(console::ConsoleCapture::new()));
        context.with(|ctx| install_globals(&ctx, &console, bridge_tx, &deadline, origin));
        if context.with(|ctx| modules.install_reload(&ctx)).is_err() {
            return;
        }
        let Ok(evaluator) = context.with(|ctx| evaluate::Evaluator::new(&ctx)) else {
            return;
        };
        let Ok(host) =
            context.with(|ctx| async_host::AsyncHost::install(&ctx, Arc::clone(stopped)))
        else {
            return;
        };
        let Ok(displays) =
            context.with(|ctx| display::DisplaySink::install(&ctx, deadline.clone(), origin))
        else {
            return;
        };
        let mut rebuild = false;
        while let Ok(mut request) = requests.recv() {
            if stopped.load(Ordering::SeqCst) {
                return;
            }
            deadline.set(request.deadline, origin);
            modules.begin(request.owner.clone(), request.deadline);
            host.begin(request.deadline);
            displays.begin_with_output(request.output.clone());
            console.borrow_mut().begin(request.output.take());
            let mut response = run_cell(
                &context, &runtime, &request, &deadline, origin, &evaluator, &host, bridge_tx,
            );
            let display_error = displays.failure();
            let display_failed = display_error.is_some();
            response.displays = displays.finish();
            let console_result = console.borrow_mut().finish();
            let console_failed = console_result.is_err();
            match console_result {
                Ok(text) => response.console = text,
                Err(error) => {
                    response.ok = false;
                    response.result = None;
                    response.error = Some(format!("EVAL_CONSOLE_CAPTURE: {error}"));
                }
            }
            if let Some(error) = display_error {
                // A guest catch cannot hide a disconnected/backpressured
                // rich stream or make this realm reusable after data loss.
                response.ok = false;
                response.result = None;
                let detail = response.error.take().unwrap_or_default();
                response.error = Some(format!("EVAL_DISPLAY_STREAM: {error}; {detail}"));
            }
            let expired = deadline.expired(origin);
            if stopped.load(Ordering::SeqCst) {
                return;
            }
            if expired {
                let detail = response.error.take().unwrap_or_default();
                response.error = Some(format!(
                    "EVAL_TIMEOUT: JavaScript cell budget exhausted; {detail}"
                ));
            }
            // Cleanup uses captured built-ins only, not arbitrary guest code.
            deadline.clear();
            let cleanup_failed = context.with(|ctx| host.finish(&ctx)).is_err();
            modules.finish();
            rebuild = expired
                || console_failed
                || display_failed
                || cleanup_failed
                || runtime.is_job_pending();
            if rebuild {
                response.ok = false;
                response.result = None;
                let error = response.error.get_or_insert_with(|| {
                    "EVAL_ASYNC_INCOMPLETE: unfinished JavaScript work".into()
                });
                error
                    .push_str("; EVAL_STATE_RESET: kernel state discarded, next cell starts fresh");
            }
            let _ = request.reply.send(response);
            if rebuild {
                break;
            }
        }
        if !rebuild {
            return;
        }
        // Persistent evaluator/host handles drop before context and runtime.
    }
}

fn install_globals(
    ctx: &rquickjs::Ctx<'_>,
    console: &Rc<RefCell<console::ConsoleCapture>>,
    bridge_tx: &Sender<JsBridgeRequest>,
    deadline: &DeadlineCell,
    origin: Instant,
) {
    use rquickjs::function::Func;

    let globals = ctx.globals();
    console::install(ctx, console, deadline, origin).expect("console globals");

    // Low-level bridge: (tool, payloadJson) -> content string. Typed as
    // plain Strings so no JS value lifetimes couple into the closure; the
    // ergonomic `tool.*` API wraps it in JS below.
    let tx = bridge_tx.clone();
    let deadline = deadline.clone();
    let bridge_fn = Func::from(
        move |ctx: rquickjs::Ctx<'_>, tool: String, payload: String| {
            if deadline.expired(origin) {
                return Err(rquickjs::Exception::throw_message(
                    &ctx,
                    "EVAL_TIMEOUT: cell owner expired",
                ));
            }
            let input: Value = serde_json::from_str(&payload).unwrap_or_else(|_| json!({}));
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            tx.send(JsBridgeRequest {
                tool,
                input,
                reply: reply_tx,
            })
            .map_err(|_| {
                rquickjs::Exception::throw_message(&ctx, "EVAL_BRIDGE_CLOSED: host disconnected")
            })?;
            loop {
                if deadline.expired(origin) {
                    return Err(rquickjs::Exception::throw_message(
                        &ctx,
                        "EVAL_TIMEOUT: tool reply exceeded the cell budget",
                    ));
                }
                match reply_rx.recv_timeout(Duration::from_millis(5)) {
                    Ok(Ok(content)) => return Ok(content),
                    Ok(Err(err)) => return Err(rquickjs::Exception::throw_message(&ctx, &err)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(rquickjs::Exception::throw_message(
                            &ctx,
                            "EVAL_BRIDGE_CLOSED: host disconnected",
                        ));
                    }
                }
            }
        },
    );
    globals.set("__pi_bridge", bridge_fn).expect("bridge fn");
    let _: () = ctx
        .eval(
            r"globalThis.tool = {
                read: (path, extra) => __pi_bridge('read', JSON.stringify({ path, ...(extra || {}) })),
                grep: (pattern, extra) => __pi_bridge('grep', JSON.stringify({ pattern, ...(extra || {}) })),
                find: (pattern, extra) => __pi_bridge('find', JSON.stringify({ pattern, ...(extra || {}) })),
                ls: (path, extra) => __pi_bridge('ls', JSON.stringify({ path: path || '.', ...(extra || {}) })),
            };",
        )
        .expect("tool wrapper");
}

#[allow(clippy::too_many_arguments)] // one invocation owns deadline, evaluator and host reactor
fn run_cell(
    context: &rquickjs::Context,
    runtime: &rquickjs::Runtime,
    request: &JsCellRequest,
    deadline: &DeadlineCell,
    origin: Instant,
    evaluator: &evaluate::Evaluator,
    host: &async_host::AsyncHost,
    bridge_tx: &Sender<JsBridgeRequest>,
) -> JsCellResponse {
    // Phase 1: evaluate. A returned promise is saved as a Persistent so job
    // pumping can happen OUTSIDE Context::with (execute_pending_job inside
    // with() double-borrows the runtime's inner RefCell).
    enum Phase1 {
        Done(JsCellResponse),
        Pending(rquickjs::Persistent<rquickjs::Promise<'static>>),
    }
    let phase1 = context.with(|ctx| {
        // Select the grammar before executing any user statements. A runtime
        // SyntaxError (including JSON.parse failures) never retries the cell.
        let evaluated = evaluator.evaluate(&ctx, &request.code, request.filename.as_deref());
        match evaluated {
            Err(err) => Phase1::Done(error_response(&ctx, &err)),
            Ok(value) => rquickjs::Promise::from_value(value.clone()).map_or_else(
                |_| Phase1::Done(extract_result(&ctx, &value)),
                |promise| Phase1::Pending(rquickjs::Persistent::save(&ctx, promise)),
            ),
        }
    });

    let saved = match phase1 {
        Phase1::Done(response) => {
            // Drain microtasks the cell queued (outside with()).
            if let Some(error) = drain_jobs(runtime, deadline, origin) {
                return error;
            }
            return response;
        }
        Phase1::Pending(saved) => saved,
    };

    // Phase 2: pump jobs until the promise settles or the budget expires.
    loop {
        let state = context.with(|ctx| {
            let promise = saved.clone().restore(&ctx).ok();
            promise.map(|p| p.state())
        });
        match state {
            Some(rquickjs::promise::PromiseState::Pending) => {
                if deadline.expired(origin) {
                    return JsCellResponse {
                        ok: false,
                        console: String::new(),
                        result: None,
                        error: Some(String::from(
                            "EVAL_TIMEOUT: promise still pending at the cell budget",
                        )),
                        displays: Vec::new(),
                    };
                }
                match runtime.execute_pending_job() {
                    Ok(true) => {}
                    Ok(false) => {
                        let progressed = context.with(|ctx| {
                            host.pump(&ctx, bridge_tx)
                                .map_err(|error| error_response(&ctx, &error))
                        });
                        match progressed {
                            Ok(true) => {}
                            Ok(false) => std::thread::sleep(Duration::from_millis(5)),
                            Err(response) => return response,
                        }
                    }
                    Err(error) => {
                        return error
                            .0
                            .with(|ctx| error_response(&ctx, &rquickjs::Error::Exception));
                    }
                }
            }
            _ => break,
        }
    }
    if let Some(error) = drain_jobs(runtime, deadline, origin) {
        return error;
    }

    // Phase 3: extract the settled value.
    context.with(|ctx| {
        let Some(promise) = saved.clone().restore(&ctx).ok() else {
            return JsCellResponse {
                ok: false,
                console: String::new(),
                result: None,
                error: Some(String::from("EVAL_PROTOCOL: promise restore failed")),
                displays: Vec::new(),
            };
        };
        match promise.result::<rquickjs::Value<'_>>() {
            Some(Ok(value)) => extract_result(&ctx, &value),
            Some(Err(err)) => error_response(&ctx, &err),
            None => JsCellResponse {
                ok: false,
                console: String::new(),
                result: None,
                error: Some(String::from("EVAL_PROTOCOL: promise never settled")),
                displays: Vec::new(),
            },
        }
    })
}

fn drain_jobs(
    runtime: &rquickjs::Runtime,
    deadline: &DeadlineCell,
    origin: Instant,
) -> Option<JsCellResponse> {
    loop {
        if deadline.expired(origin) {
            return Some(JsCellResponse {
                ok: false,
                console: String::new(),
                result: None,
                error: Some("EVAL_TIMEOUT: microtasks exceeded the cell budget".into()),
                displays: Vec::new(),
            });
        }
        match runtime.execute_pending_job() {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => {
                return Some(
                    error
                        .0
                        .with(|ctx| error_response(&ctx, &rquickjs::Error::Exception)),
                );
            }
        }
    }
}

fn extract_result<'js>(ctx: &rquickjs::Ctx<'js>, value: &rquickjs::Value<'js>) -> JsCellResponse {
    let result = if value.is_undefined() || value.is_null() {
        None
    } else {
        ctx.json_stringify_replacer_space(
            value.clone(),
            rquickjs::Value::new_undefined(ctx.clone()),
            rquickjs::Value::new_undefined(ctx.clone()),
        )
        .ok()
        .flatten()
        .and_then(|s| s.to_string().ok())
        .or_else(|| {
            rquickjs::Coerced::<String>::from_js(ctx, value.clone())
                .ok()
                .map(|coerced| coerced.0)
        })
    };
    JsCellResponse {
        ok: true,
        console: String::new(),
        result,
        error: None,
        displays: Vec::new(),
    }
}

fn error_response(ctx: &rquickjs::Ctx<'_>, err: &rquickjs::Error) -> JsCellResponse {
    let detail = if err.is_exception() {
        ctx.catch()
            .as_exception()
            .map_or_else(|| err.to_string(), |exception| format!("{exception}"))
    } else {
        err.to_string()
    };
    JsCellResponse {
        ok: false,
        console: String::new(),
        result: None,
        error: Some(detail),
        displays: Vec::new(),
    }
}

use rquickjs::FromJs;
