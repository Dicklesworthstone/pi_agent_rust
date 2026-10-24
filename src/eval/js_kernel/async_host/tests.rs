use super::*;
use crate::eval::js_kernel::{JsCellResponse, JsKernel};

fn execute(kernel: &JsKernel, code: &str) -> JsCellResponse {
    kernel
        .submit(code.into(), Instant::now() + Duration::from_secs(5))
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .expect("cell must finish")
}

fn value(kernel: &JsKernel, code: &str) -> String {
    let response = execute(kernel, code);
    assert!(response.ok, "{:?}", response.error);
    response.result.unwrap_or_default()
}

fn state() -> State {
    let mut state = State::new(Arc::new(AtomicBool::new(false)));
    state.deadline = Some(Instant::now() + Duration::from_secs(10));
    state
}

#[test]
fn timeout_callbacks_receive_arguments_and_capture_output() {
    let kernel = JsKernel::spawn().unwrap();
    let response = execute(
        &kernel,
        "new Promise(resolve => setTimeout((a,b) => { console.log(a+b); resolve(a+b); }, 0, 19, 23))",
    );
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(response.result.as_deref(), Some("42"));
    assert_eq!(response.console, "42\n");
}

#[test]
fn sleep_and_nested_timers_settle_top_level_await() {
    let kernel = JsKernel::spawn().unwrap();
    assert!(execute(&kernel, "await sleep(0); globalThis.answer = await new Promise(resolve => setTimeout(() => setTimeout(resolve, 0, 42), 0))").ok);
    assert_eq!(value(&kernel, "answer"), "42");
}

#[test]
fn interval_can_cancel_itself_and_clear_timeout_prevents_callback() {
    let kernel = JsKernel::spawn().unwrap();
    assert_eq!(
        value(
            &kernel,
            "new Promise(resolve => { let ticks=0; const abandoned=setTimeout(()=>{ticks=99}, 0); clearTimeout(abandoned); const id=setInterval(()=>{ if (++ticks === 3) { clearInterval(id); resolve(ticks); } }, 0); })"
        ),
        "3"
    );
}

#[test]
fn microtasks_run_before_timer_callbacks() {
    let kernel = JsKernel::spawn().unwrap();
    assert_eq!(
        value(
            &kernel,
            "new Promise(resolve => { const order=[]; setTimeout(()=>{order.push('timer');resolve(order)},0); Promise.resolve().then(()=>order.push('microtask')); })"
        ),
        "[\"microtask\",\"timer\"]"
    );
}

#[test]
fn invalid_callbacks_and_delays_are_rejected_without_losing_globals() {
    let kernel = JsKernel::spawn().unwrap();
    value(&kernel, "globalThis.kept = 7");
    for code in [
        "setTimeout('kept=99',0)",
        "sleep(Infinity)",
        "sleep(NaN)",
        "sleep(-Infinity)",
        "sleep(2147483648)",
    ] {
        assert!(!execute(&kernel, code).ok, "{code}");
    }
    assert_eq!(value(&kernel, "kept"), "7");
}

#[test]
fn unawaited_operations_do_not_escape_the_cell() {
    let kernel = JsKernel::spawn().unwrap();
    value(
        &kernel,
        "globalThis.late=0; setTimeout(()=>{late++},0); tool.call('read',{path:'old'}); 42",
    );
    assert!(kernel.bridge.try_recv().is_err());
    assert_eq!(
        value(
            &kernel,
            "new Promise(resolve => setTimeout(()=>resolve(late),0))"
        ),
        "0"
    );
}

