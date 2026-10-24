//! A cell-owned rich-output sink. Serialize before borrowing it; callbacks and
//! proxy traps must never execute under a native RefCell borrow. The final
//! ToolOutput and each live frame are independently admitted by eval's shared
//! MIME validator. Live output uses the console's bounded, revocable channel;
//! a broken delivery cannot be caught by guest code and turned into success.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::SyncSender;
use std::time::Instant;

use rquickjs::{Ctx, Function};
use serde_json::{Value, json};

use super::super::display::{MAX_DISPLAYS, MAX_ITEM_BYTES, MAX_TOTAL_BYTES};
use super::DeadlineCell;

#[derive(Default)]
struct Capture {
    frames: Vec<Value>,
    bytes: usize,
    output: Option<SyncSender<Value>>,
    failure: Option<String>,
}

impl Capture {
    fn push(&mut self, encoded: &str) -> Result<(), &'static str> {
        if self.failure.is_some() {
            return Err("EVAL_DISPLAY_STREAM: live delivery has already failed");
        }
        let bytes = self
            .bytes
            .checked_add(encoded.len())
            .filter(|bytes| *bytes <= MAX_TOTAL_BYTES)
            .ok_or("EVAL_DISPLAY_LIMIT: aggregate rich output is too large")?;
        if self.frames.len() >= MAX_DISPLAYS || encoded.len() > MAX_ITEM_BYTES {
            return Err("EVAL_DISPLAY_LIMIT: too many or oversized displays");
        }
        let frame = serde_json::from_str(encoded)
            .map_err(|_| "EVAL_DISPLAY_INVALID: malformed display JSON")?;
        // Commit only after parsing succeeds. A caught error consumes no slot.
        self.frames.push(frame);
        self.bytes = bytes;
        Ok(())
    }
}

pub(super) struct DisplaySink(Rc<RefCell<Option<Capture>>>);

impl DisplaySink {
    pub(super) fn install(
        ctx: &Ctx<'_>,
        deadline: DeadlineCell,
        origin: Instant,
    ) -> rquickjs::Result<Self> {
        let sink = Rc::new(RefCell::new(None::<Capture>));
        let native_sink = Rc::clone(&sink);
        let native = Function::new(ctx.clone(), move |ctx: Ctx<'_>, encoded: String| {
            if deadline.expired(origin) {
                return Err(rquickjs::Exception::throw_message(
                    &ctx,
                    "EVAL_TIMEOUT: display owner expired",
                ));
            }
            let delivery = {
                let mut active = native_sink.borrow_mut();
                active
                    .as_mut()
                    .ok_or("EVAL_DISPLAY_CLOSED: no active cell")
                    .and_then(|capture| {
                        capture.push(&encoded)?;
                        // Detached JSON only. Serialization/proxy traps have
                        // already completed, and no guest values cross threads.
                        Ok(capture.output.as_ref().map(|output| {
                            (
                                output.clone(),
                                json!({
                                    "id": 1,
                                    "stream": {
                                        "sequence": capture.frames.len(),
                                        "name": "display",
                                        "display": capture.frames.last(),
                                    },
                                }),
                            )
                        }))
                    })
            };
            // Release the capture borrow before backpressure or exception
            // construction. The host alone invokes user-facing callbacks.
            let delivery =
                delivery.map_err(|message| rquickjs::Exception::throw_message(&ctx, message))?;
            if let Some((output, frame)) = delivery
                && let Err(error) =
                    super::console::deliver(&output, frame, &deadline, origin, "rich display")
            {
                if let Some(capture) = native_sink.borrow_mut().as_mut() {
                    capture.failure = Some(error.clone());
                }
                return Err(rquickjs::Exception::throw_message(&ctx, &error));
            }
            Ok(())
        })?;
        let factory: Function<'_> = ctx.eval(include_str!("display.js"))?;
        let display: Function<'_> = factory.call((native,))?;
        ctx.globals().set("display", display)?;
        Ok(Self(sink))
    }

    #[cfg(test)]
    pub(super) fn begin(&self) {
        self.begin_with_output(None);
    }

    pub(super) fn begin_with_output(&self, output: Option<SyncSender<Value>>) {
        *self.0.borrow_mut() = Some(Capture {
            output,
            ..Capture::default()
        });
    }

    /// Transport failure is latched independently of the guest exception.
    /// Even `try { display(...) } catch {}` must retire the broken realm.
    pub(super) fn failure(&self) -> Option<String> {
        self.0
            .borrow()
            .as_ref()
            .and_then(|capture| capture.failure.clone())
    }

    /// No JS is executed during retirement, including after interruption.
    /// Drop this invocation's sender before the final kernel reply. A failed
    /// delivery may have an unsent frame here; the worker reports failure and
    /// the host independently refuses a mismatch with the accepted prefix.
    pub(super) fn finish(&self) -> Vec<Value> {
        self.0
            .borrow_mut()
            .take()
            .map_or_else(Vec::new, |capture| capture.frames)
    }
}

