//! Persistent JavaScript kernel for the eval tool (bd-cv653.1.4).
//!
//! A dedicated OS thread owns a **sync** QuickJS `Runtime` + `Context` — a
//! separate trust domain from the extension realm (`extensions_js`), with no
//! module loader and no extension hostcalls. Cells evaluate as scripts (the
//! completion value of the last statement returns, REPL-style), `console.*`
//! output is captured per cell, globals persist across cells, and top-level
//! promises are settled by pumping pending jobs. Per-cell timeouts use the
//! QuickJS interrupt handler — the cell aborts but the KERNEL (and its
//! state) survives, unlike the Python kernel's kill-and-restart.
//!
//! The tool re-entry bridge mirrors the Python design: `tool.read(...)` etc.
//! block the kernel thread on a channel round-trip that the host resolves
//! through the same whitelisted tool implementations.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A cell request for the kernel thread.
pub(crate) struct JsCellRequest {
    pub code: String,
    pub deadline: Instant,
    pub reply: Sender<JsCellResponse>,
}

/// Cell outcome from the kernel thread.
pub(crate) struct JsCellResponse {
    pub ok: bool,
    pub console: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// A tool re-entry request from inside a JS cell.
pub(crate) struct JsBridgeRequest {
    pub tool: String,
    pub input: Value,
    pub reply: Sender<Result<String, String>>,
}

pub(crate) struct JsKernel {
    requests: Sender<JsCellRequest>,
    pub bridge: Receiver<JsBridgeRequest>,
    pub cells_run: u64,
}

impl JsKernel {
    pub fn spawn() -> std::io::Result<Self> {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<JsCellRequest>();
        let (bridge_tx, bridge_rx) = std::sync::mpsc::channel::<JsBridgeRequest>();
        std::thread::Builder::new()
            .name("eval-js-kernel".into())
            .spawn(move || kernel_thread(&req_rx, &bridge_tx))?;
        Ok(Self {
            requests: req_tx,
            bridge: bridge_rx,
            cells_run: 0,
        })
    }

    /// Submit a cell; the response arrives on the returned receiver. The
    /// kernel thread exits when the sender side is dropped.
    pub fn submit(
        &self,
        code: String,
        deadline: Instant,
    ) -> Result<Receiver<JsCellResponse>, String> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.requests
            .send(JsCellRequest {
                code,
                deadline,
                reply: reply_tx,
            })
            .map_err(|_| String::from("kernel thread gone"))?;
        Ok(reply_rx)
    }
}

/// Deadline cell shared with the interrupt handler.
#[derive(Clone)]
struct DeadlineCell(Arc<AtomicU64>);

impl DeadlineCell {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(u64::MAX)))
    }
    fn set(&self, deadline: Instant, origin: Instant) {
        let millis = deadline
            .saturating_duration_since(origin)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.0.store(millis, Ordering::SeqCst);
    }
    fn clear(&self) {
        self.0.store(u64::MAX, Ordering::SeqCst);
    }
    fn expired(&self, origin: Instant) -> bool {
        let budget = self.0.load(Ordering::SeqCst);
        budget != u64::MAX && origin.elapsed().as_millis() as u64 > budget
    }
}

fn kernel_thread(requests: &Receiver<JsCellRequest>, bridge_tx: &Sender<JsBridgeRequest>) {
    let Ok(runtime) = rquickjs::Runtime::new() else {
        return;
    };
    let Ok(context) = rquickjs::Context::full(&runtime) else {
        return;
    };

    let origin = Instant::now();
    let deadline = DeadlineCell::new();
    {
        let deadline = deadline.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || deadline.expired(origin))));
    }

    let console: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    // Install the console shim + tool bridge once; globals persist.
    context.with(|ctx| {
        install_globals(&ctx, &console, bridge_tx);
    });

    while let Ok(request) = requests.recv() {
        deadline.set(request.deadline, origin);
        console.borrow_mut().clear();
        let response = context.with(|ctx| run_cell(&ctx, &runtime, &request, &deadline, origin));
        deadline.clear();
        let response = JsCellResponse {
            console: std::mem::take(&mut *console.borrow_mut()),
            ..response
        };
        let _ = request.reply.send(response);
    }
}

