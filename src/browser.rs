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

mod cdp;
mod dialog;
mod download;
mod exports;
mod frames;
mod interaction;
mod launch;
mod mock;
mod policy;
mod session;
mod storage;

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
    sessions: session::Sessions,
    cdp_endpoint: Option<String>,
    launch_options: Option<BrowserLaunchOptions>,
    domain_allowlist: Option<Vec<String>>,
}

impl BrowserTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            mock_mode: None,
            sessions: session::Sessions::default(),
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
         Storage-state actions require an explicit tab and private workspace state_path; files contain plaintext credentials, never returned inline. \
         Attach diagnostics to an action to capture console output and uncaught exceptions before \
         and after it; diagnostics.network:true also correlates request failures, HTTP status and redirects. \
         Use action:diagnostics to observe an explicit tab without triggering work. \
         start launches or attaches; status never launches; stop only stops a Pi-owned browser. \
         Browser state is conversation-owned; switching back reuses its tabs. release discards only \
         this conversation's browser state and requests owned-process cleanup, never stopping an \
         externally attached browser. Four namespaces are retained without automatic eviction. \
         Failures are errors, never simulated successes. Selecting upload files exposes them to page scripts. \
         Semantic selectors support text/, label/, placeholder/, testid/, alt/, title/ and role/<role>[name=\"...\" exact]; ambiguous matches are refused. \
         click accepts left/middle/right buttons, left double-clicks and modifier keys; sequences never replay after interruption. Dispatch acknowledgement does not verify the application's outcome. \
         batch executes ordered native calls under one conversation lease and total deadline, preserving partial results and stopping at the first error without replay. \
         wait_for supports visible/hidden/attached/detached, optional rendered text and a sampled stable_ms interval; waits pin the current document and never send input. \
         click/type/fill/press may include wait_after to observe a same-document postcondition after that one input, without replay or a new timeout."
    }

    fn parameters(&self) -> Value {
        let mut schema = json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "status", "stop", "open", "goto", "close", "list_tabs",
                             "list_frames", "snapshot", "ax_tree", "evaluate", "click", "type", "fill", "press",
                             "scroll", "wait_for", "select_option", "set_checked", "upload", "download", "screenshot", "print_pdf",
                             "handle_dialog", "diagnostics", "release"],
                    "description": "Browser action; ordinary actions lazily start the managed browser. release accepts only action and timeout_ms: discard this conversation's state and request owned-browser cleanup; attached pages/processes remain open."
                },
                "tab": {"type": "string", "description": "Tab name or target ID; default: active tab"},
                "frame": {"type": "string", "maxLength": 256,
                          "description": "Frame ID from list_frames; requires an explicit tab. Scopes snapshot, ax_tree, evaluate, click, type, fill, press, scroll, wait_for, select_option or set_checked to that frame for this call only, including nested out-of-process frames. Scoped press and scroll also require selector. Omit for the main document. Frames and all ancestors must pass domainAllowlist; detached or navigated documents fail, never fall back to the main frame. Cross-process click/scroll project through embedding content quads and verify ancestor hit tests before native page input; occluded, invalid or pinch-zoomed geometry is rejected. Frame-scoped uploads/downloads, exports, navigation and dialog responses are not supported."},
                "url": {"type": "string", "description": "HTTP(S) URL or about:blank for open/goto"},
                "script": {"type": "string", "description": "JavaScript expression for evaluate; frame-scoped evaluation uses an isolated DOM world, not the page's JavaScript globals"},
                "selector": {"type": "string", "description": "CSS or snapshot @eN; also text/, label/, placeholder/, testid/, alt/, title/ and role/button[name=\"Save\" exact]. Semantic selectors are case-sensitive, bounded to 16 KiB and require one matching element; text-like names normalize whitespace and match substrings, testid matches data-testid exactly. Role/label use Chromium accessible names, not field values. Semantic selectors work on click/type/fill/press/wait_for/select_option/set_checked/upload and frame-scoped scroll, not inspection/export/download actions. Frame references require the same tab and frame; scoped press/scroll require selector."},
                "text": {"type": "string", "description": "Literal input for type/fill; wait_for optionally matches nonempty rendered text up to 16 KiB, without reading input/textarea values."},
                "state": {"type":"string","enum":["visible","hidden","attached","detached"],"description":"wait_for only; default visible. hidden means absent or not rendered visibly. attached/detached test DOM connection. Non-visible states require CSS or a known snapshot reference; semantic lookup omits hidden nodes. Navigation fails rather than proving absence."},
                "text_match": {"type":"string","enum":["contains","exact"],"description":"wait_for with text only; default contains. Case-sensitive normalized rendered DOM text; excludes input/textarea values and hidden descendants. Not regular expressions. Text requires visible or attached state."},
                "stable_ms": {"type":"integer","minimum":0,"maximum":10000,"default":0,"description":"wait_for only. Require matching sampled observations on the same node (or absence) for this interval. A mismatch or replacement resets it; timeout_ms still bounds the whole call. Not continuous observation or a guarantee about the next action."},
                "wait_after": interaction::wait::after_schema(),
                "key": {"type": "string", "description": "Key for press, e.g. Enter, Tab, ArrowDown"},
                "button": {"type":"string","enum":["left","middle","right"],"default":"left","description":"click only: native pointer button. Right opens native page context menus where supported; middle can open links in another tab. Existing tool approvals still apply."},
                "click_count": {"type":"integer","enum":[1,2],"default":1,"description":"click only: 2 sends a native left double-click (two press/release pairs with increasing clickCount). Right/middle require 1. A changed document or detached original node stops remaining input; completed effects are not undone or retried."},
                "modifiers": {"type":"array","maxItems":4,"uniqueItems":true,"items":{"type":"string","enum":["Alt","Control","Meta","Shift"]},"description":"click only: modifier flags on each native pointer event, not sticky keyboard presses. Distinct names only. Defaults to []."},
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
        });
        session::batch::extend_schema(&mut schema);
        storage::parameters(schema)
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
            .union(ToolEffects::write())
            .union(ToolEffects::network())
            .union(ToolEffects::process())
    }

    fn bind_job_session_scope(&mut self, scope: crate::jobs::JobSessionScope) {
        self.sessions.bind(scope);
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        self.sessions.execute(self, &args).await
    }
}

impl BrowserTool {
    /// The session owner chooses the namespace, never an argument from the model.
    /// Keep all backend validation, deadlines and process ownership in the native path.
    async fn execute_in_namespace(
        &self,
        namespace: &session::Namespace,
        args: &Value,
    ) -> Result<ToolOutput> {
        let action = required(args, "action")?;
        let semantic_selector = interaction::selectors::validate_input(args)?;
        let configured_pointer = interaction::pointer::validate_input(args)?;
        let configured_wait = interaction::wait::validate_input(args)?;
        if self.is_mock() {
            if storage::is_action(action) || semantic_selector || configured_pointer || configured_wait || matches!(
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
                    "browser lifecycle, storage state, frames, structured forms, file uploads, dialogs and page exports require the native backend",
                ));
            }
            return namespace
                .mock_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .execute(&self.cwd, self.domain_allowlist.as_deref(), args);
        }
        cdp::execute(
            &namespace.live_state,
            self.cdp_endpoint.as_deref(),
            self.launch_options.as_ref(),
            &self.cwd,
            self.domain_allowlist.as_deref(),
            args,
        )
        .await
    }
}
