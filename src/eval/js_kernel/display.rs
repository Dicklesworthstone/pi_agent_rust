//! A cell-owned rich-output sink. Serialize before borrowing it; callbacks and
//! proxy traps must never execute under a native RefCell borrow. The final
//! ToolOutput is independently admitted by eval's shared MIME validator.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use rquickjs::{Ctx, Function};
use serde_json::Value;

use super::DeadlineCell;
use super::super::display::{MAX_DISPLAYS, MAX_ITEM_BYTES, MAX_TOTAL_BYTES};

#[derive(Default)]
struct Capture {
    frames: Vec<Value>,
    bytes: usize,
}

impl Capture {
    fn push(&mut self, encoded: &str) -> Result<(), &'static str> {
        let bytes = self.bytes.checked_add(encoded.len())
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
                return Err(rquickjs::Exception::throw_message(&ctx,
                    "EVAL_TIMEOUT: display owner expired"));
            }
            let result = {
                let mut active = native_sink.borrow_mut();
                active.as_mut()
                    .ok_or("EVAL_DISPLAY_CLOSED: no active cell")
                    .and_then(|capture| capture.push(&encoded))
            };
            result.map_err(|message| rquickjs::Exception::throw_message(&ctx, message))
        })?;
        let factory: Function<'_> = ctx.eval(include_str!("display.js"))?;
        let display: Function<'_> = factory.call((native,))?;
        ctx.globals().set("display", display)?;
        Ok(Self(sink))
    }

    pub(super) fn begin(&self) {
        *self.0.borrow_mut() = Some(Capture::default());
    }

    /// No JS is executed during retirement, including after interruption.
    pub(super) fn finish(&self) -> Vec<Value> {
        self.0.borrow_mut().take().map_or_else(Vec::new, |capture| capture.frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::JsKernel;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use serde_json::json;

    fn cell(kernel: &JsKernel, code: &str) -> super::super::JsCellResponse {
        kernel.submit(code.into(), Instant::now() + Duration::from_secs(10)).unwrap()
            .recv_timeout(Duration::from_secs(15)).expect("kernel response")
    }

    #[test]
    fn mixed_json_text_and_png_snapshots_leave_final_result_unchanged() {
        let kernel = JsKernel::spawn().unwrap();
        let response = cell(&kernel, "const v = {answer:[42,true,null]}; display(v); v.answer[0]=0; display('# Answer','text/markdown'); display('plain','text/plain'); 17");
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.result.as_deref(), Some("17"));
        assert_eq!(response.displays, vec![
            json!({"mimeType":"application/json","data":{"answer":[42,true,null]}}),
            json!({"mimeType":"text/markdown","data":"# Answer"}),
            json!({"mimeType":"text/plain","data":"plain"}),
        ]);
        let out = cell(&kernel, "display(new Uint8Array([0,255,1,2]),'image/png')");
        assert!(out.ok, "{:?}", out.error);
        assert_eq!(out.displays[0]["data"], "AP8BAg=="); // raster admission belongs to the host
    }

    #[test]
    fn ordinary_failure_retains_displays_and_next_cell_has_a_fresh_sink() {
        let kernel = JsKernel::spawn().unwrap();
        let response = cell(&kernel, "globalThis.keep = 42; display(keep); throw new Error('after display')");
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
        let response = cell(&kernel, "display(1); await sleep(1); display(2); await Promise.resolve(); display(3)");
        assert!(response.ok, "{:?}", response.error);
        let values: Vec<_> = response.displays.iter().map(|frame| frame["data"].clone()).collect();
        assert_eq!(values, vec![json!(1), json!(2), json!(3)]);
        let response = cell(&kernel, "setTimeout(() => display('old'), 10); void 0");
        assert!(response.ok);
        assert!(response.displays.is_empty());
        let response = cell(&kernel, "await sleep(20); display('new')");
        assert!(response.ok);
        assert_eq!(response.displays, vec![json!({"mimeType":"application/json","data":"new"})]);
    }

    #[test]
    fn unsupported_json_is_refused_without_calling_serialization_hooks() {
        let kernel = JsKernel::spawn().unwrap();
        assert!(cell(&kernel, "globalThis.called = 0").ok);
        for code in [
            "display({get x(){called++; return 1;}})",
            "display({toJSON(){called++; return 1;}})",
            "display({x:undefined})", "display([,1])", "display(1n)",
            "display(NaN)", "display(Infinity)", "display(new Date())",
            "display({[Symbol('x')]: 1})", "display('\\ud800')",
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
        let response = cell(&kernel, "const cyclic={}; cyclic.self=cyclic; try {display(cyclic)} catch(e) {} display({ok:true})");
        assert!(response.ok);
        assert_eq!(response.displays, vec![json!({"mimeType":"application/json","data":{"ok":true}})]);
    }

    #[test]
    fn captured_intrinsics_do_not_depend_on_replaced_global_helpers() {
        let kernel = JsKernel::spawn().unwrap();
        let response = cell(&kernel, "JSON.stringify = () => 'corrupt'; Object.getOwnPropertyDescriptor = () => {throw 1}; Reflect.apply = () => {throw 2}; Set.prototype.has = () => true; display({answer:42})");
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.displays[0]["data"], json!({"answer":42}));
    }

    #[test]
    fn native_capture_enforces_count_and_bytes_transactionally() {
        let small = r#"{"mimeType":"application/json","data":null}"#;
        let mut capture = Capture::default();
        assert!(capture.push("{").is_err());
        assert_eq!(capture.bytes, 0);
        for _ in 0..MAX_DISPLAYS { capture.push(small).unwrap(); }
        assert!(capture.push(small).is_err());
        assert_eq!(capture.frames.len(), MAX_DISPLAYS);
        let overhead = r#"{"mimeType":"text/plain","data":""}"#.len();
        let large = format!(r#"{{"mimeType":"text/plain","data":"{}"}}"#, "x".repeat(MAX_ITEM_BYTES - overhead));
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
            ctx.eval::<(), _>("globalThis.savedDisplay = display; savedDisplay(2)").unwrap();
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
}
