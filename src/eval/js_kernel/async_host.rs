//! Invocation-owned timers and nonblocking tool replies for the sync JS realm.
//!
//! Rust stores only IDs, deadlines and channel data; JavaScript callbacks stay
//! in a private registry. Pumping never holds a RefCell borrow while running JS.
//! Registries and undelivered requests are retired at the end of each cell.

use super::JsBridgeRequest;
use rquickjs::{Ctx, Function, Persistent};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

const MAX_PENDING: usize = 128;
const MAX_OPERATIONS: usize = 4096;
const MAX_PAYLOAD: usize = 256 * 1024;
const MAX_QUEUED_BYTES: usize = 1024 * 1024;
const MAX_EVENTS: usize = 65_536;

struct Timer {
    due: Instant,
    repeat: Option<Duration>,
}

struct Call {
    id: u32,
    input: Option<(String, serde_json::Value)>,
    receiver: Option<Receiver<Result<String, String>>>,
    bytes: usize,
}

#[derive(Debug)]
enum Event {
    Timer(u32, bool),
    Reply(u32, Result<String, String>),
}

struct State {
    deadline: Option<Instant>,
    stopped: Arc<AtomicBool>,
    next_id: u32,
    operations: usize,
    events: usize,
    queued_bytes: usize,
    timers: BTreeMap<u32, Timer>,
    calls: VecDeque<Call>,
}

impl State {
    fn new(stopped: Arc<AtomicBool>) -> Self {
        Self {
            deadline: None,
            stopped,
            next_id: 0,
            operations: 0,
            events: 0,
            queued_bytes: 0,
            timers: BTreeMap::new(),
            calls: VecDeque::new(),
        }
    }

    fn admit(&self) -> Result<(), &'static str> {
        if self.stopped.load(Ordering::SeqCst)
            || self.deadline.is_none_or(|deadline| Instant::now() >= deadline)
        {
            return Err("EVAL_TIMEOUT: asynchronous cell owner is no longer active");
        }
        Ok(())
    }

    fn allocate(&mut self) -> Result<u32, &'static str> {
        self.admit()?;
        if self.timers.len() + self.calls.len() >= MAX_PENDING
            || self.operations >= MAX_OPERATIONS
        {
            return Err("EVAL_ASYNC_LIMIT: too many pending or scheduled operations");
        }
        self.next_id = self.next_id.checked_add(1).ok_or("EVAL_ASYNC_LIMIT: ID space exhausted")?;
        self.operations += 1;
        Ok(self.next_id)
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // validated finite u31 milliseconds
    fn schedule(&mut self, milliseconds: f64, repeat: bool) -> Result<u32, &'static str> {
        if !milliseconds.is_finite() || !(0.0..=2_147_483_647.0).contains(&milliseconds) {
            return Err("EVAL_TIMER_DELAY: expected finite milliseconds from 0 to 2147483647");
        }
        let millis = milliseconds as u64;
        let delay = Duration::from_millis(if repeat { millis.max(1) } else { millis });
        let due = Instant::now().checked_add(delay).ok_or("EVAL_TIMER_DELAY: deadline overflow")?;
        let id = self.allocate()?;
        self.timers.insert(id, Timer { due, repeat: repeat.then_some(delay) });
        Ok(id)
    }

    fn enqueue(&mut self, tool: String, payload: &str) -> Result<u32, &'static str> {
        self.admit()?;
        if tool.is_empty() || tool.len() > 128 || payload.len() > MAX_PAYLOAD
            || self.queued_bytes.saturating_add(payload.len()) > MAX_QUEUED_BYTES
        {
            return Err("EVAL_ASYNC_LIMIT: tool request exceeds the queue byte budget");
        }
        let input: serde_json::Value = serde_json::from_str(payload)
            .map_err(|_| "EVAL_BRIDGE_INPUT: expected JSON object arguments")?;
        if !input.is_object() {
            return Err("EVAL_BRIDGE_INPUT: expected JSON object arguments");
        }
        let id = self.allocate()?;
        self.queued_bytes += payload.len();
        self.calls.push_back(Call { id, input: Some((tool, input)), receiver: None, bytes: payload.len() });
        Ok(id)
    }

    fn charge_event(&mut self) -> Result<(), &'static str> {
        if self.events >= MAX_EVENTS {
            return Err("EVAL_ASYNC_LIMIT: asynchronous event budget exhausted");
        }
        self.events += 1;
        Ok(())
    }

    fn poll(&mut self, bridge: &Sender<JsBridgeRequest>) -> Result<Option<Event>, &'static str> {
        self.admit()?;
        // At most one asynchronous tool exchange is in flight. The Rust host
        // still owns policy, paths and execution; Promise.all is not a bypass.
        let reply = self.calls.front().and_then(|call| call.receiver.as_ref()).and_then(|receiver| {
            match receiver.try_recv() {
                Ok(reply) => Some(reply),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("EVAL_BRIDGE_CLOSED: host disconnected".into())),
            }
        });
        if let Some(reply) = reply {
            self.charge_event()?;
            let call = self.calls.pop_front().expect("reply belongs to front call");
            self.queued_bytes -= call.bytes;
            return Ok(Some(Event::Reply(call.id, reply)));
        }
        if let Some(call) = self.calls.front_mut()
            && let Some((tool, input)) = call.input.take()
        {
            let (reply, receiver) = std::sync::mpsc::channel();
            bridge.send(JsBridgeRequest { tool, input, reply })
                .map_err(|_| "EVAL_BRIDGE_CLOSED: host disconnected")?;
            call.receiver = Some(receiver);
        }
        let next = self.timers.iter().min_by_key(|(id, timer)| (timer.due, **id))
            .filter(|(_, timer)| timer.due <= Instant::now()).map(|(id, _)| *id);
        if let Some(id) = next {
            self.charge_event()?;
            let timer = self.timers.remove(&id).expect("selected timer exists");
            if let Some(delay) = timer.repeat {
                let due = Instant::now().checked_add(delay).ok_or("EVAL_TIMER_DELAY: deadline overflow")?;
                // Reinsert before the callback so an interval can clear itself.
                self.timers.insert(id, Timer { due, repeat: Some(delay) });
            }
            return Ok(Some(Event::Timer(id, timer.repeat.is_some())));
        }
        Ok(None)
    }

    fn clear(&mut self) {
        self.deadline = None;
        self.timers.clear();
        self.calls.clear();
        self.queued_bytes = 0;
        self.operations = 0;
        self.events = 0;
        // IDs never restart: an old clearTimeout handle cannot cancel a timer
        // created by a later cell in the same retained realm.
    }
}

