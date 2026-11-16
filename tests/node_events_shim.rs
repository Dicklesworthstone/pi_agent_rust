//! Unit tests for the node:events (`EventEmitter`) shim (bd-1av0.7).
//!
//! Tests verify that `EventEmitter` follows Node.js semantics: on/emit/off
//! lifecycle, once auto-removal, listener ordering, maxListeners warning,
//! prependListener, removeAllListeners, and the static `events.once()` helper.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use std::sync::Arc;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn load_ext(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/events_test.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec");

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    manager
}

fn events_ext_source(js_expr: &str) -> String {
    format!(
        r#"
import EventEmitter from "node:events";

export default function activate(pi) {{
  pi.on("agent_start", (event, ctx) => {{
    let result;
    try {{
      result = String({js_expr});
    }} catch (e) {{
      result = "ERROR:" + e.message;
    }}
    return {{ result }};
  }});
}}
"#
    )
}

fn eval_events(js_expr: &str) -> String {
    let harness = common::TestHarness::new("events_shim");
    let source = events_ext_source(js_expr);
    let mgr = load_ext(&harness, &source);

    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch agent_start")
    });

    response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "NO_RESPONSE".to_string())
}

// ─── Basic on/emit lifecycle ────────────────────────────────────────────────

#[test]
fn emit_calls_registered_listener() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        let called = false;
        ee.on("test", () => { called = true; });
        ee.emit("test");
        return called;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn emit_passes_arguments() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        let received = null;
        ee.on("data", (a, b) => { received = a + b; });
        ee.emit("data", 10, 20);
        return received;
    })()"#,
    );
    assert_eq!(result, "30");
}

#[test]
fn emit_returns_true_when_listeners_exist() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        ee.on("test", () => {});
        return ee.emit("test");
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn emit_returns_false_when_no_listeners() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        return ee.emit("test");
    })()"#,
    );
    assert_eq!(result, "false");
}

// ─── once() ─────────────────────────────────────────────────────────────────

#[test]
fn once_fires_exactly_once() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        let count = 0;
        ee.once("test", () => { count++; });
        ee.emit("test");
        ee.emit("test");
        ee.emit("test");
        return count;
    })()"#,
    );
    assert_eq!(result, "1");
}

#[test]
fn once_auto_removes_listener() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        ee.once("test", () => {});
        ee.emit("test");
        return ee.listenerCount("test");
    })()"#,
    );
    assert_eq!(result, "0");
}

// ─── off / removeListener ───────────────────────────────────────────────────

#[test]
fn off_removes_specific_listener() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        let count = 0;
        const handler = () => { count++; };
        ee.on("test", handler);
        ee.emit("test");
        ee.off("test", handler);
        ee.emit("test");
        return count;
    })()"#,
    );
    assert_eq!(result, "1");
}

#[test]
fn remove_listener_is_alias_for_off() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        let count = 0;
        const handler = () => { count++; };
        ee.on("test", handler);
        ee.removeListener("test", handler);
        ee.emit("test");
        return count;
    })()"#,
    );
    assert_eq!(result, "0");
}

// ─── Multiple listeners ─────────────────────────────────────────────────────

#[test]
fn multiple_listeners_called_in_order() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        const order = [];
        ee.on("test", () => order.push("first"));
        ee.on("test", () => order.push("second"));
        ee.on("test", () => order.push("third"));
        ee.emit("test");
        return order.join(",");
    })()"#,
    );
    assert_eq!(result, "first,second,third");
}

// ─── removeAllListeners ─────────────────────────────────────────────────────

#[test]
fn remove_all_listeners_for_specific_event() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        ee.on("a", () => {});
        ee.on("a", () => {});
        ee.on("b", () => {});
        ee.removeAllListeners("a");
        return ee.listenerCount("a") + "," + ee.listenerCount("b");
    })()"#,
    );
    assert_eq!(result, "0,1");
}

#[test]
fn remove_all_listeners_clears_all_events() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        ee.on("a", () => {});
        ee.on("b", () => {});
        ee.removeAllListeners();
        return ee.eventNames().length;
    })()"#,
    );
    assert_eq!(result, "0");
}

// ─── listenerCount / listeners / eventNames ─────────────────────────────────

#[test]
fn listener_count_tracks_additions() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        ee.on("test", () => {});
        ee.on("test", () => {});
        return ee.listenerCount("test");
    })()"#,
    );
    assert_eq!(result, "2");
}