#[cfg(test)]
mod tests {
    use super::super::JsKernel;
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    fn cell(kernel: &JsKernel, code: &str) -> super::super::JsCellResponse {
        kernel
            .submit(code.into(), Instant::now() + Duration::from_secs(10))
            .unwrap()
            .recv_timeout(Duration::from_secs(15))
            .expect("kernel response")
    }

    #[test]
    fn mixed_json_text_and_png_snapshots_leave_final_result_unchanged() {
        let kernel = JsKernel::spawn().unwrap();
        let response = cell(
            &kernel,
            "const v = {answer:[42,true,null]}; display(v); v.answer[0]=0; display('# Answer','text/markdown'); display('plain','text/plain'); 17",
        );
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.result.as_deref(), Some("17"));
        assert_eq!(
            response.displays,
            vec![
                json!({"mimeType":"application/json","data":{"answer":[42,true,null]}}),
                json!({"mimeType":"text/markdown","data":"# Answer"}),
                json!({"mimeType":"text/plain","data":"plain"}),
            ]
        );
        let out = cell(&kernel, "display(new Uint8Array([0,255,1,2]),'image/png')");
        assert!(out.ok, "{:?}", out.error);
        assert_eq!(out.displays[0]["data"], "AP8BAg=="); // raster admission belongs to the host
    }

    #[test]
    fn ordinary_failure_retains_displays_and_next_cell_has_a_fresh_sink() {
        let kernel = JsKernel::spawn().unwrap();
        let response = cell(
            &kernel,
            "globalThis.keep = 42; display(keep); throw new Error('after display')",
        );
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("after display"));
        assert_eq!(response.displays[0]["data"], 42);
        let response = cell(&kernel, "keep");
        assert!(response.ok);
        assert_eq!(response.result.as_deref(), Some("42"));
        assert!(response.displays.is_empty());
    }

    #[test]
    fn awaited_timers_capture_in_call_order_and_abandoned_timers_do_not_leak() {
        let kernel = JsKernel::spawn().unwrap();
        let response = cell(
            &kernel,
            "display(1); await sleep(1); display(2); await Promise.resolve(); display(3)",
        );
        assert!(response.ok, "{:?}", response.error);
        let values: Vec<_> = response
            .displays
            .iter()
            .map(|frame| frame["data"].clone())
            .collect();
        assert_eq!(values, vec![json!(1), json!(2), json!(3)]);
        let response = cell(&kernel, "setTimeout(() => display('old'), 10); void 0");
        assert!(response.ok);
        assert!(response.displays.is_empty());
        let response = cell(&kernel, "await sleep(20); display('new')");
        assert!(response.ok);
        assert_eq!(
            response.displays,
            vec![json!({"mimeType":"application/json","data":"new"})]
        );
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // JS object literal, not a format string
    fn unsupported_json_is_refused_without_calling_serialization_hooks() {
        let kernel = JsKernel::spawn().unwrap();
        assert!(cell(&kernel, "globalThis.called = 0").ok);
        for code in [
            "display({get x(){called++; return 1;}})",
            "display({toJSON(){called++; return 1;}})",
            "display({x:undefined})",
            "display([,1])",
            "display(1n)",
            "display(NaN)",
            "display(Infinity)",
            "display(new Date())",
            "display({[Symbol('x')]: 1})",
            "display('\\ud800')",
        ] {
            let response = cell(&kernel, code);
            assert!(!response.ok, "{code}");
            assert!(response.displays.is_empty(), "{code}");
        }
        assert_eq!(cell(&kernel, "called").result.as_deref(), Some("0"));
    }

    #[test]
    fn invalid_display_can_be_caught_without_poisoning_valid_output() {
        let kernel = JsKernel::spawn().unwrap();
        let response = cell(
            &kernel,
            "const cyclic={}; cyclic.self=cyclic; try {display(cyclic)} catch(e) {} display({ok:true})",
        );
        assert!(response.ok);
        assert_eq!(
            response.displays,
            vec![json!({"mimeType":"application/json","data":{"ok":true}})]
        );
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // JS object literal, not a format string
    fn captured_intrinsics_do_not_depend_on_replaced_global_helpers() {
        let kernel = JsKernel::spawn().unwrap();
        let response = cell(
            &kernel,
            "JSON.stringify = () => 'corrupt'; Object.getOwnPropertyDescriptor = () => {throw 1}; Reflect.apply = () => {throw 2}; Set.prototype.has = () => true; display({answer:42})",
        );
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.displays[0]["data"], json!({"answer":42}));
    }

    #[test]
    fn native_capture_enforces_count_and_bytes_transactionally() {
        let small = r#"{"mimeType":"application/json","data":null}"#;
        let mut capture = Capture::default();
        assert!(capture.push("{").is_err());
        assert_eq!(capture.bytes, 0);
        for _ in 0..MAX_DISPLAYS {
            capture.push(small).unwrap();
        }
        assert!(capture.push(small).is_err());
        assert_eq!(capture.frames.len(), MAX_DISPLAYS);
        let overhead = r#"{"mimeType":"text/plain","data":""}"#.len();
        let large = format!(
            r#"{{"mimeType":"text/plain","data":"{}"}}"#,
            "x".repeat(MAX_ITEM_BYTES - overhead)
        );
        let mut capture = Capture::default();
        capture.push(&large).unwrap();
        capture.push(&large).unwrap();
        assert_eq!(capture.bytes, MAX_TOTAL_BYTES);
        assert!(capture.push(small).is_err());
        assert_eq!(capture.frames.len(), 2);
    }

    #[test]
    fn inactive_and_expired_sinks_refuse_even_captured_display_functions() {
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        let origin = Instant::now();
        let deadline = DeadlineCell::new(Arc::new(AtomicBool::new(false)));
        context.with(|ctx| {
            let sink = DisplaySink::install(&ctx, deadline.clone(), origin).unwrap();
            assert!(ctx.eval::<(), _>("display(1)").is_err());
            let _ = ctx.catch();
            sink.begin();
            ctx.eval::<(), _>("globalThis.savedDisplay = display; savedDisplay(2)")
                .unwrap();
            assert_eq!(sink.finish()[0]["data"], 2);
            assert!(ctx.eval::<(), _>("savedDisplay(3)").is_err());
            let _ = ctx.catch();
            sink.begin();
            deadline.set(origin, origin);
            assert!(ctx.eval::<(), _>("savedDisplay(4)").is_err());
            let _ = ctx.catch();
            assert!(sink.finish().is_empty());
        });
    }

    #[test]
    fn live_proxy_reentrancy_keeps_display_and_console_sequences_independent() {
        let kernel = JsKernel::spawn().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(8);
        let reply = kernel
            .submit_with_output(
                concat!(
                    "const observed = new Proxy({answer:42}, {ownKeys(target) {",
                    "display('inner'); console.log('inside'); return Reflect.ownKeys(target); }});",
                    "display(observed); 17"
                )
                .into(),
                Instant::now() + Duration::from_secs(10),
                Some(sender),
            )
            .unwrap();
        let response = reply.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.result.as_deref(), Some("17"));
        assert!(
            response.console.is_empty(),
            "no duplicate console in the reply"
        );
        let frames: Vec<_> = receiver.try_iter().collect();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0],
            json!({"id":1,"stream":{
                "name":"display","sequence":1,
                "display":{"mimeType":"application/json","data":"inner"}
            }})
        );
        assert_eq!(
            frames[1],
            json!({"id":1,"stream":{
                "name":"stdout","sequence":1,"text":"inside\n"
            }})
        );
        assert_eq!(frames[2]["stream"]["sequence"], 2);
        assert_eq!(frames[2]["stream"]["display"]["data"], json!({"answer":42}));
        assert_eq!(
            response.displays,
            vec![
                frames[0]["stream"]["display"].clone(),
                frames[2]["stream"]["display"].clone(),
            ]
        );
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            ),
            "both native sinks close before the final reply"
        );
    }

    #[test]
    fn caught_live_delivery_failure_still_fails_and_rebuilds_the_realm() {
        let kernel = JsKernel::spawn().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        drop(receiver);
        let reply = kernel
            .submit_with_output(
                "globalThis.keep = 42; try { display(1); } catch (error) {} 17".into(),
                Instant::now() + Duration::from_secs(10),
                Some(sender),
            )
            .unwrap();
        let response = reply.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(
            !response.ok,
            "guest catch must not turn a broken stream into success"
        );
        assert!(response.result.is_none());
        let error = response.error.unwrap();
        assert!(error.contains("EVAL_DISPLAY_STREAM"), "{error}");
        assert!(error.contains("EVAL_OUTPUT_CLOSED"), "{error}");
        assert!(error.contains("EVAL_STATE_RESET"), "{error}");
        let next = cell(&kernel, "typeof keep === 'undefined'");
        assert!(next.ok);
        assert_eq!(next.result.as_deref(), Some("true"));
        assert!(next.displays.is_empty());
    }

    #[test]
    fn full_live_display_channel_expires_without_silently_dropping_a_frame() {
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        let origin = Instant::now();
        let deadline = DeadlineCell::new(Arc::new(AtomicBool::new(false)));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        context.with(|ctx| {
            let sink = DisplaySink::install(&ctx, deadline.clone(), origin).unwrap();
            sink.begin_with_output(Some(sender));
            ctx.eval::<(), _>("display('accepted')").unwrap();
            // Arm only after the first frame is known queued. The second
            // call must block on the real bounded channel until its budget.
            deadline.set(Instant::now() + Duration::from_secs(1), origin);
            let started = Instant::now();
            ctx.eval::<(), _>("try {display('blocked')} catch (error) {}")
                .unwrap();
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(sink.failure().unwrap().contains("EVAL_TIMEOUT"));
            let accepted = receiver.try_recv().unwrap();
            assert_eq!(accepted["stream"]["display"]["data"], "accepted");
            assert_eq!(accepted["stream"]["sequence"], 1);
            deadline.clear();
            assert!(ctx.eval::<(), _>("display('must remain refused')").is_err());
            let _ = ctx.catch();
            let captured = sink.finish();
            assert_eq!(
                captured.len(),
                2,
                "failure must not pretend every frame was delivered"
            );
            assert!(matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            ));
        });
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // JS object literal, not a format string
    fn dropping_a_kernel_revokes_live_display_backpressure() {
        let kernel = JsKernel::spawn().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reply = kernel
            .submit_with_output(
                "for (let i=0; i<32; i++) display({index:i});".into(),
                Instant::now() + Duration::from_secs(60),
                Some(sender),
            )
            .unwrap();
        let first = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first["stream"]["display"]["data"]["index"], 0);
        drop(kernel);
        assert!(matches!(
            reply.recv_timeout(Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(
            receiver.try_iter().count() <= 1,
            "one bounded queue, no spill producer"
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }
}