pub(super) struct AsyncHost {
    dispatch: Persistent<Function<'static>>,
    clear_callbacks: Persistent<Function<'static>>,
    state: Rc<RefCell<State>>,
}

impl AsyncHost {
    pub(super) fn install(ctx: &Ctx<'_>, stopped: Arc<AtomicBool>) -> rquickjs::Result<Self> {
        use rquickjs::function::Func;
        let state = Rc::new(RefCell::new(State::new(stopped)));
        let weak = Rc::downgrade(&state);
        ctx.globals().set("__pi_set_timer", Func::from(move |ctx: Ctx<'_>, ms: f64, repeat: bool| {
            let state = weak.upgrade().ok_or_else(|| rquickjs::Exception::throw_message(&ctx, "EVAL_ASYNC_CLOSED"))?;
            let result = state.borrow_mut().schedule(ms, repeat);
            result.map_err(|error| rquickjs::Exception::throw_message(&ctx, error))
        }))?;
        let weak = Rc::downgrade(&state);
        ctx.globals().set("__pi_clear_timer", Func::from(move |id: u32| {
            if let Some(state) = weak.upgrade() {
                state.borrow_mut().timers.remove(&id);
            }
        }))?;
        let weak = Rc::downgrade(&state);
        ctx.globals().set("__pi_async_call", Func::from(move |ctx: Ctx<'_>, tool: String, payload: String| {
            let state = weak.upgrade().ok_or_else(|| rquickjs::Exception::throw_message(&ctx, "EVAL_ASYNC_CLOSED"))?;
            let result = state.borrow_mut().enqueue(tool, &payload);
            result.map_err(|error| rquickjs::Exception::throw_message(&ctx, error))
        }))?;
        let handlers: rquickjs::Object<'_> = ctx.eval(include_str!("async_host.js"))?;
        ctx.eval::<(), _>(include_str!("browser.js"))?;
        Ok(Self {
            dispatch: Persistent::save(ctx, handlers.get::<_, Function<'_>>("dispatch")?),
            clear_callbacks: Persistent::save(ctx, handlers.get::<_, Function<'_>>("clear")?),
            state,
        })
    }

    pub(super) fn begin(&self, deadline: Instant) {
        let mut state = self.state.borrow_mut();
        state.clear();
        state.deadline = Some(deadline);
    }

    pub(super) fn pump(&self, ctx: &Ctx<'_>, bridge: &Sender<JsBridgeRequest>) -> rquickjs::Result<bool> {
        let event = self.state.borrow_mut().poll(bridge)
            .map_err(|error| rquickjs::Exception::throw_message(ctx, error))?;
        let Some(event) = event else { return Ok(false); };
        let dispatch = self.dispatch.clone().restore(ctx)?;
        match event {
            Event::Timer(id, repeat) => dispatch.call::<_, ()>((id, true, repeat, ""))?,
            Event::Reply(id, result) => {
                let ok = result.is_ok();
                let content = result.unwrap_or_else(|error| error);
                dispatch.call::<_, ()>((id, false, ok, content))?;
            }
        }
        Ok(true)
    }

    /// This invokes only captured built-in Map.clear methods, never guest
    /// callbacks. The caller disables the expired interrupt for this cleanup.
    pub(super) fn finish(&self, ctx: &Ctx<'_>) -> rquickjs::Result<()> {
        self.state.borrow_mut().clear();
        self.clear_callbacks.clone().restore(ctx)?.call(())
    }
}

#[cfg(test)]
mod tests;
