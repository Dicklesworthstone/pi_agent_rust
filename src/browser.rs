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
    /// Stop an already-running browser before changing connection configuration.
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
         workspace file uploads, JavaScript dialogs, screenshots and PDF export. start launches or attaches; \
         status never launches; stop only stops a Pi-owned browser. Failures are errors, \
         never simulated successes. Selecting upload files exposes them to page scripts."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "status", "stop", "open", "goto", "close", "list_tabs",
                             "snapshot", "ax_tree", "evaluate", "click", "type", "fill", "press",
                             "scroll", "wait_for", "upload", "download", "screenshot", "print_pdf",
                             "handle_dialog"],
                    "description": "Browser action; ordinary actions lazily start the managed browser"
                },
                "tab": {"type": "string", "description": "Tab name or target ID; default: active tab"},
                "url": {"type": "string", "description": "HTTP(S) URL or about:blank for open/goto"},
                "script": {"type": "string", "description": "JavaScript expression for evaluate"},
                "selector": {"type": "string", "description": "CSS selector or snapshot element ref, e.g. @e1"},
                "text": {"type": "string", "description": "Text for type/fill"},
                "key": {"type": "string", "description": "Key for press, e.g. Enter, Tab, ArrowDown"},
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
                "start" | "status" | "stop" | "upload" | "download" | "print_pdf"
                    | "handle_dialog"
            ) || args.get("full_page").is_some() || args.get("dialog_response").is_some()
            {
                return Err(Error::tool(
                    "browser",
                    "browser lifecycle, file uploads, dialogs and page exports require the native backend",
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
