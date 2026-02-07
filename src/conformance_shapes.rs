//! Shape-aware conformance harness for extension types.
//!
//! This module provides a unified harness that can load, exercise, and validate
//! each extension shape (tool, command, provider, event hook, UI component,
//! configuration, multi, general) through standardized lifecycle hooks:
//!
//! 1. **Load** — Parse and load the extension into the QuickJS runtime.
//! 2. **Verify registrations** — Check that expected registration types appeared.
//! 3. **Invoke** — Exercise the extension's primary dispatch path (tool call,
//!    command execution, event dispatch, etc.).
//! 4. **Shutdown** — Cleanly tear down the runtime and verify no panics or leaks.
//!
//! Each lifecycle step emits structured JSONL events for diagnostics.
//! Error reporting classifies failures into actionable categories.

use crate::extension_conformance_matrix::HostCapability;
use crate::extension_inclusion::ExtensionCategory;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

// ────────────────────────────────────────────────────────────────────────────
// Extension shape descriptor
// ────────────────────────────────────────────────────────────────────────────

/// Maps 1:1 to `ExtensionCategory` but is specialized for harness dispatch.
///
/// Each variant knows what registration types to expect and what invocation
/// protocol to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionShape {
    Tool,
    Command,
    Provider,
    EventHook,
    UiComponent,
    Configuration,
    Multi,
    General,
}

impl ExtensionShape {
    /// Convert from the inclusion-list category.
    #[must_use]
    pub const fn from_category(cat: &ExtensionCategory) -> Self {
        match cat {
            ExtensionCategory::Tool => Self::Tool,
            ExtensionCategory::Command => Self::Command,
            ExtensionCategory::Provider => Self::Provider,
            ExtensionCategory::EventHook => Self::EventHook,
            ExtensionCategory::UiComponent => Self::UiComponent,
            ExtensionCategory::Configuration => Self::Configuration,
            ExtensionCategory::Multi => Self::Multi,
            ExtensionCategory::General => Self::General,
        }
    }

    /// All shapes in canonical order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Tool,
            Self::Command,
            Self::Provider,
            Self::EventHook,
            Self::UiComponent,
            Self::Configuration,
            Self::Multi,
            Self::General,
        ]
    }

    /// Registration fields that MUST be non-empty after loading.
    ///
    /// Returns the JSON field names in the `RegisterPayload` that this shape
    /// is expected to populate.  For `Multi` and `General`, returns an empty
    /// slice (checked separately).
    #[must_use]
    pub const fn expected_registration_fields(&self) -> &'static [&'static str] {
        match self {
            Self::Tool => &["tools"],
            Self::Command => &["slash_commands"],
            Self::Provider => &["providers"],
            Self::EventHook => &["event_hooks"],
            Self::UiComponent => &["message_renderers"],
            Self::Configuration => &["flags", "shortcuts"],
            Self::Multi | Self::General => &[],
        }
    }

    /// Whether this shape supports runtime invocation (beyond registration).
    #[must_use]
    pub const fn supports_invocation(&self) -> bool {
        matches!(
            self,
            Self::Tool | Self::Command | Self::EventHook | Self::Multi
        )
    }

    /// The `HostCapability` values typically exercised by this shape.
    #[must_use]
    pub fn typical_capabilities(&self) -> Vec<HostCapability> {
        match self {
            Self::Tool => vec![
                HostCapability::Read,
                HostCapability::Write,
                HostCapability::Exec,
                HostCapability::Tool,
            ],
            Self::Command => vec![HostCapability::Session, HostCapability::Ui],
            Self::Provider => vec![HostCapability::Http, HostCapability::Env],
            Self::EventHook => vec![
                HostCapability::Session,
                HostCapability::Ui,
                HostCapability::Exec,
            ],
            Self::UiComponent => vec![HostCapability::Ui],
            Self::Configuration => vec![HostCapability::Env],
            Self::Multi => vec![HostCapability::Session, HostCapability::Tool],
            Self::General => vec![HostCapability::Log],
        }
    }
}

impl fmt::Display for ExtensionShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tool => write!(f, "tool"),
            Self::Command => write!(f, "command"),
            Self::Provider => write!(f, "provider"),
            Self::EventHook => write!(f, "event_hook"),
            Self::UiComponent => write!(f, "ui_component"),
            Self::Configuration => write!(f, "configuration"),
            Self::Multi => write!(f, "multi"),
            Self::General => write!(f, "general"),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Error classification
// ────────────────────────────────────────────────────────────────────────────

