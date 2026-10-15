//! Opt-in Chromium automation through the native Chrome DevTools Protocol.
//!
//! By default, Pi launches an installed browser with an isolated temporary
//! profile. An explicit CDP endpoint attaches without taking process ownership.
//! Production never falls back to simulated results. Deterministic fixtures
//! must select `with_mock(true)` or `PI_BROWSER_MOCK=1` explicitly.

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

mod cdp;
mod dialog;
mod download;
mod exports;
mod frames;
mod interaction;
mod launch;
mod mock;
mod policy;

pub use launch::BrowserLaunchOptions;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserTabInfo {
    pub name: String,
    pub url: String,
    pub title: String,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserElementRef {
    pub ref_id: String,
    pub tag: String,
    pub role: String,
    pub text: String,
    pub selector: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserSnapshot {
    pub url: String,
    pub title: String,
    pub elements: Vec<BrowserElementRef>,
    pub summary: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct BrowserSettings {
    #[serde(alias = "enableBrowser")]
    pub enable_browser: Option<bool>,
    #[serde(alias = "executablePath")]
    pub executable_path: Option<String>,
    #[serde(alias = "remoteDebuggingPort")]
    pub remote_debugging_port: Option<u16>,
    pub headless: Option<bool>,
    #[serde(alias = "userAgent")]
    pub user_agent: Option<String>,
    #[serde(alias = "domainAllowlist")]
    pub domain_allowlist: Option<Vec<String>>,
}

pub struct BrowserTool {
    cwd: PathBuf,
    mock_mode: Option<bool>,
    mock_state: Mutex<mock::State>,
    /// An owned guard keeps the serialized CDP/managed-process lifetime Send.
    live_state: Arc<asupersync::sync::Mutex<cdp::Session>>,
    cdp_endpoint: Option<String>,
    launch_options: Option<BrowserLaunchOptions>,
    domain_allowlist: Option<Vec<String>>,
}

impl BrowserTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            mock_mode: None,
            mock_state: Mutex::new(mock::State::default()),
            live_state: Arc::new(asupersync::sync::Mutex::new(cdp::Session::default())),
            cdp_endpoint: None,
            launch_options: None,
            domain_allowlist: None,
        }
    }

    #[must_use]
    pub const fn with_mock(mut self, mock: bool) -> Self {
        self.mock_mode = Some(mock);
        self
    }

    /// Attach to an explicitly selected loopback CDP HTTP endpoint.
    /// Takes precedence over environment configuration. Pi will not stop it.
    #[must_use]
    pub fn with_cdp_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.cdp_endpoint = Some(endpoint.into());
        self.launch_options = None;
        self
    }

    /// Select managed launch from trusted host code, overriding the environment.
    /// Stop an already-running browser before changing its connection configuration.
    #[must_use]
    pub fn with_launch_options(mut self, options: BrowserLaunchOptions) -> Self {
        self.launch_options = Some(options);
        self.cdp_endpoint = None;
        self
    }

    #[must_use]
    pub fn with_domain_allowlist(mut self, allowlist: Option<Vec<String>>) -> Self {
        self.domain_allowlist = allowlist;
        self
    }

    fn is_mock(&self) -> bool {
        self.mock_mode
            .unwrap_or_else(|| std::env::var("PI_BROWSER_MOCK").is_ok_and(|v| v == "1"))
    }
}

fn output(text: impl Into<String>, details: Value) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        details: Some(details),
        is_error: false,
    }
}