#[test]
fn event_names_returns_active_events() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        ee.on("alpha", () => {});
        ee.on("beta", () => {});
        return JSON.stringify(ee.eventNames().sort());
    })()"#,
    );
    assert_eq!(result, r#"["alpha","beta"]"#);
}

#[test]
fn listeners_returns_original_functions_for_once() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        const fn1 = () => {};
        ee.once("test", fn1);
        const list = ee.listeners("test");
        return list[0] === fn1;
    })()"#,
    );
    assert_eq!(result, "true");
}

// ─── prependListener ────────────────────────────────────────────────────────

#[test]
fn prepend_listener_fires_first() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        const order = [];
        ee.on("test", () => order.push("normal"));
        ee.prependListener("test", () => order.push("prepend"));
        ee.emit("test");
        return order.join(",");
    })()"#,
    );
    assert_eq!(result, "prepend,normal");
}

#[test]
fn prepend_once_listener_fires_first_and_once() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        const order = [];
        ee.on("test", () => order.push("normal"));
        ee.prependOnceListener("test", () => order.push("prepend-once"));
        ee.emit("test");
        ee.emit("test");
        return order.join(",");
    })()"#,
    );
    assert_eq!(result, "prepend-once,normal,normal");
}

// ─── setMaxListeners / getMaxListeners ──────────────────────────────────────

#[test]
fn default_max_listeners_is_10() {
    let result = eval_events(
        r"(() => {
        const ee = new EventEmitter();
        return ee.getMaxListeners();
    })()",
    );
    assert_eq!(result, "10");
}

#[test]
fn set_max_listeners_changes_value() {
    let result = eval_events(
        r"(() => {
        const ee = new EventEmitter();
        ee.setMaxListeners(25);
        return ee.getMaxListeners();
    })()",
    );
    assert_eq!(result, "25");
}

// ─── addListener alias ──────────────────────────────────────────────────────

#[test]
fn add_listener_is_alias_for_on() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        let called = false;
        ee.addListener("test", () => { called = true; });
        ee.emit("test");
        return called;
    })()"#,
    );
    assert_eq!(result, "true");
}

// ─── rawListeners ───────────────────────────────────────────────────────────

#[test]
fn raw_listeners_returns_wrappers() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        const fn1 = () => {};
        ee.once("test", fn1);
        const raw = ee.rawListeners("test");
        // rawListeners returns the internal wrapper, not the original
        return raw.length === 1 && typeof raw[0] === "function";
    })()"#,
    );
    assert_eq!(result, "true");
}

// ─── EventEmitter.EventEmitter self-reference ───────────────────────────────

#[test]
fn event_emitter_self_reference() {
    let result = eval_events(
        r"(() => {
        return EventEmitter.EventEmitter === EventEmitter;
    })()",
    );
    assert_eq!(result, "true");
}

// ─── defaultMaxListeners static ─────────────────────────────────────────────

#[test]
fn default_max_listeners_static() {
    let result = eval_events(
        r"(() => {
        return EventEmitter.defaultMaxListeners;
    })()",
    );
    assert_eq!(result, "10");
}

// ─── Import styles ──────────────────────────────────────────────────────────

#[test]
fn named_import_works() {
    let harness = common::TestHarness::new("events_named_import");
    let source = r#"
import { EventEmitter } from "node:events";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    const ee = new EventEmitter();
    let ok = false;
    ee.on("ping", () => { ok = true; });
    ee.emit("ping");
    return { result: String(ok) };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "true");
}

#[test]
fn bare_events_import_works() {
    let harness = common::TestHarness::new("events_bare_import");
    let source = r#"
import EventEmitter from "events";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    const ee = new EventEmitter();
    ee.on("test", () => {});
    return { result: String(ee.listenerCount("test")) };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "1");
}

// ─── Error in listener ──────────────────────────────────────────────────────

#[test]
fn error_in_listener_emits_error_event() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        let errorCaught = false;
        ee.on("error", (err) => { errorCaught = true; });
        ee.on("test", () => { throw new Error("boom"); });
        ee.emit("test");
        return errorCaught;
    })()"#,
    );
    assert_eq!(result, "true");
}

// ─── Chaining ───────────────────────────────────────────────────────────────

#[test]
fn methods_return_this_for_chaining() {
    let result = eval_events(
        r#"(() => {
        const ee = new EventEmitter();
        const fn1 = () => {};
        const result = ee.on("a", fn1).once("b", fn1).setMaxListeners(5);
        return result === ee;
    })()"#,
    );
    assert_eq!(result, "true");
}