#[test]
fn promise_all_bridge_requests_are_dispatched_in_order() {
    let kernel = JsKernel::spawn().unwrap();
    let response = kernel
        .submit(
            "Promise.all([tool.call('read',{path:'a'}),tool.call('read',{path:'b'})])".into(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
    let first = kernel.bridge.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first.input["path"], "a");
    assert!(
        kernel.bridge.try_recv().is_err(),
        "second async request must await first reply"
    );
    first.reply.send(Ok("A".into())).unwrap();
    let second = kernel.bridge.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(second.input["path"], "b");
    second.reply.send(Ok("B".into())).unwrap();
    let response = response.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(response.result.as_deref(), Some("[\"A\",\"B\"]"));
}

#[test]
fn a_timer_runs_while_an_async_host_reply_is_withheld() {
    let kernel = JsKernel::spawn().unwrap();
    let response = kernel.submit("(async()=>{ const pending=tool.call('read',{path:'held'}); setTimeout(()=>tool.grep('timer-ran'),0); return await pending; })()".into(), Instant::now()+Duration::from_secs(5)).unwrap();
    let first = kernel.bridge.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first.tool, "read");
    // This second request originates in the timer callback. It is observable
    // before replying to the first only if tool.call did not block the kernel.
    let timer = kernel.bridge.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(timer.tool, "grep");
    assert_eq!(timer.input["pattern"], "timer-ran");
    timer.reply.send(Ok("tick".into())).unwrap();
    first.reply.send(Ok("answer".into())).unwrap();
    let response = response.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(response.result.as_deref(), Some("\"answer\""));
}

#[test]
fn host_errors_reject_an_awaitable_error_object() {
    let kernel = JsKernel::spawn().unwrap();
    let response = kernel
        .submit(
            "tool.call('write',{}).catch(error=>[error instanceof Error,error.message])".into(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
    let request = kernel.bridge.recv_timeout(Duration::from_secs(5)).unwrap();
    request.reply.send(Err("policy denied".into())).unwrap();
    let response = response.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(response.ok);
    assert_eq!(response.result.as_deref(), Some("[true,\"policy denied\"]"));
}

#[test]
fn malformed_bridge_arguments_never_reach_the_host() {
    let kernel = JsKernel::spawn().unwrap();
    for code in [
        "tool.call('read',null)",
        "tool.call('read',[])",
        "tool.call('read',42)",
        "tool.call(42,{})",
    ] {
        assert!(!execute(&kernel, code).ok, "{code}");
        assert!(kernel.bridge.try_recv().is_err());
    }
}

#[test]
fn callback_failure_reports_error_and_retains_completed_mutations() {
    let kernel = JsKernel::spawn().unwrap();
    value(&kernel, "globalThis.beforeFailure=0");
    let response = execute(
        &kernel,
        "new Promise(()=>setTimeout(()=>{beforeFailure++;throw new Error('timer failed')},0))",
    );
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("timer failed"));
    assert_eq!(value(&kernel, "beforeFailure"), "1");
}

#[test]
fn timeout_discards_stale_continuations_and_creates_a_fresh_realm() {
    let kernel = JsKernel::spawn().unwrap();
    let response = kernel
        .submit(
            "globalThis.old=42; await sleep(60000); old=99".into(),
            Instant::now() + Duration::from_millis(100),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("EVAL_STATE_RESET"));
    assert_eq!(value(&kernel, "typeof old"), "\"undefined\"");
    assert_eq!(
        value(&kernel, "new Promise(resolve=>setTimeout(resolve,0,7))"),
        "7"
    );
}

#[test]
fn synchronous_bridge_wait_uses_cell_deadline_not_fixed_120_seconds() {
    let kernel = JsKernel::spawn().unwrap();
    let response = kernel
        .submit(
            "tool.read('held')".into(),
            Instant::now() + Duration::from_millis(250),
        )
        .unwrap();
    let held = kernel.bridge.recv_timeout(Duration::from_secs(5)).unwrap();
    let response = response.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("EVAL_STATE_RESET"));
    assert!(held.reply.send(Ok("too late".into())).is_err());
    assert_eq!(value(&kernel, "6*7"), "42");
}

