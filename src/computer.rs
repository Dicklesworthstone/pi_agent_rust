//! Opt-in desktop automation (bd-cv653.2.5).
//!
//! Live operations use OS helpers, never the deterministic fixture backend.
//! Host approval is required for input and clipboard mutations by default.
//! Screenshots and clipboard reads can expose private desktop data: enable this
//! tool only for a desktop the session is authorized to inspect.

use crate::agent_cx::AgentCx;
use crate::ask::{AskHandler, AskOption, AskQuestion, AskRequest};
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use async_trait::async_trait;
use futures::future::{Either, select};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod mock;
mod native;
#[cfg(unix)]
mod process;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
    pub scale_factor: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub app_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_minimized: bool,
    pub is_focused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AxNode {
    pub role: String,
    pub title: Option<String>,
    pub value: Option<String>,
    pub enabled: bool,
    pub focused: bool,
    pub children: Vec<Self>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputerAuditEntry {
    pub timestamp_ms: u64,
    pub action: String,
    pub details: Value,
    pub allowed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ComputerSettings {
    #[serde(alias = "enableComputer")]
    pub enable_computer: Option<bool>,
    #[serde(alias = "requireApproval")]
    pub require_approval: Option<bool>,
    #[serde(alias = "screenshotDir")]
    pub screenshot_dir: Option<String>,
}

pub struct ComputerTool {
    cwd: PathBuf,
    mock_mode: Option<bool>,
    clipboard_buffer: Mutex<String>,
    audit_log: Mutex<VecDeque<ComputerAuditEntry>>,
    require_approval: bool,
    approval_handler: Option<AskHandler>,
    helpers: BTreeMap<String, PathBuf>,
    state: Arc<asupersync::sync::Mutex<native::State>>,
}

impl ComputerTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            mock_mode: None,
            clipboard_buffer: Mutex::new(String::new()),
            audit_log: Mutex::new(VecDeque::new()),
            require_approval: true,
            approval_handler: None,
            helpers: BTreeMap::new(),
            state: Arc::new(asupersync::sync::Mutex::new(native::State::default())),
        }
    }

    #[must_use]
    pub const fn with_mock(mut self, mock: bool) -> Self {
        self.mock_mode = Some(mock);
        self
    }

    /// Trusted host policy, not a model-facing argument. The registry forwards
    /// computer.requireApproval here. Disabling it grants session-wide input.
    #[must_use]
    pub const fn with_require_approval(mut self, require: bool) -> Self {
        self.require_approval = require;
        self
    }

    /// Install the host's actual picker. There is no noninteractive recommended
    /// answer fallback: absence, dismissal, errors and malformed replies deny.
    #[must_use]
    pub fn with_approval_handler(mut self, handler: AskHandler) -> Self {
        self.approval_handler = Some(handler);
        self
    }

    /// Override an OS helper from trusted host code (also useful for protocol
    /// fixtures). Tool arguments cannot choose executables or inject a PATH.
    pub fn with_helper_path(mut self, name: &str, path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if !native::HELPERS.contains(&name) || !path.is_absolute() {
            return Err(error(
                "helper override requires a known helper and an absolute path",
            ));
        }
        self.helpers.insert(name.to_string(), path);
        Ok(self)
    }

    pub fn get_audit_log(&self) -> Vec<ComputerAuditEntry> {
        self.audit_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    fn record_audit(&self, action: &str, args: &Value, allowed: bool, outcome: &str, mock: bool) {
        // Never duplicate typed text, clipboard contents, window titles or
        // screenshots in a long-lived audit log. Retain only bounded metadata.
        let timestamp_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let details = json!({
            "outcome": outcome, "mock": mock,
            "window_id": args.get("window_id").and_then(Value::as_u64),
            "display_id": args.get("display_id").and_then(Value::as_u64),
            "text_bytes": args.get("text").and_then(Value::as_str).map(str::len)
        });
        let mut log = self
            .audit_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if log.len() == 256 {
            log.pop_front();
        }
        log.push_back(ComputerAuditEntry {
            timestamp_ms,
            action: action.chars().take(64).collect(),
            details,
            allowed,
        });
    }

    fn is_mock(&self) -> bool {
        self.mock_mode
            .unwrap_or_else(|| std::env::var("PI_COMPUTER_MOCK").as_deref() == Ok("1"))
    }

    async fn authorize(&self, action: &str, args: &Value) -> Result<()> {
        if !self.require_approval || !mutating(action) {
            return Ok(());
        }
        let handler = self.approval_handler.as_ref().ok_or_else(|| error(
            "desktop input requires host approval; no approval handler is installed. The host may install with_approval_handler, or explicitly grant session-wide input with computer.requireApproval=false. Tool arguments cannot grant permission",
        ))?;
        let id = uuid::Uuid::new_v4().to_string();
        let request = AskRequest {
            questions: vec![AskQuestion {
                id: Some(id.clone()),
                header: Some("Desktop permission".into()),
                question: format!(
                    "Allow this desktop action once? It can affect the active application.\n{args}"
                ),
                options: vec![
                    AskOption {
                        label: "Deny".into(),
                        description: None,
                    },
                    AskOption {
                        label: "Allow once".into(),
                        description: None,
                    },
                ],
                recommended: Some(0),
                multi: false,
            }],
        };
        let response = handler(request).await?;
        if response.dismissed || response.answers.len() != 1 {
            return Err(error("desktop action was not approved"));
        }
        let answer = &response.answers[0];
        if answer.question_id != id
            || answer.other.is_some()
            || answer.selected.as_slice() != ["Allow once"]
        {
            return Err(error("desktop action was not approved"));
        }
        Ok(())
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        "computer"
    }
    fn label(&self) -> &str {
        "Computer"
    }
    fn description(&self) -> &str {
        "Inspect the authorized desktop, capture actual screenshots, and perform OS input/clipboard actions. Native capabilities depend on the desktop backend and installed helpers. Input requires trusted host approval; unavailable operations fail explicitly."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object", "required": ["action"], "additionalProperties": false,
            "properties": {
                "action": {"type":"string", "enum":["list_displays","list_windows","screenshot","mouse_move","mouse_click","mouse_drag","key_type","key_press","ax_tree","clipboard_read","clipboard_write","scroll","focus_window"]},
                "display_id": {"type":"integer", "minimum":1, "description":"Monitor ID from list_displays"},
                "window_id": {"type":"integer", "minimum":1, "description":"Window ID from list_windows; input with this field requires that window to be focused"},
                "x": {"type":"integer", "description":"Desktop X coordinate: pixels on X11, points on macOS. Convert screenshot pixels using its bounds and pixels_per_point_x metadata"},
                "y": {"type":"integer", "description":"Desktop Y coordinate: pixels on X11, points on macOS. Convert screenshot pixels using its bounds and pixels_per_point_y metadata"},
                "button": {"type":"string", "enum":["left","right","middle"]},
                "text": {"type":"string", "maxLength":4096, "description":"Literal input/clipboard text, never executed as helper commands"},
                "key": {"type":"string", "description":"One key/chord, e.g. Return, Tab, Escape, Ctrl+C"},
                "direction": {"type":"string", "enum":["up","down","left","right"]},
                "amount": {"type":"integer", "minimum":1, "maximum":100},
                "output_path": {"type":"string", "description":"New workspace-relative PNG destination; existing files are never overwritten"},
                "timeout_ms": {"type":"integer", "minimum":1, "maximum":120_000, "default":30_000}
            }
        })
    }
    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
            .union(ToolEffects::write())
            .union(ToolEffects::process())
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let mock = self.is_mock();
        let duration: Duration = match native::validate(&args) {
            Ok(duration) => duration,
            Err(failure) => {
                self.record_audit(action, &args, false, "invalid", mock);
                return Err(failure);
            }
        };
        if mock {
            let result = mock::execute(self, &args);
            self.record_audit(
                action,
                &args,
                true,
                if result.is_ok() { "success" } else { "error" },
                true,
            );
            return result;
        }
        let owner = AgentCx::for_current_or_request();
        let mut allowed = false;
        let operation = async {
            native::check_owner(&owner)?;
            self.authorize(action, &args).await?;
            allowed = true;
            // This owned guard is Send, unlike asupersync's borrowed guard.
            let mut state =
                asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&self.state), owner.cx())
                    .await
                    .map_err(|_| error("desktop session lock cancelled"))?;
            native::execute(&owner, &self.cwd, &self.helpers, &mut state, &args).await
        };
        let cancelled = async {
            let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
            let _ = receiver.recv(owner.cx()).await;
            drop(sender);
        };
        let watchdog = async {
            match select(Box::pin(owner.time().sleep(duration)), Box::pin(cancelled)).await {
                Either::Left(_) => {
                    "desktop action timed out; OS side effects may already have occurred"
                }
                Either::Right(_) => {
                    "desktop action cancelled; OS side effects may already have occurred"
                }
            }
        };
        let result = match select(Box::pin(operation), Box::pin(watchdog)).await {
            Either::Left((result, _)) => result,
            Either::Right((message, pending)) => {
                drop(pending);
                Err(error(message))
            }
        };
        self.record_audit(
            action,
            &args,
            allowed,
            if result.is_ok() { "success" } else { "error" },
            false,
        );
        result
    }
}