/// Failure category for conformance diagnostics.
///
/// Each variant maps to a human-readable explanation and a remediation hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Extension file could not be parsed or loaded.
    LoadError,
    /// Expected registration type was missing after load.
    MissingRegistration,
    /// Registration was present but structurally invalid.
    MalformedRegistration,
    /// Tool/command/event invocation returned an error.
    InvocationError,
    /// Invocation result did not match expectations.
    OutputMismatch,
    /// Extension timed out during load or invocation.
    Timeout,
    /// Extension shape is incompatible with the requested operation.
    IncompatibleShape,
    /// Shutdown did not complete cleanly.
    ShutdownError,
    /// QuickJS runtime or shim gap.
    RuntimeShimGap,
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadError => write!(f, "load_error"),
            Self::MissingRegistration => write!(f, "missing_registration"),
            Self::MalformedRegistration => write!(f, "malformed_registration"),
            Self::InvocationError => write!(f, "invocation_error"),
            Self::OutputMismatch => write!(f, "output_mismatch"),
            Self::Timeout => write!(f, "timeout"),
            Self::IncompatibleShape => write!(f, "incompatible_shape"),
            Self::ShutdownError => write!(f, "shutdown_error"),
            Self::RuntimeShimGap => write!(f, "runtime_shim_gap"),
        }
    }
}

/// A classified conformance failure with context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShapeFailure {
    pub class: FailureClass,
    pub message: String,
    /// JSON path or field where the failure was detected (if applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Remediation hint for the developer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ShapeFailure {
    #[must_use]
    pub fn new(class: FailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            path: None,
            hint: None,
        }
    }

    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for ShapeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.class, self.message)?;
        if let Some(path) = &self.path {
            write!(f, " (at {path})")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, " — hint: {hint}")?;
        }
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// JSONL event logging
// ────────────────────────────────────────────────────────────────────────────

/// Lifecycle phase for JSONL event tagging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    Load,
    VerifyRegistrations,
    Invoke,
    Shutdown,
}

impl fmt::Display for LifecyclePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load => write!(f, "load"),
            Self::VerifyRegistrations => write!(f, "verify_registrations"),
            Self::Invoke => write!(f, "invoke"),
            Self::Shutdown => write!(f, "shutdown"),
        }
    }
}

/// Structured JSONL event emitted by the harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShapeEvent {
    pub timestamp: String,
    pub correlation_id: String,
    pub extension_id: String,
    pub shape: ExtensionShape,
    pub phase: LifecyclePhase,
    pub status: ShapeEventStatus,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<ShapeFailure>,
}

/// Status of a lifecycle event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeEventStatus {
    Ok,
    Fail,
    Skip,
}

impl ShapeEvent {
    /// Create a new event with the current timestamp.
    pub fn new(
        correlation_id: &str,
        extension_id: &str,
        shape: ExtensionShape,
        phase: LifecyclePhase,
    ) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            correlation_id: correlation_id.to_string(),
            extension_id: extension_id.to_string(),
            shape,
            phase,
            status: ShapeEventStatus::Ok,
            duration_ms: 0,
            details: None,
            failures: Vec::new(),
        }
    }

    /// Serialize to a single JSONL line (no trailing newline).
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Registration verification
// ────────────────────────────────────────────────────────────────────────────

/// Registration snapshot extracted after extension load.
///
/// This is the subset of `RegisterPayload` fields needed for shape verification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistrationSnapshot {
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub slash_commands: Vec<Value>,
    #[serde(default)]
    pub shortcuts: Vec<Value>,
    #[serde(default)]
    pub flags: Vec<Value>,
    #[serde(default)]
    pub event_hooks: Vec<String>,
    #[serde(default)]
    pub providers: Vec<Value>,
    #[serde(default)]
    pub models: Vec<Value>,
    #[serde(default)]
    pub message_renderers: Vec<Value>,
}

impl RegistrationSnapshot {
    /// Get the count of a registration field by name.
    #[must_use]
    pub fn field_count(&self, field: &str) -> usize {
        match field {
            "tools" => self.tools.len(),
            "slash_commands" => self.slash_commands.len(),
            "shortcuts" => self.shortcuts.len(),
            "flags" => self.flags.len(),
            "event_hooks" => self.event_hooks.len(),
            "providers" => self.providers.len(),
            "models" => self.models.len(),
            "message_renderers" => self.message_renderers.len(),
            _ => 0,
        }
    }

    /// Total number of registrations across all fields.
    #[must_use]
    pub fn total_registrations(&self) -> usize {
        self.tools.len()
            + self.slash_commands.len()
            + self.shortcuts.len()
            + self.flags.len()
            + self.event_hooks.len()
            + self.providers.len()
            + self.message_renderers.len()
    }

    /// Classify the shape based on what was actually registered.
    #[must_use]
    pub fn detected_shape(&self) -> ExtensionShape {
        let mut types = Vec::new();
        if !self.tools.is_empty() {
            types.push("tool");
        }
        if !self.slash_commands.is_empty() {
            types.push("command");
        }
        if !self.providers.is_empty() {
            types.push("provider");
        }
        if !self.event_hooks.is_empty() {
            types.push("event_hook");
        }
        if !self.message_renderers.is_empty() {
            types.push("ui_component");
        }
        if !self.flags.is_empty() || !self.shortcuts.is_empty() {
            types.push("configuration");
        }

        match types.len() {
            0 => ExtensionShape::General,
            1 => match types[0] {
                "tool" => ExtensionShape::Tool,
                "command" => ExtensionShape::Command,
                "provider" => ExtensionShape::Provider,
                "event_hook" => ExtensionShape::EventHook,
                "ui_component" => ExtensionShape::UiComponent,
                "configuration" => ExtensionShape::Configuration,
                _ => ExtensionShape::General,
            },
            _ => ExtensionShape::Multi,
        }
    }
}