fn install_globals(
    ctx: &rquickjs::Ctx<'_>,
    console: &Rc<RefCell<String>>,
    bridge_tx: &Sender<JsBridgeRequest>,
) {
    use rquickjs::function::Func;

    let globals = ctx.globals();

    // console.log / console.error append to the per-cell buffer.
    let sink = Rc::clone(console);
    let log = Func::from(
        move |value: rquickjs::function::Rest<rquickjs::Coerced<String>>| {
            let mut buffer = sink.borrow_mut();
            let parts: Vec<String> = value.0.into_iter().map(|part| part.0).collect();
            buffer.push_str(&parts.join(" "));
            buffer.push('\n');
        },
    );
    let console_obj = rquickjs::Object::new(ctx.clone()).expect("console object");
    console_obj.set("log", log).expect("console.log");
    let sink = Rc::clone(console);
    let error = Func::from(
        move |value: rquickjs::function::Rest<rquickjs::Coerced<String>>| {
            let mut buffer = sink.borrow_mut();
            let parts: Vec<String> = value.0.into_iter().map(|part| part.0).collect();
            buffer.push_str(&parts.join(" "));
            buffer.push('\n');
        },
    );
    console_obj.set("error", error).expect("console.error");
    globals.set("console", console_obj).expect("console global");

    // Low-level bridge: (tool, payloadJson) -> content string. Typed as
    // plain Strings so no JS value lifetimes couple into the closure; the
    // ergonomic `tool.*` API wraps it in JS below.
    let tx = bridge_tx.clone();
    let bridge_fn = Func::from(move |ctx: rquickjs::Ctx<'_>, tool: String, payload: String| {
        let input: Value = serde_json::from_str(&payload).unwrap_or_else(|_| json!({}));
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let _ = tx.send(JsBridgeRequest {
            tool,
            input,
            reply: reply_tx,
        });
        match reply_rx.recv_timeout(Duration::from_secs(120)) {
            Ok(Ok(content)) => Ok(content),
            Ok(Err(err)) => Err(rquickjs::Exception::throw_message(&ctx, &err)),
            Err(_) => Err(rquickjs::Exception::throw_message(
                &ctx,
                "EVAL_BRIDGE_TIMEOUT: host did not answer",
            )),
        }
    });
    globals.set("__pi_bridge", bridge_fn).expect("bridge fn");
    let _: () = ctx
        .eval(
            r#"globalThis.tool = {
                read: (path, extra) => __pi_bridge('read', JSON.stringify({ path, ...(extra || {}) })),
                grep: (pattern, extra) => __pi_bridge('grep', JSON.stringify({ pattern, ...(extra || {}) })),
                find: (pattern, extra) => __pi_bridge('find', JSON.stringify({ pattern, ...(extra || {}) })),
                ls: (path, extra) => __pi_bridge('ls', JSON.stringify({ path: path || '.', ...(extra || {}) })),
            };"#,
        )
        .expect("tool wrapper");
}

fn run_cell(
    ctx: &rquickjs::Ctx<'_>,
    runtime: &rquickjs::Runtime,
    request: &JsCellRequest,
    deadline: &DeadlineCell,
    origin: Instant,
) -> JsCellResponse {
    let evaluated: rquickjs::Result<rquickjs::Value<'_>> = ctx.eval(request.code.as_bytes());
    let value = match evaluated {
        Ok(value) => value,
        Err(err) => return error_response(ctx, &err),
    };

    // Settle a top-level promise by pumping jobs under the same deadline.
    let value = if let Ok(promise) = rquickjs::Promise::from_value(value.clone()) {
        loop {
            match promise.state() {
                rquickjs::promise::PromiseState::Pending => {
                    if deadline.expired(origin) {
                        return JsCellResponse {
                            ok: false,
                            console: String::new(),
                            result: None,
                            error: Some(String::from(
                                "EVAL_TIMEOUT: promise still pending at the cell budget \
                                 (kernel state preserved)",
                            )),
                        };
                    }
                    match runtime.execute_pending_job() {
                        Ok(true) => {}
                        Ok(false) => std::thread::sleep(Duration::from_millis(5)),
                        Err(_) => break promise.result::<rquickjs::Value<'_>>(),
                    }
                }
                _ => break promise.result::<rquickjs::Value<'_>>(),
            }
        }
        .map_or_else(
            || Ok(rquickjs::Value::new_undefined(ctx.clone())),
            |result| result,
        )
    } else {
        Ok(value)
    };
    // Drain any remaining microtasks queued by the cell.
    while matches!(runtime.execute_pending_job(), Ok(true)) {}

    match value {
        Ok(value) => {
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
                    rquickjs::Coerced::<String>::from_js(ctx, value)
                        .ok()
                        .map(|coerced| coerced.0)
                })
            };
            JsCellResponse {
                ok: true,
                console: String::new(),
                result,
                error: None,
            }
        }
        Err(err) => error_response(ctx, &err),
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
    }
}

use rquickjs::FromJs;