#[test]
fn dropping_the_kernel_revokes_a_blocked_bridge_wait() {
    let kernel = JsKernel::spawn().unwrap();
    let response = kernel
        .submit(
            "tool.read('held')".into(),
            Instant::now() + Duration::from_secs(120),
        )
        .unwrap();
    let held = kernel.bridge.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(kernel);
    assert!(matches!(
        response.recv_timeout(Duration::from_secs(5)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
    assert!(held.reply.send(Ok("late".into())).is_err());
}

#[test]
fn recursive_microtasks_cannot_turn_timeout_into_success() {
    let kernel = JsKernel::spawn().unwrap();
    let response = kernel
        .submit(
            "Promise.resolve().then(function again(){Promise.resolve().then(again)}); 42".into(),
            Instant::now() + Duration::from_millis(100),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("EVAL_STATE_RESET"));
    assert_eq!(value(&kernel, "42"), "42");
}

#[test]
fn registry_cleanup_does_not_invoke_mutated_guest_methods() {
    let kernel = JsKernel::spawn().unwrap();
    value(
        &kernel,
        "globalThis.retained=42; Map.prototype.clear=()=>{throw new Error('guest cleanup')}; 42",
    );
    assert_eq!(value(&kernel, "retained"), "42");
}

#[test]
fn native_pending_and_total_operation_limits_are_independent() {
    let mut state = state();
    for _ in 0..MAX_PENDING {
        state.schedule(0.0, false).unwrap();
    }
    assert!(state.schedule(0.0, false).is_err());
    state.timers.clear();
    for _ in MAX_PENDING..MAX_OPERATIONS {
        let id = state.schedule(0.0, false).unwrap();
        state.timers.remove(&id);
    }
    assert!(state.schedule(0.0, false).is_err());
}

#[test]
fn native_bridge_byte_limits_and_shapes_are_checked_before_dispatch() {
    let mut state = state();
    let payload = format!("{{\"x\":\"{}\"}}", "x".repeat(MAX_PAYLOAD - 8));
    assert_eq!(payload.len(), MAX_PAYLOAD);
    for _ in 0..MAX_QUEUED_BYTES / MAX_PAYLOAD {
        state.enqueue("read".into(), &payload).unwrap();
    }
    assert!(state.enqueue("read".into(), "{}").is_err());
    state.clear();
    state.deadline = Some(Instant::now() + Duration::from_secs(10));
    for input in ["[]", "null", "{} trailing"] {
        assert!(state.enqueue("read".into(), input).is_err());
    }
    assert!(state.enqueue("read".into(), &(payload + " ")).is_err());
    assert!(state.calls.is_empty());
}

#[test]
fn native_ids_never_alias_across_cells_or_wrap() {
    let mut state = state();
    let old = state.schedule(0.0, false).unwrap();
    state.clear();
    state.deadline = Some(Instant::now() + Duration::from_secs(10));
    let new = state.schedule(0.0, false).unwrap();
    state.timers.remove(&old);
    assert!(state.timers.contains_key(&new));
    state.next_id = u32::MAX;
    assert!(state.schedule(0.0, false).is_err());
}

#[test]
fn native_retirement_drops_receivers_and_refuses_dispatch() {
    let mut state = state();
    let (tx, rx) = std::sync::mpsc::channel();
    state.enqueue("read".into(), "{}").unwrap();
    assert!(state.poll(&tx).unwrap().is_none());
    let held = rx.try_recv().unwrap();
    state.clear();
    assert!(held.reply.send(Ok("late".into())).is_err());
    assert!(state.poll(&tx).is_err());
    assert!(state.schedule(0.0, false).is_err());
}

#[test]
fn native_event_budget_and_cancellation_refuse_further_callbacks() {
    let mut state = state();
    let (tx, _rx) = std::sync::mpsc::channel();
    state.schedule(0.0, false).unwrap();
    state.events = MAX_EVENTS;
    assert!(state.poll(&tx).is_err());
    state.events = 0;
    state.stopped.store(true, Ordering::SeqCst);
    assert!(state.poll(&tx).is_err());
    assert!(state.enqueue("read".into(), "{}").is_err());
}