/// Verify that a registration snapshot matches the expected shape.
///
/// Returns a list of failures (empty means verification passed).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn verify_registrations(
    shape: ExtensionShape,
    snapshot: &RegistrationSnapshot,
) -> Vec<ShapeFailure> {
    let mut failures = Vec::new();

    match shape {
        ExtensionShape::Tool => {
            if snapshot.tools.is_empty() {
                failures.push(
                    ShapeFailure::new(
                        FailureClass::MissingRegistration,
                        "Tool extension must register at least one tool via registerTool()",
                    )
                    .with_path("registrations.tools")
                    .with_hint("Call pi.registerTool({name, description, parameters, handler})"),
                );
            }
            // Validate each tool has required fields
            for (idx, tool) in snapshot.tools.iter().enumerate() {
                if tool.get("name").and_then(Value::as_str).is_none() {
                    failures.push(
                        ShapeFailure::new(
                            FailureClass::MalformedRegistration,
                            format!("Tool [{idx}] missing 'name' field"),
                        )
                        .with_path(format!("registrations.tools[{idx}].name")),
                    );
                }
            }
        }
        ExtensionShape::Command => {
            if snapshot.slash_commands.is_empty() {
                failures.push(
                    ShapeFailure::new(
                        FailureClass::MissingRegistration,
                        "Command extension must register at least one slash command",
                    )
                    .with_path("registrations.slash_commands")
                    .with_hint("Call pi.registerCommand(name, {description, handler})"),
                );
            }
        }
        ExtensionShape::Provider => {
            if snapshot.providers.is_empty() {
                failures.push(
                    ShapeFailure::new(
                        FailureClass::MissingRegistration,
                        "Provider extension must register at least one provider",
                    )
                    .with_path("registrations.providers")
                    .with_hint("Call pi.registerProvider(name, {api, baseUrl, models})"),
                );
            }
        }
        ExtensionShape::EventHook => {
            if snapshot.event_hooks.is_empty() {
                failures.push(
                    ShapeFailure::new(
                        FailureClass::MissingRegistration,
                        "EventHook extension must register at least one event listener",
                    )
                    .with_path("registrations.event_hooks")
                    .with_hint("Call pi.on(eventName, handler)"),
                );
            }
        }
        ExtensionShape::UiComponent => {
            if snapshot.message_renderers.is_empty() {
                failures.push(
                    ShapeFailure::new(
                        FailureClass::MissingRegistration,
                        "UiComponent extension must register at least one message renderer",
                    )
                    .with_path("registrations.message_renderers")
                    .with_hint("Call pi.registerMessageRenderer({contentType, render})"),
                );
            }
        }
        ExtensionShape::Configuration => {
            if snapshot.flags.is_empty() && snapshot.shortcuts.is_empty() {
                failures.push(
                    ShapeFailure::new(
                        FailureClass::MissingRegistration,
                        "Configuration extension must register at least one flag or shortcut",
                    )
                    .with_path("registrations.flags|shortcuts")
                    .with_hint("Call pi.registerFlag(spec) or pi.registerShortcut(spec)"),
                );
            }
        }
        ExtensionShape::Multi => {
            // Multi extensions should have at least 2 distinct registration types
            let distinct_types = [
                !snapshot.tools.is_empty(),
                !snapshot.slash_commands.is_empty(),
                !snapshot.providers.is_empty(),
                !snapshot.event_hooks.is_empty(),
                !snapshot.message_renderers.is_empty(),
                !snapshot.flags.is_empty() || !snapshot.shortcuts.is_empty(),
            ]
            .iter()
            .filter(|&&present| present)
            .count();

            if distinct_types < 2 {
                failures.push(
                    ShapeFailure::new(
                        FailureClass::MissingRegistration,
                        format!(
                            "Multi extension should register 2+ distinct types, found {distinct_types}"
                        ),
                    )
                    .with_hint("Register combinations like tool+event_hook or command+flag"),
                );
            }
        }
        ExtensionShape::General => {
            // General extensions have no required registrations.
            // But they should at least load without error (checked elsewhere).
        }
    }

    failures
}

// ────────────────────────────────────────────────────────────────────────────
// Invocation descriptors
// ────────────────────────────────────────────────────────────────────────────