fn mutating(action: &str) -> bool {
    matches!(
        action,
        "mouse_move"
            | "mouse_click"
            | "mouse_drag"
            | "key_type"
            | "key_press"
            | "clipboard_write"
            | "scroll"
            | "focus_window"
    )
}
fn error(message: impl Into<String>) -> Error {
    Error::tool("computer", message)
}
fn output(text: String, mut details: Value, mock: bool) -> ToolOutput {
    details["mock"] = json!(mock);
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_flags_cannot_authorize_input() {
        assert!(
            native::validate(&json!({"action":"key_type","text":"test","confirmed":true})).is_err()
        );
        let tool = ComputerTool::new(Path::new("."));
        assert!(
            futures::executor::block_on(tool.authorize("key_type", &json!({"text":"test"})))
                .is_err()
        );
    }
    #[test]
    fn audit_is_bounded_and_excludes_private_payloads() {
        let tool = ComputerTool::new(Path::new("."));
        for _ in 0..300 {
            tool.record_audit(
                "key_type",
                &json!({"text":"private-secret"}),
                false,
                "denied",
                false,
            );
        }
        let entries = tool.get_audit_log();
        assert_eq!(entries.len(), 256);
        assert!(!entries[0].allowed);
        assert!(
            !serde_json::to_string(&entries)
                .unwrap()
                .contains("private-secret")
        );
    }
    #[test]
    fn only_exact_host_selection_approves() {
        use crate::ask::{AskAnswer, AskResponse};
        for selected in ["Deny", "Allow once"] {
            let handler: AskHandler = Arc::new(move |request| {
                Box::pin(async move {
                    Ok(AskResponse {
                        dismissed: false,
                        answers: vec![AskAnswer {
                            question_id: request.questions[0].id.clone().unwrap(),
                            selected: vec![selected.into()],
                            other: None,
                        }],
                    })
                })
            });
            let tool = ComputerTool::new(Path::new(".")).with_approval_handler(handler);
            assert_eq!(
                futures::executor::block_on(tool.authorize("key_press", &json!({"key":"Return"})))
                    .is_ok(),
                selected == "Allow once"
            );
        }
    }

    #[test]
    fn native_failure_never_publishes_a_fixture_screenshot() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ComputerTool::new(dir.path())
            .with_mock(false)
            .with_helper_path("scrot", dir.path().join("missing-scrot"))
            .unwrap()
            .with_helper_path("swift", dir.path().join("missing-swift"))
            .unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let result = runtime.block_on(tool.execute(
            "missing-native",
            json!({"action":"screenshot","output_path":"must-not-exist.png"}),
            None,
        ));
        assert!(result.is_err());
        assert!(!dir.path().join("must-not-exist.png").exists());
        assert_eq!(tool.get_audit_log()[0].details["outcome"], "error");
    }

    #[test]
    fn tool_boundary_enforces_approval_before_any_native_input() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ComputerTool::new(dir.path())
            .with_mock(false)
            .with_helper_path("xdotool", dir.path().join("must-not-run"))
            .unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let result = runtime.block_on(tool.execute(
            "denied-input",
            json!({"action":"key_type","text":"private-secret"}),
            None,
        ));
        let failure = result.unwrap_err();
        assert!(failure.to_string().contains("host approval"));
        let log = tool.get_audit_log();
        assert!(!log[0].allowed);
        assert!(
            !serde_json::to_string(&log)
                .unwrap()
                .contains("private-secret")
        );
    }
}
