// Integration tests are separate crates, so src/lib.rs's `recursion_limit`
// does not reach here; with the `wasm-host` feature, proving `Send` for the
// extension-loading futures exceeds the default 128.
#![recursion_limit = "256"]

//! Tests for npm package virtual module stubs (bd-3opk).
//!
//! Verifies that extensions can import the stubbed npm packages and
//! call their APIs without crashing. These stubs exist so that
//! extensions requiring native bindings or large dependency trees can
//! at least *load* and register tools/commands in the `QuickJS` sandbox.
#![allow(clippy::needless_raw_string_hashes)]

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use std::sync::Arc;
use std::time::Duration;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn load_ext(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/stub_test.mjs", source.as_bytes());
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

fn eval_ext(imports: &str, js_expr: &str) -> String {
    let harness = common::TestHarness::new("npm_stub_test");
    let source = format!(
        r#"
{imports}

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
    );
    let mgr = load_ext(&harness, &source);

    let response = common::run_async({
        let mgr2 = mgr.clone();
        async move {
            mgr2.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
                .await
                .expect("dispatch agent_start")
        }
    });

    common::run_async({
        async move {
            let _ = mgr.shutdown(Duration::from_secs(3)).await;
        }
    });

    response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "NO_RESPONSE".to_string())
}

// ─── chokidar ───────────────────────────────────────────────────────────────

#[test]
fn chokidar_import_default() {
    let result = eval_ext(
        r#"import chokidar from "chokidar";"#,
        r#"typeof chokidar.watch"#,
    );
    assert_eq!(result, "function");
}