/// What to invoke after loading an extension.
///
/// Each variant carries the minimum data needed to exercise that shape's
/// primary dispatch path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShapeInvocation {
    /// Call a registered tool by name.
    ToolCall { tool_name: String, arguments: Value },
    /// Execute a slash command.
    CommandExec {
        command_name: String,
        #[serde(default)]
        args: String,
    },
    /// Dispatch an event to registered hooks.
    EventDispatch {
        event_name: String,
        #[serde(default)]
        payload: Value,
    },
    /// Verify provider registration (no runtime call needed).
    ProviderCheck,
    /// Verify UI component registration (no runtime call needed).
    UiComponentCheck,
    /// Verify flag/shortcut registration (no runtime call needed).
    ConfigurationCheck,
    /// No invocation (general extensions just need to load).
    NoOp,
}

impl ShapeInvocation {
    /// Build a default invocation for a shape + registration snapshot.
    ///
    /// Uses the first registered tool/command/event to construct a minimal
    /// invocation.  Returns `NoOp` if the shape doesn't support invocation
    /// or no matching registration was found.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn default_for_shape(shape: ExtensionShape, snapshot: &RegistrationSnapshot) -> Self {
        match shape {
            ExtensionShape::Tool => snapshot.tools.first().map_or(Self::NoOp, |tool| {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                Self::ToolCall {
                    tool_name: name,
                    arguments: Value::Object(serde_json::Map::new()),
                }
            }),
            ExtensionShape::Command => {
                snapshot
                    .slash_commands
                    .first()
                    .map_or(Self::NoOp, |cmd| {
                        let name = cmd
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        Self::CommandExec {
                            command_name: name,
                            args: String::new(),
                        }
                    })
            }
            ExtensionShape::EventHook => {
                snapshot.event_hooks.first().map_or(Self::NoOp, |event| {
                    Self::EventDispatch {
                        event_name: event.clone(),
                        payload: Value::Object(serde_json::Map::new()),
                    }
                })
            }
            ExtensionShape::Provider => Self::ProviderCheck,
            ExtensionShape::UiComponent => Self::UiComponentCheck,
            ExtensionShape::Configuration => Self::ConfigurationCheck,
            ExtensionShape::Multi => Self::multi_invocation(snapshot),
            ExtensionShape::General => Self::NoOp,
        }
    }

    /// Build invocation for Multi-type extensions by trying tool → command → event.
    fn multi_invocation(snapshot: &RegistrationSnapshot) -> Self {
        fn name_from_value(v: &Value) -> String {
            v.get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string()
        }

        snapshot.tools.first().map_or_else(
            || {
                snapshot.slash_commands.first().map_or_else(
                    || {
                        snapshot
                            .event_hooks
                            .first()
                            .map_or(Self::NoOp, |event| Self::EventDispatch {
                                event_name: event.clone(),
                                payload: Value::Object(serde_json::Map::new()),
                            })
                    },
                    |cmd| Self::CommandExec {
                        command_name: name_from_value(cmd),
                        args: String::new(),
                    },
                )
            },
            |tool| Self::ToolCall {
                tool_name: name_from_value(tool),
                arguments: Value::Object(serde_json::Map::new()),
            },
        )
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Shape test result
// ────────────────────────────────────────────────────────────────────────────

/// Aggregate result for a single extension run through the shape harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShapeTestResult {
    pub extension_id: String,
    pub extension_path: PathBuf,
    pub shape: ExtensionShape,
    pub detected_shape: ExtensionShape,
    pub passed: bool,
    pub events: Vec<ShapeEvent>,
    pub failures: Vec<ShapeFailure>,
    pub total_duration_ms: u64,
}