fn required<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::tool("browser", format!("missing required {name} parameter")))
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn label(&self) -> &str {
        "Browser"
    }

    fn description(&self) -> &str {
        "Chromium automation with an owned isolated browser, or explicit loopback attachment \
         through PI_BROWSER_CDP_URL. Supports tabs, navigation, JavaScript, snapshots, input, \
         structured option selection and checked-state control, frame discovery and frame-scoped \
         inspection/input, workspace file uploads, JavaScript dialogs, screenshots and PDF export. \
         Attach diagnostics to an action to capture console output and uncaught exceptions before \
         and after it; diagnostics.network:true also correlates request failures, HTTP status and redirects. \
         Use action:diagnostics to observe an explicit tab without triggering work. \
         start launches or attaches; status never launches; stop only stops a Pi-owned browser. \
         Failures are errors, never simulated successes. Selecting upload files exposes them to page scripts."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "status", "stop", "open", "goto", "close", "list_tabs",
                             "list_frames", "snapshot", "ax_tree", "evaluate", "click", "type", "fill", "press",
                             "scroll", "wait_for", "select_option", "set_checked", "upload", "download", "screenshot", "print_pdf",
                             "handle_dialog", "diagnostics"],
                    "description": "Browser action; ordinary actions lazily start the managed browser"
                },
                "tab": {"type": "string", "description": "Tab name or target ID; default: active tab"},
                "frame": {"type": "string", "maxLength": 256,
                          "description": "Frame ID from list_frames; requires an explicit tab. Scopes snapshot, ax_tree, evaluate, click, type, fill, press, scroll, wait_for, select_option or set_checked to that frame for this call only, including nested out-of-process frames. Scoped press and scroll also require selector. Omit for the main document. Frames and all ancestors must pass domainAllowlist; detached or navigated documents fail, never fall back to the main frame. Cross-process click/scroll project through embedding content quads and verify ancestor hit tests before native page input; occluded, invalid or pinch-zoomed geometry is rejected. Frame-scoped uploads/downloads, exports, navigation and dialog responses are not supported."},
                "url": {"type": "string", "description": "HTTP(S) URL or about:blank for open/goto"},
                "script": {"type": "string", "description": "JavaScript expression for evaluate; frame-scoped evaluation uses an isolated DOM world, not the page's JavaScript globals"},
                "selector": {"type": "string", "description": "CSS selector or snapshot element ref, e.g. @e1. Frame references require the same tab and frame; frame-scoped press/scroll require a selector."},
                "text": {"type": "string", "description": "Text for type/fill"},
                "key": {"type": "string", "description": "Key for press, e.g. Enter, Tab, ArrowDown"},
                "diagnostics": {
                    "type": "object", "additionalProperties": false,
                    "description": "Opt-in native console/exception capture around a main-document action, or options for action:diagnostics. Requires an explicit tab. Records only admitted main-world contexts; includes buffered console entries when Chromium supplies them. Object previews/getters are never read. Free-form console text can contain application secrets. Bounded latest records appear in details.diagnostics and text; failures remain errors and work is never replayed. Unsupported with frame, lifecycle, handle_dialog, uploads, downloads or exports. No persistent/background subscription.",
                    "properties": {
                        "network": {"type":"boolean","default":false,"description":"Also capture admitted main-frame HTTP request hops, responses, redirects and loading failures on this connection. Query/fragment fields, headers, cookies and bodies are omitted; no interception, response-body fetch or network-idle guarantee. Pending requests remain pending at capture end. max_events also bounds a separate 64-KiB latest request tail."},
                        "settle_ms": {"type":"integer","minimum":0,"maximum":10000,"default":250,"description":"Fixed receive window after success; shares timeout_ms, never extends it. 0 stops at action completion. Not a network-idle guarantee."},
                        "max_events": {"type":"integer","minimum":1,"maximum":200,"default":100,"description":"Maximum latest console records, also bounded to 64 KiB; dropped records are counted."}
                    }
                },
                "checked": {"type":"boolean", "description":"set_checked: required desired state for a native checkbox/radio input. Already-matching state is a no-op; otherwise uses one native click and verifies the same node. Cannot uncheck a selected radio; choose another in its group. Mixed/indeterminate and custom ARIA controls are rejected. Requires selector; no implicit dialog consent or retry."},
                "options": {
                    "type": "array", "maxItems": 100,
                    "description": "select_option: required full selection for an ordinary HTML select. Each item specifies exactly one literal value, exact DOM label, or zero-based index. Missing, ambiguous, duplicate and disabled options fail before mutation. [] clears; multiple entries require a multiple select. At most 64 KiB of selector text. Uses DOM selectedness setters and input/change events, not trusted native mouse selection; verifies retained results on the same node.",
                    "items": {
                        "type": "object", "additionalProperties": false,
                        "properties": {
                            "value": {"type":"string"},
                            "label": {"type":"string"},
                            "index": {"type":"integer", "minimum":0, "maximum":4095}
                        },
                        "oneOf": [{"required":["value"]}, {"required":["label"]}, {"required":["index"]}]
                    }
                },
                "accept": {"type": "boolean", "description": "handle_dialog: required explicit choice; true accepts, false dismisses the selected tab's current JavaScript dialog. Requires an explicit tab; never replays the triggering action."},
                "prompt_text": {"type": "string", "description": "handle_dialog: exact prompt answer, at most 64 KiB. Only with accept=true; omit to send no promptText override."},
                "dialog_response": {
                    "type": "object", "additionalProperties": false,
                    "required": ["type", "message", "url", "accept"],
                    "description": "evaluate/click/type/fill/press: expect exactly one dialog on an explicit tab, matching type, message and URL exactly. Respond once and wait within timeout_ms; never replay the trigger. If no dialog appears, the operation times out. Not a persistent auto-accept policy.",
                    "properties": {
                        "type": {"type":"string","enum":["alert","confirm","prompt","beforeunload"]},
                        "message": {"type":"string","description":"Exact dialog message, at most 64 KiB"},
                        "url": {"type":"string","description":"Exact dialog source URL; must pass domainAllowlist"},
                        "accept": {"type":"boolean"},
                        "prompt_text": {"type":"string","description":"Exact answer for an accepted prompt only, at most 64 KiB; empty is allowed"}
                    }
                },
                "files": {"type": "array", "maxItems": 10, "items": {"type": "string"},
                          "description": "upload: workspace-relative regular files, no symlinks or parent traversal; [] clears selection. At most 20 MiB per call."},
                "output_path": {"type": "string", "description": "New workspace-relative artifact destination; screenshot requires .png, print_pdf requires .pdf, download keeps any extension; existing files are never overwritten"},
                "full_page": {"type": "boolean", "description": "screenshot: capture beyond the viewport (default false)"},
                "landscape": {"type": "boolean", "description": "print_pdf: landscape paper orientation (default false)"},
                "print_background": {"type": "boolean", "description": "print_pdf: include background graphics (default true)"},
                "page_ranges": {"type": "string", "description": "print_pdf: page ranges such as 1-3,5; omit for all pages"},
                "delta_x": {"type": "number", "description": "Horizontal scroll delta in CSS pixels"},
                "delta_y": {"type": "number", "description": "Vertical scroll delta in CSS pixels; default 600"},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 120_000,
                               "description": "Whole-operation deadline, including launch, connection and lock wait"}
            }
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
            .union(ToolEffects::write())
            .union(ToolEffects::network())
            .union(ToolEffects::process())
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let action = required(&args, "action")?;
        if self.is_mock() {
            if matches!(
                action,
                "start"
                    | "status"
                    | "stop"
                    | "list_frames"
                    | "upload"
                    | "download"
                    | "print_pdf"
                    | "handle_dialog"
                    | "diagnostics"
            ) || interaction::forms::is_action(action)
                || args.get("frame").is_some()
                || args.get("full_page").is_some()
                || args.get("dialog_response").is_some()
                || args.get("diagnostics").is_some()
            {
                return Err(Error::tool(
                    "browser",
                    "browser lifecycle, frames, structured forms, file uploads, dialogs and page exports require the native backend",
                ));
            }
            return self
                .mock_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .execute(&self.cwd, self.domain_allowlist.as_deref(), &args);
        }
        cdp::execute(
            &self.live_state,
            self.cdp_endpoint.as_deref(),
            self.launch_options.as_ref(),
            &self.cwd,
            self.domain_allowlist.as_deref(),
            &args,
        )
        .await
    }
}