#[test]
fn chokidar_import_named() {
    let result = eval_ext(r#"import { watch } from "chokidar";"#, r#"typeof watch"#);
    assert_eq!(result, "function");
}

#[test]
fn chokidar_watch_returns_watcher() {
    let result = eval_ext(
        r#"import { watch } from "chokidar";"#,
        r#"(() => {
            const w = watch('/tmp/test');
            return typeof w.on === 'function' && typeof w.close === 'function' && typeof w.add === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn chokidar_watcher_chaining() {
    let result = eval_ext(
        r#"import { watch } from "chokidar";"#,
        r#"(() => {
            const w = watch('/tmp');
            const w2 = w.on('change', () => {}).on('add', () => {}).on('ready', () => {});
            return w === w2;
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── node-pty ───────────────────────────────────────────────────────────────

#[test]
fn node_pty_import() {
    let result = eval_ext(r#"import pty from "node-pty";"#, r#"typeof pty.spawn"#);
    assert_eq!(result, "function");
}

#[test]
fn node_pty_named_import() {
    let result = eval_ext(r#"import { spawn } from "node-pty";"#, r#"typeof spawn"#);
    assert_eq!(result, "function");
}

#[test]
fn node_pty_spawn_returns_pty() {
    let result = eval_ext(
        r#"import { spawn } from "node-pty";"#,
        r#"(() => {
            const p = spawn('/bin/sh', []);
            return typeof p.pid === 'number' && typeof p.write === 'function' && typeof p.kill === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── jsdom ──────────────────────────────────────────────────────────────────

#[test]
fn jsdom_import() {
    let result = eval_ext(r#"import { JSDOM } from "jsdom";"#, r#"typeof JSDOM"#);
    assert_eq!(result, "function");
}

#[test]
fn jsdom_construct() {
    let result = eval_ext(
        r#"import { JSDOM } from "jsdom";"#,
        r#"(() => {
            const dom = new JSDOM('<p>hello</p>');
            return dom.window !== undefined && dom.window.document !== undefined;
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn jsdom_document_query_selector() {
    let result = eval_ext(
        r#"import { JSDOM } from "jsdom";"#,
        r#"(() => {
            const dom = new JSDOM('<div>test</div>');
            const els = dom.window.document.querySelectorAll('div');
            return Array.isArray(els);
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn jsdom_body_text_content() {
    let result = eval_ext(
        r#"import { JSDOM } from "jsdom";"#,
        r#"(() => {
            const dom = new JSDOM('<p>hello world</p>');
            return dom.window.document.body.textContent;
        })()"#,
    );
    assert_eq!(result, "hello world");
}

// ─── @mozilla/readability ───────────────────────────────────────────────────

#[test]
fn readability_import() {
    let result = eval_ext(
        r#"import { Readability } from "@mozilla/readability";"#,
        r#"typeof Readability"#,
    );
    assert_eq!(result, "function");
}

#[test]
fn readability_parse() {
    let result = eval_ext(
        r#"import { Readability } from "@mozilla/readability";
import { JSDOM } from "jsdom";"#,
        r#"(() => {
            const dom = new JSDOM('<article><p>Some article text</p></article>');
            const reader = new Readability(dom.window.document);
            const article = reader.parse();
            return article !== null && typeof article.title === 'string' && typeof article.content === 'string';
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── beautiful-mermaid ──────────────────────────────────────────────────────

#[test]
fn beautiful_mermaid_import() {
    let result = eval_ext(
        r#"import { renderMermaidAscii } from "beautiful-mermaid";"#,
        r#"typeof renderMermaidAscii"#,
    );
    assert_eq!(result, "function");
}

#[test]
fn beautiful_mermaid_render() {
    let result = eval_ext(
        r#"import { renderMermaidAscii } from "beautiful-mermaid";"#,
        r#"(() => {
            const out = renderMermaidAscii('graph TD\n  A-->B');
            return typeof out === 'string' && out.length > 0;
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── @aliou/pi-utils-settings ───────────────────────────────────────────────

#[test]
fn aliou_settings_import() {
    let result = eval_ext(
        r#"import { ConfigLoader, ArrayEditor, registerSettingsCommand, getNestedValue, setNestedValue } from "@aliou/pi-utils-settings";"#,
        r#"(() => {
            return [ConfigLoader, ArrayEditor, registerSettingsCommand, getNestedValue, setNestedValue]
                .every(x => typeof x !== 'undefined');
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn aliou_settings_config_loader() {
    let result = eval_ext(
        r#"import { ConfigLoader } from "@aliou/pi-utils-settings";"#,
        r#"(() => {
            const loader = new ConfigLoader({ path: '/tmp/config.json' });
            const data = loader.load();
            return typeof data === 'object';
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn aliou_settings_nested_value() {
    let result = eval_ext(
        r#"import { getNestedValue, setNestedValue } from "@aliou/pi-utils-settings";"#,
        r#"(() => {
            const obj = {};
            setNestedValue(obj, 'a.b.c', 42);
            return getNestedValue(obj, 'a.b.c');
        })()"#,
    );
    assert_eq!(result, "42");
}

// ─── @aliou/sh ──────────────────────────────────────────────────────────────

#[test]
fn aliou_sh_import() {
    let result = eval_ext(
        r#"import { parse, tokenize, quote } from "@aliou/sh";"#,
        r#"(() => {
            return typeof parse === 'function' && typeof tokenize === 'function' && typeof quote === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn aliou_sh_parse() {
    let result = eval_ext(
        r#"import { parse } from "@aliou/sh";"#,
        r#"(() => {
            const result = parse('echo hello');
            return !!(
                result &&
                result.ast &&
                Array.isArray(result.ast.body) &&
                result.ast.body.length === 1 &&
                result.ast.body[0] &&
                result.ast.body[0].command &&
                result.ast.body[0].command.type === 'SimpleCommand'
            );
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── @marckrenn/pi-sub-shared ───────────────────────────────────────────────

#[test]
fn marckrenn_shared_import() {
    let result = eval_ext(
        r#"import { PROVIDERS, getDefaultCoreSettings } from "@marckrenn/pi-sub-shared";"#,
        r#"(() => {
            return Array.isArray(PROVIDERS) && typeof getDefaultCoreSettings === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn marckrenn_shared_providers() {
    let result = eval_ext(
        r#"import { PROVIDERS } from "@marckrenn/pi-sub-shared";"#,
        r#"PROVIDERS.length > 0"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn marckrenn_shared_default_settings() {
    let result = eval_ext(
        r#"import { getDefaultCoreSettings } from "@marckrenn/pi-sub-shared";"#,
        r#"(() => {
            const settings = getDefaultCoreSettings();
            return typeof settings === 'object' && settings !== null;
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn marckrenn_shared_model_multipliers() {
    let result = eval_ext(
        r#"import { MODEL_MULTIPLIERS } from "@marckrenn/pi-sub-shared";"#,
        r#"typeof MODEL_MULTIPLIERS === 'object'"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn marckrenn_shared_provider_metadata_detection_fields() {
    let result = eval_ext(
        r#"import { PROVIDERS, PROVIDER_METADATA } from "@marckrenn/pi-sub-shared";"#,
        r#"(() => {
            return PROVIDERS.every((provider) => {
                const detection = PROVIDER_METADATA[provider] && PROVIDER_METADATA[provider].detection;
                return detection &&
                    Array.isArray(detection.providerTokens) &&
                    Array.isArray(detection.modelTokens);
            });
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── turndown ───────────────────────────────────────────────────────────────

#[test]
fn turndown_import() {
    let result = eval_ext(
        r#"import TurndownService from "turndown";"#,
        r#"typeof TurndownService"#,
    );
    assert_eq!(result, "function");
}

#[test]
fn turndown_convert_html() {
    let result = eval_ext(
        r#"import TurndownService from "turndown";"#,
        r#"(() => {
            const td = new TurndownService();
            const md = td.turndown('<p>hello <b>world</b></p>');
            return typeof md === 'string' && md.length > 0;
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── @xterm/headless ────────────────────────────────────────────────────────

#[test]
fn xterm_headless_import() {
    let result = eval_ext(
        r#"import { Terminal } from "@xterm/headless";"#,
        r#"typeof Terminal"#,
    );
    assert_eq!(result, "function");
}

#[test]
fn xterm_headless_construct() {
    let result = eval_ext(
        r#"import { Terminal } from "@xterm/headless";"#,
        r#"(() => {
            const term = new Terminal({ cols: 120, rows: 40 });
            return term.cols === 120 && term.rows === 40;
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── @opentelemetry/api ─────────────────────────────────────────────────────

#[test]
fn opentelemetry_import() {
    let result = eval_ext(
        r#"import { trace, context, SpanStatusCode } from "@opentelemetry/api";"#,
        r#"(() => {
            return typeof trace.getTracer === 'function'
                && typeof context.active === 'function'
                && SpanStatusCode.OK === 1;
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn opentelemetry_tracer() {
    let result = eval_ext(
        r#"import { trace } from "@opentelemetry/api";"#,
        r#"(() => {
            const tracer = trace.getTracer('test');
            const span = tracer.startSpan('op');
            span.setAttribute('key', 'value');
            span.setStatus({ code: 1 });
            span.end();
            return true;
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── @juanibiapina/pi-extension-settings ────────────────────────────────────

#[test]
fn juanibiapina_settings_import() {
    let result = eval_ext(
        r#"import { getSetting } from "@juanibiapina/pi-extension-settings";"#,
        r#"typeof getSetting"#,
    );
    assert_eq!(result, "function");
}

#[test]
fn juanibiapina_settings_default() {
    let result = eval_ext(
        r#"import { getSetting } from "@juanibiapina/pi-extension-settings";"#,
        r#"(() => {
            const val = getSetting(null, 'theme', 'dark');
            return val;
        })()"#,
    );
    assert_eq!(result, "dark");
}

// ─── @xterm/addon-serialize ─────────────────────────────────────────────────

#[test]
fn xterm_addon_serialize_import() {
    let result = eval_ext(
        r#"import { SerializeAddon } from "@xterm/addon-serialize";"#,
        r#"typeof SerializeAddon"#,
    );
    assert_eq!(result, "function");
}

// ─── turndown-plugin-gfm ───────────────────────────────────────────────────

#[test]
fn turndown_plugin_gfm_import() {
    let result = eval_ext(
        r#"import { gfm, tables } from "turndown-plugin-gfm";"#,
        r#"typeof gfm === 'function' && typeof tables === 'function'"#,
    );
    assert_eq!(result, "true");
}

// ─── @opentelemetry/resources ───────────────────────────────────────────────

#[test]
fn otel_resources_import() {
    let result = eval_ext(
        r#"import { Resource, resourceFromAttributes } from "@opentelemetry/resources";"#,
        r#"typeof resourceFromAttributes === 'function'"#,
    );
    assert_eq!(result, "true");
}

// ─── @opentelemetry/sdk-trace-base ──────────────────────────────────────────

#[test]
fn otel_sdk_trace_import() {
    let result = eval_ext(
        r#"import { BasicTracerProvider, BatchSpanProcessor } from "@opentelemetry/sdk-trace-base";"#,
        r#"typeof BasicTracerProvider === 'function' && typeof BatchSpanProcessor === 'function'"#,
    );
    assert_eq!(result, "true");
}

// ─── @opentelemetry/semantic-conventions ─────────────────────────────────────

#[test]
fn otel_semantic_conventions_import() {
    let result = eval_ext(
        r#"import { SemanticResourceAttributes } from "@opentelemetry/semantic-conventions";"#,
        r#"SemanticResourceAttributes.SERVICE_NAME === 'service.name'"#,
    );
    assert_eq!(result, "true");
}

// ─── node:util stripVTControlCharacters ─────────────────────────────────────

#[test]
fn node_util_strip_vt_control_chars() {
    let result = eval_ext(
        r#"import { stripVTControlCharacters } from "node:util";"#,
        r#"typeof stripVTControlCharacters"#,
    );
    assert_eq!(result, "function");
}

// ─── openai ────────────────────────────────────────────────────────────────

#[test]
fn openai_imports() {
    let result = eval_ext(
        r#"import OpenAI, { OpenAI as NamedOpenAI } from "openai";"#,
        r#"(() => {
            const client = new OpenAI({ apiKey: "test" });
            return typeof client.chat.completions.create === 'function'
                && typeof NamedOpenAI === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── adm-zip ───────────────────────────────────────────────────────────────

#[test]
fn adm_zip_import() {
    let result = eval_ext(
        r#"import AdmZip from "adm-zip";"#,
        r#"(() => {
            const zip = new AdmZip();
            return Array.isArray(zip.getEntries())
                && typeof zip.readAsText === 'function'
                && typeof zip.extractAllTo === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── linkedom ──────────────────────────────────────────────────────────────

#[test]
fn linkedom_parse_html() {
    let result = eval_ext(
        r#"import { parseHTML } from "linkedom";"#,
        r#"(() => {
            const { document } = parseHTML('<div>ok</div>');
            return document.documentElement.outerHTML === '<div>ok</div>'
                && typeof document.querySelector === 'function'
                && Array.isArray(document.querySelectorAll('div'));
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── @sourcegraph/scip-typescript ──────────────────────────────────────────

#[test]
fn scip_typescript_import() {
    let result = eval_ext(
        r#"import { scip } from "@sourcegraph/scip-typescript";"#,
        r#"(() => {
            const idx = new scip.Index();
            return typeof scip.Index === 'function' && typeof idx === 'object';
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn scip_typescript_subpath_import() {
    let result = eval_ext(
        r#"import { scip } from "@sourcegraph/scip-typescript/dist/src/scip.js";"#,
        r#"(() => {
            const idx = new scip.Index();
            return typeof scip.Index === 'function' && typeof idx === 'object';
        })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn scip_typescript_main_import() {
    let result = eval_ext(
        r#"import main from "@sourcegraph/scip-typescript/dist/src/main.js";"#,
        r#"typeof main"#,
    );
    assert_eq!(result, "function");
}

// ─── ws ────────────────────────────────────────────────────────────────────

#[test]
fn ws_imports() {
    let result = eval_ext(
        r#"import WebSocket, { WebSocketServer } from "ws";"#,
        r#"(() => {
            const ws = new WebSocket("ws://example");
            const server = new WebSocketServer();
            return typeof ws.send === 'function'
                && ws.readyState === WebSocket.CONNECTING
                && typeof server.on === 'function'
                && typeof WebSocket.Server === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── axios ─────────────────────────────────────────────────────────────────

#[test]
fn axios_imports() {
    let result = eval_ext(
        r#"import axios from "axios";"#,
        r#"(() => {
            const req = axios.get("https://example.com");
            return typeof axios.create === 'function' && typeof req.then === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── open ──────────────────────────────────────────────────────────────────

#[test]
fn open_imports() {
    let result = eval_ext(
        r#"import open from "open";"#,
        r#"(() => {
            const p = open("https://example.com");
            return typeof p.then === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── commander ─────────────────────────────────────────────────────────────

#[test]
fn commander_imports() {
    let result = eval_ext(
        r#"import { Command, program } from "commander";"#,
        r#"(() => {
            const cmd = new Command("test").option("-f, --flag").action(() => {});
            return typeof cmd.parse === 'function' && typeof program.parse === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}

// ─── chalk ─────────────────────────────────────────────────────────────────

#[test]
fn chalk_imports() {
    let result = eval_ext(
        r#"import chalk from "chalk";"#,
        r#"(() => {
            return chalk.red.bold("hi") === "hi" && typeof chalk.hex("fff") === 'function';
        })()"#,
    );
    assert_eq!(result, "true");
}