impl ShapeTestResult {
    /// Render a compact summary line (for test output).
    #[must_use]
    pub fn summary_line(&self) -> String {
        let status = if self.passed { "PASS" } else { "FAIL" };
        let shape_match = if self.shape == self.detected_shape {
            String::new()
        } else {
            format!(" (detected: {})", self.detected_shape)
        };
        format!(
            "[{status}] {id} ({shape}{shape_match}) — {dur}ms, {n_fail} failures",
            id = self.extension_id,
            shape = self.shape,
            dur = self.total_duration_ms,
            n_fail = self.failures.len(),
        )
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Shape harness configuration
// ────────────────────────────────────────────────────────────────────────────

/// Configuration for running the shape harness.
#[derive(Debug, Clone)]
pub struct ShapeHarnessConfig {
    /// Maximum time for extension load (ms).
    pub load_timeout_ms: u64,
    /// Maximum time for invocation (ms).
    pub invoke_timeout_ms: u64,
    /// Maximum time for shutdown (ms).
    pub shutdown_timeout_ms: u64,
    /// Deterministic time base (ms since epoch).
    pub deterministic_time_ms: u64,
    /// Deterministic CWD.
    pub deterministic_cwd: PathBuf,
    /// Deterministic HOME.
    pub deterministic_home: PathBuf,
    /// Custom invocation (overrides auto-detected default).
    pub custom_invocation: Option<ShapeInvocation>,
}

impl Default for ShapeHarnessConfig {
    fn default() -> Self {
        Self {
            load_timeout_ms: 20_000,
            invoke_timeout_ms: 20_000,
            shutdown_timeout_ms: 5_000,
            deterministic_time_ms: 1_700_000_000_000,
            deterministic_cwd: PathBuf::from("/tmp/ext-conformance-shapes"),
            deterministic_home: PathBuf::from("/tmp/ext-conformance-shapes-home"),
            custom_invocation: None,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Shape-to-fixture mapping
// ────────────────────────────────────────────────────────────────────────────

/// Known base fixtures per shape (under `tests/ext_conformance/artifacts/base_fixtures/`).
#[must_use]
pub const fn base_fixture_name(shape: ExtensionShape) -> Option<&'static str> {
    match shape {
        ExtensionShape::Tool => Some("minimal_tool"),
        ExtensionShape::Command => Some("minimal_command"),
        ExtensionShape::Provider => Some("minimal_provider"),
        ExtensionShape::EventHook => Some("minimal_event"),
        ExtensionShape::UiComponent => Some("minimal_ui_component"),
        ExtensionShape::Configuration => Some("minimal_configuration"),
        ExtensionShape::Multi => Some("minimal_multi"),
        ExtensionShape::General => Some("minimal_resources"),
    }
}

/// Build the path to a base fixture's entry point.
#[must_use]
pub fn base_fixture_path(repo_root: &Path, shape: ExtensionShape) -> Option<PathBuf> {
    base_fixture_name(shape).map(|name| {
        repo_root
            .join("tests/ext_conformance/artifacts/base_fixtures")
            .join(name)
            .join("index.ts")
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Batch runner types
// ────────────────────────────────────────────────────────────────────────────

/// Input specification for running the shape harness on one extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShapeHarnessInput {
    pub extension_id: String,
    pub extension_path: PathBuf,
    pub shape: ExtensionShape,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_invocation: Option<ShapeInvocation>,
}

/// Summary of a batch run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShapeBatchSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub pass_rate: f64,
    pub by_shape: BTreeMap<String, ShapeShapeSummary>,
    pub by_failure_class: BTreeMap<String, usize>,
}

/// Per-shape aggregate in batch summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShapeShapeSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
}

impl ShapeBatchSummary {
    /// Build from a list of test results.
    #[must_use]
    pub fn from_results(results: &[ShapeTestResult]) -> Self {
        let total = results.len();
        let passed = results.iter().filter(|r| r.passed).count();
        let failed = total - passed;

        let mut by_shape: BTreeMap<String, ShapeShapeSummary> = BTreeMap::new();
        let mut by_failure_class: BTreeMap<String, usize> = BTreeMap::new();

        for result in results {
            let shape_key = result.shape.to_string();
            let entry = by_shape.entry(shape_key).or_default();
            entry.total += 1;
            if result.passed {
                entry.passed += 1;
            } else {
                entry.failed += 1;
            }
            for failure in &result.failures {
                *by_failure_class
                    .entry(failure.class.to_string())
                    .or_insert(0) += 1;
            }
        }

        let pass_rate = if total == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                passed as f64 / total as f64
            }
        };

        Self {
            total,
            passed,
            failed,
            skipped: 0,
            pass_rate,
            by_shape,
            by_failure_class,
        }
    }

    /// Render a compact Markdown summary.
    #[must_use]
    pub fn render_markdown(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        out.push_str("# Shape Conformance Summary\n\n");
        let _ = write!(
            out,
            "Pass Rate: {:.1}% ({}/{})\n\n",
            self.pass_rate * 100.0,
            self.passed,
            self.total
        );

        out.push_str("## By Shape\n\n");
        out.push_str("| Shape | Total | Pass | Fail |\n");
        out.push_str("|---|---:|---:|---:|\n");
        for (shape, summary) in &self.by_shape {
            let _ = writeln!(
                out,
                "| {shape} | {} | {} | {} |",
                summary.total, summary.passed, summary.failed
            );
        }

        if !self.by_failure_class.is_empty() {
            out.push_str("\n## By Failure Class\n\n");
            out.push_str("| Class | Count |\n");
            out.push_str("|---|---:|\n");
            for (class, count) in &self.by_failure_class {
                let _ = writeln!(out, "| {class} | {count} |");
            }
        }

        out
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Unit tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_from_category_roundtrip() {
        let categories = [
            (ExtensionCategory::Tool, ExtensionShape::Tool),
            (ExtensionCategory::Command, ExtensionShape::Command),
            (ExtensionCategory::Provider, ExtensionShape::Provider),
            (ExtensionCategory::EventHook, ExtensionShape::EventHook),
            (ExtensionCategory::UiComponent, ExtensionShape::UiComponent),
            (
                ExtensionCategory::Configuration,
                ExtensionShape::Configuration,
            ),
            (ExtensionCategory::Multi, ExtensionShape::Multi),
            (ExtensionCategory::General, ExtensionShape::General),
        ];
        for (cat, expected_shape) in categories {
            assert_eq!(ExtensionShape::from_category(&cat), expected_shape);
        }
    }

    #[test]
    fn shape_all_is_complete() {
        assert_eq!(ExtensionShape::all().len(), 8);
    }

    #[test]
    fn verify_tool_missing_registration() {
        let snapshot = RegistrationSnapshot::default();
        let failures = verify_registrations(ExtensionShape::Tool, &snapshot);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, FailureClass::MissingRegistration);
        assert!(failures[0].message.contains("registerTool"));
    }

    #[test]
    fn verify_tool_with_registration_passes() {
        let snapshot = RegistrationSnapshot {
            tools: vec![serde_json::json!({"name": "greet", "description": "Greets"})],
            ..Default::default()
        };
        let failures = verify_registrations(ExtensionShape::Tool, &snapshot);
        assert!(failures.is_empty());
    }

    #[test]
    fn verify_tool_malformed_name() {
        let snapshot = RegistrationSnapshot {
            tools: vec![serde_json::json!({"description": "no name"})],
            ..Default::default()
        };
        let failures = verify_registrations(ExtensionShape::Tool, &snapshot);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, FailureClass::MalformedRegistration);
    }

    #[test]
    fn verify_command_missing() {
        let snapshot = RegistrationSnapshot::default();
        let failures = verify_registrations(ExtensionShape::Command, &snapshot);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, FailureClass::MissingRegistration);
    }

    #[test]
    fn verify_command_present() {
        let snapshot = RegistrationSnapshot {
            slash_commands: vec![serde_json::json!({"name": "ping"})],
            ..Default::default()
        };
        let failures = verify_registrations(ExtensionShape::Command, &snapshot);
        assert!(failures.is_empty());
    }

    #[test]
    fn verify_provider_missing() {
        let snapshot = RegistrationSnapshot::default();
        let failures = verify_registrations(ExtensionShape::Provider, &snapshot);
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn verify_provider_present() {
        let snapshot = RegistrationSnapshot {
            providers: vec![serde_json::json!({"name": "mock"})],
            ..Default::default()
        };
        let failures = verify_registrations(ExtensionShape::Provider, &snapshot);
        assert!(failures.is_empty());
    }

    #[test]
    fn verify_event_hook_missing() {
        let snapshot = RegistrationSnapshot::default();
        let failures = verify_registrations(ExtensionShape::EventHook, &snapshot);
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn verify_event_hook_present() {
        let snapshot = RegistrationSnapshot {
            event_hooks: vec!["agent_start".to_string()],
            ..Default::default()
        };
        let failures = verify_registrations(ExtensionShape::EventHook, &snapshot);
        assert!(failures.is_empty());
    }

    #[test]
    fn verify_ui_component_missing() {
        let snapshot = RegistrationSnapshot::default();
        let failures = verify_registrations(ExtensionShape::UiComponent, &snapshot);
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn verify_configuration_missing() {
        let snapshot = RegistrationSnapshot::default();
        let failures = verify_registrations(ExtensionShape::Configuration, &snapshot);
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn verify_configuration_with_flag() {
        let snapshot = RegistrationSnapshot {
            flags: vec![serde_json::json!({"name": "verbose"})],
            ..Default::default()
        };
        let failures = verify_registrations(ExtensionShape::Configuration, &snapshot);
        assert!(failures.is_empty());
    }

    #[test]
    fn verify_configuration_with_shortcut() {
        let snapshot = RegistrationSnapshot {
            shortcuts: vec![serde_json::json!({"key_id": "ctrl+t"})],
            ..Default::default()
        };
        let failures = verify_registrations(ExtensionShape::Configuration, &snapshot);
        assert!(failures.is_empty());
    }

    #[test]
    fn verify_multi_insufficient_types() {
        let snapshot = RegistrationSnapshot {
            tools: vec![serde_json::json!({"name": "t"})],
            ..Default::default()
        };
        let failures = verify_registrations(ExtensionShape::Multi, &snapshot);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("2+ distinct types"));
    }

    #[test]
    fn verify_multi_sufficient_types() {
        let snapshot = RegistrationSnapshot {
            tools: vec![serde_json::json!({"name": "t"})],
            event_hooks: vec!["agent_start".to_string()],
            ..Default::default()
        };
        let failures = verify_registrations(ExtensionShape::Multi, &snapshot);
        assert!(failures.is_empty());
    }

    #[test]
    fn verify_general_always_passes() {
        let snapshot = RegistrationSnapshot::default();
        let failures = verify_registrations(ExtensionShape::General, &snapshot);
        assert!(failures.is_empty());
    }

    #[test]
    fn detected_shape_classification() {
        let empty = RegistrationSnapshot::default();
        assert_eq!(empty.detected_shape(), ExtensionShape::General);

        let tool = RegistrationSnapshot {
            tools: vec![serde_json::json!({"name": "t"})],
            ..Default::default()
        };
        assert_eq!(tool.detected_shape(), ExtensionShape::Tool);

        let multi = RegistrationSnapshot {
            tools: vec![serde_json::json!({"name": "t"})],
            slash_commands: vec![serde_json::json!({"name": "c"})],
            ..Default::default()
        };
        assert_eq!(multi.detected_shape(), ExtensionShape::Multi);
    }

    #[test]
    fn default_invocation_tool() {
        let snapshot = RegistrationSnapshot {
            tools: vec![serde_json::json!({"name": "greet"})],
            ..Default::default()
        };
        let inv = ShapeInvocation::default_for_shape(ExtensionShape::Tool, &snapshot);
        matches!(inv, ShapeInvocation::ToolCall { tool_name, .. } if tool_name == "greet");
    }

    #[test]
    fn default_invocation_command() {
        let snapshot = RegistrationSnapshot {
            slash_commands: vec![serde_json::json!({"name": "ping"})],
            ..Default::default()
        };
        let inv = ShapeInvocation::default_for_shape(ExtensionShape::Command, &snapshot);
        matches!(inv, ShapeInvocation::CommandExec { command_name, .. } if command_name == "ping");
    }

    #[test]
    fn default_invocation_event() {
        let snapshot = RegistrationSnapshot {
            event_hooks: vec!["agent_start".to_string()],
            ..Default::default()
        };
        let inv = ShapeInvocation::default_for_shape(ExtensionShape::EventHook, &snapshot);
        matches!(inv, ShapeInvocation::EventDispatch { event_name, .. } if event_name == "agent_start");
    }

    #[test]
    fn default_invocation_provider() {
        let snapshot = RegistrationSnapshot {
            providers: vec![serde_json::json!({"name": "mock"})],
            ..Default::default()
        };
        let inv = ShapeInvocation::default_for_shape(ExtensionShape::Provider, &snapshot);
        matches!(inv, ShapeInvocation::ProviderCheck);
    }

    #[test]
    fn default_invocation_general() {
        let snapshot = RegistrationSnapshot::default();
        let inv = ShapeInvocation::default_for_shape(ExtensionShape::General, &snapshot);
        matches!(inv, ShapeInvocation::NoOp);
    }

    #[test]
    fn shape_failure_display() {
        let f = ShapeFailure::new(FailureClass::LoadError, "file not found")
            .with_path("extensions/foo.ts")
            .with_hint("Check the file path");
        let s = f.to_string();
        assert!(s.contains("load_error"));
        assert!(s.contains("file not found"));
        assert!(s.contains("extensions/foo.ts"));
        assert!(s.contains("Check the file path"));
    }

    #[test]
    fn shape_event_jsonl_serialization() {
        let mut event = ShapeEvent::new(
            "corr-1",
            "hello",
            ExtensionShape::Tool,
            LifecyclePhase::Load,
        );
        event.duration_ms = 42;
        let jsonl = event.to_jsonl();
        let parsed: Value = serde_json::from_str(&jsonl).expect("valid JSON");
        assert_eq!(parsed["extension_id"], "hello");
        assert_eq!(parsed["shape"], "tool");
        assert_eq!(parsed["phase"], "load");
        assert_eq!(parsed["duration_ms"], 42);
    }

    #[test]
    fn batch_summary_from_results() {
        let results = vec![
            ShapeTestResult {
                extension_id: "a".to_string(),
                extension_path: PathBuf::from("/a"),
                shape: ExtensionShape::Tool,
                detected_shape: ExtensionShape::Tool,
                passed: true,
                events: vec![],
                failures: vec![],
                total_duration_ms: 10,
            },
            ShapeTestResult {
                extension_id: "b".to_string(),
                extension_path: PathBuf::from("/b"),
                shape: ExtensionShape::Command,
                detected_shape: ExtensionShape::Command,
                passed: false,
                events: vec![],
                failures: vec![ShapeFailure::new(
                    FailureClass::MissingRegistration,
                    "no commands",
                )],
                total_duration_ms: 20,
            },
        ];
        let summary = ShapeBatchSummary::from_results(&results);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 1);
        assert!((summary.pass_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(summary.by_shape["tool"].passed, 1);
        assert_eq!(summary.by_shape["command"].failed, 1);
        assert_eq!(summary.by_failure_class["missing_registration"], 1);
    }

    #[test]
    fn batch_summary_markdown() {
        let results = vec![ShapeTestResult {
            extension_id: "a".to_string(),
            extension_path: PathBuf::from("/a"),
            shape: ExtensionShape::Tool,
            detected_shape: ExtensionShape::Tool,
            passed: true,
            events: vec![],
            failures: vec![],
            total_duration_ms: 10,
        }];
        let summary = ShapeBatchSummary::from_results(&results);
        let md = summary.render_markdown();
        assert!(md.contains("100.0%"));
        assert!(md.contains("| tool |"));
    }

    #[test]
    fn registration_snapshot_field_count() {
        let snapshot = RegistrationSnapshot {
            tools: vec![
                serde_json::json!({"name": "a"}),
                serde_json::json!({"name": "b"}),
            ],
            flags: vec![serde_json::json!({"name": "f"})],
            ..Default::default()
        };
        assert_eq!(snapshot.field_count("tools"), 2);
        assert_eq!(snapshot.field_count("flags"), 1);
        assert_eq!(snapshot.field_count("slash_commands"), 0);
        assert_eq!(snapshot.field_count("unknown"), 0);
    }

    #[test]
    fn registration_snapshot_total() {
        let snapshot = RegistrationSnapshot {
            tools: vec![serde_json::json!({"name": "t"})],
            event_hooks: vec!["e".to_string()],
            ..Default::default()
        };
        assert_eq!(snapshot.total_registrations(), 2);
    }

    #[test]
    fn base_fixture_paths() {
        let root = Path::new("/repo");
        assert!(base_fixture_path(root, ExtensionShape::Tool).is_some());
        assert!(base_fixture_path(root, ExtensionShape::Command).is_some());
        assert!(base_fixture_path(root, ExtensionShape::Provider).is_some());
        assert!(base_fixture_path(root, ExtensionShape::EventHook).is_some());
        assert!(base_fixture_path(root, ExtensionShape::General).is_some());
        assert!(base_fixture_path(root, ExtensionShape::UiComponent).is_some());
        assert!(base_fixture_path(root, ExtensionShape::Configuration).is_some());
        assert!(base_fixture_path(root, ExtensionShape::Multi).is_some());
    }

    #[test]
    fn shape_display() {
        assert_eq!(ExtensionShape::Tool.to_string(), "tool");
        assert_eq!(ExtensionShape::EventHook.to_string(), "event_hook");
        assert_eq!(ExtensionShape::UiComponent.to_string(), "ui_component");
    }

    #[test]
    fn failure_class_display() {
        assert_eq!(FailureClass::LoadError.to_string(), "load_error");
        assert_eq!(
            FailureClass::MissingRegistration.to_string(),
            "missing_registration"
        );
        assert_eq!(FailureClass::RuntimeShimGap.to_string(), "runtime_shim_gap");
    }

    #[test]
    fn shape_serde_roundtrip() {
        for shape in ExtensionShape::all() {
            let json = serde_json::to_string(shape).unwrap();
            let back: ExtensionShape = serde_json::from_str(&json).unwrap();
            assert_eq!(*shape, back);
        }
    }

    #[test]
    fn failure_class_serde_roundtrip() {
        let classes = [
            FailureClass::LoadError,
            FailureClass::MissingRegistration,
            FailureClass::MalformedRegistration,
            FailureClass::InvocationError,
            FailureClass::OutputMismatch,
            FailureClass::Timeout,
            FailureClass::IncompatibleShape,
            FailureClass::ShutdownError,
            FailureClass::RuntimeShimGap,
        ];
        for class in classes {
            let json = serde_json::to_string(&class).unwrap();
            let back: FailureClass = serde_json::from_str(&json).unwrap();
            assert_eq!(class, back);
        }
    }

    #[test]
    fn shape_result_summary_line() {
        let result = ShapeTestResult {
            extension_id: "hello".to_string(),
            extension_path: PathBuf::from("/ext/hello"),
            shape: ExtensionShape::Tool,
            detected_shape: ExtensionShape::Tool,
            passed: true,
            events: vec![],
            failures: vec![],
            total_duration_ms: 42,
        };
        let line = result.summary_line();
        assert!(line.contains("[PASS]"));
        assert!(line.contains("hello"));
        assert!(line.contains("tool"));
        assert!(line.contains("42ms"));
    }

    #[test]
    fn shape_result_summary_line_mismatch() {
        let result = ShapeTestResult {
            extension_id: "x".to_string(),
            extension_path: PathBuf::from("/x"),
            shape: ExtensionShape::Tool,
            detected_shape: ExtensionShape::Multi,
            passed: false,
            events: vec![],
            failures: vec![ShapeFailure::new(FailureClass::OutputMismatch, "wrong")],
            total_duration_ms: 100,
        };
        let line = result.summary_line();
        assert!(line.contains("[FAIL]"));
        assert!(line.contains("(detected: multi)"));
        assert!(line.contains("1 failures"));
    }

    #[test]
    fn typical_capabilities_nonempty() {
        for shape in ExtensionShape::all() {
            // All shapes have at least one typical capability
            let caps = shape.typical_capabilities();
            assert!(
                !caps.is_empty(),
                "Shape {shape} should have typical capabilities",
            );
        }
    }

    #[test]
    fn supports_invocation_matches_expected() {
        assert!(ExtensionShape::Tool.supports_invocation());
        assert!(ExtensionShape::Command.supports_invocation());
        assert!(ExtensionShape::EventHook.supports_invocation());
        assert!(ExtensionShape::Multi.supports_invocation());
        assert!(!ExtensionShape::Provider.supports_invocation());
        assert!(!ExtensionShape::General.supports_invocation());
    }
}
