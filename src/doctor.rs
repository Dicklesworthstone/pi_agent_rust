//! Comprehensive environment health checker for `pi doctor`.
//!
//! When invoked without a path, checks config, directories, auth, shell tools,
//! and sessions. When invoked with a path, runs extension preflight analysis.
//! With `--fix`, automatically repairs safe issues (missing dirs, permissions).

use crate::auth::{AuthStorage, CredentialStatus};
use crate::config::Config;
use crate::error::Result;
use crate::provider_metadata::provider_auth_env_keys;
use crate::resource_governor::{
    AdmissionAction, HostResourceBudgets, HostResourceSample, ResourceOperationKind,
    ResourceRequest, SwarmAdmissionController, SwarmCapacityDimension,
    SwarmCapacityEvidenceSummary, SwarmCapacityPlan, SwarmCapacityPlanError,
    SwarmCapacityPlannerConfig, SwarmHostInventory, SwarmLiveLoad, TailLatencyRegimeSample,
};
use crate::semantic_workspace_graph::{
    ContextArtifactCacheScope, ContextBundleBudget, ContextBundleRequest, EvidenceFreshnessStatus,
    RedactionStatus, SemanticContextBundle, SemanticContextBundlePlanner, SemanticEdgeType,
    SemanticNodeType, SemanticWorkspaceGraph, SemanticWorkspaceGraphBuilder,
};
use crate::session::SessionHeader;
use crate::session_index::walk_sessions;
use crate::swarm_progress_slo::{ProgressSloReport, ProgressSloStatus, SWARM_PROGRESS_SLO_SCHEMA};
use crate::validation_broker::{ValidationSlotLease, ValidationSlotState, ValidationSlotStore};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SWARM_STALE_IN_PROGRESS_HOURS: i64 = 24;
const SWARM_DETAIL_LIMIT: usize = 5;
const SWARM_DISK_WARN_AVAILABLE_KB: u64 = 10 * 1024 * 1024;
const SWARM_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const SWARM_DOCTOR_ADMISSION_SCHEMA: &str = "pi.doctor.swarm_admission.v1";
const SWARM_DOCTOR_RCH_FAILURE_SCHEMA: &str = "pi.doctor.rch_failure.v1";
const SWARM_DOCTOR_TEMP_DIR_SCHEMA: &str = "pi.doctor.swarm_temp_dir.v1";
const SWARM_DOCTOR_RESOURCE_PREFLIGHT_SCHEMA: &str = "pi.doctor.swarm_resource_preflight.v1";
const SWARM_DOCTOR_BUILD_SLOT_SCHEMA: &str = "pi.doctor.agent_mail_build_slots.v1";
const SWARM_DOCTOR_CONTACTS_SCHEMA: &str = "pi.doctor.agent_mail_contacts.v1";
const SWARM_DOCTOR_AGENT_MAIL_DEGRADED_SCHEMA: &str = "pi.doctor.agent_mail_degraded_mode.v1";
const SWARM_DOCTOR_STALLED_REAPER_SCHEMA: &str = "pi.doctor.stalled_bead_reaper.v1";
const SWARM_DOCTOR_NEXT_ACTION_SCHEMA: &str = "pi.doctor.communication_purgatory_next_action.v1";
const SWARM_DOCTOR_OPERATIONS_DASHBOARD_SCHEMA: &str = "pi.doctor.swarm_operations_dashboard.v1";
const SWARM_DOCTOR_CONTEXT_INTELLIGENCE_SCHEMA: &str = "pi.doctor.context_intelligence_posture.v1";
const SWARM_DOCTOR_VALIDATION_BROKER_SCHEMA: &str = "pi.doctor.validation_broker_posture.v1";
const SWARM_DOCTOR_PROGRESS_SLO_SCHEMA: &str = "pi.doctor.swarm_progress_slo_posture.v1";
const SWARM_DOCTOR_RCH_AFFINITY_SCHEMA: &str = "pi.doctor.rch_warm_target_affinity.v1";
const SWARM_DOCTOR_INCIDENT_DIAGNOSTICS_SCHEMA: &str = "pi.doctor.swarm_incident_diagnostics.v1";
const SWARM_DOCTOR_LANE_PLACEMENT_SCHEMA: &str = "pi.doctor.swarm_lane_placement.v1";
const SWARM_RCH_AFFINITY_PLAN_ARTIFACT_SCHEMA: &str = "pi.swarm.rch_affinity_plan.v1";
const SWARM_DOCTOR_RESERVATION_HEATMAP_SCHEMA: &str =
    "pi.doctor.agent_mail_reservation_conflict_heatmap.v1";
const SWARM_DOCTOR_CONFLICT_PREDICTOR_SCHEMA: &str = "pi.doctor.cross_agent_conflict_predictor.v1";
const SWARM_DOCTOR_RESERVATION_RECOMMENDATIONS_SCHEMA: &str =
    "pi.doctor.swarm_reservation_recommendations.v1";
const SWARM_CARGO_SCRATCH_ROOT: &str = "/data/tmp/pi_agent_rust_cargo";
const SWARM_RCH_AFFINITY_JOBS_ENV: &str = "PI_DOCTOR_RCH_AFFINITY_JOBS_JSON";
const SWARM_RESOURCE_PREFLIGHT_RCH_QUEUE_JSON_ENV: &str = "PI_DOCTOR_RCH_QUEUE_JSON";
const SWARM_RESOURCE_PREFLIGHT_RCH_QUEUE_JSON_PATH_ENV: &str = "PI_DOCTOR_RCH_QUEUE_JSON_PATH";
const SWARM_RESOURCE_PREFLIGHT_LOCAL_BUILD_PROCESS_COUNT_ENV: &str =
    "PI_DOCTOR_LOCAL_BUILD_PROCESS_COUNT";
const SWARM_RESOURCE_PREFLIGHT_LOGICAL_CPU_CORES_ENV: &str = "PI_DOCTOR_LOGICAL_CPU_CORES";
const SWARM_VALIDATION_BROKER_STORE_ENV: &str = "PI_VALIDATION_BROKER_STORE";
const SWARM_PROGRESS_SLO_JSON_ENV: &str = "PI_SWARM_PROGRESS_SLO_JSON";
const SWARM_BUILD_SLOT_SOON_EXPIRING_MINUTES: i64 = 30;
const SWARM_ACTIVE_AGENT_WINDOW_HOURS: i64 = 24;
const SWARM_DASHBOARD_AGENT_LIMIT: usize = 12;
const SWARM_RESERVATION_RECENT_HOURS: i64 = 24;
const SWARM_RESERVATION_STALE_HOLDER_HOURS: i64 = 24;
const SWARM_RESERVATION_EXPIRING_SOON_MINUTES: i64 = 30;
const MIB_BYTES: u64 = 1024 * 1024;
const CGROUP_UNLIMITED_MEMORY_THRESHOLD_BYTES: u64 = 1 << 60;
const MAX_NUMERIC_RANGE_SPAN: u64 = 16_384;
const SWARM_LANE_TIGHT_MEMORY_BYTES: u64 = 4 * 1024 * MIB_BYTES;

// ── Core Types ──────────────────────────────────────────────────────

/// How severe a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Pass,
    Info,
    Warn,
    Fail,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS"),
            Self::Info => write!(f, "INFO"),
            Self::Warn => write!(f, "WARN"),
            Self::Fail => write!(f, "FAIL"),
        }
    }
}

/// Whether a finding can be auto-fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Fixability {
    /// Cannot be auto-fixed.
    NotFixable,
    /// Can be auto-fixed with `--fix`.
    AutoFixable,
    /// Was auto-fixed in this run.
    Fixed,
}

/// Which subsystem a check belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckCategory {
    Config,
    Dirs,
    Auth,
    Shell,
    Sessions,
    Swarm,
    Extensions,
}

impl CheckCategory {
    const fn label(self) -> &'static str {
        match self {
            Self::Config => "Configuration",
            Self::Dirs => "Directories",
            Self::Auth => "Authentication",
            Self::Shell => "Shell & Tools",
            Self::Sessions => "Sessions",
            Self::Swarm => "Swarm Coordination",
            Self::Extensions => "Extensions",
        }
    }
}

impl fmt::Display for CheckCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl std::str::FromStr for CheckCategory {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "config" => Ok(Self::Config),
            "dirs" | "directories" => Ok(Self::Dirs),
            "auth" | "authentication" => Ok(Self::Auth),
            "shell" => Ok(Self::Shell),
            "sessions" => Ok(Self::Sessions),
            "swarm" | "coordination" | "leases" => Ok(Self::Swarm),
            "extensions" | "ext" => Ok(Self::Extensions),
            other => Err(format!("unknown category: {other}")),
        }
    }
}

/// A single diagnostic finding.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub category: CheckCategory,
    pub severity: Severity,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    pub fixability: Fixability,
}

impl Finding {
    fn pass(category: CheckCategory, title: impl Into<String>) -> Self {
        Self {
            category,
            severity: Severity::Pass,
            title: title.into(),
            detail: None,
            remediation: None,
            data: None,
            fixability: Fixability::NotFixable,
        }
    }

    fn info(category: CheckCategory, title: impl Into<String>) -> Self {
        Self {
            category,
            severity: Severity::Info,
            title: title.into(),
            detail: None,
            remediation: None,
            data: None,
            fixability: Fixability::NotFixable,
        }
    }

    fn warn(category: CheckCategory, title: impl Into<String>) -> Self {
        Self {
            category,
            severity: Severity::Warn,
            title: title.into(),
            detail: None,
            remediation: None,
            data: None,
            fixability: Fixability::NotFixable,
        }
    }

    fn fail(category: CheckCategory, title: impl Into<String>) -> Self {
        Self {
            category,
            severity: Severity::Fail,
            title: title.into(),
            detail: None,
            remediation: None,
            data: None,
            fixability: Fixability::NotFixable,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }

    fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    const fn auto_fixable(mut self) -> Self {
        self.fixability = Fixability::AutoFixable;
        self
    }

    const fn fixed(mut self) -> Self {
        self.fixability = Fixability::Fixed;
        self.severity = Severity::Pass;
        self
    }
}

/// Summary counters.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DoctorSummary {
    pub pass: usize,
    pub info: usize,
    pub warn: usize,
    pub fail: usize,
}

/// Full diagnostic report.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub findings: Vec<Finding>,
    pub summary: DoctorSummary,
    pub overall: Severity,
}

impl DoctorReport {
    fn from_findings(findings: Vec<Finding>) -> Self {
        let mut summary = DoctorSummary::default();
        let mut overall = Severity::Pass;
        for f in &findings {
            match f.severity {
                Severity::Pass => summary.pass += 1,
                Severity::Info => summary.info += 1,
                Severity::Warn => {
                    summary.warn += 1;
                    if overall < Severity::Warn {
                        overall = Severity::Warn;
                    }
                }
                Severity::Fail => {
                    summary.fail += 1;
                    overall = Severity::Fail;
                }
            }
        }
        Self {
            findings,
            summary,
            overall,
        }
    }

    /// Render human-friendly text output.
    pub fn render_text(&self) -> String {
        let mut out = String::with_capacity(2048);
        out.push_str("Pi Doctor\n=========\n");

        // Group findings by category, preserving insertion order
        let mut seen_categories: Vec<CheckCategory> = Vec::new();
        for f in &self.findings {
            if !seen_categories.contains(&f.category) {
                seen_categories.push(f.category);
            }
        }

        for cat in &seen_categories {
            let cat_findings: Vec<&Finding> = self
                .findings
                .iter()
                .filter(|f| f.category.eq(cat))
                .collect();
            let cat_worst = cat_findings
                .iter()
                .map(|f| f.severity)
                .max()
                .unwrap_or(Severity::Pass);
            let _ = writeln!(out, "\n[{cat_worst}] {cat}");
            for f in &cat_findings {
                let _ = writeln!(out, "  [{}] {}", f.severity, f.title);
                if let Some(detail) = &f.detail {
                    let _ = writeln!(out, "       {detail}");
                }
                if let Some(rem) = &f.remediation {
                    let _ = writeln!(out, "       Fix: {rem}");
                }
                if matches!(f.fixability, Fixability::AutoFixable) {
                    out.push_str("       (fixable with --fix)\n");
                }
            }
        }

        let _ = writeln!(
            out,
            "\nOverall: {} ({} pass, {} info, {} warn, {} fail)",
            self.overall,
            self.summary.pass,
            self.summary.info,
            self.summary.warn,
            self.summary.fail
        );
        out
    }

    /// Render as JSON.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Render as markdown.
    pub fn render_markdown(&self) -> String {
        let mut out = String::with_capacity(2048);
        out.push_str("# Pi Doctor Report\n\n");

        let mut seen_categories: Vec<CheckCategory> = Vec::new();
        for f in &self.findings {
            if !seen_categories.contains(&f.category) {
                seen_categories.push(f.category);
            }
        }

        for cat in &seen_categories {
            let _ = writeln!(out, "## {cat}\n");
            for f in self.findings.iter().filter(|f| f.category.eq(cat)) {
                let icon = match f.severity {
                    Severity::Pass => "✅",
                    Severity::Info => "ℹ️",
                    Severity::Warn => "⚠️",
                    Severity::Fail => "❌",
                };
                let _ = write!(out, "- {icon} **{}**", f.title);
                if let Some(detail) = &f.detail {
                    let _ = write!(out, " — {detail}");
                }
                out.push('\n');
                if let Some(rem) = &f.remediation {
                    let _ = writeln!(out, "  - Fix: {rem}");
                }
            }
            out.push('\n');
        }

        let _ = writeln!(
            out,
            "**Overall: {}** ({} pass, {} info, {} warn, {} fail)",
            self.overall,
            self.summary.pass,
            self.summary.info,
            self.summary.warn,
            self.summary.fail
        );
        out
    }
}

// ── Options ─────────────────────────────────────────────────────────

/// Options for `run_doctor`.
pub struct DoctorOptions<'a> {
    pub cwd: &'a Path,
    pub extension_path: Option<&'a str>,
    pub policy_override: Option<&'a str>,
    pub fix: bool,
    pub only: Option<HashSet<CheckCategory>>,
}

// ── Entry Point ─────────────────────────────────────────────────────

/// Run all applicable doctor checks and return a report.
#[allow(clippy::too_many_lines)]
pub fn run_doctor(opts: &DoctorOptions<'_>) -> Result<DoctorReport> {
    let mut findings = Vec::new();
    let extension_only_default = opts.extension_path.is_some() && opts.only.is_none();

    let should_run = |cat: CheckCategory| -> bool {
        if extension_only_default {
            return false;
        }
        opts.only.as_ref().is_none_or(|set| set.contains(&cat))
    };

    if let Some(ext_path) = opts.extension_path {
        if opts
            .only
            .as_ref()
            .is_none_or(|set| set.contains(&CheckCategory::Extensions))
        {
            check_extension(opts.cwd, ext_path, opts.policy_override, &mut findings);
        }
    } else if opts
        .only
        .as_ref()
        .is_some_and(|set| set.contains(&CheckCategory::Extensions))
    {
        findings.push(
            Finding::fail(
                CheckCategory::Extensions,
                "Extensions check requires an extension path",
            )
            .with_remediation(
                "Run `pi doctor <path-to-extension>` to evaluate extension compatibility",
            ),
        );
    }

    if should_run(CheckCategory::Config) {
        check_config(opts.cwd, &mut findings);
    }
    if should_run(CheckCategory::Dirs) {
        check_dirs(opts.fix, &mut findings);
    }
    if should_run(CheckCategory::Auth) {
        check_auth(opts.fix, &mut findings);
    }
    if should_run(CheckCategory::Shell) {
        check_shell(&mut findings);
    }
    if should_run(CheckCategory::Sessions) {
        check_sessions(&mut findings);
    }
    if should_run(CheckCategory::Swarm) {
        check_swarm(opts.cwd, &mut findings);
    }

    Ok(DoctorReport::from_findings(findings))
}

// ── Check: Config ───────────────────────────────────────────────────

fn check_config(cwd: &Path, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Config;

    // Global settings
    let global_path = Config::global_dir().join("settings.json");
    check_settings_file(cat, &global_path, "Global settings", findings);

    // Project settings
    let project_path = cwd.join(Config::project_dir()).join("settings.json");
    if project_path.exists() {
        check_settings_file(
            cat,
            &project_path,
            "Project settings (.pi/settings.json)",
            findings,
        );
    } else {
        findings.push(Finding::pass(cat, "No project settings (OK)"));
    }
}

fn check_settings_file(cat: CheckCategory, path: &Path, label: &str, findings: &mut Vec<Finding>) {
    if !path.exists() {
        findings.push(Finding::pass(cat, format!("{label}: not present (OK)")));
        return;
    }
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let value: serde_json::Value = match serde_json::from_str(&content) {
                Ok(value) => value,
                Err(e) => {
                    findings.push(
                        Finding::fail(cat, format!("{label}: JSON parse error"))
                            .with_detail(e.to_string())
                            .with_remediation(format!("Fix the JSON syntax in {}", path.display())),
                    );
                    return;
                }
            };

            let serde_json::Value::Object(map) = value else {
                findings.push(
                    Finding::fail(
                        cat,
                        format!("{label}: top-level value must be a JSON object"),
                    )
                    .with_detail(format!("Found non-object JSON in {}", path.display()))
                    .with_remediation(format!("Wrap settings in {{ ... }} in {}", path.display())),
                );
                return;
            };

            let unknown: Vec<&String> = map.keys().filter(|k| !is_known_config_key(k)).collect();
            if unknown.is_empty() {
                findings.push(Finding::pass(cat, label.to_string()));
            } else {
                findings.push(
                    Finding::warn(cat, format!("{label}: unknown keys"))
                        .with_detail(format!(
                            "Unknown keys: {}",
                            unknown
                                .iter()
                                .map(|k| k.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                        .with_remediation("Check for typos in settings key names"),
                );
            }
        }
        Err(e) => {
            findings.push(
                Finding::fail(cat, format!("{label}: read error"))
                    .with_detail(e.to_string())
                    .with_remediation(format!("Check file permissions on {}", path.display())),
            );
        }
    }
}

/// Known top-level config keys (from `Config` struct fields + their camelCase aliases).
fn is_known_config_key(key: &str) -> bool {
    matches!(
        key,
        "theme"
            | "hideThinkingBlock"
            | "hide_thinking_block"
            | "showHardwareCursor"
            | "show_hardware_cursor"
            | "defaultProvider"
            | "default_provider"
            | "defaultModel"
            | "default_model"
            | "defaultThinkingLevel"
            | "default_thinking_level"
            | "enabledModels"
            | "enabled_models"
            | "steeringMode"
            | "steering_mode"
            | "followUpMode"
            | "follow_up_mode"
            | "quietStartup"
            | "quiet_startup"
            | "collapseChangelog"
            | "collapse_changelog"
            | "lastChangelogVersion"
            | "last_changelog_version"
            | "doubleEscapeAction"
            | "double_escape_action"
            | "editorPaddingX"
            | "editor_padding_x"
            | "autocompleteMaxVisible"
            | "autocomplete_max_visible"
            | "sessionPickerInput"
            | "session_picker_input"
            | "sessionStore"
            | "sessionBackend"
            | "session_store"
            | "compaction"
            | "branchSummary"
            | "branch_summary"
            | "retry"
            | "shellPath"
            | "shell_path"
            | "shellCommandPrefix"
            | "shell_command_prefix"
            | "ghPath"
            | "gh_path"
            | "images"
            | "terminal"
            | "thinkingBudgets"
            | "thinking_budgets"
            | "packages"
            | "extensions"
            | "skills"
            | "prompts"
            | "themes"
            | "enableSkillCommands"
            | "enable_skill_commands"
            | "extensionPolicy"
            | "extension_policy"
            | "repairPolicy"
            | "repair_policy"
            | "extensionRisk"
            | "extension_risk"
            | "checkForUpdates"
            | "check_for_updates"
            | "sessionDurability"
            | "session_durability"
            | "markdown"
            | "queueMode"
    )
}

// ── Check: Dirs ─────────────────────────────────────────────────────

fn check_dirs(fix: bool, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Dirs;
    let dirs = [
        ("Agent directory", Config::global_dir()),
        ("Sessions directory", Config::sessions_dir()),
        ("Packages directory", Config::package_dir()),
    ];

    for (label, dir) in &dirs {
        check_dir(cat, label, dir, fix, findings);
    }
}

fn check_dir(cat: CheckCategory, label: &str, dir: &Path, fix: bool, findings: &mut Vec<Finding>) {
    if dir.is_dir() {
        // Check write permission
        match tempfile::NamedTempFile::new_in(dir) {
            Ok(mut probe_file) => match probe_file.write_all(b"probe") {
                Ok(()) => {
                    findings.push(Finding::pass(cat, format!("{label} ({})", dir.display())));
                }
                Err(e) => {
                    findings.push(
                        Finding::fail(cat, format!("{label}: not writable"))
                            .with_detail(format!("{}: {e}", dir.display()))
                            .with_remediation(format!("chmod u+w {}", dir.display())),
                    );
                }
            },
            Err(e) => {
                findings.push(
                    Finding::fail(cat, format!("{label}: not writable"))
                        .with_detail(format!("{}: {e}", dir.display()))
                        .with_remediation(format!("chmod u+w {}", dir.display())),
                );
            }
        }
    } else if fix {
        match std::fs::create_dir_all(dir) {
            Ok(()) => {
                findings.push(
                    Finding::pass(cat, format!("{label}: created ({})", dir.display())).fixed(),
                );
            }
            Err(e) => {
                findings.push(
                    Finding::fail(cat, format!("{label}: could not create"))
                        .with_detail(format!("{}: {e}", dir.display()))
                        .with_remediation(format!("mkdir -p {}", dir.display())),
                );
            }
        }
    } else {
        findings.push(
            Finding::warn(cat, format!("{label}: missing"))
                .with_detail(format!("{} does not exist", dir.display()))
                .with_remediation(format!("mkdir -p {}", dir.display()))
                .auto_fixable(),
        );
    }
}

// ── Check: Auth ─────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
#[cfg_attr(not(unix), allow(unused_variables))]
fn check_auth(fix: bool, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Auth;
    let auth_path = Config::auth_path();

    if !auth_path.exists() {
        findings.push(
            Finding::info(cat, "auth.json: not present")
                .with_detail("No credentials stored yet")
                .with_remediation("Run `pi` and follow the login prompt, or set ANTHROPIC_API_KEY"),
        );
        // Still check env vars
        check_auth_env_vars(cat, findings);
        return;
    }

    // Check if auth.json parses
    let auth = match AuthStorage::load(auth_path.clone()) {
        Ok(auth) => {
            findings.push(Finding::pass(cat, "auth.json parses correctly"));
            Some(auth)
        }
        Err(e) => {
            findings.push(
                Finding::fail(cat, "auth.json: parse error")
                    .with_detail(e.to_string())
                    .with_remediation("Check auth.json syntax or delete and re-authenticate"),
            );
            None
        }
    };

    // Check file permissions (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&auth_path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode.eq(&0o600) {
                findings.push(Finding::pass(cat, "auth.json permissions (600)"));
            } else if fix {
                match std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600)) {
                    Ok(()) => {
                        findings.push(
                            Finding::pass(
                                cat,
                                format!("auth.json permissions fixed (was {mode:o}, now 600)"),
                            )
                            .fixed(),
                        );
                    }
                    Err(e) => {
                        findings.push(
                            Finding::fail(cat, "auth.json: could not fix permissions")
                                .with_detail(e.to_string()),
                        );
                    }
                }
            } else {
                findings.push(
                    Finding::warn(
                        cat,
                        format!("auth.json permissions are {mode:o}, should be 600"),
                    )
                    .with_remediation(format!("chmod 600 {}", auth_path.display()))
                    .auto_fixable(),
                );
            }
        }
    }

    // Check stored credentials
    if let Some(auth) = &auth {
        let providers = auth.provider_names();
        if providers.is_empty() {
            findings.push(
                Finding::info(cat, "No stored credentials")
                    .with_remediation("Run `pi` to authenticate or set an API key env var"),
            );
        } else {
            for provider in &providers {
                let status = auth.credential_status(provider);
                match status {
                    CredentialStatus::ApiKey => {
                        findings.push(Finding::pass(
                            cat,
                            format!("{provider}: API key configured"),
                        ));
                    }
                    CredentialStatus::OAuthValid { .. } => {
                        findings.push(Finding::pass(cat, format!("{provider}: OAuth token valid")));
                    }
                    CredentialStatus::OAuthExpired { .. } => {
                        findings.push(
                            Finding::warn(cat, format!("{provider}: OAuth token expired"))
                                .with_remediation(format!("Run `pi /login {provider}` to refresh")),
                        );
                    }
                    CredentialStatus::BearerToken => {
                        findings.push(Finding::pass(
                            cat,
                            format!("{provider}: bearer token configured"),
                        ));
                    }
                    CredentialStatus::AwsCredentials => {
                        findings.push(Finding::pass(
                            cat,
                            format!("{provider}: AWS credentials configured"),
                        ));
                    }
                    CredentialStatus::ServiceKey => {
                        findings.push(Finding::pass(
                            cat,
                            format!("{provider}: service key configured"),
                        ));
                    }
                    CredentialStatus::Missing => {
                        // Shouldn't happen since we're iterating stored providers
                        findings.push(Finding::info(cat, format!("{provider}: no credentials")));
                    }
                }
            }
        }
    }

    check_auth_env_vars(cat, findings);
}

/// Check common auth-related environment variables.
fn check_auth_env_vars(cat: CheckCategory, findings: &mut Vec<Finding>) {
    let key_providers = [
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
        ("google", "GOOGLE_API_KEY"),
    ];

    for (provider, env_key) in &key_providers {
        let env_keys = provider_auth_env_keys(provider);
        let has_env = env_keys.iter().any(|k| std::env::var(k).is_ok());
        if has_env {
            findings.push(Finding::pass(
                cat,
                format!("{provider}: env var set ({env_key})"),
            ));
        } else {
            findings.push(
                Finding::info(cat, format!("{provider}: no env var"))
                    .with_detail(format!("Set {env_key} or run `pi /login {provider}`")),
            );
        }
    }
}

// ── Check: Shell ────────────────────────────────────────────────────

fn check_shell(findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Shell;

    // Required tools (Fail if missing)
    check_tool(
        cat,
        "bash",
        &["--version"],
        Severity::Fail,
        ToolCheckMode::PresenceOnly,
        findings,
    );
    check_tool(
        cat,
        "sh",
        &["--version"],
        Severity::Fail,
        ToolCheckMode::PresenceOnly,
        findings,
    );

    // Important tools (Warn if missing)
    check_tool(
        cat,
        "git",
        &["--version"],
        Severity::Warn,
        ToolCheckMode::PresenceOnly,
        findings,
    );
    check_tool(
        cat,
        "rg",
        &["--version"],
        Severity::Warn,
        ToolCheckMode::PresenceOnly,
        findings,
    );

    let fd_bin = if which_tool("fd").is_some() {
        "fd"
    } else {
        "fdfind"
    };
    check_tool(
        cat,
        fd_bin,
        &["--version"],
        Severity::Warn,
        ToolCheckMode::PresenceOnly,
        findings,
    );

    // Optional tools (Info if missing)
    check_tool(
        cat,
        "gh",
        &["--version"],
        Severity::Info,
        ToolCheckMode::PresenceOnly,
        findings,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCheckMode {
    PresenceOnly,
    ProbeExecution,
}

fn check_tool(
    cat: CheckCategory,
    tool: &str,
    args: &[&str],
    missing_severity: Severity,
    mode: ToolCheckMode,
    findings: &mut Vec<Finding>,
) {
    let discovered_path = which_tool(tool);
    if matches!(mode, ToolCheckMode::PresenceOnly) {
        if let Some(path) = discovered_path {
            findings.push(Finding::pass(cat, format!("{tool} ({path})")));
            return;
        }
        report_missing_tool(cat, tool, missing_severity, findings);
        return;
    }

    let command_target = discovered_path.as_deref().unwrap_or(tool);

    let mut command = Command::new(command_target); // ubs:ignore false positive: private doctor tool probe; production callers pass fixed tool names.
    match command
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => {
            // Extract version from first line of stdout
            let version = String::from_utf8_lossy(&output.stdout);
            let first_line = version.lines().next().unwrap_or("").trim();
            let label = discovered_path.as_ref().map_or_else(
                || {
                    if first_line.is_empty() {
                        tool.to_string()
                    } else {
                        format!("{tool}: {first_line}")
                    }
                },
                |path| format!("{tool} ({path})"),
            );
            findings.push(Finding::pass(cat, label));
        }
        Ok(output)
            if discovered_path.is_some()
                && probe_failure_is_known_nonfatal(tool, args, &output) =>
        {
            // Some shells (e.g. dash as /bin/sh) do not support --version.
            // If this is the known non-fatal probe case, treat tool as present.
            let path = discovered_path.unwrap_or_default();
            findings.push(Finding::pass(cat, format!("{tool} ({path})")));
        }
        Ok(output) => {
            let suffix = if matches!(missing_severity, Severity::Info) {
                " (optional)"
            } else {
                ""
            };
            let detail = {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if stderr.is_empty() {
                    format!("Exit status: {:?}", output.status.code())
                } else {
                    stderr
                }
            };
            findings.push(Finding {
                category: cat,
                severity: missing_severity,
                title: format!("{tool}: invocation failed{suffix}"),
                detail: Some(detail),
                remediation: discovered_path
                    .as_ref()
                    .map(|path| format!("Verify this executable is healthy: {path}")),
                data: None,
                fixability: Fixability::NotFixable,
            });
        }
        Err(err) => {
            if discovered_path.is_some() || !matches!(err.kind(), std::io::ErrorKind::NotFound) {
                let suffix = if matches!(missing_severity, Severity::Info) {
                    " (optional)"
                } else {
                    ""
                };
                findings.push(Finding {
                    category: cat,
                    severity: missing_severity,
                    title: format!("{tool}: invocation failed{suffix}"),
                    detail: Some(err.to_string()),
                    remediation: discovered_path
                        .as_ref()
                        .map(|path| format!("Verify this executable is healthy: {path}")),
                    data: None,
                    fixability: Fixability::NotFixable,
                });
            } else {
                report_missing_tool(cat, tool, missing_severity, findings);
            }
        }
    }
}

fn report_missing_tool(
    cat: CheckCategory,
    tool: &str,
    missing_severity: Severity,
    findings: &mut Vec<Finding>,
) {
    let suffix = if matches!(missing_severity, Severity::Info) {
        " (optional)"
    } else {
        ""
    };
    let mut f = Finding {
        category: cat,
        severity: missing_severity,
        title: format!("{tool}: not found{suffix}"),
        detail: None,
        remediation: None,
        data: None,
        fixability: Fixability::NotFixable,
    };
    if tool.eq("gh") {
        f.remediation = Some("Install: https://cli.github.com/".to_string());
    }
    findings.push(f);
}

fn probe_failure_is_known_nonfatal(
    tool: &str,
    args: &[&str],
    output: &std::process::Output,
) -> bool {
    if tool.ne("sh") || args.ne(&["--version"]) {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("illegal option")
        || stderr.contains("unknown option")
        || stderr.contains("invalid option")
}

fn which_tool(tool: &str) -> Option<String> {
    let tool_path = Path::new(tool);
    if tool_path.components().count() > 1 {
        return is_executable(tool_path).then(|| tool_path.display().to_string());
    }

    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if let Some(path) = resolve_executable_in_dir(&dir, tool) {
            return Some(path.display().to_string());
        }
    }
    None
}

fn resolve_executable_in_dir(dir: &Path, tool: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let candidate = dir.join(tool);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        let pathext = std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        for ext in std::env::split_paths(&pathext) {
            let ext = ext.to_string_lossy();
            let suffix = ext.trim_matches('.');
            if suffix.is_empty() {
                continue;
            }
            let candidate = dir.join(format!("{tool}.{suffix}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    #[cfg(not(windows))]
    {
        let candidate = dir.join(tool);
        is_executable(&candidate).then_some(candidate)
    }
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

// ── Check: Swarm Coordination ───────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn check_swarm(cwd: &Path, findings: &mut Vec<Finding>) {
    check_swarm_beads(cwd, findings);
    check_swarm_resource_preflight(findings);
    check_swarm_live_admission(cwd, findings);
    check_swarm_br_status(cwd, findings);
    check_swarm_agent_mail(cwd, findings);
    check_swarm_stalled_bead_reaper(cwd, findings);
    check_swarm_conflict_predictor(cwd, findings);
    check_swarm_next_action(cwd, findings);
    check_swarm_git(cwd, findings);
    check_swarm_rch(findings);
    check_swarm_rch_affinity(cwd, findings);
    check_swarm_temp_dirs(findings);
    check_swarm_context_intelligence(cwd, findings);
    check_swarm_validation_broker(cwd, findings);
    check_swarm_progress_slo(cwd, findings);
    check_swarm_operations_dashboard(cwd, findings);
    check_swarm_incident_diagnostics(findings);
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BeadsLedgerSummary {
    total: usize,
    open: usize,
    in_progress: usize,
    active: usize,
    parse_errors: usize,
    stale_in_progress: Vec<StaleIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleIssue {
    id: String,
    title: String,
    updated_at: String,
    age_hours: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct BeadsIssueRecord {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default, alias = "type")]
    issue_type: Option<String>,
    status: String,
    updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentMailActivity {
    last_active_ts: String,
    age_hours: i64,
}

fn check_swarm_beads(cwd: &Path, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Swarm;
    let ledger_path = cwd.join(".beads/issues.jsonl");
    if !ledger_path.is_file() {
        findings.push(
            Finding::warn(cat, "Beads ledger not found")
                .with_detail(format!("Expected {}", ledger_path.display()))
                .with_remediation("Run from a Beads-backed checkout or initialize Beads first"),
        );
        return;
    }

    let content = match std::fs::read_to_string(&ledger_path) {
        Ok(content) => content,
        Err(err) => {
            findings.push(
                Finding::fail(cat, "Beads ledger is not readable")
                    .with_detail(format!("{}: {err}", ledger_path.display()))
                    .with_remediation("Check ledger permissions before starting more agents"),
            );
            return;
        }
    };

    let summary = summarize_beads_ledger(&content, Utc::now(), SWARM_STALE_IN_PROGRESS_HOURS);
    if summary.parse_errors.eq(&0) {
        findings.push(
            Finding::pass(cat, "Beads ledger parses").with_detail(format!(
                "{} issues; {} active ({} open, {} in_progress)",
                summary.total, summary.active, summary.open, summary.in_progress
            )),
        );
    } else {
        findings.push(
            Finding::fail(cat, "Beads ledger has malformed JSONL rows")
                .with_detail(format!(
                    "{} parse error(s) in {} rows",
                    summary.parse_errors, summary.total
                ))
                .with_remediation("Run `br doctor --json` and rebuild from healthy issues.jsonl before claiming more work"),
        );
    }

    if summary.stale_in_progress.is_empty() {
        findings.push(Finding::pass(cat, "No stale in_progress beads detected"));
    } else {
        findings.push(
            Finding::warn(cat, "Stale in_progress beads need coordination")
                .with_detail(format_stale_issues(&summary.stale_in_progress))
                .with_remediation("Use Agent Mail to contact owners; only reset a bead after confirming the owner is stale"),
        );
    }
}

fn check_swarm_stalled_bead_reaper(cwd: &Path, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Swarm;
    let ledger_path = cwd.join(".beads/issues.jsonl");
    let content = match std::fs::read_to_string(&ledger_path) {
        Ok(content) => content,
        Err(err) => {
            findings.push(
                Finding::warn(cat, "Stalled bead reaper audit unavailable")
                    .with_detail(format!("{}: {err}", ledger_path.display()))
                    .with_remediation(
                        "Run `br list --status=in_progress --json` manually before resetting beads",
                    ),
            );
            return;
        }
    };

    let (agent_roster, agent_roster_error) = read_agent_mail_agents_roster(cwd);
    let finding = classify_stalled_bead_reaper(
        &content,
        agent_roster.as_ref(),
        agent_roster_error.as_deref(),
        Utc::now(),
        SWARM_STALE_IN_PROGRESS_HOURS,
    );
    findings.push(finding);
}

fn read_agent_mail_health(cwd: &Path) -> (Option<serde_json::Value>, Option<String>) {
    if which_tool("am").is_none() {
        return (None, Some("Agent Mail CLI not found".to_string()));
    }

    let project = cwd.display().to_string();
    let args = [
        "robot",
        "health",
        "--format",
        "json",
        "--project",
        project.as_str(),
    ];
    match run_probe_json(SwarmProbeCommand::Am, &args, Some(cwd), "am robot health") {
        Ok(value) => (Some(value), None),
        Err(err) => (None, Some(err)),
    }
}

fn read_agent_mail_agents_roster(cwd: &Path) -> (Option<serde_json::Value>, Option<String>) {
    if which_tool("am").is_none() {
        return (None, Some("Agent Mail CLI not found".to_string()));
    }

    let project = cwd.display().to_string();
    let args = [
        "robot",
        "agents",
        "--format",
        "json",
        "--project",
        project.as_str(),
    ];
    match run_tool_with_timeout(SwarmProbeCommand::Am, &args, Some(cwd), SWARM_PROBE_TIMEOUT) {
        Ok(outcome) if outcome.timed_out => (
            None,
            Some("am robot agents timed out before returning activity data".to_string()),
        ),
        Ok(outcome) if !outcome.success => (
            None,
            Some(format!(
                "am robot agents unavailable: {}",
                command_failure_detail(&outcome)
            )),
        ),
        Ok(outcome) => match serde_json::from_str::<serde_json::Value>(&outcome.stdout) {
            Ok(value) => (Some(value), None),
            Err(err) => (
                None,
                Some(format!(
                    "am robot agents returned non-JSON output: {err}; {}",
                    redacted_output_snippet(&outcome)
                )),
            ),
        },
        Err(err) => (
            None,
            Some(format!("am robot agents failed to start: {err}")),
        ),
    }
}

fn check_swarm_live_admission(cwd: &Path, findings: &mut Vec<Finding>) {
    let ledger_path = cwd.join(".beads/issues.jsonl");
    let content = match std::fs::read_to_string(&ledger_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let warnings = vec!["Beads ledger missing".to_string()];
            findings.push(swarm_admission_blocked_finding(
                Severity::Warn,
                "Live swarm admission decision denies new work",
                format!("Beads ledger not found at {}", ledger_path.display()),
                &warnings,
            ));
            return;
        }
        Err(err) => {
            let warnings = vec!["Beads ledger unreadable".to_string()];
            findings.push(swarm_admission_blocked_finding(
                Severity::Fail,
                "Live swarm admission decision unavailable",
                format!("{} is not readable: {err}", ledger_path.display()),
                &warnings,
            ));
            return;
        }
    };

    let summary = summarize_beads_ledger(&content, Utc::now(), SWARM_STALE_IN_PROGRESS_HOURS);
    if summary.parse_errors > 0 {
        let warnings = vec!["Beads ledger has malformed JSONL rows".to_string()];
        findings.push(swarm_admission_blocked_finding(
            Severity::Fail,
            "Live swarm admission decision denies new work",
            format!(
                "Beads ledger has {} malformed JSONL row(s); coordination state is corrupted",
                summary.parse_errors
            ),
            &warnings,
        ));
        return;
    }

    let sample = HostResourceSample::current();
    let mut warnings = resource_sample_warnings(&sample);
    if !summary.stale_in_progress.is_empty() {
        warnings.push(format!(
            "{} stale in_progress bead(s) may overstate live agent load",
            summary.stale_in_progress.len()
        ));
    }

    let plan = match build_swarm_doctor_capacity_plan(&sample) {
        Ok(plan) => plan,
        Err(err) => {
            findings.push(swarm_admission_blocked_finding(
                Severity::Fail,
                "Live swarm admission decision unavailable",
                format!("capacity plan could not be built from live host sample: {err}"),
                &warnings,
            ));
            return;
        }
    };

    let live_load = live_load_from_beads_summary(&summary);
    findings.push(classify_swarm_admission(plan, sample, live_load, &warnings));
}

fn build_swarm_doctor_capacity_plan(
    sample: &HostResourceSample,
) -> std::result::Result<SwarmCapacityPlan, SwarmCapacityPlanError> {
    let budgets = HostResourceBudgets::from_host();
    let cpu_cores = budgets.cpu_cores.max(1);
    let inventory = SwarmHostInventory::new(
        cpu_cores,
        cpu_cores,
        bytes_to_mib_ceil(budgets.max_rss_bytes.saturating_mul(2)).max(1),
    );
    build_swarm_doctor_capacity_plan_with_inventory(
        sample,
        inventory,
        budgets.max_queue_depth.max(1),
    )
}

fn build_swarm_doctor_capacity_plan_with_inventory(
    sample: &HostResourceSample,
    inventory: SwarmHostInventory,
    max_queue_depth: usize,
) -> std::result::Result<SwarmCapacityPlan, SwarmCapacityPlanError> {
    let evidence = SwarmCapacityEvidenceSummary {
        complete_records: 1,
        host_capacity_rows: 1,
        host_capacity_mismatch_rows: 0,
        max_p99_ms: 250,
        max_p999_ms: 1_000,
        max_queue_depth: max_queue_depth.max(1),
        max_rss_mb: sample.rss_bytes.map_or(256, bytes_to_mib_ceil).max(1),
        max_cpu_pct: 1.0,
    };

    SwarmCapacityPlannerConfig::default().plan_from_summary(evidence, inventory)
}

fn live_load_from_beads_summary(summary: &BeadsLedgerSummary) -> SwarmLiveLoad {
    SwarmLiveLoad::empty()
        .with_active_agents(
            env_u64("PI_DOCTOR_SWARM_ACTIVE_AGENTS")
                .unwrap_or_else(|| usize_to_u64(summary.in_progress)),
        )
        .with_active_tool_calls(env_u64("PI_DOCTOR_SWARM_ACTIVE_TOOL_CALLS").unwrap_or(0))
        .with_extension_hostcall_lanes(
            env_u64("PI_DOCTOR_SWARM_EXTENSION_HOSTCALL_LANES").unwrap_or(0),
        )
        .with_active_rch_jobs(env_u64("PI_DOCTOR_SWARM_ACTIVE_RCH_JOBS").unwrap_or(0))
}

fn classify_swarm_admission(
    plan: SwarmCapacityPlan,
    sample: HostResourceSample,
    live_load: SwarmLiveLoad,
    warnings: &[String],
) -> Finding {
    let mut controller = SwarmAdmissionController::from_plan(plan);
    let queue_depth = u64_to_usize_saturating(live_load.active_tool_calls.saturating_add(1));
    let resource_pressure_ratio =
        resource_pressure_ratio(&sample, &controller.plan().resource_budgets);
    let tail_latency_sample = TailLatencyRegimeSample::new(
        controller.plan().evidence.max_p99_ms,
        controller.plan().evidence.max_p999_ms,
        queue_depth,
        resource_pressure_ratio,
    );
    let request = ResourceRequest::new(ResourceOperationKind::Tool, "doctor.swarm_admission")
        .with_estimated_tool_output_bytes(
            controller
                .plan()
                .resource_budgets
                .max_tool_output_bytes
                .min(MIB_BYTES),
        )
        .with_queue_depth(queue_depth);
    let decision = controller.decide(&request, sample, tail_latency_sample, live_load);
    let next_actions = swarm_admission_next_actions(decision.action, !warnings.is_empty());
    let remediation = next_actions.join("; ");
    let detail = format_swarm_admission_detail(&decision, warnings);
    let data = swarm_admission_data(&decision, warnings, &next_actions);

    match (decision.action, warnings.is_empty()) {
        (AdmissionAction::Admit, true) => {
            Finding::pass(CheckCategory::Swarm, "Live swarm admission decision: admit")
        }
        (AdmissionAction::Admit, false) => Finding::warn(
            CheckCategory::Swarm,
            "Live swarm admission decision degraded: admit",
        ),
        (AdmissionAction::Backpressure, _) => Finding::warn(
            CheckCategory::Swarm,
            "Live swarm admission decision: backpressure",
        ),
        (AdmissionAction::Deny, _) => {
            Finding::fail(CheckCategory::Swarm, "Live swarm admission decision: deny")
        }
    }
    .with_detail(detail)
    .with_remediation(remediation)
    .with_data(data)
}

fn swarm_admission_blocked_finding(
    severity: Severity,
    title: impl Into<String>,
    detail: String,
    warnings: &[String],
) -> Finding {
    let next_actions = vec![
        "Do not launch new swarm work while the admission action is deny".to_string(),
        "Repair or refresh the coordination inputs".to_string(),
        "Rerun `pi doctor --only swarm --format json`".to_string(),
    ];
    let remediation = next_actions.join("; ");
    let data = serde_json::json!({
        "schema": SWARM_DOCTOR_ADMISSION_SCHEMA,
        "source": {
            "capacity_plan": "unavailable",
            "live_load": "unavailable",
            "resource_sample": "unavailable"
        },
        "action": action_label(AdmissionAction::Deny),
        "reason": detail,
        "retry_after_ms": 0,
        "pressure_dimension": capacity_dimension_label(SwarmCapacityDimension::None),
        "capacity_pressure": null,
        "planned_budgets": null,
        "live_counts": null,
        "resource_sample": null,
        "admission_decision": null,
        "stale_data_warnings": warnings,
        "next_actions": next_actions
    });

    match severity {
        Severity::Pass => Finding::pass(CheckCategory::Swarm, title),
        Severity::Info => Finding::info(CheckCategory::Swarm, title),
        Severity::Warn => Finding::warn(CheckCategory::Swarm, title),
        Severity::Fail => Finding::fail(CheckCategory::Swarm, title),
    }
    .with_detail(detail)
    .with_remediation(remediation)
    .with_data(data)
}

fn swarm_admission_data(
    decision: &crate::resource_governor::SwarmAdmissionControllerDecision,
    warnings: &[String],
    next_actions: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_DOCTOR_ADMISSION_SCHEMA,
        "source": {
            "capacity_plan": "doctor_live_host_sample",
            "live_load": "beads_ledger_with_env_overrides",
            "resource_sample": "host_resource_sample_current"
        },
        "action": action_label(decision.action),
        "reason": decision.reason,
        "retry_after_ms": decision.retry_after_ms,
        "pressure_dimension": capacity_dimension_label(decision.capacity_pressure.dimension),
        "capacity_pressure": decision.capacity_pressure,
        "planned_budgets": {
            "agent_concurrency": decision.recommended_agent_concurrency,
            "tool_concurrency": decision.recommended_tool_concurrency,
            "extension_hostcall_lanes": decision.recommended_extension_hostcall_lanes,
            "rch_verification_fanout": decision.recommended_rch_verification_fanout,
            "max_queue_depth": decision.resource_decision.budgets.max_queue_depth,
            "max_tool_output_bytes": decision.resource_decision.budgets.max_tool_output_bytes,
            "memory_pressure_threshold_ratio": decision.resource_decision.budgets.backpressure_ratio,
            "deny_ratio": decision.resource_decision.budgets.deny_ratio,
            "plan_confidence": decision.plan_confidence
        },
        "live_counts": decision.live_load,
        "resource_sample": decision.resource_decision.sample,
        "admission_decision": decision.telemetry(),
        "stale_data_warnings": warnings,
        "next_actions": next_actions
    })
}

fn format_swarm_admission_detail(
    decision: &crate::resource_governor::SwarmAdmissionControllerDecision,
    warnings: &[String],
) -> String {
    let pressure = decision.capacity_pressure;
    let mut detail = format!(
        "action={}, reason={}, retry_after_ms={}, pressure={} {}/{} ({:.2}x), live_agents={}, live_tool_calls={}, live_extension_lanes={}, live_rch_jobs={}, planned_agents={}, planned_tool_calls={}, planned_extension_lanes={}, planned_rch_jobs={}",
        action_label(decision.action),
        decision.reason,
        decision.retry_after_ms,
        capacity_dimension_label(pressure.dimension),
        pressure.observed,
        pressure.budget,
        pressure.ratio,
        decision.live_load.active_agents,
        decision.live_load.active_tool_calls,
        decision.live_load.extension_hostcall_lanes,
        decision.live_load.active_rch_jobs,
        decision.recommended_agent_concurrency,
        decision.recommended_tool_concurrency,
        decision.recommended_extension_hostcall_lanes,
        decision.recommended_rch_verification_fanout
    );
    if !warnings.is_empty() {
        detail.push_str("; stale_data_warnings=");
        detail.push_str(&warnings.join("; "));
    }
    detail
}

fn swarm_admission_next_actions(action: AdmissionAction, has_warnings: bool) -> Vec<String> {
    let mut actions = match action {
        AdmissionAction::Admit => vec![
            "Proceed with new swarm work only after current leases are visible".to_string(),
            "Keep CARGO_TARGET_DIR and TMPDIR on high-capacity storage before cargo checks"
                .to_string(),
        ],
        AdmissionAction::Backpressure => vec![
            "Delay new swarm work until the reported retry_after_ms has elapsed".to_string(),
            "Reduce active agents or tool calls on the pressure dimension".to_string(),
            "Rerun `pi doctor --only swarm --format json` before heavyweight cargo checks"
                .to_string(),
        ],
        AdmissionAction::Deny => vec![
            "Do not launch new swarm work while the admission action is deny".to_string(),
            "Stop or defer agents on the pressure dimension".to_string(),
            "Rerun `pi doctor --only swarm --format json` after pressure clears".to_string(),
        ],
    };
    if has_warnings {
        actions.push(
            "Repair stale or missing inputs before treating the decision as healthy".to_string(),
        );
    }
    actions
}

const fn action_label(action: AdmissionAction) -> &'static str {
    match action {
        AdmissionAction::Admit => "admit",
        AdmissionAction::Backpressure => "backpressure",
        AdmissionAction::Deny => "deny",
    }
}

const fn capacity_dimension_label(dimension: SwarmCapacityDimension) -> &'static str {
    match dimension {
        SwarmCapacityDimension::ActiveAgents => "active_agents",
        SwarmCapacityDimension::ActiveToolCalls => "active_tool_calls",
        SwarmCapacityDimension::ExtensionHostcallLanes => "extension_hostcall_lanes",
        SwarmCapacityDimension::RchVerificationFanout => "rch_verification_fanout",
        SwarmCapacityDimension::None => "none",
    }
}

fn resource_sample_warnings(sample: &HostResourceSample) -> Vec<String> {
    let mut warnings = Vec::new();
    if sample.load_avg_1m.is_none() {
        warnings.push("host load average unavailable".to_string());
    }
    if sample.rss_bytes.is_none() {
        warnings.push("process RSS sample unavailable".to_string());
    }
    if sample.process_count.is_none() {
        warnings.push("host process-count sample unavailable".to_string());
    }
    if sample.fd_count.is_none() {
        warnings.push("file-descriptor sample unavailable".to_string());
    }
    warnings
}

#[allow(clippy::cast_precision_loss)]
fn resource_pressure_ratio(sample: &HostResourceSample, budgets: &HostResourceBudgets) -> f64 {
    let mut ratio = 0.0_f64;
    if let Some(load_avg_1m) = sample.load_avg_1m {
        ratio = ratio.max(load_avg_1m / budgets.max_load_avg_1m.max(f64::EPSILON));
    }
    if let Some(rss_bytes) = sample.rss_bytes {
        ratio = ratio.max((rss_bytes as f64) / (budgets.max_rss_bytes.max(1) as f64));
    }
    if let Some(process_count) = sample.process_count {
        ratio = ratio.max((process_count as f64) / (budgets.max_processes.max(1) as f64));
    }
    if let Some(fd_count) = sample.fd_count {
        ratio = ratio.max((fd_count as f64) / (budgets.max_fds.max(1) as f64));
    }
    ratio
}

fn check_swarm_resource_preflight(findings: &mut Vec<Finding>) {
    let sample = HostResourceSample::current();
    let snapshot = build_swarm_resource_preflight_snapshot(&sample);
    findings.push(classify_swarm_resource_preflight(&snapshot));
}

#[derive(Debug, Clone, PartialEq)]
struct SwarmResourcePreflightSnapshot {
    logical_cpu_cores: u64,
    cpu_quota: CgroupCpuQuota,
    cpuset: CpuSetSnapshot,
    numa: NumaTopologySnapshot,
    memory: MemoryLimitSnapshot,
    effective_cpu_cores: u64,
    local_build_pressure: LocalBuildPressureSnapshot,
    rch_queue_posture: RchQueuePostureSnapshot,
    headroom_paths: Vec<SwarmHeadroomPath>,
    recommended_budgets: Option<SwarmResourceBudgetRecommendation>,
    plan_error: Option<String>,
    source_errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct CgroupCpuQuota {
    source: Option<String>,
    quota_us: Option<u64>,
    period_us: Option<u64>,
    quota_cores: Option<f64>,
    unlimited: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CpuSetSnapshot {
    source: Option<String>,
    raw: Option<String>,
    cpus: Vec<u64>,
    cpu_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NumaTopologySnapshot {
    source: Option<String>,
    raw_online: Option<String>,
    nodes: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoryLimitSnapshot {
    mem_total_bytes: Option<u64>,
    cgroup_limit_bytes: Option<u64>,
    effective_limit_bytes: Option<u64>,
    unlimited: bool,
    source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CgroupMemoryLimitParse {
    Limited(u64),
    Unlimited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmHeadroomPath {
    env_name: String,
    path: Option<String>,
    exists: bool,
    under_expected_root: Option<bool>,
    available_kb: Option<u64>,
    ready: bool,
    problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalBuildPressureSnapshot {
    source: String,
    active_processes: Option<u64>,
    process_budget: u64,
    status: String,
    patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RchQueuePostureSnapshot {
    source: String,
    status: String,
    recommended_action: String,
    reason: Option<String>,
    queue_depth: Option<u64>,
    active_builds: Option<u64>,
    queued_builds: Option<u64>,
    slots_available: Option<u64>,
    slots_total: Option<u64>,
    workers_healthy: Option<u64>,
    workers_total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmBudgetExplanation {
    budget: String,
    value: String,
    rationale: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmResourceBudgetRecommendation {
    agent_concurrency: u64,
    tool_concurrency: u64,
    extension_hostcall_lanes: u64,
    rch_verification_fanout: u64,
    max_queue_depth: usize,
    max_rss_bytes: u64,
    local_build_process_budget: u64,
    rch_queue_depth_budget: usize,
    numa_node_count: usize,
    memory_pressure_threshold_ratio_label: String,
    plan_confidence: String,
    explanations: Vec<SwarmBudgetExplanation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmLanePlacementPlan {
    status: String,
    lane_groups: Vec<SwarmLaneGroup>,
    caveats: Vec<String>,
    recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmLaneGroup {
    lane_id: String,
    numa_node: Option<u64>,
    cpu_affinity_hint: String,
    cpus: Vec<u64>,
    target_dir: String,
    tmpdir: String,
    max_agents: u64,
    max_tool_batches: u64,
    max_extension_hostcall_lanes: u64,
    max_rch_verification_fanout: u64,
}

fn build_swarm_resource_preflight_snapshot(
    sample: &HostResourceSample,
) -> SwarmResourcePreflightSnapshot {
    let mut source_errors = Vec::new();
    let logical_cpu_cores = env_u64(SWARM_RESOURCE_PREFLIGHT_LOGICAL_CPU_CORES_ENV)
        .filter(|cores| *cores > 0)
        .or_else(|| {
            std::thread::available_parallelism()
                .ok()
                .and_then(|value| u64::try_from(value.get()).ok())
        })
        .unwrap_or(1);
    let cpu_quota = read_cgroup_cpu_quota(&mut source_errors);
    let cpuset = read_cpuset_snapshot(&mut source_errors);
    let numa = read_numa_topology(&mut source_errors);
    let memory = read_memory_limit_snapshot(&mut source_errors);

    if cpu_quota.source.is_none() {
        source_errors.push("cgroup CPU quota unavailable".to_string());
    }
    if cpuset.source.is_none() {
        source_errors.push("cpuset CPU list unavailable".to_string());
    }
    if numa.source.is_none() {
        source_errors.push("NUMA topology unavailable".to_string());
    }
    if memory.effective_limit_bytes.is_none() {
        source_errors.push("effective memory limit unavailable".to_string());
    }

    let effective_cpu_cores =
        effective_swarm_cpu_cores(logical_cpu_cores, cpu_quota.quota_cores, cpuset.cpu_count);
    let local_build_pressure = collect_local_build_pressure(effective_cpu_cores);
    let rch_queue_posture = read_rch_queue_posture(&mut source_errors);
    let headroom_paths = ["CARGO_TARGET_DIR", "TMPDIR"]
        .into_iter()
        .map(collect_swarm_headroom_path)
        .collect::<Vec<_>>();
    let (recommended_budgets, plan_error) =
        match build_swarm_resource_preflight_plan(sample, effective_cpu_cores, &memory) {
            Ok(plan) => (
                Some(resource_budget_recommendation(
                    &plan,
                    &numa,
                    &local_build_pressure,
                    &rch_queue_posture,
                )),
                None,
            ),
            Err(err) => (None, Some(err.to_string())),
        };

    SwarmResourcePreflightSnapshot {
        logical_cpu_cores,
        cpu_quota,
        cpuset,
        numa,
        memory,
        effective_cpu_cores,
        local_build_pressure,
        rch_queue_posture,
        headroom_paths,
        recommended_budgets,
        plan_error,
        source_errors,
    }
}

fn classify_swarm_resource_preflight(snapshot: &SwarmResourcePreflightSnapshot) -> Finding {
    let critical_failures = swarm_resource_preflight_critical_failures(snapshot);
    let data = swarm_resource_preflight_data(snapshot, &critical_failures);
    let detail = format_swarm_resource_preflight_detail(snapshot, &critical_failures);
    if !critical_failures.is_empty() {
        return Finding::fail(CheckCategory::Swarm, "Swarm resource preflight blocked")
            .with_detail(detail)
            .with_remediation(format!(
                "Export CARGO_TARGET_DIR and TMPDIR under {SWARM_CARGO_SCRATCH_ROOT}/<agent>/ with at least {} free before launching swarms or heavyweight cargo gates",
                format_available_kb(SWARM_DISK_WARN_AVAILABLE_KB)
            ))
            .with_data(data);
    }
    if !snapshot.source_errors.is_empty() || snapshot.plan_error.is_some() {
        return Finding::warn(CheckCategory::Swarm, "Swarm resource preflight degraded")
            .with_detail(detail)
            .with_remediation(
                "Review unavailable cgroup, cpuset, NUMA, memory, or budget inputs before using the recommended concurrency as an operator limit",
            )
            .with_data(data);
    }

    Finding::pass(CheckCategory::Swarm, "Swarm resource preflight ready")
        .with_detail(detail)
        .with_data(data)
}

fn swarm_resource_preflight_critical_failures(
    snapshot: &SwarmResourcePreflightSnapshot,
) -> Vec<String> {
    let mut failures = snapshot
        .headroom_paths
        .iter()
        .filter_map(|path| {
            if path.ready {
                None
            } else {
                Some(format!(
                    "{}:{}",
                    path.env_name,
                    path.problem.as_deref().unwrap_or("headroom_unavailable")
                ))
            }
        })
        .collect::<Vec<_>>();
    if snapshot.effective_cpu_cores == 0 {
        failures.push("effective_cpu_cores:unavailable".to_string());
    }
    if matches!(
        snapshot.rch_queue_posture.recommended_action.as_str(),
        "backoff"
    ) {
        failures.push(format!(
            "rch_queue:{}",
            snapshot
                .rch_queue_posture
                .reason
                .as_deref()
                .unwrap_or("backoff")
        ));
    }
    failures
}

fn swarm_resource_preflight_data(
    snapshot: &SwarmResourcePreflightSnapshot,
    critical_failures: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_DOCTOR_RESOURCE_PREFLIGHT_SCHEMA,
        "source": "linux_proc_cgroup_sysfs",
        "cpu": {
            "logical_cores": snapshot.logical_cpu_cores,
            "effective_cores": snapshot.effective_cpu_cores,
            "cgroup_quota": {
                "source": snapshot.cpu_quota.source,
                "quota_us": snapshot.cpu_quota.quota_us,
                "period_us": snapshot.cpu_quota.period_us,
                "quota_cores": snapshot.cpu_quota.quota_cores,
                "unlimited": snapshot.cpu_quota.unlimited,
            },
            "cpuset": {
                "source": snapshot.cpuset.source,
                "raw": snapshot.cpuset.raw,
                "cpus": snapshot.cpuset.cpus,
                "cpu_count": snapshot.cpuset.cpu_count,
            },
        },
        "numa": {
            "source": snapshot.numa.source,
            "raw_online": snapshot.numa.raw_online,
            "node_count": snapshot.numa.nodes.len(),
            "nodes": snapshot.numa.nodes,
        },
        "memory": {
            "source": snapshot.memory.source,
            "mem_total_bytes": snapshot.memory.mem_total_bytes,
            "cgroup_limit_bytes": snapshot.memory.cgroup_limit_bytes,
            "effective_limit_bytes": snapshot.memory.effective_limit_bytes,
            "unlimited": snapshot.memory.unlimited,
        },
        "local_build_pressure": local_build_pressure_json(&snapshot.local_build_pressure),
        "rch_queue_posture": rch_queue_posture_json(&snapshot.rch_queue_posture),
        "tmpfs_headroom": {
            "expected_root": SWARM_CARGO_SCRATCH_ROOT,
            "warn_available_kb": SWARM_DISK_WARN_AVAILABLE_KB,
            "paths": snapshot.headroom_paths.iter().map(swarm_headroom_path_json).collect::<Vec<_>>(),
        },
        "recommended_budgets": snapshot.recommended_budgets.as_ref().map(resource_budget_recommendation_json),
        "lane_placement": swarm_lane_placement_plan_json(&swarm_lane_placement_plan(snapshot, critical_failures)),
        "plan_error": snapshot.plan_error,
        "critical_failures": critical_failures,
        "source_errors": snapshot.source_errors,
    })
}

fn local_build_pressure_json(pressure: &LocalBuildPressureSnapshot) -> serde_json::Value {
    serde_json::json!({
        "source": pressure.source,
        "active_processes": pressure.active_processes,
        "process_budget": pressure.process_budget,
        "status": pressure.status,
        "patterns": pressure.patterns,
    })
}

fn rch_queue_posture_json(posture: &RchQueuePostureSnapshot) -> serde_json::Value {
    serde_json::json!({
        "source": posture.source,
        "status": posture.status,
        "recommended_action": posture.recommended_action,
        "reason": posture.reason,
        "queue_depth": posture.queue_depth,
        "active_builds": posture.active_builds,
        "queued_builds": posture.queued_builds,
        "slots_available": posture.slots_available,
        "slots_total": posture.slots_total,
        "workers_healthy": posture.workers_healthy,
        "workers_total": posture.workers_total,
    })
}

fn swarm_headroom_path_json(path: &SwarmHeadroomPath) -> serde_json::Value {
    serde_json::json!({
        "env_name": path.env_name,
        "path": path.path,
        "exists": path.exists,
        "under_expected_root": path.under_expected_root,
        "available_kb": path.available_kb,
        "ready": path.ready,
        "problem": path.problem,
    })
}

fn resource_budget_recommendation_json(
    recommendation: &SwarmResourceBudgetRecommendation,
) -> serde_json::Value {
    serde_json::json!({
        "agent_concurrency": recommendation.agent_concurrency,
        "tool_concurrency": recommendation.tool_concurrency,
        "extension_hostcall_lanes": recommendation.extension_hostcall_lanes,
        "rch_verification_fanout": recommendation.rch_verification_fanout,
        "max_queue_depth": recommendation.max_queue_depth,
        "max_rss_bytes": recommendation.max_rss_bytes,
        "local_build_process_budget": recommendation.local_build_process_budget,
        "rch_queue_depth_budget": recommendation.rch_queue_depth_budget,
        "numa_node_count": recommendation.numa_node_count,
        "memory_pressure_threshold_ratio": recommendation.memory_pressure_threshold_ratio_label,
        "plan_confidence": recommendation.plan_confidence,
        "explanations": recommendation.explanations.iter().map(budget_explanation_json).collect::<Vec<_>>(),
    })
}

fn swarm_lane_placement_plan_json(plan: &SwarmLanePlacementPlan) -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_DOCTOR_LANE_PLACEMENT_SCHEMA,
        "status": plan.status,
        "lane_groups": plan.lane_groups.iter().map(swarm_lane_group_json).collect::<Vec<_>>(),
        "caveats": plan.caveats,
        "recommendations": plan.recommendations,
    })
}

fn swarm_lane_group_json(group: &SwarmLaneGroup) -> serde_json::Value {
    serde_json::json!({
        "lane_id": group.lane_id,
        "numa_node": group.numa_node,
        "cpu_affinity_hint": group.cpu_affinity_hint,
        "cpus": group.cpus,
        "target_dir": group.target_dir,
        "tmpdir": group.tmpdir,
        "max_agents": group.max_agents,
        "max_tool_batches": group.max_tool_batches,
        "max_extension_hostcall_lanes": group.max_extension_hostcall_lanes,
        "max_rch_verification_fanout": group.max_rch_verification_fanout,
    })
}

fn budget_explanation_json(explanation: &SwarmBudgetExplanation) -> serde_json::Value {
    serde_json::json!({
        "budget": explanation.budget,
        "value": explanation.value,
        "rationale": explanation.rationale,
    })
}

fn swarm_lane_placement_plan(
    snapshot: &SwarmResourcePreflightSnapshot,
    critical_failures: &[String],
) -> SwarmLanePlacementPlan {
    let mut caveats = swarm_lane_placement_caveats(snapshot);
    if let Some(plan_error) = &snapshot.plan_error {
        caveats.push(format!("capacity_plan_unavailable:{plan_error}"));
    }
    if !critical_failures.is_empty() {
        caveats.push("critical_preflight_failures".to_string());
    }
    caveats.sort();
    caveats.dedup();

    let lane_nodes = if snapshot.numa.nodes.is_empty() {
        vec![None]
    } else {
        snapshot
            .numa
            .nodes
            .iter()
            .copied()
            .map(Some)
            .collect::<Vec<_>>()
    };
    let cpu_groups = distribute_cpus_across_lanes(&snapshot.cpuset.cpus, lane_nodes.len());
    let lane_groups = lane_nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let lane_id =
                node.map_or_else(|| "shared".to_string(), |node| format!("numa-node-{node}"));
            let cpus = cpu_groups.get(index).cloned().unwrap_or_default();
            let cpu_affinity_hint = if cpus.is_empty() {
                "cpuset-unavailable".to_string()
            } else {
                format_numeric_ranges(&cpus)
            };
            let recommended_budgets = snapshot.recommended_budgets.as_ref();
            SwarmLaneGroup {
                target_dir: format!("{SWARM_CARGO_SCRATCH_ROOT}/<agent>/{lane_id}/target"),
                tmpdir: format!("{SWARM_CARGO_SCRATCH_ROOT}/<agent>/{lane_id}/tmp"),
                max_agents: recommended_budgets.map_or(1, |budget| {
                    split_nonzero_budget(budget.agent_concurrency, lane_nodes.len())
                }),
                max_tool_batches: recommended_budgets.map_or(1, |budget| {
                    split_nonzero_budget(budget.tool_concurrency, lane_nodes.len())
                }),
                max_extension_hostcall_lanes: recommended_budgets.map_or(1, |budget| {
                    split_nonzero_budget(budget.extension_hostcall_lanes, lane_nodes.len())
                }),
                max_rch_verification_fanout: recommended_budgets.map_or(0, |budget| {
                    split_optional_budget(budget.rch_verification_fanout, lane_nodes.len())
                }),
                lane_id,
                numa_node: *node,
                cpu_affinity_hint,
                cpus,
            }
        })
        .collect();

    let status = if !critical_failures.is_empty() {
        "blocked"
    } else if caveats.is_empty() {
        "ready"
    } else {
        "degraded"
    }
    .to_string();

    let recommendations = swarm_lane_placement_recommendations(&status);
    SwarmLanePlacementPlan {
        status,
        lane_groups,
        caveats,
        recommendations,
    }
}

fn swarm_lane_placement_caveats(snapshot: &SwarmResourcePreflightSnapshot) -> Vec<String> {
    let mut caveats = Vec::new();
    if snapshot.numa.source.is_none() || snapshot.numa.nodes.is_empty() {
        caveats.push("numa_topology_unavailable".to_string());
    }
    if snapshot.cpuset.source.is_none() || snapshot.cpuset.cpus.is_empty() {
        caveats.push("cpuset_unavailable".to_string());
    } else if snapshot
        .cpuset
        .cpu_count
        .is_some_and(|count| count < snapshot.logical_cpu_cores)
    {
        caveats.push(format!(
            "cpuset_partial:{}/{}",
            snapshot.cpuset.cpu_count.unwrap_or(0),
            snapshot.logical_cpu_cores
        ));
    }
    if let Some(bytes) = snapshot
        .memory
        .effective_limit_bytes
        .filter(|bytes| *bytes <= SWARM_LANE_TIGHT_MEMORY_BYTES)
    {
        caveats.push(format!(
            "tight_memory_limit:{}MiB",
            bytes_to_mib_ceil(bytes)
        ));
    }
    if !matches!(
        snapshot.rch_queue_posture.recommended_action.as_str(),
        "proceed"
    ) {
        caveats.push(format!(
            "rch_queue_pressure:{}",
            snapshot.rch_queue_posture.recommended_action
        ));
    }
    if snapshot.local_build_pressure.status != "clear" {
        caveats.push(format!(
            "local_build_pressure:{}",
            snapshot.local_build_pressure.status
        ));
    }
    if snapshot.headroom_paths.iter().any(|path| !path.ready) {
        caveats.push("scratch_headroom_incomplete".to_string());
    }
    if !snapshot.source_errors.is_empty() {
        caveats.push("source_inputs_degraded".to_string());
    }
    caveats
}

fn swarm_lane_placement_recommendations(status: &str) -> Vec<String> {
    let mut recommendations = vec![
        "Use the listed target_dir and tmpdir roots as per-agent scratch directories; do not share repo-local target directories.".to_string(),
        "Treat CPU affinity hints as operator guidance only; Doctor does not pin processes or mutate OS/RCH state.".to_string(),
        "Increase per-lane agent, tool, extension-hostcall, and RCH fanout only after rerunning the swarm preflight.".to_string(),
    ];
    if status == "blocked" {
        recommendations.push(
            "Resolve critical_failures before launching heavyweight swarm or cargo work."
                .to_string(),
        );
    }
    recommendations
}

fn distribute_cpus_across_lanes(cpus: &[u64], lane_count: usize) -> Vec<Vec<u64>> {
    let lane_count = lane_count.max(1);
    if cpus.is_empty() {
        return vec![Vec::new(); lane_count];
    }
    let chunk_size = cpus.len().div_ceil(lane_count).max(1);
    let mut lanes = cpus
        .chunks(chunk_size)
        .map(<[u64]>::to_vec)
        .collect::<Vec<_>>();
    lanes.resize_with(lane_count, Vec::new);
    lanes
}

fn split_nonzero_budget(total: u64, lane_count: usize) -> u64 {
    split_optional_budget(total, lane_count).max(1)
}

fn split_optional_budget(total: u64, lane_count: usize) -> u64 {
    if total == 0 {
        0
    } else {
        total
            .checked_div(usize_to_u64(lane_count.max(1)))
            .unwrap_or(0)
            .max(1)
    }
}

fn format_numeric_ranges(values: &[u64]) -> String {
    let Some((&first, rest)) = values.split_first() else {
        return "unknown".to_string();
    };
    let mut ranges = Vec::new();
    let mut start = first;
    let mut previous = first;
    for value in rest {
        if *value != previous.saturating_add(1) {
            ranges.push(format_numeric_range(start, previous));
            start = *value;
        }
        previous = *value;
    }
    ranges.push(format_numeric_range(start, previous));
    ranges.join(",")
}

fn format_numeric_range(start: u64, end: u64) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn format_swarm_resource_preflight_detail(
    snapshot: &SwarmResourcePreflightSnapshot,
    critical_failures: &[String],
) -> String {
    let quota = snapshot
        .cpu_quota
        .quota_cores
        .map_or_else(|| "unlimited".to_string(), format_ratio_label);
    let cpuset = snapshot
        .cpuset
        .cpu_count
        .map_or_else(|| "unknown".to_string(), |count| count.to_string());
    let memory = snapshot.memory.effective_limit_bytes.map_or_else(
        || "unknown".to_string(),
        |bytes| format!("{} MiB", bytes_to_mib_ceil(bytes)),
    );
    let budget = snapshot.recommended_budgets.as_ref().map_or_else(
        || "unavailable".to_string(),
        |recommendation| {
            format!(
                "agents={}, tools={}, extension_lanes={}, rch_fanout={}",
                recommendation.agent_concurrency,
                recommendation.tool_concurrency,
                recommendation.extension_hostcall_lanes,
                recommendation.rch_verification_fanout
            )
        },
    );
    let headroom_ready = snapshot
        .headroom_paths
        .iter()
        .filter(|path| path.ready)
        .count();
    let mut detail = format!(
        "cpu logical={}, effective={}, quota_cores={}, cpuset_cpus={}, numa_nodes={}, memory_effective={}, local_build_pressure={}, rch_queue_action={}, headroom_ready={}/{}, recommended={budget}",
        snapshot.logical_cpu_cores,
        snapshot.effective_cpu_cores,
        quota,
        cpuset,
        snapshot.numa.nodes.len(),
        memory,
        snapshot.local_build_pressure.status,
        snapshot.rch_queue_posture.recommended_action,
        headroom_ready,
        snapshot.headroom_paths.len(),
    );
    if !critical_failures.is_empty() {
        detail.push_str("; critical_failures=");
        detail.push_str(&critical_failures.join("; "));
    }
    if !snapshot.source_errors.is_empty() {
        detail.push_str("; source_errors=");
        detail.push_str(&snapshot.source_errors.join("; "));
    }
    if let Some(plan_error) = &snapshot.plan_error {
        detail.push_str("; plan_error=");
        detail.push_str(plan_error);
    }
    detail
}

fn read_cgroup_cpu_quota(source_errors: &mut Vec<String>) -> CgroupCpuQuota {
    if let Some((source, raw)) = read_first_existing_trimmed(
        "PI_DOCTOR_CGROUP_CPU_MAX_PATH",
        &["/sys/fs/cgroup/cpu.max"],
        source_errors,
    ) {
        if let Some(mut quota) = parse_cgroup_cpu_max(&raw) {
            quota.source = Some(source);
            return quota;
        }
        source_errors.push(format!("invalid cgroup cpu.max at {source}: {raw}"));
    }

    let quota = read_first_existing_trimmed(
        "PI_DOCTOR_CGROUP_CPU_CFS_QUOTA_US_PATH",
        &["/sys/fs/cgroup/cpu/cpu.cfs_quota_us"],
        source_errors,
    );
    let period = read_first_existing_trimmed(
        "PI_DOCTOR_CGROUP_CPU_CFS_PERIOD_US_PATH",
        &["/sys/fs/cgroup/cpu/cpu.cfs_period_us"],
        source_errors,
    );
    if let (Some((quota_source, quota_raw)), Some((period_source, period_raw))) = (quota, period) {
        if let Some(mut parsed) = parse_cgroup_cpu_cfs(&quota_raw, &period_raw) {
            parsed.source = Some(format!("{quota_source};{period_source}"));
            return parsed;
        }
        source_errors.push(format!(
            "invalid cgroup v1 CPU quota at {quota_source}/{period_source}: {quota_raw}/{period_raw}"
        ));
    }

    CgroupCpuQuota {
        source: None,
        quota_us: None,
        period_us: None,
        quota_cores: None,
        unlimited: false,
    }
}

fn read_cpuset_snapshot(source_errors: &mut Vec<String>) -> CpuSetSnapshot {
    let Some((source, raw)) = read_first_existing_trimmed(
        "PI_DOCTOR_CPUSET_CPUS_PATH",
        &[
            "/sys/fs/cgroup/cpuset.cpus.effective",
            "/sys/fs/cgroup/cpuset.cpus",
            "/sys/fs/cgroup/cpuset/cpuset.cpus",
        ],
        source_errors,
    ) else {
        return CpuSetSnapshot {
            source: None,
            raw: None,
            cpus: Vec::new(),
            cpu_count: None,
        };
    };

    let cpus = parse_numeric_range_list(&raw);
    let cpu_count = cpus
        .as_ref()
        .and_then(|cpus| u64::try_from(cpus.len()).ok());
    if cpu_count.is_none() {
        source_errors.push(format!("invalid cpuset CPU list at {source}: {raw}"));
    }
    CpuSetSnapshot {
        source: Some(source),
        raw: Some(raw),
        cpus: cpus.unwrap_or_default(),
        cpu_count,
    }
}

fn read_numa_topology(source_errors: &mut Vec<String>) -> NumaTopologySnapshot {
    let Some((source, raw)) = read_first_existing_trimmed(
        "PI_DOCTOR_NUMA_ONLINE_PATH",
        &["/sys/devices/system/node/online"],
        source_errors,
    ) else {
        return NumaTopologySnapshot {
            source: None,
            raw_online: None,
            nodes: Vec::new(),
        };
    };

    let nodes = parse_numeric_range_list(&raw).unwrap_or_else(|| {
        source_errors.push(format!("invalid NUMA node list at {source}: {raw}"));
        Vec::new()
    });
    NumaTopologySnapshot {
        source: Some(source),
        raw_online: Some(raw),
        nodes,
    }
}

fn read_memory_limit_snapshot(source_errors: &mut Vec<String>) -> MemoryLimitSnapshot {
    let mem_total_bytes =
        read_first_existing_trimmed("PI_DOCTOR_MEMINFO_PATH", &["/proc/meminfo"], source_errors)
            .and_then(|(source, raw)| {
                let parsed = parse_mem_total_bytes(&raw);
                if parsed.is_none() {
                    source_errors.push(format!("invalid MemTotal in {source}"));
                }
                parsed
            });
    let (source, parsed_limit) = read_first_existing_trimmed(
        "PI_DOCTOR_CGROUP_MEMORY_MAX_PATH",
        &[
            "/sys/fs/cgroup/memory.max",
            "/sys/fs/cgroup/memory/memory.limit_in_bytes",
        ],
        source_errors,
    )
    .map_or((None, None), |(source, raw)| {
        let parsed = parse_cgroup_memory_limit_bytes(&raw);
        if parsed.is_none() {
            source_errors.push(format!("invalid cgroup memory limit at {source}: {raw}"));
        }
        (Some(source), parsed)
    });
    let limit_bytes = match parsed_limit {
        Some(CgroupMemoryLimitParse::Limited(bytes)) => Some(bytes),
        Some(CgroupMemoryLimitParse::Unlimited) | None => None,
    };
    let unlimited = match parsed_limit {
        Some(CgroupMemoryLimitParse::Limited(bytes)) => {
            bytes >= CGROUP_UNLIMITED_MEMORY_THRESHOLD_BYTES
                || mem_total_bytes.is_some_and(|total| bytes > total.saturating_mul(16))
        }
        Some(CgroupMemoryLimitParse::Unlimited) | None => true,
    };
    let cgroup_limit_bytes = limit_bytes.filter(|_| !unlimited);
    let effective_limit_bytes = match (mem_total_bytes, cgroup_limit_bytes) {
        (Some(total), Some(limit)) => Some(total.min(limit)),
        (Some(total), None) => Some(total),
        (None, Some(limit)) => Some(limit),
        (None, None) => None,
    };

    MemoryLimitSnapshot {
        mem_total_bytes,
        cgroup_limit_bytes,
        effective_limit_bytes,
        unlimited,
        source,
    }
}

fn collect_swarm_headroom_path(env_name: &str) -> SwarmHeadroomPath {
    let Some(raw_path) = std::env::var_os(env_name) else {
        return SwarmHeadroomPath {
            env_name: env_name.to_string(),
            path: None,
            exists: false,
            under_expected_root: None,
            available_kb: None,
            ready: false,
            problem: Some("env_not_set".to_string()),
        };
    };
    let path = PathBuf::from(raw_path);
    let path_label = path.display().to_string();
    if !path.is_dir() {
        return SwarmHeadroomPath {
            env_name: env_name.to_string(),
            path: Some(path_label),
            exists: false,
            under_expected_root: Some(path_under_swarm_scratch_root(&path)),
            available_kb: None,
            ready: false,
            problem: Some("path_not_directory".to_string()),
        };
    }

    let under_expected_root = path_under_swarm_scratch_root(&path);
    let available_kb = disk_available_kb(&path).ok().flatten();
    let problem = if !under_expected_root {
        Some("outside_expected_root".to_string())
    } else if available_kb.is_none() {
        Some("available_space_unavailable".to_string())
    } else if available_kb.is_some_and(|available| available < SWARM_DISK_WARN_AVAILABLE_KB) {
        Some("low_available_space".to_string())
    } else {
        None
    };
    SwarmHeadroomPath {
        env_name: env_name.to_string(),
        path: Some(path_label),
        exists: true,
        under_expected_root: Some(under_expected_root),
        available_kb,
        ready: problem.is_none(),
        problem,
    }
}

fn collect_local_build_pressure(effective_cpu_cores: u64) -> LocalBuildPressureSnapshot {
    let process_budget = local_build_process_budget(effective_cpu_cores);
    let patterns = local_build_process_patterns();
    let (source, active_processes) =
        env_u64(SWARM_RESOURCE_PREFLIGHT_LOCAL_BUILD_PROCESS_COUNT_ENV).map_or_else(
            || ("/proc".to_string(), count_local_build_processes()),
            |count| {
                (
                    format!("env:{SWARM_RESOURCE_PREFLIGHT_LOCAL_BUILD_PROCESS_COUNT_ENV}"),
                    Some(count),
                )
            },
        );
    let status = local_build_pressure_status(active_processes, process_budget);
    LocalBuildPressureSnapshot {
        source,
        active_processes,
        process_budget,
        status,
        patterns,
    }
}

fn local_build_process_budget(effective_cpu_cores: u64) -> u64 {
    effective_cpu_cores.saturating_div(2).max(1)
}

fn local_build_pressure_status(active_processes: Option<u64>, process_budget: u64) -> String {
    let Some(active_processes) = active_processes else {
        return "unknown".to_string();
    };
    if active_processes == 0 {
        "clear".to_string()
    } else if active_processes >= process_budget.max(1) {
        "saturated".to_string()
    } else {
        "pressure".to_string()
    }
}

fn local_build_process_patterns() -> Vec<String> {
    ["cargo", "rustc", "cargo-clippy", "clippy-driver"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn count_local_build_processes() -> Option<u64> {
    let patterns = local_build_process_patterns();
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut count = 0_u64;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let cmdline_path = entry.path().join("cmdline");
        let Ok(raw) = std::fs::read(&cmdline_path) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        let lower = cmdline.to_ascii_lowercase();
        if patterns.iter().any(|pattern| lower.contains(pattern)) {
            count = count.saturating_add(1);
        }
    }
    Some(count)
}

fn read_rch_queue_posture(source_errors: &mut Vec<String>) -> RchQueuePostureSnapshot {
    if let Ok(raw) = std::env::var(SWARM_RESOURCE_PREFLIGHT_RCH_QUEUE_JSON_ENV) {
        return parse_rch_queue_posture_json(
            &raw,
            format!("env:{SWARM_RESOURCE_PREFLIGHT_RCH_QUEUE_JSON_ENV}"),
            source_errors,
        );
    }

    if let Some(path) = std::env::var_os(SWARM_RESOURCE_PREFLIGHT_RCH_QUEUE_JSON_PATH_ENV) {
        let path = PathBuf::from(path);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                return parse_rch_queue_posture_json(
                    &raw,
                    format!(
                        "env:{SWARM_RESOURCE_PREFLIGHT_RCH_QUEUE_JSON_PATH_ENV}:{}",
                        path.display()
                    ),
                    source_errors,
                );
            }
            Err(err) => source_errors.push(format!(
                "{SWARM_RESOURCE_PREFLIGHT_RCH_QUEUE_JSON_PATH_ENV} failed to read {}: {err}",
                path.display()
            )),
        }
    }

    RchQueuePostureSnapshot {
        source: "not_provided".to_string(),
        status: "not_checked".to_string(),
        recommended_action: "proceed".to_string(),
        reason: Some("queue_posture_not_provided".to_string()),
        queue_depth: None,
        active_builds: None,
        queued_builds: None,
        slots_available: None,
        slots_total: None,
        workers_healthy: None,
        workers_total: None,
    }
}

fn parse_rch_queue_posture_json(
    raw: &str,
    source: String,
    source_errors: &mut Vec<String>,
) -> RchQueuePostureSnapshot {
    let value = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => value,
        Err(err) => {
            source_errors.push(format!("invalid RCH queue JSON from {source}: {err}"));
            return RchQueuePostureSnapshot {
                source,
                status: "malformed".to_string(),
                recommended_action: "backoff".to_string(),
                reason: Some("queue_json_malformed".to_string()),
                queue_depth: None,
                active_builds: None,
                queued_builds: None,
                slots_available: None,
                slots_total: None,
                workers_healthy: None,
                workers_total: None,
            };
        }
    };
    rch_queue_posture_from_value(&value, source)
}

fn rch_queue_posture_from_value(
    value: &serde_json::Value,
    source: String,
) -> RchQueuePostureSnapshot {
    let queue_depth = json_number_by_key(value, "queue_depth")
        .or_else(|| json_array_len_by_key(value, "queued_builds").map(usize_to_u64));
    let active_builds = json_number_by_key(value, "active_builds")
        .or_else(|| json_array_len_by_key(value, "active_builds").map(usize_to_u64));
    let queued_builds = json_number_by_key(value, "queued_builds")
        .or_else(|| json_array_len_by_key(value, "queued_builds").map(usize_to_u64));
    let slots_available = json_number_by_key(value, "slots_available");
    let slots_total = json_number_by_key(value, "slots_total");
    let workers_healthy = json_number_by_key(value, "workers_healthy");
    let workers_total = json_number_by_key(value, "workers_total");
    let status = json_string_field(value, "status").unwrap_or_else(|| {
        if matches!(slots_available, Some(0)) {
            "saturated".to_string()
        } else if active_builds.unwrap_or(0) > 0 || queue_depth.unwrap_or(0) > 0 {
            "pressure".to_string()
        } else {
            "ok".to_string()
        }
    });
    let slot_pressure = json_string_field(value, "slot_pressure");
    let recommended_action = json_string_field(value, "recommended_action").unwrap_or_else(|| {
        if matches!(status.as_str(), "saturated")
            || matches!(slot_pressure.as_deref(), Some("saturated"))
            || matches!(slots_available, Some(0))
        {
            "backoff".to_string()
        } else if active_builds.unwrap_or(0) > 0 || queue_depth.unwrap_or(0) > 0 {
            "split".to_string()
        } else {
            "proceed".to_string()
        }
    });
    let reason = json_string_field(value, "reason").or(slot_pressure);
    RchQueuePostureSnapshot {
        source,
        status,
        recommended_action,
        reason,
        queue_depth,
        active_builds,
        queued_builds,
        slots_available,
        slots_total,
        workers_healthy,
        workers_total,
    }
}

fn build_swarm_resource_preflight_plan(
    sample: &HostResourceSample,
    effective_cpu_cores: u64,
    memory: &MemoryLimitSnapshot,
) -> std::result::Result<SwarmCapacityPlan, SwarmCapacityPlanError> {
    let effective_memory_mb = memory
        .effective_limit_bytes
        .map_or(1024, bytes_to_mib_ceil)
        .max(1);
    let inventory = SwarmHostInventory::new(
        effective_cpu_cores.max(1),
        effective_cpu_cores.max(1),
        effective_memory_mb,
    );
    build_swarm_doctor_capacity_plan_with_inventory(
        sample,
        inventory,
        swarm_preflight_queue_depth_budget(effective_cpu_cores),
    )
}

fn swarm_preflight_queue_depth_budget(effective_cpu_cores: u64) -> usize {
    u64_to_usize_saturating(effective_cpu_cores.saturating_mul(64)).clamp(128_usize, 4096_usize)
}

fn resource_budget_recommendation(
    plan: &SwarmCapacityPlan,
    numa: &NumaTopologySnapshot,
    local_build_pressure: &LocalBuildPressureSnapshot,
    rch_queue_posture: &RchQueuePostureSnapshot,
) -> SwarmResourceBudgetRecommendation {
    let mut recommendation = SwarmResourceBudgetRecommendation {
        agent_concurrency: plan.recommended_agent_concurrency,
        tool_concurrency: plan.recommended_tool_concurrency,
        extension_hostcall_lanes: plan.recommended_extension_hostcall_lanes,
        rch_verification_fanout: plan.recommended_rch_verification_fanout,
        max_queue_depth: plan.resource_budgets.max_queue_depth,
        max_rss_bytes: plan.resource_budgets.max_rss_bytes,
        local_build_process_budget: local_build_pressure.process_budget,
        rch_queue_depth_budget: plan.resource_budgets.max_queue_depth,
        numa_node_count: numa.nodes.len(),
        memory_pressure_threshold_ratio_label: format_ratio_label(
            plan.memory_pressure_threshold_ratio,
        ),
        plan_confidence: format!("{:?}", plan.confidence).to_ascii_lowercase(),
        explanations: base_budget_explanations(plan, numa, local_build_pressure),
    };
    apply_local_build_pressure(&mut recommendation, local_build_pressure);
    apply_rch_queue_posture(&mut recommendation, rch_queue_posture);
    recommendation
}

fn base_budget_explanations(
    plan: &SwarmCapacityPlan,
    numa: &NumaTopologySnapshot,
    local_build_pressure: &LocalBuildPressureSnapshot,
) -> Vec<SwarmBudgetExplanation> {
    vec![
        budget_explanation(
            "agent_concurrency",
            &plan.recommended_agent_concurrency,
            "bounded by effective cgroup/cpuset CPU cores and memory-derived per-agent RSS headroom",
        ),
        budget_explanation(
            "tool_concurrency",
            &plan.recommended_tool_concurrency,
            "derived from agent concurrency and queue-depth headroom so tool hostcalls cannot exhaust the scheduler queue",
        ),
        budget_explanation(
            "extension_hostcall_lanes",
            &plan.recommended_extension_hostcall_lanes,
            "capped by effective CPU cores and tool concurrency for conservative extension dispatch",
        ),
        budget_explanation(
            "rch_verification_fanout",
            &plan.recommended_rch_verification_fanout,
            "sized from effective CPU cores and later reduced by replayed RCH queue posture",
        ),
        budget_explanation(
            "local_build_process_budget",
            &local_build_pressure.process_budget,
            "maximum local cargo/rustc/clippy process pressure before fanout is reduced",
        ),
        SwarmBudgetExplanation {
            budget: "numa_node_count".to_string(),
            value: numa.nodes.len().to_string(),
            rationale: if numa.nodes.len() > 1 {
                "multi-socket hosts should spread agents and RCH validation work across NUMA nodes"
                    .to_string()
            } else {
                "single-node or unknown NUMA topology keeps conservative shared-lane budgets"
                    .to_string()
            },
        },
    ]
}

fn budget_explanation<T: ToString + ?Sized>(
    budget: &str,
    value: &T,
    rationale: &str,
) -> SwarmBudgetExplanation {
    SwarmBudgetExplanation {
        budget: budget.to_string(),
        value: value.to_string(),
        rationale: rationale.to_string(),
    }
}

fn apply_local_build_pressure(
    recommendation: &mut SwarmResourceBudgetRecommendation,
    pressure: &LocalBuildPressureSnapshot,
) {
    let Some(active_processes) = pressure.active_processes else {
        recommendation.explanations.push(budget_explanation(
            "local_build_pressure",
            "unknown",
            "local cargo/rustc process pressure could not be sampled; keep the base conservative budgets",
        ));
        return;
    };
    if active_processes == 0 {
        recommendation.explanations.push(budget_explanation(
            "local_build_pressure",
            &active_processes,
            "no local cargo/rustc/clippy pressure was observed",
        ));
        return;
    }

    let divisor = if matches!(pressure.status.as_str(), "saturated") {
        4
    } else {
        2
    };
    recommendation.agent_concurrency =
        divide_budget_floor_one(recommendation.agent_concurrency, divisor);
    recommendation.tool_concurrency =
        divide_budget_floor_one(recommendation.tool_concurrency, divisor);
    recommendation.extension_hostcall_lanes =
        divide_budget_floor_one(recommendation.extension_hostcall_lanes, divisor);
    recommendation.explanations.push(budget_explanation(
        "local_build_pressure",
        &active_processes,
        "observed local cargo/rustc/clippy pressure reduced agent, tool, and extension-hostcall budgets",
    ));
}

fn apply_rch_queue_posture(
    recommendation: &mut SwarmResourceBudgetRecommendation,
    posture: &RchQueuePostureSnapshot,
) {
    let queue_depth_over_budget = posture
        .queue_depth
        .is_some_and(|depth| depth > usize_to_u64(recommendation.rch_queue_depth_budget));
    if matches!(posture.recommended_action.as_str(), "backoff") || queue_depth_over_budget {
        recommendation.rch_verification_fanout = 0;
        recommendation.explanations.push(budget_explanation(
            "rch_verification_fanout",
            &0,
            "RCH queue posture requires backoff or exceeds the queue-depth budget",
        ));
    } else if matches!(posture.recommended_action.as_str(), "split")
        || matches!(posture.status.as_str(), "pressure")
    {
        recommendation.rch_verification_fanout =
            divide_budget_floor_one(recommendation.rch_verification_fanout, 2);
        recommendation.explanations.push(budget_explanation(
            "rch_verification_fanout",
            &recommendation.rch_verification_fanout,
            "RCH queue pressure recommends split validation instead of broad fanout",
        ));
    } else {
        recommendation.explanations.push(budget_explanation(
            "rch_verification_fanout",
            &recommendation.rch_verification_fanout,
            "RCH queue posture allows the base verification fanout",
        ));
    }
}

fn divide_budget_floor_one(value: u64, divisor: u64) -> u64 {
    value.checked_div(divisor.max(1)).unwrap_or(1).max(1)
}

fn read_first_existing_trimmed(
    env_key: &str,
    defaults: &[&str],
    source_errors: &mut Vec<String>,
) -> Option<(String, String)> {
    let override_path = std::env::var_os(env_key).map(PathBuf::from);
    let candidates = override_path.as_ref().map_or_else(
        || defaults.iter().map(PathBuf::from).collect(),
        |path| vec![path.clone()],
    );
    for path in candidates {
        if !path.is_file() {
            if override_path.is_some() {
                source_errors.push(format!(
                    "{env_key} points to missing file {}",
                    path.display()
                ));
            }
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(raw) => return Some((path.display().to_string(), raw.trim().to_string())),
            Err(err) => source_errors.push(format!("failed to read {}: {err}", path.display())),
        }
    }
    None
}

#[allow(clippy::cast_precision_loss)]
fn parse_cgroup_cpu_max(raw: &str) -> Option<CgroupCpuQuota> {
    let mut parts = raw.split_whitespace();
    let quota = parts.next()?;
    let period_us = parts.next()?.parse::<u64>().ok()?;
    if period_us == 0 {
        return None;
    }
    if quota == "max" {
        return Some(CgroupCpuQuota {
            source: None,
            quota_us: None,
            period_us: Some(period_us),
            quota_cores: None,
            unlimited: true,
        });
    }
    let quota_us = quota.parse::<u64>().ok()?;
    Some(CgroupCpuQuota {
        source: None,
        quota_us: Some(quota_us),
        period_us: Some(period_us),
        quota_cores: Some((quota_us as f64) / (period_us as f64)),
        unlimited: false,
    })
}

#[allow(clippy::cast_precision_loss)]
fn parse_cgroup_cpu_cfs(quota_raw: &str, period_raw: &str) -> Option<CgroupCpuQuota> {
    let quota_value = quota_raw.trim().parse::<i64>().ok()?;
    let period_us = period_raw.trim().parse::<u64>().ok()?;
    if period_us == 0 {
        return None;
    }
    if quota_value < 0 {
        return Some(CgroupCpuQuota {
            source: None,
            quota_us: None,
            period_us: Some(period_us),
            quota_cores: None,
            unlimited: true,
        });
    }
    let quota_us = u64::try_from(quota_value).ok()?;
    Some(CgroupCpuQuota {
        source: None,
        quota_us: Some(quota_us),
        period_us: Some(period_us),
        quota_cores: Some((quota_us as f64) / (period_us as f64)),
        unlimited: false,
    })
}

fn parse_numeric_range_list(raw: &str) -> Option<Vec<u64>> {
    let mut values = BTreeSet::new();
    for segment in raw.trim().split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            return None;
        }
        if let Some((start, end)) = segment.split_once('-') {
            let start = start.trim().parse::<u64>().ok()?;
            let end = end.trim().parse::<u64>().ok()?;
            if start > end || end.saturating_sub(start) > MAX_NUMERIC_RANGE_SPAN {
                return None;
            }
            for value in start..=end {
                values.insert(value);
            }
        } else {
            values.insert(segment.parse::<u64>().ok()?);
        }
    }
    (!values.is_empty()).then(|| values.into_iter().collect())
}

fn parse_cgroup_memory_limit_bytes(raw: &str) -> Option<CgroupMemoryLimitParse> {
    let value = raw.trim();
    if value == "max" {
        return Some(CgroupMemoryLimitParse::Unlimited);
    }
    value
        .parse::<u64>()
        .ok()
        .map(CgroupMemoryLimitParse::Limited)
}

fn parse_mem_total_bytes(raw: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        let rest = line.strip_prefix("MemTotal:")?.trim();
        let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
        kb.checked_mul(1024)
    })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn effective_swarm_cpu_cores(
    logical_cpu_cores: u64,
    quota_cores: Option<f64>,
    cpuset_count: Option<u64>,
) -> u64 {
    let quota_limit = quota_cores
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value.floor().max(1.0) as u64);
    [Some(logical_cpu_cores), cpuset_count, quota_limit]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(1)
        .max(1)
}

fn format_ratio_label(value: f64) -> String {
    format!("{value:.2}")
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse::<u64>().ok()
}

const fn bytes_to_mib_ceil(bytes: u64) -> u64 {
    bytes.saturating_add(MIB_BYTES - 1) / MIB_BYTES
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn u64_to_usize_saturating(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn summarize_beads_ledger(
    content: &str,
    now: DateTime<Utc>,
    stale_after_hours: i64,
) -> BeadsLedgerSummary {
    let mut summary = BeadsLedgerSummary::default();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        summary.total += 1;
        let Ok(issue) = serde_json::from_str::<BeadsIssueRecord>(line) else {
            summary.parse_errors += 1;
            continue;
        };

        match issue.status.as_str() {
            "open" => {
                summary.open += 1;
                summary.active += 1;
            }
            "in_progress" => {
                summary.in_progress += 1;
                summary.active += 1;
                if let Some(stale) = stale_issue_from_record(issue, now, stale_after_hours) {
                    summary.stale_in_progress.push(stale);
                }
            }
            _ => {}
        }
    }
    summary
}

fn stale_issue_from_record(
    issue: BeadsIssueRecord,
    now: DateTime<Utc>,
    stale_after_hours: i64,
) -> Option<StaleIssue> {
    let updated_at = issue.updated_at?;
    let parsed = DateTime::parse_from_rfc3339(&updated_at).ok()?;
    let age_hours = now
        .signed_duration_since(parsed.with_timezone(&Utc))
        .num_hours();
    (age_hours >= stale_after_hours).then_some(StaleIssue {
        id: issue.id,
        title: issue.title,
        updated_at,
        age_hours,
    })
}

fn format_stale_issues(issues: &[StaleIssue]) -> String {
    let mut parts: Vec<String> = issues
        .iter()
        .take(SWARM_DETAIL_LIMIT)
        .map(|issue| {
            let title = truncate_chars(&issue.title, 54);
            format!("{}: {title} ({}h old)", issue.id, issue.age_hours)
        })
        .collect();
    if issues.len() > SWARM_DETAIL_LIMIT {
        parts.push(format!("+{} more", issues.len() - SWARM_DETAIL_LIMIT));
    }
    parts.join("; ")
}

#[derive(Default)]
struct StalledReaperAudit {
    parse_errors: usize,
    in_progress_count: usize,
    recently_updated_count: usize,
    active_agent_count: usize,
    blocked_by_note_count: usize,
    unknown_assignee_count: usize,
    suggestions: Vec<serde_json::Value>,
}

impl StalledReaperAudit {
    fn candidate_count(&self) -> usize {
        self.suggestions
            .iter()
            .filter(|suggestion| {
                suggestion.get("action").and_then(serde_json::Value::as_str)
                    == Some("notify_then_reopen_or_claim")
            })
            .count()
    }

    fn detail(&self, candidate_count: usize) -> String {
        format!(
            "in_progress={}, candidates={candidate_count}, active_agent={}, recently_updated={}, blocked_by_note={}, unknown_assignee={}",
            self.in_progress_count,
            self.active_agent_count,
            self.recently_updated_count,
            self.blocked_by_note_count,
            self.unknown_assignee_count
        )
    }
}

fn classify_stalled_bead_reaper(
    content: &str,
    agent_roster: Option<&serde_json::Value>,
    agent_roster_error: Option<&str>,
    now: DateTime<Utc>,
    stale_after_hours: i64,
) -> Finding {
    let activities =
        agent_roster.map_or_else(HashMap::new, |value| agent_mail_activity_index(value, now));
    let audit = collect_stalled_reaper_audit(content, &activities, now, stale_after_hours);
    let candidate_count = audit.candidate_count();
    let detail = audit.detail(candidate_count);
    let parse_errors = audit.parse_errors;
    let in_progress_count = audit.in_progress_count;
    let recently_updated_count = audit.recently_updated_count;
    let active_agent_count = audit.active_agent_count;
    let blocked_by_note_count = audit.blocked_by_note_count;
    let unknown_assignee_count = audit.unknown_assignee_count;
    let suggestions = audit.suggestions;

    let data = serde_json::json!({
        "schema": SWARM_DOCTOR_STALLED_REAPER_SCHEMA,
        "mode": "audit_only",
        "mutation_performed": false,
        "requires_explicit_operator_command": true,
        "stale_after_hours": stale_after_hours,
        "agent_activity_source": if agent_roster.is_some() { "agent_mail" } else { "unavailable" },
        "agent_activity_error": agent_roster_error,
        "parse_errors": parse_errors,
        "in_progress_count": in_progress_count,
        "candidate_count": candidate_count,
        "active_agent_count": active_agent_count,
        "recently_updated_count": recently_updated_count,
        "blocked_by_note_count": blocked_by_note_count,
        "unknown_assignee_count": unknown_assignee_count,
        "suggestions": suggestions,
    });

    if parse_errors > 0 {
        return Finding::warn(CheckCategory::Swarm, "Stalled bead reaper audit degraded")
            .with_detail(format!("{detail}; parse_errors={parse_errors}"))
            .with_remediation("Run `br doctor --json` before reopening any in_progress bead")
            .with_data(data);
    }
    if candidate_count > 0 {
        return Finding::warn(CheckCategory::Swarm, "Stalled bead reaper found reopen candidates")
            .with_detail(detail)
            .with_remediation(
                "Review notification drafts; only run suggested `br update` commands after confirming ownership is stale",
            )
            .with_data(data);
    }
    Finding::pass(
        CheckCategory::Swarm,
        "Stalled bead reaper found no reopen candidates",
    )
    .with_detail(detail)
    .with_data(data)
}

fn collect_stalled_reaper_audit(
    content: &str,
    activities: &HashMap<String, AgentMailActivity>,
    now: DateTime<Utc>,
    stale_after_hours: i64,
) -> StalledReaperAudit {
    let mut audit = StalledReaperAudit::default();

    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(issue) = serde_json::from_str::<BeadsIssueRecord>(line) else {
            audit.parse_errors += 1;
            continue;
        };
        if !matches!(issue.status.as_str(), "in_progress") {
            continue;
        }
        audit.in_progress_count += 1;

        let updated_at = issue.updated_at.as_deref().unwrap_or_default();
        let Some(br_age_hours) = age_hours_since(updated_at, now) else {
            audit.suggestions.push(stalled_reaper_suggestion(
                &issue,
                None,
                None,
                "missing_or_invalid_br_updated_at",
                stale_after_hours,
            ));
            continue;
        };
        if br_age_hours < stale_after_hours {
            audit.recently_updated_count += 1;
            continue;
        }

        if let Some(note_excerpt) = blocked_note_excerpt(&issue) {
            audit.blocked_by_note_count += 1;
            audit.suggestions.push(stalled_reaper_watch_item(
                &issue,
                br_age_hours,
                "blocked_by_note",
                &note_excerpt,
            ));
            continue;
        }

        let assignee = issue_assignee(&issue);
        let activity = assignee.and_then(|name| activities.get(name));
        if activity.is_some_and(|activity| activity.age_hours < stale_after_hours) {
            audit.active_agent_count += 1;
            continue;
        }
        if assignee.is_none() || activity.is_none() {
            audit.unknown_assignee_count += 1;
        }
        audit.suggestions.push(stalled_reaper_suggestion(
            &issue,
            Some(br_age_hours),
            activity,
            "stale_in_progress",
            stale_after_hours,
        ));
    }

    audit
}

fn stalled_reaper_suggestion(
    issue: &BeadsIssueRecord,
    br_age_hours: Option<i64>,
    activity: Option<&AgentMailActivity>,
    reason: &str,
    stale_after_hours: i64,
) -> serde_json::Value {
    let assignee = issue_assignee(issue).map(str::to_string);
    let to = assignee.iter().cloned().collect::<Vec<_>>();
    serde_json::json!({
        "issue_id": issue.id,
        "title": issue.title,
        "assignee": assignee,
        "reason": reason,
        "action": "notify_then_reopen_or_claim",
        "br_updated_at": issue.updated_at.as_deref(),
        "br_age_hours": br_age_hours,
        "agent_mail_last_active_ts": activity.map(|activity| activity.last_active_ts.as_str()),
        "agent_mail_age_hours": activity.map(|activity| activity.age_hours),
        "stale_after_hours": stale_after_hours,
        "suggested_commands": [
            format!("br update {} --status=open", issue.id),
            format!("br update {} --status=in_progress --assignee <agent-name>", issue.id),
        ],
        "notification_draft": {
            "to": to,
            "thread_id": issue.id,
            "subject": format!("[{}] Stalled in-progress audit", issue.id),
            "ack_required": true,
            "body_md": stalled_reaper_draft_body(issue, br_age_hours, activity, stale_after_hours),
        },
    })
}

fn stalled_reaper_watch_item(
    issue: &BeadsIssueRecord,
    br_age_hours: i64,
    reason: &str,
    note_excerpt: &str,
) -> serde_json::Value {
    serde_json::json!({
        "issue_id": issue.id,
        "title": issue.title,
        "assignee": issue_assignee(issue),
        "reason": reason,
        "action": "keep_in_progress_and_review_blocker_note",
        "br_updated_at": issue.updated_at.as_deref(),
        "br_age_hours": br_age_hours,
        "note_excerpt": note_excerpt,
        "suggested_commands": [
            format!("br show {}", issue.id),
        ],
    })
}

fn stalled_reaper_draft_body(
    issue: &BeadsIssueRecord,
    br_age_hours: Option<i64>,
    activity: Option<&AgentMailActivity>,
    stale_after_hours: i64,
) -> String {
    let assignee = issue_assignee(issue).unwrap_or("<unassigned>");
    let br_age = br_age_hours.map_or_else(|| "unknown".to_string(), |age| format!("{age}h"));
    let agent_activity = activity.map_or_else(
        || "no recent Agent Mail activity found".to_string(),
        |activity| format!("last Agent Mail activity {}h ago", activity.age_hours),
    );
    format!(
        "`{}` is still in_progress for `{assignee}`. Beads last update age: {br_age}; {agent_activity}. If this is still owned, please reply or refresh the bead. Otherwise I plan to reopen it after the {stale_after_hours}h stale threshold review.",
        issue.id
    )
}

fn issue_assignee(issue: &BeadsIssueRecord) -> Option<&str> {
    issue
        .assignee
        .as_deref()
        .or(issue.owner.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn age_hours_since(timestamp: &str, now: DateTime<Utc>) -> Option<i64> {
    let parsed = DateTime::parse_from_rfc3339(timestamp).ok()?;
    Some(
        now.signed_duration_since(parsed.with_timezone(&Utc))
            .num_hours(),
    )
}

fn blocked_note_excerpt(issue: &BeadsIssueRecord) -> Option<String> {
    let text = issue.notes.as_deref().unwrap_or(&issue.description).trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    [
        "blocked",
        "blocker",
        "waiting on",
        "do not reset",
        "do not reopen",
        "do not reclaim",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    .then(|| truncate_chars(text, 180))
}

fn agent_mail_activity_index(
    value: &serde_json::Value,
    now: DateTime<Utc>,
) -> HashMap<String, AgentMailActivity> {
    let mut activities = HashMap::new();
    collect_agent_mail_activity_rows(value, now, &mut activities);
    activities
}

fn collect_agent_mail_activity_rows(
    value: &serde_json::Value,
    now: DateTime<Utc>,
    activities: &mut HashMap<String, AgentMailActivity>,
) {
    match value {
        serde_json::Value::Object(map) => {
            let name = map
                .get("name")
                .or_else(|| map.get("agent"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let last_active = [
                "last_active_ts",
                "last_active",
                "last_seen_ts",
                "last_seen",
                "updated_at",
            ]
            .iter()
            .find_map(|key| map.get(*key).and_then(serde_json::Value::as_str));
            if let (Some(name), Some(last_active)) = (name, last_active)
                && let Some(age_hours) = age_hours_since(last_active, now)
            {
                activities.insert(
                    name.to_string(),
                    AgentMailActivity {
                        last_active_ts: last_active.to_string(),
                        age_hours,
                    },
                );
                return;
            }
            for child in map.values() {
                collect_agent_mail_activity_rows(child, now, activities);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                collect_agent_mail_activity_rows(child, now, activities);
            }
        }
        _ => {}
    }
}

fn check_swarm_conflict_predictor(cwd: &Path, findings: &mut Vec<Finding>) {
    let ledger_path = cwd.join(".beads/issues.jsonl");
    let (beads_content, beads_error) = match std::fs::read_to_string(&ledger_path) {
        Ok(content) => (Some(content), None),
        Err(err) => (
            None,
            Some(format!(
                "beads ledger unavailable at {}: {err}",
                ledger_path.display()
            )),
        ),
    };
    let project = cwd.display().to_string();
    let (reservations, reservations_error) = read_agent_mail_reservations(cwd, &project);
    let (git_porcelain, git_error) = read_git_porcelain(cwd);

    let inputs = SwarmConflictPredictorInputs {
        beads_content: beads_content.as_deref(),
        beads_error,
        reservations: reservations.as_ref(),
        reservations_error,
        git_porcelain: git_porcelain.as_deref(),
        git_error,
    };
    let plan = build_swarm_conflict_prediction_plan(inputs, Utc::now());
    findings.push(classify_swarm_conflict_prediction_plan(&plan));
}

fn read_agent_mail_reservations(
    cwd: &Path,
    project: &str,
) -> (Option<serde_json::Value>, Option<String>) {
    if which_tool("am").is_none() {
        return (
            None,
            Some("Agent Mail CLI not found for conflict predictor reservations".to_string()),
        );
    }
    let args = [
        "robot",
        "reservations",
        "--format",
        "json",
        "--project",
        project,
        "--all",
        "--expiring",
        "30",
    ];
    match run_probe_json(
        SwarmProbeCommand::Am,
        &args,
        Some(cwd),
        "am robot reservations",
    ) {
        Ok(value) => (Some(value), None),
        Err(err) => (None, Some(err)),
    }
}

fn read_git_porcelain(cwd: &Path) -> (Option<String>, Option<String>) {
    if which_tool("git").is_none() {
        return (
            None,
            Some("git CLI not found for conflict predictor dirty-state probe".to_string()),
        );
    }
    match run_tool_with_timeout(
        SwarmProbeCommand::Git,
        &["status", "--porcelain=v1", "--untracked-files=all"],
        Some(cwd),
        SWARM_PROBE_TIMEOUT,
    ) {
        Ok(outcome) if outcome.success => (Some(outcome.stdout), None),
        Ok(outcome) => (
            None,
            Some(format!(
                "git status failed for conflict predictor: {}",
                command_failure_detail(&outcome)
            )),
        ),
        Err(err) => (
            None,
            Some(format!(
                "git status failed to start for conflict predictor: {err}"
            )),
        ),
    }
}

#[derive(Debug, Default)]
struct SwarmConflictPredictorInputs<'a> {
    beads_content: Option<&'a str>,
    beads_error: Option<String>,
    reservations: Option<&'a serde_json::Value>,
    reservations_error: Option<String>,
    git_porcelain: Option<&'a str>,
    git_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SwarmConflictPredictionPlan {
    active_bead_count: usize,
    parse_errors: usize,
    reservations_unavailable: bool,
    git_unavailable: bool,
    dirty_paths: Vec<String>,
    source_errors: Vec<String>,
    predictions: Vec<SwarmConflictPrediction>,
    safe_fallback_lanes: Vec<SwarmConflictFallbackLane>,
}

impl SwarmConflictPredictionPlan {
    fn high_risk_count(&self) -> usize {
        self.predictions
            .iter()
            .filter(|prediction| prediction.risk_level() == "high")
            .count()
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": SWARM_DOCTOR_CONFLICT_PREDICTOR_SCHEMA,
            "mode": "audit_only",
            "mutation_performed": false,
            "stale_after_hours": SWARM_STALE_IN_PROGRESS_HOURS,
            "input_sources": {
                "beads": if self.active_bead_count > 0 || self.parse_errors > 0 { "available" } else { "unavailable_or_empty" },
                "agent_mail_reservations": if self.reservations_unavailable { "unavailable" } else { "available" },
                "git_status": if self.git_unavailable { "unavailable" } else { "available" }
            },
            "source_errors": self.source_errors,
            "active_bead_count": self.active_bead_count,
            "parse_errors": self.parse_errors,
            "dirty_paths": self.dirty_paths,
            "high_risk_count": self.high_risk_count(),
            "predictions": self
                .predictions
                .iter()
                .map(SwarmConflictPrediction::to_json)
                .collect::<Vec<_>>(),
            "safe_fallback_lanes": self
                .safe_fallback_lanes
                .iter()
                .map(SwarmConflictFallbackLane::to_json)
                .collect::<Vec<_>>(),
            "reservation_recommendations": self.reservation_recommendations_json()
        })
    }

    fn reservation_recommendations_json(&self) -> serde_json::Value {
        let degraded_caveats = self.reservation_recommendation_caveats();
        let avoid = self
            .predictions
            .iter()
            .filter(|prediction| prediction.risk_level() != "low")
            .map(SwarmConflictPrediction::to_reservation_recommendation_json)
            .collect::<Vec<_>>();
        let observe = self
            .predictions
            .iter()
            .filter(|prediction| prediction.risk_level() == "low")
            .map(SwarmConflictPrediction::to_reservation_recommendation_json)
            .collect::<Vec<_>>();

        serde_json::json!({
            "schema": SWARM_DOCTOR_RESERVATION_RECOMMENDATIONS_SCHEMA,
            "diagnostic_only": true,
            "mutation_performed": false,
            "mail_available": !self.reservations_unavailable,
            "degraded_caveats": degraded_caveats,
            "avoid": avoid,
            "observe": observe,
            "safe_fallback_lanes": self
                .safe_fallback_lanes
                .iter()
                .map(SwarmConflictFallbackLane::to_json)
                .collect::<Vec<_>>()
        })
    }

    fn reservation_recommendation_caveats(&self) -> Vec<&'static str> {
        let mut caveats = Vec::new();
        if self.reservations_unavailable {
            caveats.push("agent_mail_reservations_unavailable");
        }
        if self.git_unavailable {
            caveats.push("git_status_unavailable");
        }
        if self.parse_errors > 0 {
            caveats.push("beads_parse_errors");
        }
        caveats
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SwarmConflictPrediction {
    path_pattern: String,
    score: u32,
    reasons: BTreeSet<String>,
    beads: BTreeSet<String>,
    agents: BTreeSet<String>,
}

impl SwarmConflictPrediction {
    const fn risk_level(&self) -> &'static str {
        if self.score >= 70 {
            "high"
        } else if self.score >= 35 {
            "medium"
        } else {
            "low"
        }
    }

    fn recommended_action(&self) -> &'static str {
        if self.path_pattern == ".beads/issues.jsonl" {
            "Serialize br close/sync windows; rerun `br sync --flush-only` before committing"
        } else if self.has_reason("overlapping_active_reservations")
            || self.has_reason("active_exclusive_reservation")
        {
            "Contact listed reservation holders or pick a non-overlapping bead before editing"
        } else if self.has_reason("dirty_path")
            || self.has_reason("dirty_path_overlaps_predicted_surface")
        {
            "Inspect `git status --short` and avoid overwriting dirty worktree paths"
        } else {
            "Reserve this path explicitly before editing or choose a lower-risk fallback lane"
        }
    }

    fn has_reason(&self, reason: &str) -> bool {
        self.reasons.iter().any(|value| value == reason)
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path_pattern": self.path_pattern,
            "score": self.score,
            "risk_level": self.risk_level(),
            "reasons": self.reasons.iter().cloned().collect::<Vec<_>>(),
            "beads": self.beads.iter().cloned().collect::<Vec<_>>(),
            "agents": self.agents.iter().cloned().collect::<Vec<_>>(),
            "suggested_reservation": self.path_pattern,
            "recommended_action": self.recommended_action()
        })
    }

    fn to_reservation_recommendation_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path_pattern": self.path_pattern,
            "suggested_reservation": self.path_pattern,
            "risk_level": self.risk_level(),
            "score": self.score,
            "bead_families": self.bead_families(),
            "conflict_reasons": self.reasons.iter().cloned().collect::<Vec<_>>(),
            "beads": self.beads.iter().cloned().collect::<Vec<_>>(),
            "agents": self.agents.iter().cloned().collect::<Vec<_>>(),
            "recommended_action": self.recommended_action()
        })
    }

    fn bead_families(&self) -> Vec<&'static str> {
        let mut families = BTreeSet::new();
        for reason in &self.reasons {
            if let Some(family) = conflict_reason_family(reason) {
                families.insert(family);
            }
        }
        families.into_iter().collect()
    }
}

fn conflict_reason_family(reason: &str) -> Option<&'static str> {
    match reason {
        "agent_mail_reservations_unavailable"
        | "active_exclusive_reservation"
        | "active_shared_reservation"
        | "recent_inactive_reservation"
        | "reservation_expiring_soon"
        | "stale_reservation_holder"
        | "overlapping_active_reservations" => Some("agent_mail_reservations"),
        "bead_in_progress"
        | "open_active_bead"
        | "stale_in_progress_bead"
        | "in_progress_bead_closeout_window"
        | "dirty_beads_closeout_window"
        | "beads_ledger_coordination_work" => Some("beads"),
        "dirty_path" | "dirty_path_overlaps_predicted_surface" => Some("worktree"),
        "swarm_doctor_coordination_work" => Some("swarm_doctor"),
        "session_persistence_work" => Some("sessions"),
        "built_in_tool_work" => Some("tools"),
        "provider_work" => Some("providers"),
        "interactive_tui_work" | "rpc_surface_work" => Some("interactive_rpc"),
        "extension_runtime_work" => Some("extensions"),
        "evidence_or_certification_work" => Some("evidence"),
        "test_harness_work" => Some("tests"),
        "unmapped_active_bead" => Some("unmapped"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmConflictFallbackLane {
    path_pattern: String,
    reason: String,
    suggested_reservation: String,
}

impl SwarmConflictFallbackLane {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "path_pattern": self.path_pattern,
            "reason": self.reason,
            "suggested_reservation": self.suggested_reservation
        })
    }
}

#[derive(Debug, Clone, Default)]
struct SwarmConflictAccumulator {
    path_pattern: String,
    score: u32,
    reasons: BTreeSet<String>,
    beads: BTreeSet<String>,
    agents: BTreeSet<String>,
}

impl SwarmConflictAccumulator {
    fn into_prediction(self) -> SwarmConflictPrediction {
        SwarmConflictPrediction {
            path_pattern: self.path_pattern,
            score: self.score,
            reasons: self.reasons,
            beads: self.beads,
            agents: self.agents,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmConflictSurface {
    path_pattern: String,
    score: u32,
    reason: &'static str,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SwarmConflictBeadsSummary {
    active_beads: usize,
    parse_errors: usize,
}

fn build_swarm_conflict_prediction_plan(
    inputs: SwarmConflictPredictorInputs<'_>,
    now: DateTime<Utc>,
) -> SwarmConflictPredictionPlan {
    let reservations_unavailable = inputs.reservations.is_none();
    let git_unavailable = inputs.git_porcelain.is_none();
    let mut source_errors = Vec::new();
    extend_source_errors(
        &mut source_errors,
        [
            inputs.beads_error,
            inputs.reservations_error,
            inputs.git_error,
        ],
    );

    let mut accumulators: BTreeMap<String, SwarmConflictAccumulator> = BTreeMap::new();
    let beads = inputs
        .beads_content
        .map_or_else(SwarmConflictBeadsSummary::default, |content| {
            collect_swarm_conflict_bead_signals(content, now, &mut accumulators)
        });

    if let Some(reservations) = inputs.reservations {
        collect_swarm_conflict_reservation_signals(reservations, now, &mut accumulators);
    } else if beads.active_beads > 0 {
        record_conflict_signal(
            &mut accumulators,
            ".beads/issues.jsonl",
            24,
            "agent_mail_reservations_unavailable",
            None,
            None,
        );
    }

    let dirty_paths = inputs
        .git_porcelain
        .map(git_porcelain_paths)
        .unwrap_or_default();
    for dirty_path in &dirty_paths {
        collect_swarm_conflict_dirty_path_signal(dirty_path, &mut accumulators);
    }

    let mut predictions = accumulators
        .into_values()
        .map(SwarmConflictAccumulator::into_prediction)
        .collect::<Vec<_>>();
    predictions.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path_pattern.cmp(&right.path_pattern))
    });
    let safe_fallback_lanes = select_swarm_conflict_fallback_lanes(&predictions);

    SwarmConflictPredictionPlan {
        active_bead_count: beads.active_beads,
        parse_errors: beads.parse_errors,
        reservations_unavailable,
        git_unavailable,
        dirty_paths,
        source_errors,
        predictions,
        safe_fallback_lanes,
    }
}

fn collect_swarm_conflict_bead_signals(
    content: &str,
    now: DateTime<Utc>,
    accumulators: &mut BTreeMap<String, SwarmConflictAccumulator>,
) -> SwarmConflictBeadsSummary {
    let mut summary = SwarmConflictBeadsSummary::default();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(issue) = serde_json::from_str::<BeadsIssueRecord>(line) else {
            summary.parse_errors += 1;
            continue;
        };
        if !matches!(issue.status.as_str(), "open" | "in_progress") {
            continue;
        }
        summary.active_beads += 1;
        collect_swarm_conflict_issue_signals(&issue, now, accumulators);
    }
    summary
}

fn collect_swarm_conflict_issue_signals(
    issue: &BeadsIssueRecord,
    now: DateTime<Utc>,
    accumulators: &mut BTreeMap<String, SwarmConflictAccumulator>,
) {
    let in_progress = issue.status == "in_progress";
    let status_score = if in_progress { 30 } else { 8 };
    let status_reason = if in_progress {
        "bead_in_progress"
    } else {
        "open_active_bead"
    };
    let stale = in_progress
        && issue
            .updated_at
            .as_deref()
            .and_then(|updated_at| age_hours_since(updated_at, now))
            .is_some_and(|age| age >= SWARM_STALE_IN_PROGRESS_HOURS);

    for surface in issue_conflict_surfaces(issue) {
        record_conflict_signal(
            accumulators,
            &surface.path_pattern,
            surface.score + status_score,
            surface.reason,
            Some(issue.id.as_str()),
            issue_assignee(issue),
        );
        record_conflict_signal(
            accumulators,
            &surface.path_pattern,
            0,
            status_reason,
            Some(issue.id.as_str()),
            issue_assignee(issue),
        );
        if stale {
            record_conflict_signal(
                accumulators,
                &surface.path_pattern,
                22,
                "stale_in_progress_bead",
                Some(issue.id.as_str()),
                issue_assignee(issue),
            );
        }
    }

    if in_progress {
        record_conflict_signal(
            accumulators,
            ".beads/issues.jsonl",
            if stale { 50 } else { 32 },
            "in_progress_bead_closeout_window",
            Some(issue.id.as_str()),
            issue_assignee(issue),
        );
    }
}

fn issue_conflict_surfaces(issue: &BeadsIssueRecord) -> Vec<SwarmConflictSurface> {
    let text = issue_conflict_text(issue);
    let mut surfaces = Vec::new();
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(
            &text,
            &[
                "doctor",
                "swarm",
                "coordination",
                "agent mail",
                "agent-mail",
                "reservation",
                "conflict",
                "rch",
            ],
        ),
        "src/doctor.rs",
        42,
        "swarm_doctor_coordination_work",
    );
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(&text, &["beads", "br ", "bv ", "closeout", "issue ledger"]),
        ".beads/issues.jsonl",
        34,
        "beads_ledger_coordination_work",
    );
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(
            &text,
            &[
                "session",
                "sqlite",
                "jsonl",
                "branch",
                "persistence",
                "replay",
            ],
        ),
        "src/session*.rs",
        36,
        "session_persistence_work",
    );
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(
            &text,
            &["tool", "bash", "read", "edit", "grep", "find", "hashline"],
        ),
        "src/tools.rs",
        34,
        "built_in_tool_work",
    );
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(
            &text,
            &[
                "provider",
                "openai",
                "anthropic",
                "gemini",
                "cohere",
                "azure",
                "bedrock",
                "vertex",
                "github",
                "gitlab",
                "responses",
            ],
        ),
        "src/providers/**",
        34,
        "provider_work",
    );
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(
            &text,
            &["interactive", "tui", "rpc", "tail latency", "latency"],
        ),
        "src/interactive.rs",
        30,
        "interactive_tui_work",
    );
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(&text, &["interactive", "rpc", "stdio", "server mode"]),
        "src/rpc.rs",
        30,
        "rpc_surface_work",
    );
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(
            &text,
            &["extension", "quickjs", "hostcall", "capability", "policy"],
        ),
        "src/extensions*.rs",
        34,
        "extension_runtime_work",
    );
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(
            &text,
            &[
                "dropin",
                "drop-in",
                "parity",
                "evidence",
                "certification",
                "ledger",
            ],
        ),
        "docs/evidence/**",
        28,
        "evidence_or_certification_work",
    );
    push_conflict_surface_if(
        &mut surfaces,
        contains_any(&text, &["test", "tests", "harness", "conformance", "chaos"]),
        "tests/**",
        24,
        "test_harness_work",
    );

    if surfaces.is_empty() {
        surfaces.push(SwarmConflictSurface {
            path_pattern: "src/**".to_string(),
            score: 8,
            reason: "unmapped_active_bead",
        });
    }
    surfaces
}

fn push_conflict_surface_if(
    surfaces: &mut Vec<SwarmConflictSurface>,
    condition: bool,
    path_pattern: &str,
    score: u32,
    reason: &'static str,
) {
    if condition {
        surfaces.push(SwarmConflictSurface {
            path_pattern: path_pattern.to_string(),
            score,
            reason,
        });
    }
}

fn issue_conflict_text(issue: &BeadsIssueRecord) -> String {
    format!(
        "{} {} {} {} {} {}",
        issue.id,
        issue.title,
        issue.description,
        issue.notes.as_deref().unwrap_or_default(),
        issue.labels.join(" "),
        issue.issue_type.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase()
}

fn collect_swarm_conflict_reservation_signals(
    value: &serde_json::Value,
    now: DateTime<Utc>,
    accumulators: &mut BTreeMap<String, SwarmConflictAccumulator>,
) {
    let rows = agent_mail_reservation_rows(value, now);
    for row in &rows {
        if !(row.is_active() || row.is_recent()) {
            continue;
        }
        let score = match (row.is_active(), row.is_exclusive()) {
            (true, true) => 52,
            (true, false) => 28,
            (false, _) => 14,
        };
        let reason = match (row.is_active(), row.is_exclusive()) {
            (true, true) => "active_exclusive_reservation",
            (true, false) => "active_shared_reservation",
            (false, _) => "recent_inactive_reservation",
        };
        record_conflict_signal(
            accumulators,
            &row.path_pattern,
            score,
            reason,
            Some(row.bead.as_str()),
            Some(row.agent.as_str()),
        );
        if row.expiring_soon {
            record_conflict_signal(
                accumulators,
                &row.path_pattern,
                10,
                "reservation_expiring_soon",
                Some(row.bead.as_str()),
                Some(row.agent.as_str()),
            );
        }
        if reservation_stale_reason(row).is_some() {
            record_conflict_signal(
                accumulators,
                &row.path_pattern,
                18,
                "stale_reservation_holder",
                Some(row.bead.as_str()),
                Some(row.agent.as_str()),
            );
        }
    }

    let active_rows = rows
        .iter()
        .filter(|row| row.is_active())
        .collect::<Vec<_>>();
    for (index, left) in active_rows.iter().enumerate() {
        for right in active_rows.iter().skip(index + 1) {
            if reservation_rows_conflict(left, right) {
                for row in [*left, *right] {
                    record_conflict_signal(
                        accumulators,
                        &row.path_pattern,
                        38,
                        "overlapping_active_reservations",
                        Some(row.bead.as_str()),
                        Some(row.agent.as_str()),
                    );
                }
            }
        }
    }
}

fn collect_swarm_conflict_dirty_path_signal(
    dirty_path: &str,
    accumulators: &mut BTreeMap<String, SwarmConflictAccumulator>,
) {
    let score = if dirty_path == ".beads/issues.jsonl" {
        74
    } else {
        36
    };
    let reason = if dirty_path == ".beads/issues.jsonl" {
        "dirty_beads_closeout_window"
    } else {
        "dirty_path"
    };
    record_conflict_signal(accumulators, dirty_path, score, reason, None, None);

    let keys = accumulators.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        if key != dirty_path && reservation_patterns_overlap(&key, dirty_path) {
            record_conflict_signal(
                accumulators,
                &key,
                18,
                "dirty_path_overlaps_predicted_surface",
                None,
                None,
            );
        }
    }
}

fn record_conflict_signal(
    accumulators: &mut BTreeMap<String, SwarmConflictAccumulator>,
    path_pattern: &str,
    score: u32,
    reason: &'static str,
    bead: Option<&str>,
    agent: Option<&str>,
) {
    let accumulator = accumulators
        .entry(path_pattern.to_string())
        .or_insert_with(|| SwarmConflictAccumulator {
            path_pattern: path_pattern.to_string(),
            ..SwarmConflictAccumulator::default()
        });
    accumulator.score = accumulator.score.saturating_add(score);
    accumulator.reasons.insert(reason.to_string());
    if let Some(bead) = bead.map(str::trim).filter(|value| !value.is_empty()) {
        accumulator.beads.insert(bead.to_string());
    }
    if let Some(agent) = agent.map(str::trim).filter(|value| !value.is_empty()) {
        accumulator.agents.insert(agent.to_string());
    }
}

fn git_porcelain_paths(output: &str) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let raw_path = line.get(3..).unwrap_or(line).trim();
        let path = raw_path
            .rsplit(" -> ")
            .next()
            .unwrap_or(raw_path)
            .trim_matches('"')
            .trim();
        if !path.is_empty() {
            paths.insert(path.to_string());
        }
    }
    paths.into_iter().collect()
}

fn select_swarm_conflict_fallback_lanes(
    predictions: &[SwarmConflictPrediction],
) -> Vec<SwarmConflictFallbackLane> {
    let high_risk = predictions
        .iter()
        .filter(|prediction| prediction.risk_level() == "high")
        .collect::<Vec<_>>();
    let candidates = [
        (
            "tests/**",
            "Test-only coverage is usually easier to reserve independently",
        ),
        (
            "docs/evidence/**",
            "Evidence refresh can proceed when source modules are reserved",
        ),
        (
            "src/providers/**",
            "Provider lane is separate from swarm coordination paths",
        ),
        (
            "src/tools.rs",
            "Built-in tool work is a narrow single-file reservation",
        ),
        (
            "src/extensions*.rs",
            "Extension runtime work is separable from provider and doctor lanes",
        ),
    ];

    candidates
        .into_iter()
        .filter(|(path_pattern, _)| {
            high_risk.iter().all(|prediction| {
                !reservation_patterns_overlap(&prediction.path_pattern, path_pattern)
            })
        })
        .take(3)
        .map(|(path_pattern, reason)| SwarmConflictFallbackLane {
            path_pattern: path_pattern.to_string(),
            reason: reason.to_string(),
            suggested_reservation: path_pattern.to_string(),
        })
        .collect()
}

fn classify_swarm_conflict_prediction_plan(plan: &SwarmConflictPredictionPlan) -> Finding {
    let high_risk = plan.high_risk_count();
    let detail = format!(
        "active_beads={}, predictions={}, high_risk={}, dirty_paths={}, source_errors={}, top={}",
        plan.active_bead_count,
        plan.predictions.len(),
        high_risk,
        plan.dirty_paths.len(),
        plan.source_errors.len(),
        plan.predictions
            .first()
            .map_or("none", |prediction| prediction.path_pattern.as_str())
    );
    let data = plan.to_json();

    if plan.parse_errors > 0 {
        return Finding::warn(
            CheckCategory::Swarm,
            "Cross-agent conflict predictor degraded",
        )
        .with_detail(format!("{detail}; parse_errors={}", plan.parse_errors))
        .with_remediation("Run `br doctor --json` before trusting conflict predictions")
        .with_data(data);
    }
    if high_risk > 0 {
        return Finding::warn(
            CheckCategory::Swarm,
            "Cross-agent conflict predictor found likely collisions",
        )
        .with_detail(detail)
        .with_remediation(
            "Avoid high-risk paths, contact listed agents, or claim one of the safe fallback lanes",
        )
        .with_data(data);
    }
    if !plan.source_errors.is_empty() {
        return Finding::warn(
            CheckCategory::Swarm,
            "Cross-agent conflict predictor running with degraded inputs",
        )
        .with_detail(detail)
        .with_remediation(
            "Use Beads status/comments as the soft lock until Agent Mail and git inputs are readable",
        )
        .with_data(data);
    }
    Finding::pass(
        CheckCategory::Swarm,
        "Cross-agent conflict predictor found no likely collisions",
    )
    .with_detail(detail)
    .with_data(data)
}

fn check_swarm_next_action(cwd: &Path, findings: &mut Vec<Finding>) {
    let mut snapshot = SwarmNextActionSnapshot {
        agent_name: first_non_empty_env(&["AGENT_MAIL_AGENT", "AGENT_NAME"]),
        ..SwarmNextActionSnapshot::default()
    };

    apply_next_action_beads_snapshot(cwd, &mut snapshot);
    apply_next_action_br_ready_snapshot(cwd, &mut snapshot);
    apply_next_action_bv_snapshot(cwd, &mut snapshot);
    apply_next_action_agent_mail_snapshot(cwd, &mut snapshot);

    findings.push(classify_swarm_next_action(&snapshot));
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SwarmNextActionSnapshot {
    agent_name: Option<String>,
    mail_unread: u64,
    mail_urgent: u64,
    mail_ack_required: u64,
    reservations_active: usize,
    reservations_own_active: usize,
    beads_open: usize,
    beads_in_progress: usize,
    beads_parse_errors: usize,
    stale_in_progress: usize,
    current_in_progress: usize,
    open_epic_count: usize,
    ready_ids: Vec<String>,
    bv_highest_impact: Option<String>,
    bv_recommendation_count: usize,
    source_errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwarmNextAction {
    AckMail,
    ImplementCurrent,
    UnblockStalledBead,
    ClaimReadyBead,
    CreateFollowupBeads,
    NoWork,
}

impl SwarmNextAction {
    const fn label(self) -> &'static str {
        match self {
            Self::AckMail => "ack_mail",
            Self::ImplementCurrent => "implement_current",
            Self::UnblockStalledBead => "unblock_stalled_bead",
            Self::ClaimReadyBead => "claim_ready_bead",
            Self::CreateFollowupBeads => "create_followup_beads",
            Self::NoWork => "no_work",
        }
    }

    const fn remediation(self) -> &'static str {
        match self {
            Self::AckMail => {
                "Read and acknowledge urgent or ack-required Agent Mail before taking more work"
            }
            Self::ImplementCurrent => {
                "Continue the current claimed work; keep reservations fresh and avoid unrelated coordination churn"
            }
            Self::UnblockStalledBead => {
                "Inspect stalled in-progress beads and contact owners before reopening or reclaiming any work"
            }
            Self::ClaimReadyBead => {
                "Claim the highest-impact ready bead, reserve the edit surface, and announce the start in Agent Mail"
            }
            Self::CreateFollowupBeads => {
                "Break the open roadmap or epic into concrete ready beads before waiting for more coordination"
            }
            Self::NoWork => {
                "No actionable swarm work is visible; rerun triage after syncing Beads and Agent Mail"
            }
        }
    }

    const fn severity(self) -> Severity {
        match self {
            Self::AckMail | Self::UnblockStalledBead | Self::CreateFollowupBeads => Severity::Warn,
            Self::NoWork => Severity::Info,
            Self::ImplementCurrent | Self::ClaimReadyBead => Severity::Pass,
        }
    }
}

fn classify_swarm_next_action(snapshot: &SwarmNextActionSnapshot) -> Finding {
    let (next_action, reason) = select_swarm_next_action(snapshot);
    let mut severity = next_action.severity();
    if !snapshot.source_errors.is_empty() && matches!(severity, Severity::Pass) {
        severity = Severity::Warn;
    }

    let detail = format!(
        "next_action={}, unread={}, urgent={}, ack_required={}, own_reservations={}, ready={}, open={}, in_progress={}, stale_in_progress={}, current_in_progress={}, open_epics={}, bv_highest_impact={}",
        next_action.label(),
        snapshot.mail_unread,
        snapshot.mail_urgent,
        snapshot.mail_ack_required,
        snapshot.reservations_own_active,
        snapshot.ready_ids.len(),
        snapshot.beads_open,
        snapshot.beads_in_progress,
        snapshot.stale_in_progress,
        snapshot.current_in_progress,
        snapshot.open_epic_count,
        snapshot.bv_highest_impact.as_deref().unwrap_or("none")
    );
    let data = serde_json::json!({
        "schema": SWARM_DOCTOR_NEXT_ACTION_SCHEMA,
        "next_action": next_action.label(),
        "reason": reason,
        "agent_name": snapshot.agent_name,
        "mail": {
            "unread": snapshot.mail_unread,
            "urgent": snapshot.mail_urgent,
            "ack_required": snapshot.mail_ack_required
        },
        "reservations": {
            "active_count": snapshot.reservations_active,
            "own_active_count": snapshot.reservations_own_active
        },
        "beads": {
            "open_count": snapshot.beads_open,
            "in_progress_count": snapshot.beads_in_progress,
            "parse_errors": snapshot.beads_parse_errors,
            "ready_count": snapshot.ready_ids.len(),
            "ready_ids": snapshot.ready_ids,
            "stale_in_progress_count": snapshot.stale_in_progress,
            "current_in_progress_count": snapshot.current_in_progress,
            "open_epic_count": snapshot.open_epic_count
        },
        "bv": {
            "highest_impact": snapshot.bv_highest_impact,
            "recommendation_count": snapshot.bv_recommendation_count
        },
        "source_errors": snapshot.source_errors
    });

    finding_with_severity(
        severity,
        format!(
            "Communication-purgatory next action: {}",
            next_action.label()
        ),
    )
    .with_detail(detail)
    .with_remediation(next_action.remediation())
    .with_data(data)
}

fn select_swarm_next_action(snapshot: &SwarmNextActionSnapshot) -> (SwarmNextAction, String) {
    if snapshot.mail_urgent > 0 || snapshot.mail_ack_required > 0 {
        return (
            SwarmNextAction::AckMail,
            "Agent Mail has urgent or ack-required obligations".to_string(),
        );
    }
    if snapshot.current_in_progress > 0 || snapshot.reservations_own_active > 0 {
        return (
            SwarmNextAction::ImplementCurrent,
            "Current agent already owns in-progress work or active file reservations".to_string(),
        );
    }
    if snapshot.stale_in_progress > 0 {
        return (
            SwarmNextAction::UnblockStalledBead,
            "One or more in-progress beads exceed the stale threshold".to_string(),
        );
    }
    if let Some(first_ready) = snapshot.ready_ids.first() {
        return (
            SwarmNextAction::ClaimReadyBead,
            format!("Ready Beads work is available, starting with {first_ready}"),
        );
    }
    if snapshot.open_epic_count > 0 {
        return (
            SwarmNextAction::CreateFollowupBeads,
            "No ready beads are visible, but open epics or roadmaps need decomposition".to_string(),
        );
    }
    (
        SwarmNextAction::NoWork,
        "No mail obligations, ready beads, current work, stale work, or open epics are visible"
            .to_string(),
    )
}

fn finding_with_severity(severity: Severity, title: impl Into<String>) -> Finding {
    match severity {
        Severity::Pass => Finding::pass(CheckCategory::Swarm, title),
        Severity::Info => Finding::info(CheckCategory::Swarm, title),
        Severity::Warn => Finding::warn(CheckCategory::Swarm, title),
        Severity::Fail => Finding::fail(CheckCategory::Swarm, title),
    }
}

fn apply_next_action_beads_snapshot(cwd: &Path, snapshot: &mut SwarmNextActionSnapshot) {
    let ledger_path = cwd.join(".beads/issues.jsonl");
    match std::fs::read_to_string(&ledger_path) {
        Ok(content) => {
            let summary =
                summarize_beads_ledger(&content, Utc::now(), SWARM_STALE_IN_PROGRESS_HOURS);
            snapshot.beads_open = summary.open;
            snapshot.beads_in_progress = summary.in_progress;
            snapshot.beads_parse_errors = summary.parse_errors;
            snapshot.stale_in_progress = summary.stale_in_progress.len();
            apply_next_action_issue_counts(&content, snapshot);
        }
        Err(err) => snapshot.source_errors.push(format!(
            "beads ledger unavailable at {}: {err}",
            ledger_path.display()
        )),
    }
}

fn apply_next_action_issue_counts(content: &str, snapshot: &mut SwarmNextActionSnapshot) {
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(issue) = serde_json::from_str::<BeadsIssueRecord>(line) else {
            continue;
        };
        if issue.status.eq("open") && issue_is_epic(&issue) {
            snapshot.open_epic_count += 1;
        }
        if issue.status.eq("in_progress")
            && snapshot
                .agent_name
                .as_deref()
                .is_some_and(|agent| issue_assignee(&issue) == Some(agent))
        {
            snapshot.current_in_progress += 1;
        }
    }
}

fn issue_is_epic(issue: &BeadsIssueRecord) -> bool {
    issue
        .issue_type
        .as_deref()
        .is_some_and(|issue_type| issue_type.eq_ignore_ascii_case("epic"))
}

fn apply_next_action_br_ready_snapshot(cwd: &Path, snapshot: &mut SwarmNextActionSnapshot) {
    if which_tool("br").is_none() {
        snapshot
            .source_errors
            .push("br CLI not found for ready-work probe".to_string());
        return;
    }
    match run_probe_json(
        SwarmProbeCommand::Br,
        &["ready", "--json"],
        Some(cwd),
        "br ready",
    ) {
        Ok(value) => {
            snapshot.ready_ids = ready_issue_ids(&value)
                .into_iter()
                .take(SWARM_DETAIL_LIMIT)
                .collect();
        }
        Err(err) => snapshot.source_errors.push(err),
    }
}

fn ready_issue_ids(value: &serde_json::Value) -> Vec<String> {
    let Some(values) = value.as_array().or_else(|| {
        value
            .get("issues")
            .or_else(|| value.get("items"))
            .and_then(serde_json::Value::as_array)
    }) else {
        return Vec::new();
    };

    values
        .iter()
        .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
        .map(ToString::to_string)
        .collect()
}

fn apply_next_action_bv_snapshot(cwd: &Path, snapshot: &mut SwarmNextActionSnapshot) {
    if which_tool("bv").is_none() {
        snapshot
            .source_errors
            .push("bv CLI not found for graph recommendation probe".to_string());
        return;
    }
    match run_probe_json(
        SwarmProbeCommand::Bv,
        &["--robot-plan"],
        Some(cwd),
        "bv --robot-plan",
    ) {
        Ok(value) => {
            snapshot.bv_highest_impact = value
                .pointer("/plan/summary/highest_impact")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);
            snapshot.bv_recommendation_count = bv_plan_item_count(&value);
        }
        Err(err) => snapshot.source_errors.push(err),
    }
}

fn bv_plan_item_count(value: &serde_json::Value) -> usize {
    value
        .pointer("/plan/tracks")
        .and_then(serde_json::Value::as_array)
        .map_or(0, |tracks| {
            tracks
                .iter()
                .map(|track| {
                    track
                        .get("items")
                        .and_then(serde_json::Value::as_array)
                        .map_or(0, Vec::len)
                })
                .sum()
        })
}

fn apply_next_action_agent_mail_snapshot(cwd: &Path, snapshot: &mut SwarmNextActionSnapshot) {
    let Some(agent) = snapshot.agent_name.clone() else {
        snapshot.source_errors.push(
            "AGENT_MAIL_AGENT and AGENT_NAME are unset for Agent Mail next-action probe"
                .to_string(),
        );
        return;
    };
    if which_tool("am").is_none() {
        snapshot
            .source_errors
            .push("Agent Mail CLI not found for next-action probe".to_string());
        return;
    }

    let project = cwd.display().to_string();
    apply_next_action_agent_mail_status(cwd, &project, &agent, snapshot);
    apply_next_action_agent_mail_inbox(cwd, &project, &agent, snapshot);
    apply_next_action_agent_mail_reservations(cwd, &project, &agent, snapshot);
}

fn apply_next_action_agent_mail_status(
    cwd: &Path,
    project: &str,
    agent: &str,
    snapshot: &mut SwarmNextActionSnapshot,
) {
    let args = [
        "robot",
        "status",
        "--format",
        "json",
        "--project",
        project,
        "--agent",
        agent,
    ];
    match run_probe_json(SwarmProbeCommand::Am, &args, Some(cwd), "am robot status") {
        Ok(value) => apply_mail_obligation_counts(&value, snapshot),
        Err(err) => snapshot.source_errors.push(err),
    }
}

fn apply_next_action_agent_mail_inbox(
    cwd: &Path,
    project: &str,
    agent: &str,
    snapshot: &mut SwarmNextActionSnapshot,
) {
    let args = [
        "robot",
        "inbox",
        "--format",
        "json",
        "--project",
        project,
        "--agent",
        agent,
        "--unread",
        "--limit",
        "20",
    ];
    match run_probe_json(SwarmProbeCommand::Am, &args, Some(cwd), "am robot inbox") {
        Ok(value) => apply_mail_obligation_counts(&value, snapshot),
        Err(err) => snapshot.source_errors.push(err),
    }
}

fn apply_next_action_agent_mail_reservations(
    cwd: &Path,
    project: &str,
    agent: &str,
    snapshot: &mut SwarmNextActionSnapshot,
) {
    let args = [
        "robot",
        "reservations",
        "--format",
        "json",
        "--project",
        project,
        "--all",
    ];
    match run_probe_json(
        SwarmProbeCommand::Am,
        &args,
        Some(cwd),
        "am robot reservations",
    ) {
        Ok(value) => {
            let counts = agent_mail_reservation_counts(&value, agent);
            snapshot.reservations_active = counts.active;
            snapshot.reservations_own_active = counts.own_active;
        }
        Err(err) => snapshot.source_errors.push(err),
    }
}

fn apply_mail_obligation_counts(value: &serde_json::Value, snapshot: &mut SwarmNextActionSnapshot) {
    snapshot.mail_unread = snapshot
        .mail_unread
        .max(json_number_by_key(value, "unread").unwrap_or(0));
    snapshot.mail_urgent = snapshot
        .mail_urgent
        .max(json_number_by_key(value, "urgent").unwrap_or(0));
    snapshot.mail_ack_required = snapshot
        .mail_ack_required
        .max(json_number_by_key(value, "ack_required").unwrap_or(0));
    let message_counts = count_mail_message_obligations(value);
    snapshot.mail_urgent = snapshot.mail_urgent.max(message_counts.urgent);
    snapshot.mail_ack_required = snapshot.mail_ack_required.max(message_counts.ack_required);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MailMessageObligationCounts {
    urgent: u64,
    ack_required: u64,
}

fn count_mail_message_obligations(value: &serde_json::Value) -> MailMessageObligationCounts {
    let mut counts = MailMessageObligationCounts::default();
    count_mail_message_obligations_inner(value, &mut counts);
    counts
}

fn count_mail_message_obligations_inner(
    value: &serde_json::Value,
    counts: &mut MailMessageObligationCounts,
) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("subject")
                || map.contains_key("message_id")
                || map.contains_key("id")
            {
                if map
                    .get("ack_required")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    counts.ack_required += 1;
                }
                let importance = map
                    .get("importance")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if matches!(importance, "urgent" | "high") {
                    counts.urgent += 1;
                }
            }
            for child in map.values() {
                count_mail_message_obligations_inner(child, counts);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                count_mail_message_obligations_inner(child, counts);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AgentMailReservationNextActionCounts {
    active: usize,
    own_active: usize,
}

fn agent_mail_reservation_counts(
    value: &serde_json::Value,
    agent: &str,
) -> AgentMailReservationNextActionCounts {
    let mut counts = AgentMailReservationNextActionCounts::default();
    count_agent_mail_reservation_rows(value, agent, &mut counts);
    counts
}

fn count_agent_mail_reservation_rows(
    value: &serde_json::Value,
    agent: &str,
    counts: &mut AgentMailReservationNextActionCounts,
) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("path") || map.contains_key("path_pattern") {
                let released = map
                    .get("released_ts")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                    || map
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|status| status.eq_ignore_ascii_case("released"));
                if !released {
                    counts.active += 1;
                    let holder = map
                        .get("agent")
                        .or_else(|| map.get("holder"))
                        .or_else(|| map.get("agent_name"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if holder.eq(agent) {
                        counts.own_active += 1;
                    }
                }
                return;
            }
            for child in map.values() {
                count_agent_mail_reservation_rows(child, agent, counts);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                count_agent_mail_reservation_rows(child, agent, counts);
            }
        }
        _ => {}
    }
}

fn check_swarm_context_intelligence(cwd: &Path, findings: &mut Vec<Finding>) {
    let now = Utc::now();
    let finding = match build_swarm_context_intelligence_posture(cwd, now) {
        Ok(posture) => classify_swarm_context_intelligence_posture(&posture),
        Err(err) => swarm_context_intelligence_unavailable_finding(&err),
    };
    findings.push(finding);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmContextIntelligencePosture {
    graph_schema: String,
    graph_builder_schema: String,
    bundle_schema: String,
    graph_node_count: usize,
    graph_edge_count: usize,
    graph_trace_status_counts: BTreeMap<String, usize>,
    missing_input_count: usize,
    malformed_input_count: usize,
    unreadable_input_count: usize,
    evidence_freshness_status_counts: BTreeMap<String, usize>,
    stale_or_untrusted_evidence_count: usize,
    graph_redaction_status_counts: BTreeMap<String, usize>,
    graph_validation_link_count: usize,
    bundle_selected_count: usize,
    bundle_excluded_count: usize,
    bundle_stale_evidence_suppression_count: usize,
    bundle_selected_code_count: usize,
    bundle_selected_test_signal_count: usize,
    bundle_missing_test_link_count: usize,
    bundle_suggested_validation_command_count: usize,
    bundle_coverage_status: String,
    bundle_estimated_bytes: u64,
    bundle_estimated_tokens: u64,
    bundle_budget: ContextBundleBudget,
    cache_ttl_seconds: u64,
    cache_fingerprint_count: usize,
    cache_status_counts: BTreeMap<String, usize>,
    cache_pressure_count: usize,
    cacheable: bool,
    redaction_summary: serde_json::Value,
    degraded_reasons: Vec<String>,
    recommended_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmContextGraphPosture {
    trace_status_counts: BTreeMap<String, usize>,
    missing_input_count: usize,
    malformed_input_count: usize,
    unreadable_input_count: usize,
    evidence_freshness_status_counts: BTreeMap<String, usize>,
    stale_or_untrusted_evidence_count: usize,
    redaction_status_counts: BTreeMap<String, usize>,
    validation_link_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmContextBundlePosture {
    selected_count: usize,
    excluded_count: usize,
    stale_evidence_suppression_count: usize,
    selected_code_count: usize,
    selected_test_signal_count: usize,
    missing_test_link_count: usize,
    suggested_validation_command_count: usize,
    coverage_status: String,
    estimated_bytes: u64,
    estimated_tokens: u64,
    budget: ContextBundleBudget,
    cacheable: bool,
    redaction_summary: serde_json::Value,
    unsafe_redaction: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmContextCachePosture {
    status_counts: BTreeMap<String, usize>,
    fingerprint_count: usize,
    pressure_count: usize,
}

fn build_swarm_context_intelligence_posture(
    cwd: &Path,
    now: DateTime<Utc>,
) -> std::result::Result<SwarmContextIntelligencePosture, String> {
    let branch_identity =
        current_git_commit(cwd).unwrap_or_else(|| "git-identity-unavailable".to_string());
    let cache_scope = ContextArtifactCacheScope::new(
        "doctor-swarm-context-intelligence",
        branch_identity,
        "operator-runpack",
    );
    let graph = SemanticWorkspaceGraphBuilder::new(cwd)
        .with_reference_time(now)
        .with_cache_scope(cache_scope.clone())
        .with_cache_ttl_seconds(15 * 60)
        .build()
        .map_err(|err| redacted_doctor_text(&err.to_string()))?;
    let bundle_request = swarm_context_bundle_request(cwd, now);
    let bundle = SemanticContextBundlePlanner::new(&graph).plan(&bundle_request);
    Ok(summarize_swarm_context_intelligence(
        &graph,
        &bundle,
        &cache_scope,
        now,
    ))
}

fn swarm_context_bundle_request(cwd: &Path, now: DateTime<Utc>) -> ContextBundleRequest {
    ContextBundleRequest {
        query: Some(
            "doctor swarm runpack context intelligence graph freshness bundle coverage stale suppressions missing tests cache pressure degraded modes"
                .to_string(),
        ),
        bead_id: None,
        changed_paths: vec![
            "src/doctor.rs".to_string(),
            "scripts/build_swarm_operator_runpack.py".to_string(),
            "tests/doctor_swarm_temp_dir_json.rs".to_string(),
        ],
        failing_command: None,
        workspace_id: Some(format!("doctor-swarm:{}", stable_path_label(cwd))),
        branch: current_git_commit(cwd),
        session_id: Some("operator-runpack".to_string()),
        generated_at_utc: Some(now.to_rfc3339()),
        cache_ttl_seconds: 15 * 60,
        budget: ContextBundleBudget {
            max_items: 24,
            max_bytes: 32 * 1024,
        },
    }
}

fn stable_path_label(path: &Path) -> String {
    let text = path.display().to_string();
    if line_is_sensitive(&text) {
        "[redacted-sensitive-path]".to_string()
    } else {
        truncate_chars(&text, 120)
    }
}

#[allow(clippy::too_many_lines)]
fn summarize_swarm_context_intelligence(
    graph: &SemanticWorkspaceGraph,
    bundle: &SemanticContextBundle,
    cache_scope: &ContextArtifactCacheScope,
    now: DateTime<Utc>,
) -> SwarmContextIntelligencePosture {
    let graph_posture = summarize_swarm_context_graph(graph);
    let cache_posture = summarize_swarm_context_cache(graph, cache_scope, now);
    let bundle_posture = summarize_swarm_context_bundle(bundle);
    let degraded_reasons =
        swarm_context_degraded_reasons(&graph_posture, &bundle_posture, &cache_posture);
    let recommended_actions = swarm_context_recommended_actions(&degraded_reasons);

    SwarmContextIntelligencePosture {
        graph_schema: graph.schema.clone(),
        graph_builder_schema: graph.builder_schema.clone(),
        bundle_schema: bundle.schema.clone(),
        graph_node_count: graph.nodes.len(),
        graph_edge_count: graph.edges.len(),
        graph_trace_status_counts: graph_posture.trace_status_counts,
        missing_input_count: graph_posture.missing_input_count,
        malformed_input_count: graph_posture.malformed_input_count,
        unreadable_input_count: graph_posture.unreadable_input_count,
        evidence_freshness_status_counts: graph_posture.evidence_freshness_status_counts,
        stale_or_untrusted_evidence_count: graph_posture.stale_or_untrusted_evidence_count,
        graph_redaction_status_counts: graph_posture.redaction_status_counts,
        graph_validation_link_count: graph_posture.validation_link_count,
        bundle_selected_count: bundle_posture.selected_count,
        bundle_excluded_count: bundle_posture.excluded_count,
        bundle_stale_evidence_suppression_count: bundle_posture.stale_evidence_suppression_count,
        bundle_selected_code_count: bundle_posture.selected_code_count,
        bundle_selected_test_signal_count: bundle_posture.selected_test_signal_count,
        bundle_missing_test_link_count: bundle_posture.missing_test_link_count,
        bundle_suggested_validation_command_count: bundle_posture
            .suggested_validation_command_count,
        bundle_coverage_status: bundle_posture.coverage_status,
        bundle_estimated_bytes: bundle_posture.estimated_bytes,
        bundle_estimated_tokens: bundle_posture.estimated_tokens,
        bundle_budget: bundle_posture.budget,
        cache_ttl_seconds: graph.cache_ttl_seconds,
        cache_fingerprint_count: cache_posture.fingerprint_count,
        cache_status_counts: cache_posture.status_counts,
        cache_pressure_count: cache_posture.pressure_count,
        cacheable: bundle_posture.cacheable,
        redaction_summary: bundle_posture.redaction_summary,
        degraded_reasons,
        recommended_actions,
    }
}

fn summarize_swarm_context_graph(graph: &SemanticWorkspaceGraph) -> SwarmContextGraphPosture {
    let mut trace_status_counts = BTreeMap::new();
    for event in &graph.trace {
        increment_count(&mut trace_status_counts, enum_json_label(event.status));
    }

    let mut evidence_freshness_status_counts = BTreeMap::new();
    let mut redaction_status_counts = BTreeMap::new();
    for node in &graph.nodes {
        if let Some(status) = node.freshness_status {
            increment_count(
                &mut evidence_freshness_status_counts,
                enum_json_label(status),
            );
        }
        increment_count(
            &mut redaction_status_counts,
            enum_json_label(node.redaction_status),
        );
    }

    let stale_or_untrusted_evidence_count = [
        EvidenceFreshnessStatus::HistoricalSnapshot,
        EvidenceFreshnessStatus::Stale,
        EvidenceFreshnessStatus::Missing,
        EvidenceFreshnessStatus::Malformed,
        EvidenceFreshnessStatus::Uncertified,
        EvidenceFreshnessStatus::FreshnessUnknown,
    ]
    .into_iter()
    .map(|status| count_map_value(&evidence_freshness_status_counts, &enum_json_label(status)))
    .sum();

    SwarmContextGraphPosture {
        missing_input_count: count_map_value(&trace_status_counts, "missing"),
        malformed_input_count: count_map_value(&trace_status_counts, "malformed"),
        unreadable_input_count: count_map_value(&trace_status_counts, "unreadable"),
        trace_status_counts,
        evidence_freshness_status_counts,
        stale_or_untrusted_evidence_count,
        redaction_status_counts,
        validation_link_count: graph
            .edges
            .iter()
            .filter(|edge| edge.edge_type == SemanticEdgeType::SuggestsValidation)
            .count(),
    }
}

fn summarize_swarm_context_cache(
    graph: &SemanticWorkspaceGraph,
    cache_scope: &ContextArtifactCacheScope,
    now: DateTime<Utc>,
) -> SwarmContextCachePosture {
    let mut status_counts = BTreeMap::new();
    for fingerprint in &graph.input_fingerprints {
        let status = fingerprint.cache_validation(cache_scope, utc_timestamp_nanos(now));
        increment_count(&mut status_counts, enum_json_label(status));
    }

    SwarmContextCachePosture {
        fingerprint_count: graph.input_fingerprints.len(),
        pressure_count: graph
            .input_fingerprints
            .len()
            .saturating_sub(count_map_value(&status_counts, "valid")),
        status_counts,
    }
}

fn summarize_swarm_context_bundle(bundle: &SemanticContextBundle) -> SwarmContextBundlePosture {
    let selected_code_count = bundle
        .selected_items
        .iter()
        .filter(|item| {
            matches!(
                item.node_type,
                SemanticNodeType::CodeSymbol | SemanticNodeType::FileRegion
            ) && item.source_path.starts_with("src/")
        })
        .count();
    let selected_test_signal_count = bundle
        .selected_items
        .iter()
        .filter(|item| {
            matches!(
                item.node_type,
                SemanticNodeType::TestCase | SemanticNodeType::ValidationCommand
            )
        })
        .count();
    let redaction_summary = serde_json::to_value(&bundle.redaction_summary).unwrap_or_else(|_| {
        serde_json::json!({
            "overall_status": "serialization_failed"
        })
    });

    SwarmContextBundlePosture {
        selected_count: bundle.selected_items.len(),
        excluded_count: bundle.excluded_items.len(),
        stale_evidence_suppression_count: bundle.stale_evidence_suppressions.len(),
        selected_code_count,
        selected_test_signal_count,
        missing_test_link_count: selected_code_count.saturating_sub(selected_test_signal_count),
        suggested_validation_command_count: bundle.suggested_validation_commands.len(),
        coverage_status: context_bundle_coverage_status(bundle),
        estimated_bytes: bundle.estimated_bytes,
        estimated_tokens: bundle.estimated_tokens,
        budget: bundle.budget.clone(),
        cacheable: bundle.invalidation_policy.cacheable,
        redaction_summary,
        unsafe_redaction: bundle.redaction_summary.overall_status >= RedactionStatus::UnsafeToEmit,
    }
}

fn swarm_context_degraded_reasons(
    graph: &SwarmContextGraphPosture,
    bundle: &SwarmContextBundlePosture,
    cache: &SwarmContextCachePosture,
) -> Vec<String> {
    let mut degraded_reasons = Vec::new();
    push_reason_if(
        &mut degraded_reasons,
        graph.missing_input_count > 0,
        "semantic_graph_missing_inputs",
    );
    push_reason_if(
        &mut degraded_reasons,
        graph.malformed_input_count > 0,
        "semantic_graph_malformed_inputs",
    );
    push_reason_if(
        &mut degraded_reasons,
        graph.unreadable_input_count > 0,
        "semantic_graph_unreadable_inputs",
    );
    push_reason_if(
        &mut degraded_reasons,
        bundle.selected_count == 0,
        "context_bundle_empty",
    );
    push_reason_if(
        &mut degraded_reasons,
        bundle.coverage_status != "covered",
        "context_bundle_partial_coverage",
    );
    push_reason_if(
        &mut degraded_reasons,
        bundle.stale_evidence_suppression_count > 0,
        "stale_or_unsafe_evidence_suppressed",
    );
    push_reason_if(
        &mut degraded_reasons,
        bundle.missing_test_link_count > 0,
        "selected_code_without_test_link",
    );
    push_reason_if(
        &mut degraded_reasons,
        cache.pressure_count > 0,
        "context_cache_pressure",
    );
    push_reason_if(
        &mut degraded_reasons,
        graph.stale_or_untrusted_evidence_count > 0,
        "stale_or_untrusted_evidence_present",
    );
    push_reason_if(
        &mut degraded_reasons,
        bundle.unsafe_redaction,
        "unsafe_context_redaction_suppressed",
    );
    degraded_reasons
}

fn classify_swarm_context_intelligence_posture(
    posture: &SwarmContextIntelligencePosture,
) -> Finding {
    let detail = format!(
        "graph nodes={} edges={} missing_inputs={} malformed_inputs={} unreadable_inputs={}; bundle coverage={} selected={} excluded={} stale_suppressions={} missing_test_links={} validation_commands={}; cache pressure={} fingerprints={}",
        posture.graph_node_count,
        posture.graph_edge_count,
        posture.missing_input_count,
        posture.malformed_input_count,
        posture.unreadable_input_count,
        posture.bundle_coverage_status,
        posture.bundle_selected_count,
        posture.bundle_excluded_count,
        posture.bundle_stale_evidence_suppression_count,
        posture.bundle_missing_test_link_count,
        posture.bundle_suggested_validation_command_count,
        posture.cache_pressure_count,
        posture.cache_fingerprint_count
    );
    let data = swarm_context_intelligence_json(posture);
    if posture.degraded_reasons.is_empty() {
        Finding::pass(CheckCategory::Swarm, "Context intelligence posture clear")
            .with_detail(detail)
            .with_data(data)
    } else {
        Finding::warn(
            CheckCategory::Swarm,
            "Context intelligence posture degraded",
        )
        .with_detail(format!(
            "{detail}; degraded_reasons={}",
            posture.degraded_reasons.join(",")
        ))
        .with_remediation(posture.recommended_actions.join("; "))
        .with_data(data)
    }
}

fn swarm_context_intelligence_json(posture: &SwarmContextIntelligencePosture) -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_DOCTOR_CONTEXT_INTELLIGENCE_SCHEMA,
        "mode": "audit_only",
        "mutation_performed": false,
        "status": if posture.degraded_reasons.is_empty() { "ready" } else { "degraded" },
        "graph": {
            "schema": posture.graph_schema,
            "builder_schema": posture.graph_builder_schema,
            "node_count": posture.graph_node_count,
            "edge_count": posture.graph_edge_count,
            "trace_status_counts": posture.graph_trace_status_counts,
            "missing_input_count": posture.missing_input_count,
            "malformed_input_count": posture.malformed_input_count,
            "unreadable_input_count": posture.unreadable_input_count,
            "evidence_freshness_status_counts": posture.evidence_freshness_status_counts,
            "stale_or_untrusted_evidence_count": posture.stale_or_untrusted_evidence_count,
            "redaction_status_counts": posture.graph_redaction_status_counts,
            "validation_link_count": posture.graph_validation_link_count
        },
        "bundle": {
            "schema": posture.bundle_schema,
            "coverage_status": posture.bundle_coverage_status,
            "selected_count": posture.bundle_selected_count,
            "excluded_count": posture.bundle_excluded_count,
            "stale_evidence_suppression_count": posture.bundle_stale_evidence_suppression_count,
            "selected_code_count": posture.bundle_selected_code_count,
            "selected_test_signal_count": posture.bundle_selected_test_signal_count,
            "missing_test_link_count": posture.bundle_missing_test_link_count,
            "suggested_validation_command_count": posture.bundle_suggested_validation_command_count,
            "estimated_bytes": posture.bundle_estimated_bytes,
            "estimated_tokens": posture.bundle_estimated_tokens,
            "budget": {
                "max_items": posture.bundle_budget.max_items,
                "max_bytes": posture.bundle_budget.max_bytes
            }
        },
        "cache": {
            "cache_ttl_seconds": posture.cache_ttl_seconds,
            "fingerprint_count": posture.cache_fingerprint_count,
            "status_counts": posture.cache_status_counts,
            "pressure_count": posture.cache_pressure_count,
            "pressure": posture.cache_pressure_count > 0,
            "cacheable": posture.cacheable
        },
        "redaction_summary": posture.redaction_summary,
        "degraded_reasons": posture.degraded_reasons,
        "recommended_actions": posture.recommended_actions
    })
}

fn swarm_context_intelligence_unavailable_finding(error: &str) -> Finding {
    Finding::warn(
        CheckCategory::Swarm,
        "Context intelligence posture unavailable",
    )
    .with_detail(error)
    .with_remediation("Run `pi doctor --only swarm --format json` from a readable project root")
    .with_data(serde_json::json!({
        "schema": SWARM_DOCTOR_CONTEXT_INTELLIGENCE_SCHEMA,
        "mode": "audit_only",
        "mutation_performed": false,
        "status": "degraded",
        "graph": {
            "status": "unavailable"
        },
        "bundle": {
            "coverage_status": "unavailable",
            "selected_count": 0,
            "missing_test_link_count": 0,
            "stale_evidence_suppression_count": 0
        },
        "cache": {
            "pressure": true,
            "pressure_count": 1
        },
        "redaction_summary": {
            "overall_status": "unknown"
        },
        "degraded_reasons": ["semantic_graph_unavailable"],
        "source_errors": [error]
    }))
}

fn context_bundle_coverage_status(bundle: &SemanticContextBundle) -> String {
    if bundle.selected_items.is_empty() {
        return "empty".to_string();
    }
    if bundle
        .excluded_items
        .iter()
        .any(|item| item.reason == "budget_exceeded")
    {
        return "partial_budget_limited".to_string();
    }
    if bundle.stale_evidence_suppressions.is_empty() {
        "covered".to_string()
    } else {
        "partial_stale_suppressed".to_string()
    }
}

fn swarm_context_recommended_actions(degraded_reasons: &[String]) -> Vec<String> {
    let mut actions = Vec::new();
    if degraded_reasons
        .iter()
        .any(|reason| reason.contains("semantic_graph"))
    {
        actions.push("Refresh missing or malformed graph inputs before trusting context posture");
    }
    if degraded_reasons
        .iter()
        .any(|reason| reason == "context_bundle_partial_coverage")
    {
        actions.push(
            "Tighten the context request or raise the context budget before broad swarm work",
        );
    }
    if degraded_reasons
        .iter()
        .any(|reason| reason == "stale_or_unsafe_evidence_suppressed")
    {
        actions.push("Refresh stale evidence or keep suppressed evidence out of runpack claims");
    }
    if degraded_reasons
        .iter()
        .any(|reason| reason == "selected_code_without_test_link")
    {
        actions.push("Add or link focused validation commands for selected code context");
    }
    if degraded_reasons
        .iter()
        .any(|reason| reason == "context_cache_pressure")
    {
        actions.push("Regenerate context cache fingerprints before reusing cached bundles");
    }
    if actions.is_empty() {
        actions.push("No context-intelligence remediation required");
    }
    actions.into_iter().map(str::to_string).collect()
}

fn push_reason_if(reasons: &mut Vec<String>, condition: bool, reason: &str) {
    if condition {
        reasons.push(reason.to_string());
    }
}

fn increment_count(counts: &mut BTreeMap<String, usize>, key: String) {
    *counts.entry(key).or_default() += 1;
}

fn count_map_value(counts: &BTreeMap<String, usize>, key: &str) -> usize {
    counts.get(key).copied().unwrap_or(0)
}

#[allow(clippy::too_many_lines)]
fn check_swarm_validation_broker(cwd: &Path, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Swarm;
    let Some(store_raw) = first_non_empty_env(&[SWARM_VALIDATION_BROKER_STORE_ENV]) else {
        findings.push(
            Finding::info(cat, "Validation broker posture not configured")
                .with_detail(
                    "No validation broker slot store is configured for Doctor projection",
                )
                .with_remediation(format!(
                    "Set {SWARM_VALIDATION_BROKER_STORE_ENV} when using validation-broker advisory handoff"
                ))
                .with_data(validation_broker_not_configured_json()),
        );
        return;
    };

    let store_path = resolve_validation_broker_store_path(cwd, &store_raw);
    let store_exists = store_path.exists();
    let snapshot = ValidationSlotStore::new(&store_path).load_snapshot();
    let now_utc = Utc::now().to_rfc3339();
    let mut state_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut active_slots = 0usize;
    let mut reusable_slots = Vec::new();
    let mut stale_slots = 0usize;
    let mut stale_warning_slot_ids = BTreeSet::new();
    let mut expired_slots = Vec::new();
    let mut current_slots = Vec::new();
    let mut active_by_fingerprint: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for lease in snapshot.latest_by_slot_id.values() {
        let state = validation_slot_state_label(&lease.state);
        *state_counts.entry(state.to_string()).or_default() += 1;
        if matches!(
            lease.state,
            ValidationSlotState::Requested | ValidationSlotState::Active
        ) {
            active_slots += 1;
            active_by_fingerprint
                .entry(lease.command_fingerprint.clone())
                .or_default()
                .push(lease.slot_id.clone());
        }
        if lease.state == ValidationSlotState::Reusable {
            reusable_slots.push(validation_broker_slot_json(lease));
        }
        if lease.state == ValidationSlotState::Stale {
            stale_slots += 1;
            stale_warning_slot_ids.insert(lease.slot_id.clone());
        }
        if lease.is_stale_at(&now_utc).unwrap_or(false) {
            stale_warning_slot_ids.insert(lease.slot_id.clone());
            expired_slots.push(validation_broker_slot_json(lease));
        }
        if current_slots.len() < SWARM_DETAIL_LIMIT {
            current_slots.push(validation_broker_slot_json(lease));
        }
    }

    let duplicate_groups = active_by_fingerprint
        .into_iter()
        .filter(|(_, slot_ids)| slot_ids.len() > 1)
        .map(|(command_fingerprint, slot_ids)| {
            serde_json::json!({
                "command_fingerprint": command_fingerprint,
                "slot_ids": slot_ids,
            })
        })
        .collect::<Vec<_>>();
    let total_records = snapshot.leases.len();
    let total_slots = snapshot.latest_by_slot_id.len();
    let snapshot_degraded = snapshot.is_degraded();
    let store_schema = snapshot.schema;
    let mut degraded_reasons = snapshot.degraded_reasons;
    if !store_exists {
        degraded_reasons.push("validation_broker_store_missing".to_string());
    }
    let source_status = if !store_exists {
        "unavailable"
    } else if snapshot_degraded {
        "degraded"
    } else {
        "available"
    };
    let source_degraded = source_status != "available";
    let reusable_slot_count = reusable_slots.len();
    let duplicate_opportunity_count = duplicate_groups.len() + reusable_slot_count;
    let stale_warning_count = stale_warning_slot_ids.len();
    let recommended_actions = validation_broker_recommended_actions(
        source_degraded,
        store_exists,
        stale_warning_count,
        duplicate_opportunity_count,
    );
    let data = serde_json::json!({
        "schema": SWARM_DOCTOR_VALIDATION_BROKER_SCHEMA,
        "mode": "advisory_projection",
        "mutation_performed": false,
        "source_status": {
            "validation_slot_store": source_status,
            "source_of_truth": "validation_broker_slot_store",
            "doctor_authority": "environment_diagnostics_only"
        },
        "store": {
            "path": store_path.display().to_string(),
            "configured": true,
            "exists": store_exists,
            "schema": store_schema,
            "status": source_status,
            "total_records": total_records,
            "total_slots": total_slots,
            "state_counts": state_counts,
        },
        "current_slots": {
            "total": total_slots,
            "active": active_slots,
            "reusable": reusable_slot_count,
            "stale": stale_slots,
            "expired_at_report_time": expired_slots.len(),
            "sample": current_slots,
        },
        "duplicate_gate_opportunities": {
            "count": duplicate_opportunity_count,
            "active_equivalent_groups": duplicate_groups.into_iter().take(SWARM_DETAIL_LIMIT).collect::<Vec<_>>(),
            "reusable_slots": reusable_slots.into_iter().take(SWARM_DETAIL_LIMIT).collect::<Vec<_>>(),
        },
        "stale_build_warnings": {
            "count": stale_warning_count,
            "expired_slots": expired_slots.into_iter().take(SWARM_DETAIL_LIMIT).collect::<Vec<_>>(),
        },
        "degraded_reasons": degraded_reasons,
        "recommended_next_actions": recommended_actions,
        "guards": {
            "advisory_only": true,
            "no_live_mutation": true,
            "not_ci_success": true,
            "not_release_claim_evidence": true,
            "does_not_replace_rch_doctor_beads_agent_mail": true
        }
    });
    let title = if !store_exists {
        "Validation broker store unavailable"
    } else if snapshot_degraded {
        "Validation broker posture degraded"
    } else if stale_warning_count > 0 {
        "Validation broker has stale slot warnings"
    } else {
        "Validation broker posture clear"
    };
    let mut finding = if source_degraded || stale_warning_count > 0 {
        Finding::warn(cat, title)
    } else {
        Finding::pass(cat, title)
    };
    finding = finding
        .with_detail(format!(
            "store={} exists={} slots={} active={} reusable={} stale_warnings={} duplicate_opportunities={}",
            store_path.display(),
            store_exists,
            total_slots,
            active_slots,
            reusable_slot_count,
            stale_warning_count,
            duplicate_opportunity_count
        ))
        .with_data(data);
    if !recommended_actions.is_empty() {
        finding = finding.with_remediation(recommended_actions.join("; "));
    }
    findings.push(finding);
}

fn validation_broker_not_configured_json() -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_DOCTOR_VALIDATION_BROKER_SCHEMA,
        "mode": "advisory_projection",
        "mutation_performed": false,
        "source_status": {
            "validation_slot_store": "not_configured",
            "source_of_truth": "validation_broker_slot_store",
            "doctor_authority": "environment_diagnostics_only"
        },
        "store": {
            "configured": false,
            "exists": false,
            "status": "not_configured",
            "total_records": 0,
            "total_slots": 0,
            "state_counts": {}
        },
        "current_slots": {
            "total": 0,
            "active": 0,
            "reusable": 0,
            "stale": 0,
            "expired_at_report_time": 0,
            "sample": []
        },
        "duplicate_gate_opportunities": {
            "count": 0,
            "active_equivalent_groups": [],
            "reusable_slots": []
        },
        "stale_build_warnings": {
            "count": 0,
            "expired_slots": []
        },
        "degraded_reasons": ["validation_broker_store_not_configured"],
        "recommended_next_actions": [
            "Set PI_VALIDATION_BROKER_STORE only when broker-guided validation handoff is in use"
        ],
        "guards": {
            "advisory_only": true,
            "no_live_mutation": true,
            "not_ci_success": true,
            "not_release_claim_evidence": true,
            "does_not_replace_rch_doctor_beads_agent_mail": true
        }
    })
}

fn resolve_validation_broker_store_path(cwd: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

const fn validation_slot_state_label(state: &ValidationSlotState) -> &'static str {
    match state {
        ValidationSlotState::Requested => "requested",
        ValidationSlotState::Active => "active",
        ValidationSlotState::Reusable => "reusable",
        ValidationSlotState::Stale => "stale",
        ValidationSlotState::Failed => "failed",
        ValidationSlotState::Released => "released",
        ValidationSlotState::Expired => "expired",
        ValidationSlotState::Degraded => "degraded",
    }
}

fn validation_broker_slot_json(lease: &ValidationSlotLease) -> serde_json::Value {
    serde_json::json!({
        "slot_id": &lease.slot_id,
        "state": validation_slot_state_label(&lease.state),
        "owner_agent": &lease.owner_agent,
        "bead_id": &lease.bead_id,
        "command_class": &lease.command_class,
        "runner": &lease.runner,
        "rch_job_id": &lease.rch_job_id,
        "heartbeat_at_utc": &lease.heartbeat_at_utc,
        "expires_at_utc": &lease.expires_at_utc,
        "command_fingerprint": &lease.command_fingerprint,
    })
}

fn validation_broker_recommended_actions(
    degraded: bool,
    store_exists: bool,
    stale_warning_count: usize,
    duplicate_opportunity_count: usize,
) -> Vec<String> {
    let mut actions = Vec::new();
    if !store_exists {
        actions.push("Create the validation-broker store by acquiring a slot before relying on lease posture");
    }
    if degraded {
        actions.push("Inspect the validation-broker store and avoid coalescing gates until malformed records are resolved");
    }
    if stale_warning_count > 0 {
        actions.push("Recover stale validation slots through owner-visible broker acquire/renew/release workflow");
    }
    if duplicate_opportunity_count > 0 {
        actions.push("Consider coalescing equivalent validation gates only after matching provenance and required no-claim markers");
    }
    if actions.is_empty() {
        actions.push("No validation-broker action required");
    }
    actions.into_iter().map(str::to_string).collect()
}

fn check_swarm_progress_slo(cwd: &Path, findings: &mut Vec<Finding>) {
    let raw_path = first_non_empty_env(&[SWARM_PROGRESS_SLO_JSON_ENV]);
    findings.push(build_swarm_progress_slo_finding(cwd, raw_path.as_deref()));
}

fn build_swarm_progress_slo_finding(cwd: &Path, raw_path: Option<&str>) -> Finding {
    let Some(raw_path) = raw_path.map(str::trim).filter(|value| !value.is_empty()) else {
        return progress_slo_source_finding(
            Severity::Info,
            "Progress SLO posture not configured",
            "not_configured",
            None,
            Some(format!(
                "Set {SWARM_PROGRESS_SLO_JSON_ENV} to a pi.swarm.progress_slo.v1 JSON report when projecting progress SLO posture into Doctor"
            )),
            &[],
        );
    };

    let source_path = resolve_progress_slo_source_path(cwd, raw_path);
    let source_label = stable_path_label(&source_path);
    let raw = match std::fs::read_to_string(&source_path) {
        Ok(raw) => raw,
        Err(err) => {
            return progress_slo_source_finding(
                Severity::Warn,
                "Progress SLO posture unavailable",
                "unavailable",
                Some(&source_path),
                Some(format!(
                    "progress SLO report source is unreadable at {source_label}: {}",
                    redacted_doctor_text(&err.to_string())
                )),
                &["progress_slo_source_unavailable"],
            );
        }
    };

    let value = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(value) => value,
        Err(err) => {
            return progress_slo_source_finding(
                Severity::Warn,
                "Progress SLO posture malformed",
                "malformed",
                Some(&source_path),
                Some(format!(
                    "progress SLO report source is not valid JSON at {source_label}: {}",
                    redacted_doctor_text(&err.to_string())
                )),
                &["progress_slo_source_malformed"],
            );
        }
    };

    let observed_schema = value.get("schema").and_then(serde_json::Value::as_str);
    if observed_schema != Some(SWARM_PROGRESS_SLO_SCHEMA) {
        return progress_slo_source_finding(
            Severity::Warn,
            "Progress SLO posture malformed",
            "malformed",
            Some(&source_path),
            Some(format!(
                "progress SLO report schema mismatch at {source_label}: expected {SWARM_PROGRESS_SLO_SCHEMA}, got {}",
                observed_schema.unwrap_or("missing")
            )),
            &["progress_slo_schema_mismatch"],
        );
    }

    let report = match serde_json::from_value::<ProgressSloReport>(value) {
        Ok(report) => report,
        Err(err) => {
            return progress_slo_source_finding(
                Severity::Warn,
                "Progress SLO posture malformed",
                "malformed",
                Some(&source_path),
                Some(format!(
                    "progress SLO report source does not match expected shape at {source_label}: {}",
                    redacted_doctor_text(&err.to_string())
                )),
                &["progress_slo_shape_mismatch"],
            );
        }
    };

    classify_swarm_progress_slo_report(&report, Some(&source_path))
}

fn resolve_progress_slo_source_path(cwd: &Path, raw_path: &str) -> PathBuf {
    let path = PathBuf::from(raw_path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn classify_swarm_progress_slo_report(
    report: &ProgressSloReport,
    source_path: Option<&Path>,
) -> Finding {
    let status = enum_json_label(report.status);
    let severity = match report.status {
        ProgressSloStatus::Progressing | ProgressSloStatus::ConvergedNoOpenWork => Severity::Pass,
        ProgressSloStatus::QuietBlocked => Severity::Info,
        ProgressSloStatus::CoordinationDegraded
        | ProgressSloStatus::BuildSaturated
        | ProgressSloStatus::Stalled
        | ProgressSloStatus::MalformedSourceDegraded
        | ProgressSloStatus::InsufficientEvidenceDegraded => Severity::Warn,
    };
    let title = match report.status {
        ProgressSloStatus::Progressing => "Progress SLO posture healthy",
        ProgressSloStatus::ConvergedNoOpenWork => "Progress SLO posture converged",
        ProgressSloStatus::QuietBlocked => "Progress SLO posture quiet-blocked",
        ProgressSloStatus::CoordinationDegraded => "Progress SLO posture coordination degraded",
        ProgressSloStatus::BuildSaturated => "Progress SLO posture build saturated",
        ProgressSloStatus::Stalled => "Progress SLO posture stalled",
        ProgressSloStatus::MalformedSourceDegraded => "Progress SLO posture malformed",
        ProgressSloStatus::InsufficientEvidenceDegraded => {
            "Progress SLO posture insufficient evidence"
        }
    };
    let detail = format!(
        "status={status}, confidence={:.3}, reasons={}, closed={}, commits={}, ready={}, stale_in_progress={}, source_count={}",
        report.confidence,
        report.reason_ids.join(","),
        report.progress_metrics.closed_beads,
        report.progress_metrics.commits,
        report.progress_metrics.ready_beads,
        report.progress_metrics.stale_in_progress_candidates,
        report.source_statuses.len()
    );
    let data = progress_slo_report_data(report, "available", source_path, &[], None);
    let mut finding = Finding {
        category: CheckCategory::Swarm,
        severity,
        title: title.to_string(),
        detail: Some(detail),
        remediation: None,
        data: Some(data),
        fixability: Fixability::NotFixable,
    };
    if !report.next_actions.is_empty() {
        finding = finding.with_remediation(report.next_actions.join("; "));
    }
    finding
}

fn progress_slo_source_finding(
    severity: Severity,
    title: &str,
    source_status: &str,
    source_path: Option<&Path>,
    issue: Option<String>,
    degraded_reasons: &[&str],
) -> Finding {
    let data = progress_slo_source_data(
        source_status,
        source_path,
        issue.as_deref(),
        degraded_reasons,
    );
    Finding {
        category: CheckCategory::Swarm,
        severity,
        title: title.to_string(),
        detail: issue,
        remediation: Some(
            "Capture or refresh a pi.swarm.progress_slo.v1 report before treating progress SLO posture as current handoff evidence"
                .to_string(),
        ),
        data: Some(data),
        fixability: Fixability::NotFixable,
    }
}

fn progress_slo_report_data(
    report: &ProgressSloReport,
    source_status: &str,
    source_path: Option<&Path>,
    degraded_reasons: &[&str],
    issue: Option<&str>,
) -> serde_json::Value {
    let degraded_source_count = report
        .source_statuses
        .iter()
        .filter(|source| {
            !matches!(
                source.availability,
                crate::swarm_progress_slo::SourceAvailability::Available
            ) || !matches!(
                source.freshness_state,
                crate::swarm_progress_slo::FreshnessState::Current
            )
        })
        .count();
    let malformed_source_count = report
        .source_statuses
        .iter()
        .filter(|source| {
            matches!(
                source.availability,
                crate::swarm_progress_slo::SourceAvailability::Malformed
            ) || matches!(
                source.freshness_state,
                crate::swarm_progress_slo::FreshnessState::Malformed
            )
        })
        .count();

    serde_json::json!({
        "schema": SWARM_DOCTOR_PROGRESS_SLO_SCHEMA,
        "mode": "advisory_projection",
        "mutation_performed": false,
        "source_status": {
            "progress_slo_report": source_status,
            "source_of_truth": SWARM_PROGRESS_SLO_SCHEMA,
            "doctor_authority": "environment_diagnostics_only",
            "path": source_path.map(stable_path_label),
            "issue": issue,
        },
        "progress": {
            "schema": report.schema,
            "contract_version": report.contract_version,
            "generated_at": report.generated_at,
            "status": enum_json_label(report.status),
            "confidence": report.confidence,
            "reason_ids": report.reason_ids,
            "time_window": {
                "start_utc": report.time_window.start_utc,
                "end_utc": report.time_window.end_utc,
                "duration_seconds": report.time_window.duration_seconds,
                "comparison_baseline": report.time_window.comparison_baseline,
            },
            "next_actions": report.next_actions,
            "suppressed_claims": report.suppressed_claims,
        },
        "metrics": serde_json::to_value(&report.progress_metrics).unwrap_or_else(|_| serde_json::json!({"status": "serialization_failed"})),
        "saturation_summary": serde_json::to_value(&report.saturation_summary).unwrap_or_else(|_| serde_json::json!({"status": "serialization_failed"})),
        "redaction_summary": serde_json::to_value(&report.redaction_summary).unwrap_or_else(|_| serde_json::json!({"status": "serialization_failed"})),
        "source_summary": {
            "total": report.source_statuses.len(),
            "degraded": degraded_source_count,
            "malformed": malformed_source_count,
        },
        "degraded_reasons": degraded_reasons,
        "guards": {
            "advisory_only": true,
            "no_live_mutation": true,
            "not_ci_success": true,
            "not_release_claim_evidence": true,
            "does_not_replace_beads_agent_mail_rch_validation_broker_doctor_runpacks_context_or_ci": true,
        }
    })
}

fn progress_slo_source_data(
    source_status: &str,
    source_path: Option<&Path>,
    issue: Option<&str>,
    degraded_reasons: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_DOCTOR_PROGRESS_SLO_SCHEMA,
        "mode": "advisory_projection",
        "mutation_performed": false,
        "source_status": {
            "progress_slo_report": source_status,
            "source_of_truth": SWARM_PROGRESS_SLO_SCHEMA,
            "doctor_authority": "environment_diagnostics_only",
            "path": source_path.map(stable_path_label),
            "issue": issue,
        },
        "progress": {
            "schema": SWARM_PROGRESS_SLO_SCHEMA,
            "status": "unavailable",
            "confidence": 0.0,
            "reason_ids": [],
            "next_actions": [
                "Capture or refresh a progress SLO report before projecting progress posture"
            ],
            "suppressed_claims": [
                "progress_slo_current_posture"
            ],
        },
        "metrics": {},
        "saturation_summary": {},
        "redaction_summary": {
            "redacted_count": 0,
            "omitted_count": 0,
            "unsafe_to_emit_count": 0,
            "suppressed_claims": [
                "progress_slo_current_posture"
            ],
        },
        "source_summary": {
            "total": 0,
            "degraded": 1,
            "malformed": usize::from(source_status == "malformed"),
        },
        "degraded_reasons": degraded_reasons,
        "guards": {
            "advisory_only": true,
            "no_live_mutation": true,
            "not_ci_success": true,
            "not_release_claim_evidence": true,
            "does_not_replace_beads_agent_mail_rch_validation_broker_doctor_runpacks_context_or_ci": true,
        }
    })
}

fn enum_json_label<T>(value: T) -> String
where
    T: Serialize + fmt::Debug,
{
    serde_json::to_value(&value)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| format!("{value:?}").to_ascii_lowercase())
}

fn utc_timestamp_nanos(now: DateTime<Utc>) -> u64 {
    now.timestamp_nanos_opt()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0)
}

fn redacted_doctor_text(text: &str) -> String {
    if line_is_sensitive(text) {
        "[redacted sensitive diagnostic detail]".to_string()
    } else {
        truncate_chars(text, 220)
    }
}

#[allow(clippy::too_many_lines)]
fn check_swarm_operations_dashboard(cwd: &Path, findings: &mut Vec<Finding>) {
    let now = Utc::now();
    let agent_name = first_non_empty_env(&["AGENT_MAIL_AGENT", "AGENT_NAME"]);
    let project = cwd.display().to_string();
    let ledger_path = cwd.join(".beads/issues.jsonl");

    let (beads_content, beads_error) = match std::fs::read_to_string(&ledger_path) {
        Ok(content) => (Some(content), None),
        Err(err) => (
            None,
            Some(format!(
                "beads ledger unavailable at {}: {err}",
                ledger_path.display()
            )),
        ),
    };

    let (agent_roster, agent_roster_error) = read_agent_mail_agents_roster(cwd);

    let (br_ready, br_ready_error) = if which_tool("br").is_some() {
        match run_probe_json(
            SwarmProbeCommand::Br,
            &["ready", "--json"],
            Some(cwd),
            "br ready",
        ) {
            Ok(value) => (Some(value), None),
            Err(err) => (None, Some(err)),
        }
    } else {
        (
            None,
            Some("br CLI not found for dashboard ready-work probe".to_string()),
        )
    };

    let (bv_plan, bv_plan_error) = if which_tool("bv").is_some() {
        match run_probe_json(
            SwarmProbeCommand::Bv,
            &["--robot-plan"],
            Some(cwd),
            "bv --robot-plan",
        ) {
            Ok(value) => (Some(value), None),
            Err(err) => (None, Some(err)),
        }
    } else {
        (
            None,
            Some("bv CLI not found for dashboard graph probe".to_string()),
        )
    };

    let (mail_status, mail_status_error) = if which_tool("am").is_some()
        && agent_name.as_deref().is_some()
    {
        let agent = agent_name.as_deref().unwrap_or_default();
        let args = [
            "robot",
            "status",
            "--format",
            "json",
            "--project",
            &project,
            "--agent",
            agent,
        ];
        match run_probe_json(SwarmProbeCommand::Am, &args, Some(cwd), "am robot status") {
            Ok(value) => (Some(value), None),
            Err(err) => (None, Some(err)),
        }
    } else if which_tool("am").is_none() {
        (
            None,
            Some("Agent Mail CLI not found for dashboard status probe".to_string()),
        )
    } else {
        (
            None,
            Some(
                "AGENT_MAIL_AGENT and AGENT_NAME are unset for dashboard status probe".to_string(),
            ),
        )
    };

    let (mail_inbox, mail_inbox_error) = if which_tool("am").is_some()
        && agent_name.as_deref().is_some()
    {
        let agent = agent_name.as_deref().unwrap_or_default();
        let args = [
            "robot",
            "inbox",
            "--format",
            "json",
            "--project",
            &project,
            "--agent",
            agent,
            "--unread",
            "--limit",
            "20",
        ];
        match run_probe_json(SwarmProbeCommand::Am, &args, Some(cwd), "am robot inbox") {
            Ok(value) => (Some(value), None),
            Err(err) => (None, Some(err)),
        }
    } else if which_tool("am").is_none() {
        (
            None,
            Some("Agent Mail CLI not found for dashboard inbox probe".to_string()),
        )
    } else {
        (
            None,
            Some("AGENT_MAIL_AGENT and AGENT_NAME are unset for dashboard inbox probe".to_string()),
        )
    };

    let (reservations, reservations_error) = if which_tool("am").is_some() {
        let args = [
            "robot",
            "reservations",
            "--format",
            "json",
            "--project",
            &project,
            "--all",
        ];
        match run_probe_json(
            SwarmProbeCommand::Am,
            &args,
            Some(cwd),
            "am robot reservations",
        ) {
            Ok(value) => (Some(value), None),
            Err(err) => (None, Some(err)),
        }
    } else {
        (
            None,
            Some("Agent Mail CLI not found for dashboard reservations probe".to_string()),
        )
    };

    let (rch_status, rch_status_error) = if which_tool("rch").is_some() {
        match run_tool_with_timeout(
            SwarmProbeCommand::Rch,
            &["status"],
            None,
            SWARM_PROBE_TIMEOUT,
        ) {
            Ok(outcome) => (Some(outcome), None),
            Err(err) => (None, Some(format!("rch status failed to start: {err}"))),
        }
    } else {
        (
            None,
            Some("rch CLI not found for dashboard status probe".to_string()),
        )
    };

    let (rch_queue, rch_queue_error) = if which_tool("rch").is_some() {
        match run_tool_with_timeout(
            SwarmProbeCommand::Rch,
            &["queue"],
            None,
            SWARM_PROBE_TIMEOUT,
        ) {
            Ok(outcome) => (Some(outcome), None),
            Err(err) => (None, Some(format!("rch queue failed to start: {err}"))),
        }
    } else {
        (
            None,
            Some("rch CLI not found for dashboard queue probe".to_string()),
        )
    };

    let (git_porcelain, git_error) = if which_tool("git").is_some() {
        match run_tool_with_timeout(
            SwarmProbeCommand::Git,
            &["status", "--porcelain=v1", "--untracked-files=all"],
            Some(cwd),
            SWARM_PROBE_TIMEOUT,
        ) {
            Ok(outcome) if outcome.success => (Some(outcome.stdout), None),
            Ok(outcome) => (
                None,
                Some(format!(
                    "git status failed: {}",
                    command_failure_detail(&outcome)
                )),
            ),
            Err(err) => (None, Some(format!("git status failed to start: {err}"))),
        }
    } else {
        (
            None,
            Some("git CLI not found for dashboard dirty-state probe".to_string()),
        )
    };

    let inputs = SwarmOperationsDashboardInputs {
        agent_name,
        beads_content: beads_content.as_deref(),
        beads_error,
        agent_roster: agent_roster.as_ref(),
        agent_roster_error,
        mail_status: mail_status.as_ref(),
        mail_status_error,
        mail_inbox: mail_inbox.as_ref(),
        mail_inbox_error,
        reservations: reservations.as_ref(),
        reservations_error,
        br_ready: br_ready.as_ref(),
        br_ready_error,
        bv_plan: bv_plan.as_ref(),
        bv_plan_error,
        rch_status: rch_status.as_ref(),
        rch_status_error,
        rch_queue: rch_queue.as_ref(),
        rch_queue_error,
        git_porcelain: git_porcelain.as_deref(),
        git_error,
        source_errors: Vec::new(),
    };
    let dashboard = build_swarm_operations_dashboard_snapshot(inputs, now);
    findings.push(classify_swarm_operations_dashboard(&dashboard));
}

#[derive(Debug, Default)]
struct SwarmOperationsDashboardInputs<'a> {
    agent_name: Option<String>,
    beads_content: Option<&'a str>,
    beads_error: Option<String>,
    agent_roster: Option<&'a serde_json::Value>,
    agent_roster_error: Option<String>,
    mail_status: Option<&'a serde_json::Value>,
    mail_status_error: Option<String>,
    mail_inbox: Option<&'a serde_json::Value>,
    mail_inbox_error: Option<String>,
    reservations: Option<&'a serde_json::Value>,
    reservations_error: Option<String>,
    br_ready: Option<&'a serde_json::Value>,
    br_ready_error: Option<String>,
    bv_plan: Option<&'a serde_json::Value>,
    bv_plan_error: Option<String>,
    rch_status: Option<&'a CommandOutcome>,
    rch_status_error: Option<String>,
    rch_queue: Option<&'a CommandOutcome>,
    rch_queue_error: Option<String>,
    git_porcelain: Option<&'a str>,
    git_error: Option<String>,
    source_errors: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct SwarmOperationsDashboardSnapshot {
    agent_name: Option<String>,
    agents: SwarmDashboardAgentSummary,
    mail: SwarmDashboardMailSummary,
    beads: SwarmDashboardBeadsSummary,
    reservations: SwarmDashboardReservationSummary,
    rch: SwarmDashboardRchSummary,
    git: GitPorcelainSummary,
    next_action: String,
    next_action_reason: String,
    next_action_remediation: String,
    source_errors: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct SwarmDashboardAgentSummary {
    total_seen: usize,
    active_count: usize,
    stale_count: usize,
    truncated_count: usize,
    rows: Vec<SwarmDashboardAgentRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwarmDashboardAgentRow {
    name: String,
    last_active_ts: String,
    age_hours: i64,
    active: bool,
    task_description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SwarmDashboardMailSummary {
    unread: u64,
    urgent: u64,
    ack_required: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct SwarmDashboardBeadsSummary {
    open_count: usize,
    in_progress_count: usize,
    active_count: usize,
    parse_errors: usize,
    ready_ids: Vec<String>,
    bv_highest_impact: Option<String>,
    bv_recommendation_count: usize,
    stalled_candidates: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct SwarmDashboardReservationSummary {
    active_count: usize,
    recent_inactive_count: usize,
    conflict_group_count: usize,
    stale_holder_recommendation_count: usize,
    expiring_soon_count: usize,
    conflicts: Vec<serde_json::Value>,
    stale_holder_recommendations: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SwarmDashboardRchSummary {
    status_reachable: bool,
    queue_reachable: bool,
    active_builds: Option<u64>,
    slots_available: Option<u64>,
    slots_total: Option<u64>,
    stale_progress: bool,
    pressure: String,
    detail: Option<String>,
}

#[allow(clippy::too_many_lines)]
fn build_swarm_operations_dashboard_snapshot(
    inputs: SwarmOperationsDashboardInputs<'_>,
    now: DateTime<Utc>,
) -> SwarmOperationsDashboardSnapshot {
    let mut source_errors = inputs.source_errors;
    extend_source_errors(
        &mut source_errors,
        [
            inputs.beads_error,
            inputs.agent_roster_error,
            inputs.mail_status_error,
            inputs.mail_inbox_error,
            inputs.reservations_error,
            inputs.br_ready_error,
            inputs.bv_plan_error,
            inputs.rch_status_error,
            inputs.rch_queue_error,
            inputs.git_error,
        ],
    );

    let activities = inputs
        .agent_roster
        .map_or_else(HashMap::new, |value| agent_mail_activity_index(value, now));
    let agents = inputs
        .agent_roster
        .map_or_else(SwarmDashboardAgentSummary::default, |value| {
            swarm_dashboard_agent_summary(value, now)
        });
    let mail = swarm_dashboard_mail_summary(inputs.mail_status, inputs.mail_inbox);
    let beads = swarm_dashboard_beads_summary(
        inputs.beads_content,
        &activities,
        inputs.br_ready,
        inputs.bv_plan,
        now,
    );
    let reservations =
        swarm_dashboard_reservation_summary(inputs.reservations, inputs.agent_name.as_deref(), now);
    let rch = swarm_dashboard_rch_summary(inputs.rch_status, inputs.rch_queue);
    let git = inputs
        .git_porcelain
        .map_or_else(GitPorcelainSummary::default, summarize_git_porcelain);

    let mut next_action_snapshot = SwarmNextActionSnapshot {
        agent_name: inputs.agent_name.clone(),
        mail_unread: mail.unread,
        mail_urgent: mail.urgent,
        mail_ack_required: mail.ack_required,
        reservations_active: reservations.active_count,
        reservations_own_active: inputs
            .reservations
            .and_then(|value| {
                inputs
                    .agent_name
                    .as_deref()
                    .map(|agent| agent_mail_reservation_counts(value, agent).own_active)
            })
            .unwrap_or(0),
        beads_open: beads.open_count,
        beads_in_progress: beads.in_progress_count,
        beads_parse_errors: beads.parse_errors,
        stale_in_progress: beads.stalled_candidates.len(),
        ready_ids: beads.ready_ids.clone(),
        bv_highest_impact: beads.bv_highest_impact.clone(),
        bv_recommendation_count: beads.bv_recommendation_count,
        source_errors: source_errors.clone(),
        ..SwarmNextActionSnapshot::default()
    };
    if let Some(content) = inputs.beads_content {
        apply_next_action_issue_counts(content, &mut next_action_snapshot);
    }
    let (next_action, next_action_reason) = select_swarm_next_action(&next_action_snapshot);

    SwarmOperationsDashboardSnapshot {
        agent_name: inputs.agent_name,
        agents,
        mail,
        beads,
        reservations,
        rch,
        git,
        next_action: next_action.label().to_string(),
        next_action_reason,
        next_action_remediation: next_action.remediation().to_string(),
        source_errors,
    }
}

fn extend_source_errors<const N: usize>(errors: &mut Vec<String>, values: [Option<String>; N]) {
    errors.extend(values.into_iter().flatten());
}

fn classify_swarm_operations_dashboard(snapshot: &SwarmOperationsDashboardSnapshot) -> Finding {
    let detail = swarm_operations_dashboard_detail(snapshot);
    let data = swarm_operations_dashboard_json(snapshot);
    let mut reasons = Vec::new();
    if !snapshot.source_errors.is_empty() {
        reasons.push("one or more dashboard inputs are unavailable");
    }
    if snapshot.mail.urgent > 0 || snapshot.mail.ack_required > 0 {
        reasons.push("mail obligations require attention");
    }
    if !snapshot.beads.stalled_candidates.is_empty() {
        reasons.push("stalled bead candidates need operator review");
    }
    if snapshot.reservations.conflict_group_count > 0 {
        reasons.push("active reservation conflicts exist");
    }
    if snapshot.reservations.stale_holder_recommendation_count > 0 {
        reasons.push("stale reservation holders need coordination");
    }
    if !matches!(snapshot.rch.pressure.as_str(), "clear" | "unavailable") {
        reasons.push("RCH queue pressure is present");
    }
    if snapshot.git.total > 0 {
        reasons.push("working tree is dirty");
    }

    if reasons.is_empty() {
        Finding::pass(CheckCategory::Swarm, "Swarm operations dashboard clear")
            .with_detail(detail)
            .with_data(data)
    } else {
        Finding::warn(
            CheckCategory::Swarm,
            "Swarm operations dashboard needs attention",
        )
        .with_detail(format!("{detail}; attention={}", reasons.join(", ")))
        .with_remediation(snapshot.next_action_remediation.clone())
        .with_data(data)
    }
}

fn swarm_operations_dashboard_detail(snapshot: &SwarmOperationsDashboardSnapshot) -> String {
    format!(
        "agents active={} stale={} total_seen={}; beads open={} in_progress={} ready={} stalled_candidates={}; reservations active={} conflicts={} stale_holders={} expiring_soon={}; rch pressure={} active_builds={} slots={}; git total={} staged={} unstaged={} untracked={}; next_action={}",
        snapshot.agents.active_count,
        snapshot.agents.stale_count,
        snapshot.agents.total_seen,
        snapshot.beads.open_count,
        snapshot.beads.in_progress_count,
        snapshot.beads.ready_ids.len(),
        snapshot.beads.stalled_candidates.len(),
        snapshot.reservations.active_count,
        snapshot.reservations.conflict_group_count,
        snapshot.reservations.stale_holder_recommendation_count,
        snapshot.reservations.expiring_soon_count,
        snapshot.rch.pressure,
        option_u64_label(snapshot.rch.active_builds),
        rch_slots_label(snapshot.rch.slots_available, snapshot.rch.slots_total),
        snapshot.git.total,
        snapshot.git.staged,
        snapshot.git.unstaged,
        snapshot.git.untracked,
        snapshot.next_action
    )
}

fn swarm_operations_dashboard_json(
    snapshot: &SwarmOperationsDashboardSnapshot,
) -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_DOCTOR_OPERATIONS_DASHBOARD_SCHEMA,
        "mode": "audit_only",
        "mutation_performed": false,
        "active_agent_window_hours": SWARM_ACTIVE_AGENT_WINDOW_HOURS,
        "agent_name": snapshot.agent_name,
        "agents": {
            "total_seen": snapshot.agents.total_seen,
            "active_count": snapshot.agents.active_count,
            "stale_count": snapshot.agents.stale_count,
            "truncated_count": snapshot.agents.truncated_count,
            "rows": swarm_dashboard_agents_json(&snapshot.agents.rows)
        },
        "mail": {
            "unread": snapshot.mail.unread,
            "urgent": snapshot.mail.urgent,
            "ack_required": snapshot.mail.ack_required
        },
        "beads": {
            "open_count": snapshot.beads.open_count,
            "in_progress_count": snapshot.beads.in_progress_count,
            "active_count": snapshot.beads.active_count,
            "parse_errors": snapshot.beads.parse_errors,
            "ready_count": snapshot.beads.ready_ids.len(),
            "ready_ids": snapshot.beads.ready_ids,
            "bv_highest_impact": snapshot.beads.bv_highest_impact,
            "bv_recommendation_count": snapshot.beads.bv_recommendation_count,
            "stalled_candidate_count": snapshot.beads.stalled_candidates.len(),
            "stalled_candidates": snapshot.beads.stalled_candidates
        },
        "reservations": {
            "active_count": snapshot.reservations.active_count,
            "recent_inactive_count": snapshot.reservations.recent_inactive_count,
            "conflict_group_count": snapshot.reservations.conflict_group_count,
            "stale_holder_recommendation_count": snapshot.reservations.stale_holder_recommendation_count,
            "expiring_soon_count": snapshot.reservations.expiring_soon_count,
            "conflicts": snapshot.reservations.conflicts,
            "stale_holder_recommendations": snapshot.reservations.stale_holder_recommendations
        },
        "rch": {
            "status_reachable": snapshot.rch.status_reachable,
            "queue_reachable": snapshot.rch.queue_reachable,
            "active_builds": snapshot.rch.active_builds,
            "slots_available": snapshot.rch.slots_available,
            "slots_total": snapshot.rch.slots_total,
            "stale_progress": snapshot.rch.stale_progress,
            "pressure": snapshot.rch.pressure,
            "detail": snapshot.rch.detail
        },
        "git": {
            "total": snapshot.git.total,
            "staged": snapshot.git.staged,
            "unstaged": snapshot.git.unstaged,
            "untracked": snapshot.git.untracked,
            "deleted": snapshot.git.deleted
        },
        "next_action": {
            "action": snapshot.next_action,
            "reason": snapshot.next_action_reason,
            "remediation": snapshot.next_action_remediation
        },
        "source_errors": snapshot.source_errors
    })
}

fn check_swarm_incident_diagnostics(findings: &mut Vec<Finding>) {
    let finding = classify_swarm_incident_diagnostics(findings);
    findings.push(finding);
}

#[derive(Debug, Clone)]
struct SwarmIncidentComponent {
    domain: &'static str,
    status: &'static str,
    classification: String,
    source_schema: Option<&'static str>,
    title: String,
    detail: Option<String>,
    remediation: Option<String>,
    evidence: serde_json::Value,
}

impl SwarmIncidentComponent {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "domain": self.domain,
            "status": self.status,
            "classification": self.classification,
            "source_schema": self.source_schema,
            "title": self.title,
            "detail": self.detail,
            "remediation": self.remediation,
            "evidence": self.evidence,
        })
    }

    fn failure_json(&self) -> serde_json::Value {
        serde_json::json!({
            "domain": self.domain,
            "status": self.status,
            "classification": self.classification,
            "title": self.title,
            "detail": self.detail,
            "remediation": self.remediation,
        })
    }
}

fn classify_swarm_incident_diagnostics(findings: &[Finding]) -> Finding {
    let components = vec![
        incident_agent_mail_component(findings),
        incident_beads_component(findings),
        incident_rch_component(findings),
        incident_temp_dirs_component(findings),
        incident_resource_governor_component(findings),
        incident_session_queue_component(findings),
    ];
    let overall_status = incident_overall_status(&components);
    let primary = incident_primary_component(&components);
    let blocked_count = components
        .iter()
        .filter(|component| component.status == "blocked")
        .count();
    let degraded_count = components
        .iter()
        .filter(|component| component.status == "degraded")
        .count();
    let failure_domains = components
        .iter()
        .filter(|component| matches!(component.status, "blocked" | "degraded"))
        .map(SwarmIncidentComponent::failure_json)
        .collect::<Vec<_>>();
    let mut component_map = serde_json::Map::new();
    for component in &components {
        component_map.insert(component.domain.to_string(), component.to_json());
    }
    let remediation = primary.map_or_else(
        || "No incident action needed; rerun `pi doctor --only swarm --format json` before large swarms".to_string(),
        |component| {
            component.remediation.clone().unwrap_or_else(|| {
                "Inspect the component evidence and rerun `pi doctor --only swarm --format json` after remediation".to_string()
            })
        },
    );
    let primary_domain = primary.map(|component| component.domain);
    let primary_classification = primary.map(|component| component.classification.as_str());
    let detail = format!(
        "status={overall_status}, primary_domain={}, primary_classification={}, blocked_domains={blocked_count}, degraded_domains={degraded_count}",
        primary_domain.unwrap_or("none"),
        primary_classification.unwrap_or("none")
    );
    let data = serde_json::json!({
        "schema": SWARM_DOCTOR_INCIDENT_DIAGNOSTICS_SCHEMA,
        "mode": "audit_only",
        "mutation_performed": false,
        "status": overall_status,
        "primary_failure_domain": primary_domain,
        "primary_classification": primary_classification,
        "blocked_domain_count": blocked_count,
        "degraded_domain_count": degraded_count,
        "components": serde_json::Value::Object(component_map),
        "failure_domains": failure_domains,
        "redaction": {
            "probe_output_redaction": "enabled",
            "sensitive_line_redaction": true,
            "detail_truncation": true,
        },
        "guards": {
            "read_only": true,
            "does_not_repair_external_services": true,
            "does_not_replace_individual_doctor_findings": true,
            "beads_fallback_allowed_when_agent_mail_degraded": true,
        },
    });

    match overall_status {
        "blocked" => Finding::fail(CheckCategory::Swarm, "Swarm incident diagnostics blocked"),
        "degraded" => Finding::warn(
            CheckCategory::Swarm,
            "Swarm incident diagnostics need attention",
        ),
        _ => Finding::pass(CheckCategory::Swarm, "Swarm incident diagnostics clear"),
    }
    .with_detail(detail)
    .with_remediation(remediation)
    .with_data(data)
}

fn incident_agent_mail_component(findings: &[Finding]) -> SwarmIncidentComponent {
    let finding = finding_with_schema(findings, SWARM_DOCTOR_AGENT_MAIL_DEGRADED_SCHEMA);
    let data = finding.and_then(|finding| finding.data.as_ref());
    let reachable = data
        .and_then(|value| value.pointer("/mail_health/reachable"))
        .and_then(serde_json::Value::as_bool);
    let missing_tables = data
        .and_then(|value| value.pointer("/mail_health/missing_schema_tables"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    let mode = data
        .and_then(|value| value.get("mode"))
        .and_then(serde_json::Value::as_str);
    let classification = match (finding, reachable, missing_tables, mode) {
        (None, _, _, _) => "agent_mail_not_checked",
        (_, Some(false), _, _) => "agent_mail_unavailable",
        (_, _, count, _) if count > 0 => "agent_mail_schema_missing",
        (_, _, _, Some("beads_soft_lock_fallback")) => "agent_mail_degraded_beads_fallback",
        _ => "agent_mail_ready",
    };
    let status = finding.map_or("unknown", |finding| {
        incident_status_from_severity(finding.severity)
    });
    let evidence = serde_json::json!({
        "reachable": reachable,
        "missing_schema_table_count": missing_tables,
        "mode": mode,
        "fallback": data.and_then(|value| value.get("fallback")).cloned(),
        "write_paths": data.and_then(|value| value.get("write_paths")).cloned(),
        "mail_health": data.and_then(|value| value.get("mail_health")).cloned(),
    });

    component_from_finding(
        "agent_mail",
        status,
        classification,
        Some(SWARM_DOCTOR_AGENT_MAIL_DEGRADED_SCHEMA),
        finding,
        evidence,
    )
}

fn incident_beads_component(findings: &[Finding]) -> SwarmIncidentComponent {
    let dashboard = finding_with_schema(findings, SWARM_DOCTOR_OPERATIONS_DASHBOARD_SCHEMA);
    let dashboard_data = dashboard.and_then(|finding| finding.data.as_ref());
    let br_sync = findings.iter().find(|finding| {
        finding
            .title
            .to_ascii_lowercase()
            .contains("beads db/jsonl sync")
    });
    let parse_errors = dashboard_data
        .and_then(|value| value.pointer("/beads/parse_errors"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let ready_count = dashboard_data
        .and_then(|value| value.pointer("/beads/ready_count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let open_count = dashboard_data
        .and_then(|value| value.pointer("/beads/open_count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let in_progress_count = dashboard_data
        .and_then(|value| value.pointer("/beads/in_progress_count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let stalled_count = dashboard_data
        .and_then(|value| value.pointer("/beads/stalled_candidate_count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let source_errors = dashboard_data
        .and_then(|value| value.get("source_errors"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let beads_probe_error = source_errors.iter().any(|error| {
        error.as_str().is_some_and(|raw| {
            let lower = raw.to_ascii_lowercase();
            lower.contains("beads") || lower.contains("br ") || lower.contains("bv ")
        })
    });
    let br_sync_drift = br_sync.is_some_and(|finding| finding.severity >= Severity::Warn);
    let (status, classification) = if parse_errors > 0 {
        ("blocked", "beads_ledger_parse_errors")
    } else if beads_probe_error {
        ("degraded", "beads_probe_degraded")
    } else if br_sync_drift {
        ("degraded", "beads_sync_drift")
    } else if stalled_count > 0 {
        ("degraded", "beads_stalled_in_progress")
    } else if ready_count == 0 && open_count == 0 && in_progress_count == 0 {
        ("info", "beads_no_open_work")
    } else {
        ("healthy", "beads_ready")
    };
    let source_finding = br_sync.or(dashboard);
    let evidence = serde_json::json!({
        "open_count": open_count,
        "in_progress_count": in_progress_count,
        "ready_count": ready_count,
        "parse_errors": parse_errors,
        "stalled_candidate_count": stalled_count,
        "br_sync_drift": br_sync_drift,
        "source_errors": source_errors,
    });

    component_from_finding(
        "beads",
        status,
        classification,
        Some(SWARM_DOCTOR_OPERATIONS_DASHBOARD_SCHEMA),
        source_finding,
        evidence,
    )
}

fn incident_rch_component(findings: &[Finding]) -> SwarmIncidentComponent {
    let failure = finding_with_schema(findings, SWARM_DOCTOR_RCH_FAILURE_SCHEMA);
    let dashboard = finding_with_schema(findings, SWARM_DOCTOR_OPERATIONS_DASHBOARD_SCHEMA);
    let dashboard_data = dashboard.and_then(|finding| finding.data.as_ref());
    let pressure = dashboard_data
        .and_then(|value| value.pointer("/rch/pressure"))
        .and_then(serde_json::Value::as_str);
    let classification = failure
        .and_then(|finding| finding.data.as_ref())
        .and_then(|data| data.get("classification"))
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || match pressure {
                Some("clear") => "rch_clear".to_string(),
                Some("unavailable") | None => "rch_unavailable".to_string(),
                Some(value) => format!("rch_pressure_{value}"),
            },
            ToString::to_string,
        );
    let status = failure.map_or_else(
        || {
            if pressure == Some("clear") {
                "healthy"
            } else {
                "degraded"
            }
        },
        |finding| incident_status_from_severity(finding.severity),
    );
    let source_finding = failure.or(dashboard);
    let evidence = serde_json::json!({
        "pressure": pressure,
        "status_reachable": dashboard_data
            .and_then(|value| value.pointer("/rch/status_reachable"))
            .and_then(serde_json::Value::as_bool),
        "queue_reachable": dashboard_data
            .and_then(|value| value.pointer("/rch/queue_reachable"))
            .and_then(serde_json::Value::as_bool),
        "active_builds": dashboard_data
            .and_then(|value| value.pointer("/rch/active_builds"))
            .cloned(),
        "slots_available": dashboard_data
            .and_then(|value| value.pointer("/rch/slots_available"))
            .cloned(),
        "failure": failure.and_then(|finding| finding.data.clone()),
    });

    component_from_finding(
        "rch",
        status,
        classification.as_str(),
        Some(SWARM_DOCTOR_RCH_FAILURE_SCHEMA),
        source_finding,
        evidence,
    )
}

fn incident_temp_dirs_component(findings: &[Finding]) -> SwarmIncidentComponent {
    let temp_findings = findings_with_schema(findings, SWARM_DOCTOR_TEMP_DIR_SCHEMA);
    let status = incident_status_from_findings(&temp_findings);
    let mut missing_env = false;
    let mut missing_path = false;
    let mut outside_root = false;
    let mut low_headroom = false;
    let evidence_rows = temp_findings
        .iter()
        .filter_map(|finding| finding.data.as_ref())
        .map(|data| {
            missing_env |= data.get("path").is_some_and(serde_json::Value::is_null);
            missing_path |= data.get("exists").and_then(serde_json::Value::as_bool) == Some(false)
                && data.get("path").is_some_and(|value| !value.is_null());
            outside_root |= data
                .get("under_expected_root")
                .and_then(serde_json::Value::as_bool)
                == Some(false);
            let warn_available = data
                .get("warn_available_kb")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(SWARM_DISK_WARN_AVAILABLE_KB);
            low_headroom |= data
                .get("available_kb")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|available| available < warn_available);
            serde_json::json!({
                "env_name": data.get("env_name").cloned(),
                "path": data.get("path").cloned(),
                "exists": data.get("exists").cloned(),
                "under_expected_root": data.get("under_expected_root").cloned(),
                "available_kb": data.get("available_kb").cloned(),
                "warn_available_kb": data.get("warn_available_kb").cloned(),
            })
        })
        .collect::<Vec<_>>();
    let classification = if temp_findings.is_empty() {
        "temp_dirs_not_checked"
    } else if missing_env {
        "temp_env_missing"
    } else if missing_path {
        "temp_path_missing"
    } else if outside_root {
        "temp_outside_swarm_scratch_root"
    } else if low_headroom {
        "temp_low_headroom"
    } else {
        "temp_ready"
    };
    let source_finding = temp_findings
        .iter()
        .copied()
        .max_by_key(|finding| finding.severity);
    let evidence = serde_json::json!({
        "paths": evidence_rows,
        "missing_env": missing_env,
        "missing_path": missing_path,
        "outside_expected_root": outside_root,
        "low_headroom": low_headroom,
    });

    component_from_finding(
        "temp_dirs",
        status,
        classification,
        Some(SWARM_DOCTOR_TEMP_DIR_SCHEMA),
        source_finding,
        evidence,
    )
}

fn incident_resource_governor_component(findings: &[Finding]) -> SwarmIncidentComponent {
    let admission = finding_with_schema(findings, SWARM_DOCTOR_ADMISSION_SCHEMA);
    let preflight = finding_with_schema(findings, SWARM_DOCTOR_RESOURCE_PREFLIGHT_SCHEMA);
    let admission_data = admission.and_then(|finding| finding.data.as_ref());
    let preflight_data = preflight.and_then(|finding| finding.data.as_ref());
    let action = admission_data
        .and_then(|data| data.get("action"))
        .and_then(serde_json::Value::as_str);
    let critical_failures = preflight_data
        .and_then(|data| data.get("critical_failures"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    let stale_warnings = admission_data
        .and_then(|data| data.get("stale_data_warnings"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    let source_errors = preflight_data
        .and_then(|data| data.get("source_errors"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    let classification = if critical_failures > 0 {
        "resource_preflight_blocked".to_string()
    } else if let Some(action @ ("deny" | "backpressure")) = action {
        format!("resource_admission_{action}")
    } else if stale_warnings > 0 || source_errors > 0 {
        "resource_inputs_degraded".to_string()
    } else {
        "resource_governor_ready".to_string()
    };
    let status = incident_status_from_optional_findings([admission, preflight]);
    let source_finding = [admission, preflight]
        .into_iter()
        .flatten()
        .max_by_key(|finding| finding.severity);
    let evidence = serde_json::json!({
        "admission_action": action,
        "pressure_dimension": admission_data
            .and_then(|data| data.get("pressure_dimension"))
            .cloned(),
        "critical_failure_count": critical_failures,
        "stale_data_warning_count": stale_warnings,
        "source_error_count": source_errors,
        "recommended_budgets": preflight_data
            .and_then(|data| data.get("recommended_budgets"))
            .cloned(),
    });

    component_from_finding(
        "resource_governor",
        status,
        classification.as_str(),
        Some(SWARM_DOCTOR_ADMISSION_SCHEMA),
        source_finding,
        evidence,
    )
}

fn incident_session_queue_component(findings: &[Finding]) -> SwarmIncidentComponent {
    let session_finding = findings
        .iter()
        .find(|finding| finding.category == CheckCategory::Sessions);
    let (status, classification) =
        session_finding.map_or(("info", "session_queue_telemetry_not_checked"), |finding| {
            let status = incident_status_from_severity(finding.severity);
            let classification = if finding.severity >= Severity::Warn {
                "session_health_degraded"
            } else {
                "session_health_ready"
            };
            (status, classification)
        });
    let evidence = serde_json::json!({
        "source": "doctor_sessions_category",
        "checked": session_finding.is_some(),
        "queue_depth": null,
        "queue_depth_status": "not_available",
        "next_probe": "pi doctor --only sessions --format json",
    });

    component_from_finding(
        "session_queue",
        status,
        classification,
        None,
        session_finding,
        evidence,
    )
}

fn component_from_finding(
    domain: &'static str,
    status: &'static str,
    classification: &str,
    source_schema: Option<&'static str>,
    finding: Option<&Finding>,
    evidence: serde_json::Value,
) -> SwarmIncidentComponent {
    SwarmIncidentComponent {
        domain,
        status,
        classification: classification.to_string(),
        source_schema,
        title: finding.map_or_else(
            || format!("{domain} not checked"),
            |finding| finding.title.clone(),
        ),
        detail: finding.and_then(|finding| finding.detail.clone()),
        remediation: finding.and_then(|finding| finding.remediation.clone()),
        evidence,
    }
}

fn incident_overall_status(components: &[SwarmIncidentComponent]) -> &'static str {
    if components
        .iter()
        .any(|component| component.status == "blocked")
    {
        "blocked"
    } else if components
        .iter()
        .any(|component| component.status == "degraded")
    {
        "degraded"
    } else {
        "healthy"
    }
}

fn incident_primary_component(
    components: &[SwarmIncidentComponent],
) -> Option<&SwarmIncidentComponent> {
    components
        .iter()
        .find(|component| component.status == "blocked")
        .or_else(|| {
            components
                .iter()
                .find(|component| component.status == "degraded")
        })
}

const fn incident_status_from_severity(severity: Severity) -> &'static str {
    match severity {
        Severity::Pass => "healthy",
        Severity::Info => "info",
        Severity::Warn => "degraded",
        Severity::Fail => "blocked",
    }
}

fn incident_status_from_findings(findings: &[&Finding]) -> &'static str {
    findings
        .iter()
        .map(|finding| finding.severity)
        .max()
        .map_or("unknown", incident_status_from_severity)
}

fn incident_status_from_optional_findings<const N: usize>(
    findings: [Option<&Finding>; N],
) -> &'static str {
    findings
        .into_iter()
        .flatten()
        .map(|finding| finding.severity)
        .max()
        .map_or("unknown", incident_status_from_severity)
}

fn finding_with_schema<'a>(findings: &'a [Finding], schema: &str) -> Option<&'a Finding> {
    findings
        .iter()
        .find(|finding| finding_data_schema(finding) == Some(schema))
}

fn findings_with_schema<'a>(findings: &'a [Finding], schema: &str) -> Vec<&'a Finding> {
    findings
        .iter()
        .filter(|finding| finding_data_schema(finding) == Some(schema))
        .collect()
}

fn finding_data_schema(finding: &Finding) -> Option<&str> {
    finding
        .data
        .as_ref()
        .and_then(|data| data.get("schema"))
        .and_then(serde_json::Value::as_str)
}

fn swarm_dashboard_agent_summary(
    value: &serde_json::Value,
    now: DateTime<Utc>,
) -> SwarmDashboardAgentSummary {
    let mut rows = Vec::new();
    collect_swarm_dashboard_agent_rows(value, now, &mut rows);
    rows.sort_by(|left, right| {
        right
            .active
            .cmp(&left.active)
            .then_with(|| left.age_hours.cmp(&right.age_hours))
            .then_with(|| left.name.cmp(&right.name))
    });
    let total_seen = rows.len();
    let active_count = rows.iter().filter(|row| row.active).count();
    let stale_count = rows.len().saturating_sub(active_count);
    let truncated_count = rows.len().saturating_sub(SWARM_DASHBOARD_AGENT_LIMIT);
    rows.truncate(SWARM_DASHBOARD_AGENT_LIMIT);
    SwarmDashboardAgentSummary {
        total_seen,
        active_count,
        stale_count,
        truncated_count,
        rows,
    }
}

fn collect_swarm_dashboard_agent_rows(
    value: &serde_json::Value,
    now: DateTime<Utc>,
    rows: &mut Vec<SwarmDashboardAgentRow>,
) {
    match value {
        serde_json::Value::Object(map) => {
            let name = map
                .get("name")
                .or_else(|| map.get("agent"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let last_active = [
                "last_active_ts",
                "last_active",
                "last_seen_ts",
                "last_seen",
                "updated_at",
            ]
            .iter()
            .find_map(|key| map.get(*key).and_then(serde_json::Value::as_str));
            if let (Some(name), Some(last_active)) = (name, last_active)
                && let Some(age_hours) = age_hours_since(last_active, now)
            {
                rows.push(SwarmDashboardAgentRow {
                    name: truncate_chars(name, 80),
                    last_active_ts: truncate_chars(last_active, 80),
                    age_hours,
                    active: age_hours < SWARM_ACTIVE_AGENT_WINDOW_HOURS,
                    task_description: map
                        .get("task_description")
                        .and_then(serde_json::Value::as_str)
                        .map(|value| truncate_chars(value, 120)),
                });
                return;
            }
            for child in map.values() {
                collect_swarm_dashboard_agent_rows(child, now, rows);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                collect_swarm_dashboard_agent_rows(child, now, rows);
            }
        }
        _ => {}
    }
}

fn swarm_dashboard_agents_json(rows: &[SwarmDashboardAgentRow]) -> Vec<serde_json::Value> {
    rows.iter()
        .map(|row| {
            serde_json::json!({
                "name": row.name,
                "last_active_ts": row.last_active_ts,
                "age_hours": row.age_hours,
                "active": row.active,
                "task_description": row.task_description
            })
        })
        .collect()
}

fn swarm_dashboard_mail_summary(
    status: Option<&serde_json::Value>,
    inbox: Option<&serde_json::Value>,
) -> SwarmDashboardMailSummary {
    let mut snapshot = SwarmNextActionSnapshot::default();
    if let Some(value) = status {
        apply_mail_obligation_counts(value, &mut snapshot);
    }
    if let Some(value) = inbox {
        apply_mail_obligation_counts(value, &mut snapshot);
    }
    SwarmDashboardMailSummary {
        unread: snapshot.mail_unread,
        urgent: snapshot.mail_urgent,
        ack_required: snapshot.mail_ack_required,
    }
}

fn swarm_dashboard_beads_summary(
    content: Option<&str>,
    activities: &HashMap<String, AgentMailActivity>,
    br_ready: Option<&serde_json::Value>,
    bv_plan: Option<&serde_json::Value>,
    now: DateTime<Utc>,
) -> SwarmDashboardBeadsSummary {
    let mut summary = SwarmDashboardBeadsSummary::default();
    if let Some(content) = content {
        let ledger_summary = summarize_beads_ledger(content, now, SWARM_STALE_IN_PROGRESS_HOURS);
        let audit =
            collect_stalled_reaper_audit(content, activities, now, SWARM_STALE_IN_PROGRESS_HOURS);
        summary.open_count = ledger_summary.open;
        summary.in_progress_count = ledger_summary.in_progress;
        summary.active_count = ledger_summary.active;
        summary.parse_errors = ledger_summary.parse_errors.max(audit.parse_errors);
        summary.stalled_candidates = audit
            .suggestions
            .into_iter()
            .filter(|suggestion| {
                suggestion.get("action").and_then(serde_json::Value::as_str)
                    == Some("notify_then_reopen_or_claim")
            })
            .take(SWARM_DETAIL_LIMIT)
            .collect();
    }
    if let Some(value) = br_ready {
        summary.ready_ids = ready_issue_ids(value)
            .into_iter()
            .take(SWARM_DETAIL_LIMIT)
            .collect();
    }
    if let Some(value) = bv_plan {
        summary.bv_highest_impact = value
            .pointer("/plan/summary/highest_impact")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        summary.bv_recommendation_count = bv_plan_item_count(value);
    }
    summary
}

fn swarm_dashboard_reservation_summary(
    value: Option<&serde_json::Value>,
    agent_name: Option<&str>,
    now: DateTime<Utc>,
) -> SwarmDashboardReservationSummary {
    let Some(value) = value else {
        return SwarmDashboardReservationSummary::default();
    };
    let heatmap = build_agent_mail_reservation_heatmap(value, now);
    let mut summary = SwarmDashboardReservationSummary {
        active_count: heatmap.active,
        recent_inactive_count: heatmap.recent_inactive,
        conflict_group_count: heatmap.conflicts.len(),
        stale_holder_recommendation_count: heatmap.stale_recommendations.len(),
        expiring_soon_count: heatmap.expiring_soon,
        conflicts: heatmap
            .conflicts
            .into_iter()
            .take(SWARM_DETAIL_LIMIT)
            .collect(),
        stale_holder_recommendations: heatmap
            .stale_recommendations
            .into_iter()
            .take(SWARM_DETAIL_LIMIT)
            .collect(),
    };
    if summary.active_count == 0 {
        summary.active_count = json_number_by_key_as_usize(value, "active").unwrap_or(0);
    }
    if let Some(agent) = agent_name {
        let counts = agent_mail_reservation_counts(value, agent);
        summary.active_count = summary.active_count.max(counts.active);
    }
    summary
}

fn swarm_dashboard_rch_summary(
    status: Option<&CommandOutcome>,
    queue: Option<&CommandOutcome>,
) -> SwarmDashboardRchSummary {
    let mut summary = SwarmDashboardRchSummary {
        pressure: "unavailable".to_string(),
        ..SwarmDashboardRchSummary::default()
    };
    let mut combined_text = String::new();
    if let Some(status) = status {
        summary.status_reachable = status.success;
        summary.detail = Some(redacted_output_snippet(status));
        combined_text.push_str(&status.stdout);
        combined_text.push('\n');
        combined_text.push_str(&status.stderr);
    }
    if let Some(queue) = queue {
        summary.queue_reachable = queue.success;
        combined_text.push('\n');
        combined_text.push_str(&queue.stdout);
        combined_text.push('\n');
        combined_text.push_str(&queue.stderr);
    }
    let lower = combined_text.to_ascii_lowercase();
    summary.stale_progress = lower.contains("stale progress");
    summary.active_builds = parse_rch_active_builds(&combined_text);
    let (slots_available, slots_total) = parse_rch_slots(&combined_text);
    summary.slots_available = slots_available;
    summary.slots_total = slots_total;
    summary.pressure = if summary.stale_progress {
        "stale_progress".to_string()
    } else if summary.active_builds.unwrap_or(0) > 0 {
        "busy".to_string()
    } else if matches!(summary.slots_available, Some(0)) {
        "saturated".to_string()
    } else if summary.status_reachable || summary.queue_reachable {
        "clear".to_string()
    } else {
        "unavailable".to_string()
    };
    summary
}

fn parse_rch_active_builds(text: &str) -> Option<u64> {
    text.lines()
        .filter(|line| line.to_ascii_lowercase().contains("active build"))
        .filter_map(|line| extract_u64s(line).into_iter().next())
        .max()
}

fn parse_rch_slots(text: &str) -> (Option<u64>, Option<u64>) {
    text.lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("slots free") || lower.contains("slots available")
        })
        .filter_map(|line| match extract_u64s(line).as_slice() {
            [.., available, total] => Some((*available, *total)),
            _ => None,
        })
        .next_back()
        .map_or((None, None), |(available, total)| {
            (Some(available), Some(total))
        })
}

fn extract_u64s(text: &str) -> Vec<u64> {
    let mut numbers = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            if let Ok(value) = current.parse::<u64>() {
                numbers.push(value);
            }
            current.clear();
        }
    }
    if !current.is_empty()
        && let Ok(value) = current.parse::<u64>()
    {
        numbers.push(value);
    }
    numbers
}

fn option_u64_label(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

fn rch_slots_label(available: Option<u64>, total: Option<u64>) -> String {
    match (available, total) {
        (Some(available), Some(total)) => format!("{available}/{total}"),
        _ => "unknown".to_string(),
    }
}

fn run_probe_json(
    command: SwarmProbeCommand,
    args: &[&str],
    cwd: Option<&Path>,
    label: &str,
) -> std::result::Result<serde_json::Value, String> {
    match run_tool_with_timeout(command, args, cwd, SWARM_PROBE_TIMEOUT) {
        Ok(outcome) if outcome.timed_out => Err(format!(
            "{label} timed out after {}s",
            SWARM_PROBE_TIMEOUT.as_secs()
        )),
        Ok(outcome) if !outcome.success => Err(format!(
            "{label} failed: {}",
            command_failure_detail(&outcome)
        )),
        Ok(outcome) => serde_json::from_str::<serde_json::Value>(&outcome.stdout)
            .map_err(|err| format!("{label} returned non-JSON output: {err}")),
        Err(err) => Err(format!("{label} failed to start: {err}")),
    }
}

fn check_swarm_br_status(cwd: &Path, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Swarm;
    if which_tool("br").is_none() {
        findings.push(
            Finding::warn(cat, "br not found for Beads DB status")
                .with_remediation("Install/repair beads_rust before starting a swarm"),
        );
        return;
    }
    let args = ["sync", "--status", "--json"];
    match run_tool_with_timeout(SwarmProbeCommand::Br, &args, Some(cwd), SWARM_PROBE_TIMEOUT) {
        Ok(outcome) if outcome.timed_out => {
            findings.push(
                Finding::warn(cat, "br sync status timed out").with_remediation(
                    "Run `br doctor --json`; avoid claiming work until Beads responds",
                ),
            );
        }
        Ok(outcome) if outcome.success => {
            match serde_json::from_str::<serde_json::Value>(&outcome.stdout) {
                Ok(value) => findings.push(classify_br_sync_status(&value)),
                Err(err) => findings.push(
                    Finding::warn(cat, "br sync status returned non-JSON output")
                        .with_detail(format!("{err}; {}", redacted_output_snippet(&outcome)))
                        .with_remediation(
                            "Run `br sync --status --json` and `br doctor --json` manually",
                        ),
                ),
            }
        }
        Ok(outcome) => {
            findings.push(
                Finding::fail(cat, "Beads DB/status probe failed")
                    .with_detail(command_failure_detail(&outcome))
                    .with_remediation("Run `br doctor --json` and rebuild from healthy issues.jsonl before claiming more work"),
            );
        }
        Err(err) => {
            findings.push(
                Finding::warn(cat, "br sync status failed to start")
                    .with_detail(err.to_string())
                    .with_remediation("Run `br doctor --json` before claiming more work"),
            );
        }
    }
}

fn classify_br_sync_status(value: &serde_json::Value) -> Finding {
    let dirty_count = json_number_by_key(value, "dirty_count").unwrap_or(0);
    let jsonl_newer = json_bool_by_key(value, "jsonl_newer").unwrap_or(false);
    let db_newer = json_bool_by_key(value, "db_newer").unwrap_or(false);
    let detail =
        format!("dirty_count={dirty_count}, jsonl_newer={jsonl_newer}, db_newer={db_newer}");
    if dirty_count > 0 || jsonl_newer || db_newer {
        Finding::warn(CheckCategory::Swarm, "Beads DB/JSONL sync drift detected")
            .with_detail(detail)
            .with_remediation(
                "Run `br sync --status --json`; coordinate before importing or exporting",
            )
    } else {
        Finding::pass(CheckCategory::Swarm, "Beads DB/JSONL sync status clean").with_detail(detail)
    }
}

fn check_swarm_agent_mail(cwd: &Path, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Swarm;
    let Some(am_path) = which_tool("am") else {
        findings.push(
            Finding::warn(cat, "Agent Mail CLI not found")
                .with_remediation("Install/repair MCP Agent Mail before running a large swarm"),
        );
        return;
    };

    findings.push(Finding::pass(cat, format!("Agent Mail CLI ({am_path})")));

    let agent_name = first_non_empty_env(&["AGENT_MAIL_AGENT", "AGENT_NAME"]);
    let project = cwd.display().to_string();
    let (mail_health, mail_health_error) = read_agent_mail_health(cwd);
    let (agent_roster, agent_roster_error) = read_agent_mail_agents_roster(cwd);
    findings.push(classify_agent_mail_degraded_mode(
        project.as_str(),
        agent_name.as_deref(),
        mail_health.as_ref(),
        mail_health_error.as_deref(),
        agent_roster.as_ref(),
        agent_roster_error.as_deref(),
        Utc::now(),
    ));

    if let Some(agent) = agent_name.as_deref() {
        findings.push(Finding::pass(
            cat,
            format!("Agent Mail agent identity: {agent}"),
        ));
        check_swarm_agent_mail_agent_probes(cwd, project.as_str(), agent, findings);
    } else {
        findings.push(
            Finding::warn(cat, "Agent Mail agent identity not set")
                .with_detail("AGENT_MAIL_AGENT and AGENT_NAME are both unset")
                .with_remediation(
                    "Register with MCP Agent Mail and export AGENT_NAME before claiming swarm work",
                ),
        );
    }

    let reservations_args = [
        "robot",
        "reservations",
        "--format",
        "json",
        "--project",
        project.as_str(),
        "--all",
        "--expiring",
        "30",
    ];
    probe_agent_mail_json(
        SwarmProbeCommand::Am,
        &reservations_args,
        cwd,
        "Agent Mail reservations",
        classify_agent_mail_reservations,
        "Run `am robot reservations --all --format json` and resolve conflicts or renew expiring leases",
        findings,
    );

    check_swarm_agent_mail_build_slots(cwd, agent_name.as_deref(), findings);
}

fn classify_agent_mail_degraded_mode(
    project: &str,
    agent_name: Option<&str>,
    health: Option<&serde_json::Value>,
    health_error: Option<&str>,
    agent_roster: Option<&serde_json::Value>,
    agent_roster_error: Option<&str>,
    now: DateTime<Utc>,
) -> Finding {
    let probe = agent_mail_degraded_mode_probe(
        project,
        agent_name,
        health,
        health_error,
        agent_roster,
        agent_roster_error,
        now,
    );

    if probe.degraded {
        return Finding::warn(
            CheckCategory::Swarm,
            "Agent Mail degraded; Beads fallback required",
        )
        .with_detail(agent_mail_degraded_warning_detail(&probe.data))
        .with_remediation(
            "Use Beads status/comments as the soft lock and keep working; repair Agent Mail with `am doctor check` or archive reconstruction before trusting send/ack/reservation writes",
        )
        .with_data(probe.data);
    }

    Finding::pass(
        CheckCategory::Swarm,
        "Agent Mail degraded-mode fallback not needed",
    )
    .with_detail(agent_mail_ready_detail(&probe.data))
    .with_data(probe.data)
}

struct AgentMailDegradedModeProbe {
    data: serde_json::Value,
    degraded: bool,
}

fn agent_mail_health_string(health: Option<&serde_json::Value>, pointer: &str) -> Option<String> {
    health
        .and_then(|value| value.pointer(pointer))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn agent_mail_degraded_mode_probe(
    project: &str,
    agent_name: Option<&str>,
    health: Option<&serde_json::Value>,
    health_error: Option<&str>,
    agent_roster: Option<&serde_json::Value>,
    agent_roster_error: Option<&str>,
    now: DateTime<Utc>,
) -> AgentMailDegradedModeProbe {
    let missing_schema_tables = health
        .map(agent_mail_missing_schema_tables)
        .unwrap_or_default();
    let health_overall = agent_mail_health_string(health, "/overall");
    let health_status = agent_mail_health_string(health, "/status");
    let health_level = agent_mail_health_string(health, "/health_level");
    let semantic_readiness_status = agent_mail_health_string(health, "/semantic_readiness/status");
    let semantic_readiness_detail = agent_mail_health_string(health, "/semantic_readiness/detail")
        .map(|detail| truncate_chars(&detail, 240));
    let recovery_mode = agent_mail_health_string(health, "/recovery/mode");
    let recovery_next_action = agent_mail_health_string(health, "/recovery/next_action");
    let alert_summaries = health.map(agent_mail_alert_summaries).unwrap_or_default();
    let actions = health.map(agent_mail_actions).unwrap_or_default();
    let agents = agent_roster.map_or_else(SwarmDashboardAgentSummary::default, |value| {
        swarm_dashboard_agent_summary(value, now)
    });
    let health_is_unhealthy = health_overall
        .as_deref()
        .is_some_and(|overall| !overall.eq_ignore_ascii_case("healthy"));
    let health_status_is_unhealthy = health_status.as_deref().is_some_and(|status| {
        ["error", "failed", "fail", "degraded", "red", "unhealthy"]
            .iter()
            .any(|unhealthy| status.eq_ignore_ascii_case(unhealthy))
    });
    let degraded = health_error.is_some()
        || health_is_unhealthy
        || health_status_is_unhealthy
        || !missing_schema_tables.is_empty()
        || agent_roster_error.is_some();
    let blocked_operations = agent_mail_degraded_blocked_operations(&missing_schema_tables);
    let fallback_commands = [
        "br ready --json",
        "br update <id> --status in_progress --assignee <agent>",
        "br close <id> --reason \"Completed\"",
        "br sync --flush-only",
    ];
    let data = serde_json::json!({
        "schema": SWARM_DOCTOR_AGENT_MAIL_DEGRADED_SCHEMA,
        "project": project,
        "agent_name": agent_name,
        "mode": if degraded { "beads_soft_lock_fallback" } else { "agent_mail_ready" },
        "mutation_performed": false,
        "mail_health": {
            "reachable": health.is_some(),
            "overall": health_overall,
            "status": health_status,
            "health_level": health_level,
            "semantic_readiness": {
                "status": semantic_readiness_status,
                "detail": semantic_readiness_detail,
            },
            "recovery": {
                "mode": recovery_mode,
                "next_action": recovery_next_action,
            },
            "missing_schema_tables": missing_schema_tables,
            "alerts": alert_summaries,
            "actions": actions,
            "error": health_error,
        },
        "read_paths": {
            "health": health.is_some(),
            "agents": agent_roster.is_some(),
            "agents_error": agent_roster_error,
        },
        "active_agents": {
            "total_seen": agents.total_seen,
            "active_count": agents.active_count,
            "stale_count": agents.stale_count,
            "truncated_count": agents.truncated_count,
            "rows": swarm_dashboard_agents_json(&agents.rows),
        },
        "write_paths": {
            "expected_failed": !blocked_operations.is_empty(),
            "blocked_operations": blocked_operations,
        },
        "fallback": {
            "soft_lock": "beads",
            "non_blocking": true,
            "commands": fallback_commands,
        },
    });

    AgentMailDegradedModeProbe { data, degraded }
}

fn agent_mail_degraded_blocked_operations(missing_schema_tables: &[String]) -> Vec<&'static str> {
    if missing_schema_tables.is_empty() {
        Vec::new()
    } else {
        vec![
            "register_agent",
            "fetch_inbox",
            "send_message",
            "acknowledge_message",
            "file_reservation_paths",
        ]
    }
}

fn agent_mail_degraded_warning_detail(data: &serde_json::Value) -> String {
    format!(
        "health_reachable={}, overall={}, {}; active_agents={}, stale_agents={}; fallback=beads_soft_lock",
        data.pointer("/mail_health/reachable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        data.pointer("/mail_health/overall")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
        agent_mail_missing_schema_detail(data),
        data.pointer("/active_agents/active_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        data.pointer("/active_agents/stale_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    )
}

fn agent_mail_ready_detail(data: &serde_json::Value) -> String {
    format!(
        "overall={}, active_agents={}, stale_agents={}",
        data.pointer("/mail_health/overall")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
        data.pointer("/active_agents/active_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        data.pointer("/active_agents/stale_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    )
}

fn agent_mail_missing_schema_detail(data: &serde_json::Value) -> String {
    data.pointer("/mail_health/missing_schema_tables")
        .and_then(serde_json::Value::as_array)
        .filter(|tables| !tables.is_empty())
        .map_or_else(
            || "missing_schema_tables=none".to_string(),
            |tables| {
                format!(
                    "missing_schema_tables={}",
                    tables
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            },
        )
}

fn agent_mail_missing_schema_tables(value: &serde_json::Value) -> Vec<String> {
    let mut tables = BTreeSet::new();
    collect_agent_mail_schema_table_mentions(value, &mut tables);
    tables.into_iter().collect()
}

fn collect_agent_mail_schema_table_mentions(
    value: &serde_json::Value,
    tables: &mut BTreeSet<String>,
) {
    match value {
        serde_json::Value::String(text) => {
            let lower = text.to_ascii_lowercase();
            if lower.contains("schema") && lower.contains("missing") {
                for table in ["projects", "agents", "messages", "message_recipients"] {
                    if lower.contains(table) {
                        tables.insert(table.to_string());
                    }
                }
            }
        }
        serde_json::Value::Object(map) => {
            for child in map.values() {
                collect_agent_mail_schema_table_mentions(child, tables);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                collect_agent_mail_schema_table_mentions(child, tables);
            }
        }
        _ => {}
    }
}

fn agent_mail_alert_summaries(value: &serde_json::Value) -> Vec<String> {
    value
        .get("_alerts")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|alert| alert.get("summary").and_then(serde_json::Value::as_str))
        .map(|summary| truncate_chars(summary, 160))
        .collect()
}

fn agent_mail_actions(value: &serde_json::Value) -> Vec<String> {
    value
        .get("_actions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(|action| truncate_chars(action, 180))
        .collect()
}

fn check_swarm_agent_mail_agent_probes(
    cwd: &Path,
    project: &str,
    agent: &str,
    findings: &mut Vec<Finding>,
) {
    let status_args = [
        "robot",
        "status",
        "--format",
        "json",
        "--project",
        project,
        "--agent",
        agent,
    ];
    probe_agent_mail_json(
        SwarmProbeCommand::Am,
        &status_args,
        cwd,
        "Agent Mail status",
        classify_agent_mail_status,
        "Retry after active Agent Mail maintenance finishes; if it stays busy, inspect `am robot health --format json`",
        findings,
    );

    let inbox_args = [
        "robot",
        "inbox",
        "--format",
        "json",
        "--project",
        project,
        "--agent",
        agent,
        "--unread",
        "--limit",
        "20",
    ];
    probe_agent_mail_json(
        SwarmProbeCommand::Am,
        &inbox_args,
        cwd,
        "Agent Mail inbox",
        classify_agent_mail_inbox,
        "Run `am robot inbox --unread --format json` or fetch inbox through MCP before claiming more files",
        findings,
    );

    let contacts_args = [
        "robot",
        "contacts",
        "--format",
        "json",
        "--project",
        project,
        "--agent",
        agent,
    ];
    probe_agent_mail_json(
        SwarmProbeCommand::Am,
        &contacts_args,
        cwd,
        "Agent Mail contacts",
        classify_agent_mail_contacts,
        "Run `am robot contacts --format json` and resolve pending or degraded contact links before relying on coordination mail",
        findings,
    );
}

fn probe_agent_mail_json<F>(
    command: SwarmProbeCommand,
    args: &[&str],
    cwd: &Path,
    label: &str,
    classify: F,
    remediation: &str,
    findings: &mut Vec<Finding>,
) where
    F: FnOnce(&serde_json::Value) -> Finding,
{
    let cat = CheckCategory::Swarm;
    match run_tool_with_timeout(command, args, Some(cwd), SWARM_PROBE_TIMEOUT) {
        Ok(outcome) if outcome.timed_out => {
            findings.push(
                Finding::warn(cat, format!("{label} probe timed out"))
                    .with_detail(format!(
                        "command: am {}; timeout={}s",
                        args.join(" "),
                        SWARM_PROBE_TIMEOUT.as_secs()
                    ))
                    .with_remediation(remediation),
            );
        }
        Ok(outcome) if !outcome.success => {
            findings.push(
                Finding::warn(cat, format!("{label} probe unavailable"))
                    .with_detail(command_failure_detail(&outcome))
                    .with_remediation(remediation),
            );
        }
        Ok(outcome) => match serde_json::from_str::<serde_json::Value>(&outcome.stdout) {
            Ok(value) => findings.push(classify(&value)),
            Err(err) => findings.push(
                Finding::warn(cat, format!("{label} probe returned non-JSON output"))
                    .with_detail(format!("{err}; {}", redacted_output_snippet(&outcome)))
                    .with_remediation(remediation),
            ),
        },
        Err(err) => findings.push(
            Finding::warn(cat, format!("{label} probe failed to start"))
                .with_detail(err.to_string())
                .with_remediation(remediation),
        ),
    }
}

fn classify_agent_mail_status(value: &serde_json::Value) -> Finding {
    let unread = json_number_by_key(value, "unread").unwrap_or(0);
    let urgent = json_number_by_key(value, "urgent").unwrap_or(0);
    let ack_required = json_number_by_key(value, "ack_required").unwrap_or(0);
    let mut finding = if urgent > 0 || ack_required > 0 {
        Finding::warn(CheckCategory::Swarm, "Agent Mail status needs attention")
            .with_remediation("Read urgent or ack-required messages before taking new leases")
    } else {
        Finding::pass(CheckCategory::Swarm, "Agent Mail status reachable")
    };
    finding.detail = Some(format!(
        "unread={unread}, urgent={urgent}, ack_required={ack_required}"
    ));
    finding
}

fn classify_agent_mail_inbox(value: &serde_json::Value) -> Finding {
    let messages = json_array_len_by_key(value, "messages")
        .or_else(|| json_array_len_by_key(value, "items"))
        .unwrap_or_else(|| json_number_by_key_as_usize(value, "unread").unwrap_or(0));
    let urgent = json_number_by_key(value, "urgent").unwrap_or(0);
    if urgent > 0 {
        Finding::warn(
            CheckCategory::Swarm,
            "Agent Mail inbox has urgent unread messages",
        )
        .with_detail(format!("unread_sample={messages}, urgent={urgent}"))
        .with_remediation("Read or acknowledge urgent mail before claiming more files")
    } else {
        Finding::pass(CheckCategory::Swarm, "Agent Mail inbox reachable")
            .with_detail(format!("unread_sample={messages}, urgent={urgent}"))
    }
}

fn classify_agent_mail_reservations(value: &serde_json::Value) -> Finding {
    classify_agent_mail_reservations_at(value, Utc::now())
}

fn classify_agent_mail_reservations_at(value: &serde_json::Value, now: DateTime<Utc>) -> Finding {
    let heatmap = build_agent_mail_reservation_heatmap(value, now);
    let active = json_array_len_by_key(value, "reservations")
        .or_else(|| json_array_len_by_key(value, "items"))
        .unwrap_or_else(|| json_number_by_key_as_usize(value, "active").unwrap_or(heatmap.active));
    let has_conflict = json_truthy_key_contains(value, "conflict") || !heatmap.conflicts.is_empty();
    let expiring = json_number_by_key(value, "expiring")
        .unwrap_or_else(|| usize_to_u64(heatmap.expiring_soon));
    let detail = heatmap.detail(active, expiring);
    let data = heatmap.to_json();

    if has_conflict {
        Finding::warn(
            CheckCategory::Swarm,
            "Agent Mail reservation conflict heatmap has active conflicts",
        )
        .with_detail(detail)
        .with_remediation("Resolve conflicting file reservations before editing overlapping paths")
        .with_data(data)
    } else if !heatmap.stale_recommendations.is_empty() {
        Finding::warn(
            CheckCategory::Swarm,
            "Agent Mail reservation heatmap has stale holder recommendations",
        )
        .with_detail(detail)
        .with_remediation(
            "Contact stale reservation holders and ask them to renew or release before taking overlapping work",
        )
        .with_data(data)
    } else if expiring > 0 {
        Finding::warn(CheckCategory::Swarm, "Agent Mail reservations expire soon")
            .with_detail(detail)
            .with_remediation("Renew active reservations before long-running verification")
            .with_data(data)
    } else {
        Finding::pass(
            CheckCategory::Swarm,
            "Agent Mail reservation conflict heatmap clear",
        )
        .with_detail(detail)
        .with_data(data)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentMailReservationRow {
    path_pattern: String,
    agent: String,
    bead: String,
    reason: Option<String>,
    lease_mode: ReservationLeaseMode,
    activity: ReservationActivity,
    expired: bool,
    released: bool,
    expiring_soon: bool,
    age_hours: Option<i64>,
}

impl AgentMailReservationRow {
    const fn is_exclusive(&self) -> bool {
        matches!(self.lease_mode, ReservationLeaseMode::Exclusive)
    }

    const fn is_active(&self) -> bool {
        matches!(self.activity, ReservationActivity::Active)
    }

    const fn is_recent(&self) -> bool {
        matches!(self.activity, ReservationActivity::RecentInactive)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservationLeaseMode {
    Exclusive,
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservationActivity {
    Active,
    RecentInactive,
    Inactive,
}

#[derive(Debug, Default)]
struct AgentMailReservationHeatmap {
    rows_seen: usize,
    active: usize,
    recent_inactive: usize,
    expired: usize,
    released: usize,
    exclusive_active: usize,
    shared_active: usize,
    expiring_soon: usize,
    path_groups: HashMap<String, ReservationGroupStats>,
    agent_groups: HashMap<String, ReservationGroupStats>,
    bead_groups: HashMap<String, ReservationGroupStats>,
    conflicts: Vec<serde_json::Value>,
    stale_recommendations: Vec<serde_json::Value>,
}

impl AgentMailReservationHeatmap {
    fn detail(&self, active_fallback: usize, expiring_fallback: u64) -> String {
        let active = if self.rows_seen == 0 {
            active_fallback
        } else {
            self.active
        };
        let expiring = if self.rows_seen == 0 {
            expiring_fallback
        } else {
            usize_to_u64(self.expiring_soon).max(expiring_fallback)
        };
        format!(
            "active={}, recent_inactive={}, conflict_groups={}, stale_holder_recommendations={}, expiring_soon={}, top_paths={}",
            active,
            self.recent_inactive,
            self.conflicts.len(),
            self.stale_recommendations.len(),
            expiring,
            self.top_path_summary()
        )
    }

    fn top_path_summary(&self) -> String {
        let mut groups = self.path_groups.values().collect::<Vec<_>>();
        groups.sort_by(|left, right| {
            right
                .conflict_count
                .cmp(&left.conflict_count)
                .then_with(|| right.active_count.cmp(&left.active_count))
                .then_with(|| left.key.cmp(&right.key))
        });
        let parts = groups
            .into_iter()
            .take(SWARM_DETAIL_LIMIT)
            .map(|group| {
                format!(
                    "{} active={} conflicts={}",
                    group.key, group.active_count, group.conflict_count
                )
            })
            .collect::<Vec<_>>();
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join("; ")
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": SWARM_DOCTOR_RESERVATION_HEATMAP_SCHEMA,
            "recent_window_hours": SWARM_RESERVATION_RECENT_HOURS,
            "stale_holder_hours": SWARM_RESERVATION_STALE_HOLDER_HOURS,
            "totals": {
                "rows_seen": self.rows_seen,
                "active_count": self.active,
                "recent_inactive_count": self.recent_inactive,
                "expired_count": self.expired,
                "released_count": self.released,
                "exclusive_active_count": self.exclusive_active,
                "shared_active_count": self.shared_active,
                "expiring_soon_count": self.expiring_soon,
                "conflict_group_count": self.conflicts.len(),
                "stale_holder_recommendation_count": self.stale_recommendations.len()
            },
            "heatmap": {
                "by_path_pattern": reservation_groups_json(&self.path_groups, "path_pattern"),
                "by_agent": reservation_groups_json(&self.agent_groups, "agent"),
                "by_bead": reservation_groups_json(&self.bead_groups, "bead"),
                "conflicts": self.conflicts
            },
            "stale_holder_recommendations": self.stale_recommendations
        })
    }
}

#[derive(Debug, Clone, Default)]
struct ReservationGroupStats {
    key: String,
    active_count: usize,
    recent_inactive_count: usize,
    exclusive_active_count: usize,
    shared_active_count: usize,
    stale_holder_count: usize,
    conflict_count: usize,
    max_age_hours: Option<i64>,
    agents: HashSet<String>,
    beads: HashSet<String>,
}

fn build_agent_mail_reservation_heatmap(
    value: &serde_json::Value,
    now: DateTime<Utc>,
) -> AgentMailReservationHeatmap {
    let rows = agent_mail_reservation_rows(value, now);
    let mut heatmap = AgentMailReservationHeatmap {
        rows_seen: rows.len(),
        ..AgentMailReservationHeatmap::default()
    };

    for row in &rows {
        if row.expired {
            heatmap.expired += 1;
        }
        if row.released {
            heatmap.released += 1;
        }
        if row.expiring_soon {
            heatmap.expiring_soon += 1;
        }
        if row.is_active() {
            heatmap.active += 1;
            if row.is_exclusive() {
                heatmap.exclusive_active += 1;
            } else {
                heatmap.shared_active += 1;
            }
        } else if row.is_recent() {
            heatmap.recent_inactive += 1;
        }
        if row.is_active() || row.is_recent() {
            insert_reservation_group(&mut heatmap.path_groups, &row.path_pattern, row);
            insert_reservation_group(&mut heatmap.agent_groups, &row.agent, row);
            insert_reservation_group(&mut heatmap.bead_groups, &row.bead, row);
        }
        if reservation_stale_reason(row).is_some() {
            heatmap
                .stale_recommendations
                .push(stale_reservation_recommendation(row));
            mark_reservation_stale(&mut heatmap.path_groups, &row.path_pattern);
            mark_reservation_stale(&mut heatmap.agent_groups, &row.agent);
            mark_reservation_stale(&mut heatmap.bead_groups, &row.bead);
        }
    }

    add_reservation_conflicts(&rows, &mut heatmap);
    heatmap
}

fn agent_mail_reservation_rows(
    value: &serde_json::Value,
    now: DateTime<Utc>,
) -> Vec<AgentMailReservationRow> {
    let mut rows = Vec::new();
    collect_agent_mail_reservation_rows(value, now, &mut rows);
    rows
}

fn collect_agent_mail_reservation_rows(
    value: &serde_json::Value,
    now: DateTime<Utc>,
    rows: &mut Vec<AgentMailReservationRow>,
) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(row) = reservation_row_from_map(map, now) {
                rows.push(row);
                return;
            }
            for (key, child) in map {
                if key.to_ascii_lowercase().contains("conflict") {
                    continue;
                }
                collect_agent_mail_reservation_rows(child, now, rows);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                collect_agent_mail_reservation_rows(child, now, rows);
            }
        }
        _ => {}
    }
}

fn reservation_row_from_map(
    map: &serde_json::Map<String, serde_json::Value>,
    now: DateTime<Utc>,
) -> Option<AgentMailReservationRow> {
    let path_pattern = first_string_field(map, &["path_pattern", "pathPattern", "path"])?
        .trim()
        .to_string();
    if path_pattern.is_empty() {
        return None;
    }
    let agent = first_string_field(
        map,
        &["agent", "holder", "agent_name", "holder_agent", "owner"],
    )
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .unwrap_or("<unknown>")
    .to_string();
    let reason = first_string_field(map, &["reason", "note", "description"]).map(str::to_string);
    let bead = reservation_bead(map, reason.as_deref());
    let released_ts = first_string_field(map, &["released_ts", "released_at"]);
    let expires_ts = first_string_field(map, &["expires_ts", "expires_at", "expiration"]);
    let released = released_ts.is_some()
        || first_string_field(map, &["status"]).is_some_and(|status| {
            status.eq_ignore_ascii_case("released") || status.eq_ignore_ascii_case("closed")
        });
    let expires_at = expires_ts.and_then(parse_rfc3339_utc);
    let expired = expires_at.is_some_and(|expires_at| expires_at <= now);
    let active = !released && !expired;
    let terminal_age_hours = released_ts
        .or(expires_ts)
        .and_then(|timestamp| age_hours_since(timestamp, now));
    let activity_age_hours = first_string_field(
        map,
        &[
            "updated_at",
            "updated_ts",
            "renewed_at",
            "last_active_ts",
            "created_at",
            "created_ts",
        ],
    )
    .and_then(|timestamp| age_hours_since(timestamp, now));
    let recent = !active
        && terminal_age_hours
            .is_some_and(|age| (0..=SWARM_RESERVATION_RECENT_HOURS).contains(&age));
    let expiring_soon = active
        && expires_at.is_some_and(|expires_at| {
            let minutes = expires_at.signed_duration_since(now).num_minutes();
            (0..=SWARM_RESERVATION_EXPIRING_SOON_MINUTES).contains(&minutes)
        });

    Some(AgentMailReservationRow {
        path_pattern,
        agent,
        bead,
        reason,
        lease_mode: reservation_lease_mode(map),
        activity: if active {
            ReservationActivity::Active
        } else if recent {
            ReservationActivity::RecentInactive
        } else {
            ReservationActivity::Inactive
        },
        expired,
        released,
        expiring_soon,
        age_hours: if active {
            activity_age_hours
        } else {
            terminal_age_hours.or(activity_age_hours)
        },
    })
}

fn first_string_field<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| map.get(*key).and_then(serde_json::Value::as_str))
}

fn reservation_lease_mode(
    map: &serde_json::Map<String, serde_json::Value>,
) -> ReservationLeaseMode {
    if let Some(exclusive) = map.get("exclusive").and_then(serde_json::Value::as_bool) {
        return if exclusive {
            ReservationLeaseMode::Exclusive
        } else {
            ReservationLeaseMode::Shared
        };
    }
    if first_string_field(map, &["mode", "kind", "reservation_type", "type"]).is_some_and(|mode| {
        matches!(
            mode.to_ascii_lowercase().as_str(),
            "shared" | "non_exclusive" | "non-exclusive" | "observe" | "read"
        )
    }) {
        ReservationLeaseMode::Shared
    } else {
        ReservationLeaseMode::Exclusive
    }
}

fn reservation_bead(
    map: &serde_json::Map<String, serde_json::Value>,
    reason: Option<&str>,
) -> String {
    first_string_field(map, &["bead", "bead_id", "issue_id", "thread_id"])
        .or_else(|| reason.and_then(extract_bead_id))
        .unwrap_or("<untracked>")
        .to_string()
}

fn extract_bead_id(text: &str) -> Option<&str> {
    text.split_whitespace().find_map(|token| {
        let trimmed = token.trim_matches(|ch: char| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_'))
        });
        trimmed.starts_with("bd-").then_some(trimmed)
    })
}

fn insert_reservation_group(
    groups: &mut HashMap<String, ReservationGroupStats>,
    key: &str,
    row: &AgentMailReservationRow,
) {
    let stats = groups
        .entry(key.to_string())
        .or_insert_with(|| ReservationGroupStats {
            key: key.to_string(),
            ..ReservationGroupStats::default()
        });
    if row.is_active() {
        stats.active_count += 1;
        if row.is_exclusive() {
            stats.exclusive_active_count += 1;
        } else {
            stats.shared_active_count += 1;
        }
    } else if row.is_recent() {
        stats.recent_inactive_count += 1;
    }
    if let Some(age_hours) = row.age_hours {
        stats.max_age_hours = Some(
            stats
                .max_age_hours
                .map_or(age_hours, |current| current.max(age_hours)),
        );
    }
    stats.agents.insert(row.agent.clone());
    stats.beads.insert(row.bead.clone());
}

fn mark_reservation_stale(groups: &mut HashMap<String, ReservationGroupStats>, key: &str) {
    if let Some(stats) = groups.get_mut(key) {
        stats.stale_holder_count += 1;
    }
}

fn add_reservation_conflicts(
    rows: &[AgentMailReservationRow],
    heatmap: &mut AgentMailReservationHeatmap,
) {
    let active_rows = rows
        .iter()
        .filter(|row| row.is_active())
        .collect::<Vec<_>>();
    for (index, left) in active_rows.iter().enumerate() {
        for right in active_rows.iter().skip(index + 1) {
            if !reservation_rows_conflict(left, right) {
                continue;
            }
            mark_reservation_conflict(&mut heatmap.path_groups, &left.path_pattern);
            mark_reservation_conflict(&mut heatmap.path_groups, &right.path_pattern);
            mark_reservation_conflict(&mut heatmap.agent_groups, &left.agent);
            mark_reservation_conflict(&mut heatmap.agent_groups, &right.agent);
            mark_reservation_conflict(&mut heatmap.bead_groups, &left.bead);
            mark_reservation_conflict(&mut heatmap.bead_groups, &right.bead);
            heatmap
                .conflicts
                .push(reservation_conflict_json(left, right));
        }
    }
    heatmap
        .conflicts
        .sort_by_key(std::string::ToString::to_string);
}

fn reservation_rows_conflict(
    left: &AgentMailReservationRow,
    right: &AgentMailReservationRow,
) -> bool {
    reservation_patterns_overlap(&left.path_pattern, &right.path_pattern)
        && (left.is_exclusive() || right.is_exclusive())
        && reservation_agents_distinct(&left.agent, &right.agent)
}

fn reservation_agents_distinct(left: &str, right: &str) -> bool {
    left != right || left == "<unknown>" || right == "<unknown>"
}

fn reservation_patterns_overlap(left: &str, right: &str) -> bool {
    left == right || simple_glob_match(left, right) || simple_glob_match(right, left)
}

const fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut pattern_index = 0;
    let mut text_index = 0;
    let mut star_index = None;
    let mut star_text_index = 0;

    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_text_index = text_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_text_index += 1;
            text_index = star_text_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn mark_reservation_conflict(groups: &mut HashMap<String, ReservationGroupStats>, key: &str) {
    if let Some(stats) = groups.get_mut(key) {
        stats.conflict_count += 1;
    }
}

fn reservation_conflict_json(
    left: &AgentMailReservationRow,
    right: &AgentMailReservationRow,
) -> serde_json::Value {
    serde_json::json!({
        "path_patterns": sorted_pair(&left.path_pattern, &right.path_pattern),
        "agents": sorted_pair(&left.agent, &right.agent),
        "beads": sorted_pair(&left.bead, &right.bead),
        "exclusive_involved": left.is_exclusive() || right.is_exclusive(),
        "max_age_hours": max_option_i64(left.age_hours, right.age_hours),
        "reason": "overlapping_active_reservations",
        "recommendation": "Coordinate with holders before editing overlapping files"
    })
}

const fn reservation_stale_reason(row: &AgentMailReservationRow) -> Option<&'static str> {
    if !row.is_active() || !row.is_exclusive() {
        return None;
    }
    match row.age_hours {
        Some(age) if age >= SWARM_RESERVATION_STALE_HOLDER_HOURS => {
            Some("active_exclusive_reservation_older_than_threshold")
        }
        _ => None,
    }
}

fn stale_reservation_recommendation(row: &AgentMailReservationRow) -> serde_json::Value {
    serde_json::json!({
        "agent": row.agent,
        "path_pattern": row.path_pattern,
        "bead": row.bead,
        "reason": reservation_stale_reason(row),
        "age_hours": row.age_hours,
        "reservation_reason": row.reason,
        "suggested_action": "Contact holder and ask them to renew or release before taking overlapping work"
    })
}

fn reservation_groups_json(
    groups: &HashMap<String, ReservationGroupStats>,
    key_name: &str,
) -> Vec<serde_json::Value> {
    let mut values = groups.values().collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .conflict_count
            .cmp(&left.conflict_count)
            .then_with(|| right.active_count.cmp(&left.active_count))
            .then_with(|| left.key.cmp(&right.key))
    });
    values
        .into_iter()
        .map(|stats| reservation_group_json(stats, key_name))
        .collect()
}

fn reservation_group_json(stats: &ReservationGroupStats, key_name: &str) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert(key_name.to_string(), serde_json::json!(stats.key));
    object.insert(
        "active_count".to_string(),
        serde_json::json!(stats.active_count),
    );
    object.insert(
        "recent_inactive_count".to_string(),
        serde_json::json!(stats.recent_inactive_count),
    );
    object.insert(
        "exclusive_active_count".to_string(),
        serde_json::json!(stats.exclusive_active_count),
    );
    object.insert(
        "shared_active_count".to_string(),
        serde_json::json!(stats.shared_active_count),
    );
    object.insert(
        "stale_holder_count".to_string(),
        serde_json::json!(stats.stale_holder_count),
    );
    object.insert(
        "conflict_count".to_string(),
        serde_json::json!(stats.conflict_count),
    );
    object.insert(
        "max_age_hours".to_string(),
        serde_json::json!(stats.max_age_hours),
    );
    object.insert(
        "agents".to_string(),
        serde_json::json!(sorted_set(&stats.agents)),
    );
    object.insert(
        "beads".to_string(),
        serde_json::json!(sorted_set(&stats.beads)),
    );
    serde_json::Value::Object(object)
}

fn sorted_set(values: &HashSet<String>) -> Vec<String> {
    let mut values = values.iter().cloned().collect::<Vec<_>>();
    values.sort();
    values
}

fn sorted_pair<'a>(left: &'a str, right: &'a str) -> Vec<&'a str> {
    if left <= right {
        vec![left, right]
    } else {
        vec![right, left]
    }
}

fn max_option_i64(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[derive(Debug, Default)]
struct AgentMailContactCounts {
    total: usize,
    pending: usize,
    approved: usize,
    blocked: usize,
    degraded: usize,
    unknown_status: usize,
}

fn classify_agent_mail_contacts(value: &serde_json::Value) -> Finding {
    let mut counts = AgentMailContactCounts::default();
    count_agent_mail_contact_rows(value, &mut counts);
    let detail = format!(
        "contacts={}, pending={}, approved={}, blocked={}, degraded={}, unknown_status={}",
        counts.total,
        counts.pending,
        counts.approved,
        counts.blocked,
        counts.degraded,
        counts.unknown_status
    );
    let data = serde_json::json!({
        "schema": SWARM_DOCTOR_CONTACTS_SCHEMA,
        "contact_count": counts.total,
        "pending_count": counts.pending,
        "approved_count": counts.approved,
        "blocked_count": counts.blocked,
        "degraded_count": counts.degraded,
        "unknown_status_count": counts.unknown_status,
    });

    if counts.pending > 0 {
        return Finding::warn(CheckCategory::Swarm, "Agent Mail contact requests pending")
            .with_detail(detail)
            .with_remediation(
                "Run `am robot contacts --format json` and approve, deny, or refresh pending contact requests before relying on contact-gated mail",
            )
            .with_data(data);
    }
    if counts.degraded > 0 {
        return Finding::warn(CheckCategory::Swarm, "Agent Mail contact graph has degraded rows")
            .with_detail(detail)
            .with_remediation(
                "Inspect Agent Mail contact links for unknown agents, unknown policies, or unrecognized statuses before depending on contact-gated routing",
            )
            .with_data(data);
    }

    Finding::pass(CheckCategory::Swarm, "Agent Mail contacts reachable")
        .with_detail(detail)
        .with_data(data)
}

fn count_agent_mail_contact_rows(value: &serde_json::Value, counts: &mut AgentMailContactCounts) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("from") && map.contains_key("to") && map.contains_key("status") {
                classify_agent_mail_contact_row(map, counts);
                return;
            }
            for child in map.values() {
                count_agent_mail_contact_rows(child, counts);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                count_agent_mail_contact_rows(child, counts);
            }
        }
        _ => {}
    }
}

fn classify_agent_mail_contact_row(
    row: &serde_json::Map<String, serde_json::Value>,
    counts: &mut AgentMailContactCounts,
) {
    counts.total += 1;

    let status = row
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let from = row
        .get("from")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    let to = row
        .get("to")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    let policy = row
        .get("policy")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let recognized_status = match status.as_str() {
        "pending" | "requested" => {
            counts.pending += 1;
            true
        }
        "approved" | "accepted" => {
            counts.approved += 1;
            true
        }
        "blocked" | "denied" | "rejected" => {
            counts.blocked += 1;
            true
        }
        _ => {
            counts.unknown_status += 1;
            false
        }
    };

    let unknown_agent = [from, to]
        .iter()
        .any(|agent| agent.is_empty() || agent.starts_with("[unknown-agent-"));
    let unknown_policy = policy.is_empty() || policy == "unknown";
    if unknown_agent || unknown_policy || !recognized_status {
        counts.degraded += 1;
    }
}

fn check_swarm_agent_mail_build_slots(
    cwd: &Path,
    agent_name: Option<&str>,
    findings: &mut Vec<Finding>,
) {
    let Some(agent) = agent_name else {
        findings.push(agent_mail_build_slots_unavailable_finding(
            "AGENT_MAIL_AGENT and AGENT_NAME are both unset",
        ));
        return;
    };

    let project = cwd.display().to_string();
    let args = ["amctl", "env", "-p", project.as_str(), "-a", agent];
    match run_tool_with_timeout(SwarmProbeCommand::Am, &args, Some(cwd), SWARM_PROBE_TIMEOUT) {
        Ok(outcome) if outcome.timed_out => {
            findings.push(agent_mail_build_slots_unavailable_finding(
                "am amctl env timed out before reporting archive paths",
            ));
        }
        Ok(outcome) if !outcome.success => {
            findings.push(
                agent_mail_build_slots_unavailable_finding(
                    "am amctl env did not report archive paths",
                )
                .with_detail(command_failure_detail(&outcome)),
            );
        }
        Ok(outcome) => {
            if let Some(project_archive) = parse_agent_mail_project_archive(&outcome.stdout) {
                let archive = read_agent_mail_build_slot_archive(&project_archive);
                findings.push(classify_agent_mail_build_slots(
                    &archive.values,
                    archive.read_errors,
                    Utc::now(),
                ));
            } else {
                findings.push(
                    agent_mail_build_slots_unavailable_finding(
                        "am amctl env output did not include ARTIFACT_DIR",
                    )
                    .with_detail(redacted_output_snippet(&outcome)),
                );
            }
        }
        Err(err) => {
            findings.push(
                agent_mail_build_slots_unavailable_finding("am amctl env failed to start")
                    .with_detail(err.to_string()),
            );
        }
    }
}

#[derive(Debug, Default)]
struct AgentMailBuildSlotArchive {
    values: Vec<serde_json::Value>,
    read_errors: usize,
}

fn read_agent_mail_build_slot_archive(project_archive: &Path) -> AgentMailBuildSlotArchive {
    let build_slots_dir = project_archive.join("build_slots");
    if !build_slots_dir.exists() {
        return AgentMailBuildSlotArchive::default();
    }

    let mut archive = AgentMailBuildSlotArchive::default();
    let Ok(slot_dirs) = std::fs::read_dir(&build_slots_dir) else {
        archive.read_errors += 1;
        return archive;
    };

    for slot_dir in slot_dirs {
        let Ok(slot_dir) = slot_dir else {
            archive.read_errors += 1;
            continue;
        };
        let Ok(file_type) = slot_dir.file_type() else {
            archive.read_errors += 1;
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        read_agent_mail_build_slot_dir(&slot_dir.path(), &mut archive);
    }

    archive
}

fn read_agent_mail_build_slot_dir(path: &Path, archive: &mut AgentMailBuildSlotArchive) {
    let Ok(entries) = std::fs::read_dir(path) else {
        archive.read_errors += 1;
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            archive.read_errors += 1;
            continue;
        };
        let path = entry.path();
        if !matches!(path.extension().and_then(|ext| ext.to_str()), Some("json")) {
            continue;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        {
            Some(value) => archive.values.push(value),
            None => archive.read_errors += 1,
        }
    }
}

fn parse_agent_mail_project_archive(env_output: &str) -> Option<PathBuf> {
    env_output.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        if key != "ARTIFACT_DIR" {
            return None;
        }
        let artifact_dir = PathBuf::from(trim_matching_shell_quotes(value.trim()));
        artifact_dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    })
}

fn trim_matching_shell_quotes(value: &str) -> &str {
    value
        .strip_prefix('\'')
        .and_then(|stripped| stripped.strip_suffix('\''))
        .or_else(|| {
            value
                .strip_prefix('"')
                .and_then(|stripped| stripped.strip_suffix('"'))
        })
        .unwrap_or(value)
}

#[derive(Debug, Default)]
struct AgentMailBuildSlotCounts {
    total: usize,
    active: usize,
    active_shared: usize,
    active_exclusive: usize,
    soon_expiring: usize,
    stale: usize,
    released: usize,
    malformed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct AgentMailBuildSlotSummary {
    slot: String,
    agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    exclusive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    acquired_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    released_ts: Option<String>,
    classification: String,
}

fn classify_agent_mail_build_slots(
    values: &[serde_json::Value],
    read_errors: usize,
    now: DateTime<Utc>,
) -> Finding {
    let mut counts = AgentMailBuildSlotCounts {
        malformed: read_errors,
        ..AgentMailBuildSlotCounts::default()
    };
    let mut summaries = values
        .iter()
        .map(|value| summarize_agent_mail_build_slot(value, now, &mut counts))
        .collect::<Vec<_>>();
    summaries.sort_by(|left, right| {
        build_slot_classification_rank(&left.classification)
            .cmp(&build_slot_classification_rank(&right.classification))
            .then_with(|| left.slot.cmp(&right.slot))
            .then_with(|| left.agent.cmp(&right.agent))
            .then_with(|| left.branch.cmp(&right.branch))
    });

    let visible_slots = summaries
        .iter()
        .take(SWARM_DETAIL_LIMIT)
        .collect::<Vec<_>>();
    let truncated_slot_count = summaries.len().saturating_sub(visible_slots.len());
    let data = serde_json::json!({
        "schema": SWARM_DOCTOR_BUILD_SLOT_SCHEMA,
        "source": "agent_mail_archive_build_slots",
        "source_supported": true,
        "soon_expiring_minutes": SWARM_BUILD_SLOT_SOON_EXPIRING_MINUTES,
        "total_count": counts.total,
        "active_count": counts.active,
        "active_shared_count": counts.active_shared,
        "active_exclusive_count": counts.active_exclusive,
        "soon_expiring_count": counts.soon_expiring,
        "stale_count": counts.stale,
        "released_count": counts.released,
        "malformed_count": counts.malformed,
        "read_error_count": read_errors,
        "truncated_slot_count": truncated_slot_count,
        "slots": visible_slots
    });
    agent_mail_build_slot_finding(&counts, data)
}

fn summarize_agent_mail_build_slot(
    value: &serde_json::Value,
    now: DateTime<Utc>,
    counts: &mut AgentMailBuildSlotCounts,
) -> AgentMailBuildSlotSummary {
    counts.total += 1;

    let slot = json_string_field(value, "slot");
    let agent = json_string_field(value, "agent");
    let branch = json_string_field(value, "branch");
    let acquired_ts = json_string_field(value, "acquired_ts");
    let expires_ts = json_string_field(value, "expires_ts");
    let released_ts = json_string_field(value, "released_ts");
    let exclusive = value
        .get("exclusive")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let classification = build_slot_classification(
        slot.as_deref(),
        agent.as_deref(),
        expires_ts.as_deref(),
        released_ts.as_deref(),
        now,
    );
    count_build_slot_classification(&classification, exclusive, counts);

    AgentMailBuildSlotSummary {
        slot: slot.unwrap_or_else(|| "unknown".to_string()),
        agent: agent.unwrap_or_else(|| "unknown".to_string()),
        branch,
        exclusive,
        acquired_ts,
        expires_ts,
        released_ts,
        classification,
    }
}

fn build_slot_classification(
    slot: Option<&str>,
    agent: Option<&str>,
    expires_ts: Option<&str>,
    released_ts: Option<&str>,
    now: DateTime<Utc>,
) -> String {
    if released_ts.is_some() {
        return "released".to_string();
    }

    let Some(expires_at) = expires_ts.and_then(parse_rfc3339_utc) else {
        return "malformed".to_string();
    };
    if slot.is_none() || agent.is_none() {
        return "malformed".to_string();
    }
    if expires_at <= now {
        return "stale_expired".to_string();
    }
    let seconds_until_expiry = (expires_at - now).num_seconds();
    if seconds_until_expiry <= SWARM_BUILD_SLOT_SOON_EXPIRING_MINUTES * 60 {
        return "soon_expiring".to_string();
    }
    "active".to_string()
}

fn count_build_slot_classification(
    classification: &str,
    exclusive: bool,
    counts: &mut AgentMailBuildSlotCounts,
) {
    match classification {
        "active" | "soon_expiring" => {
            counts.active += 1;
            if exclusive {
                counts.active_exclusive += 1;
            } else {
                counts.active_shared += 1;
            }
            if classification == "soon_expiring" {
                counts.soon_expiring += 1;
            }
        }
        "stale_expired" => counts.stale += 1,
        "released" => counts.released += 1,
        _ => counts.malformed += 1,
    }
}

fn agent_mail_build_slot_finding(
    counts: &AgentMailBuildSlotCounts,
    data: serde_json::Value,
) -> Finding {
    let detail = format!(
        "active={}, shared={}, exclusive={}, soon_expiring={}, stale={}, malformed={}",
        counts.active,
        counts.active_shared,
        counts.active_exclusive,
        counts.soon_expiring,
        counts.stale,
        counts.malformed
    );

    if counts.malformed > 0 {
        return Finding::warn(
            CheckCategory::Swarm,
            "Agent Mail build-slot records malformed",
        )
        .with_detail(detail)
        .with_remediation(
            "Inspect Agent Mail build_slots archive records before relying on slot posture",
        )
        .with_data(data);
    }
    if counts.active_exclusive > 0 {
        return Finding::warn(CheckCategory::Swarm, "Agent Mail exclusive build slots active")
            .with_detail(detail)
            .with_remediation(
                "Wait for exclusive build-slot holders to finish or coordinate before launching heavyweight cargo/RCH work",
            )
            .with_data(data);
    }
    if counts.soon_expiring > 0 {
        return Finding::warn(CheckCategory::Swarm, "Agent Mail build slots expire soon")
            .with_detail(detail)
            .with_remediation("Renew build-slot leases before long-running cargo/RCH work")
            .with_data(data);
    }
    if counts.active > 0 {
        return Finding::info(CheckCategory::Swarm, "Agent Mail shared build slots active")
            .with_detail(detail)
            .with_data(data);
    }
    if counts.stale > 0 {
        return Finding::info(
            CheckCategory::Swarm,
            "Agent Mail stale build-slot records present",
        )
        .with_detail(detail)
        .with_data(data);
    }
    Finding::pass(CheckCategory::Swarm, "Agent Mail build slots clear")
        .with_detail(detail)
        .with_data(data)
}

fn agent_mail_build_slots_unavailable_finding(reason: &str) -> Finding {
    Finding::info(CheckCategory::Swarm, "Agent Mail build slots unavailable")
        .with_detail(reason)
        .with_data(serde_json::json!({
            "schema": SWARM_DOCTOR_BUILD_SLOT_SCHEMA,
            "source": "agent_mail_archive_build_slots",
            "source_supported": false,
            "reason": reason
        }))
}

fn build_slot_classification_rank(classification: &str) -> u8 {
    match classification {
        "malformed" => 0,
        "active" => 1,
        "soon_expiring" => 2,
        "stale_expired" => 3,
        "released" => 4,
        _ => 5,
    }
}

fn json_string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncate_chars(value, 120))
}

fn parse_rfc3339_utc(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn check_swarm_git(cwd: &Path, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Swarm;
    if which_tool("git").is_none() {
        findings.push(
            Finding::warn(cat, "git not found for swarm dirty-state check")
                .with_remediation("Install git or run the dirty-state check manually"),
        );
        return;
    }
    let args = ["status", "--porcelain=v1", "--untracked-files=all"];
    match run_tool_with_timeout(
        SwarmProbeCommand::Git,
        &args,
        Some(cwd),
        SWARM_PROBE_TIMEOUT,
    ) {
        Ok(outcome) if outcome.timed_out => {
            findings.push(
                Finding::warn(cat, "git status timed out")
                    .with_remediation("Run `git status --short` before staging or committing"),
            );
        }
        Ok(outcome) if outcome.success => {
            let summary = summarize_git_porcelain(&outcome.stdout);
            if summary.total.eq(&0) {
                findings.push(Finding::pass(cat, "Git working tree clean"));
            } else {
                findings.push(
                    Finding::warn(cat, "Git working tree has uncommitted changes")
                        .with_detail(format!(
                            "{} total ({} staged, {} unstaged, {} untracked, {} deleted)",
                            summary.total,
                            summary.staged,
                            summary.unstaged,
                            summary.untracked,
                            summary.deleted
                        ))
                        .with_remediation(
                            "Run `git status --short` and avoid overwriting other agents' files",
                        ),
                );
            }
        }
        Ok(outcome) => {
            findings.push(
                Finding::warn(cat, "git status failed")
                    .with_detail(command_failure_detail(&outcome))
                    .with_remediation("Run `git status --short` manually before staging"),
            );
        }
        Err(err) => {
            findings.push(
                Finding::warn(cat, "git status failed to start")
                    .with_detail(err.to_string())
                    .with_remediation("Run `git status --short` manually before staging"),
            );
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GitPorcelainSummary {
    staged: usize,
    unstaged: usize,
    untracked: usize,
    deleted: usize,
    total: usize,
}

fn summarize_git_porcelain(output: &str) -> GitPorcelainSummary {
    let mut summary = GitPorcelainSummary::default();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        summary.total += 1;
        let bytes = line.as_bytes();
        let x = bytes.first().copied().unwrap_or(b' ');
        let y = bytes.get(1).copied().unwrap_or(b' ');
        if x.eq(&b'?') && y.eq(&b'?') {
            summary.untracked += 1;
            continue;
        }
        if x.ne(&b' ') {
            summary.staged += 1;
        }
        if y.ne(&b' ') {
            summary.unstaged += 1;
        }
        if x.eq(&b'D') || y.eq(&b'D') {
            summary.deleted += 1;
        }
    }
    summary
}

fn check_swarm_rch(findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Swarm;
    if which_tool("rch").is_none() {
        findings.push(
            Finding::warn(cat, "rch not found").with_remediation(
                "Install/repair rch before running heavyweight swarm verification",
            ),
        );
        return;
    }
    match run_tool_with_timeout(
        SwarmProbeCommand::Rch,
        &["status"],
        None,
        SWARM_PROBE_TIMEOUT,
    ) {
        Ok(outcome) if outcome.timed_out => {
            findings.push(Finding::warn(cat, "rch status timed out").with_remediation(
                "Run `rch status` or defer heavyweight cargo checks until workers respond",
            ));
        }
        Ok(outcome) if outcome.success => {
            findings.push(
                Finding::pass(cat, "rch status reachable")
                    .with_detail(redacted_output_snippet(&outcome)),
            );
        }
        Ok(outcome) => findings.push(rch_failure_finding(&outcome)),
        Err(err) => {
            findings.push(
                Finding::warn(cat, "rch status failed to start")
                    .with_detail(err.to_string())
                    .with_remediation("Run `rch doctor` and use a high-capacity local target dir if rch is unavailable"),
            );
        }
    }
}

fn check_swarm_rch_affinity(cwd: &Path, findings: &mut Vec<Finding>) {
    let current_git_commit = current_git_commit(cwd);
    let finding = match build_rch_affinity_plan_from_env(current_git_commit.as_deref()) {
        Ok(plan) => classify_rch_affinity_plan(&plan),
        Err(err) => rch_affinity_parse_error_finding(&err, current_git_commit.as_deref()),
    };
    findings.push(finding);
}

#[derive(Debug, Clone, Deserialize)]
struct RchAffinityJobSpec {
    id: String,
    command: String,
    #[serde(default)]
    worker: Option<String>,
    #[serde(default)]
    target_dir: Option<String>,
    #[serde(default)]
    git_commit: Option<String>,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    worker_available: Option<bool>,
    #[serde(default)]
    target_available_kb: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RchAffinityKey {
    worker: String,
    target_dir: String,
    git_commit: String,
    command_family: String,
    profile: String,
    features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RchAffinityJob {
    id: String,
    command: String,
    key: RchAffinityKey,
    worker_available: Option<bool>,
    target_available_kb: Option<u64>,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RchAffinityGroup {
    key: RchAffinityKey,
    job_ids: Vec<String>,
    recommendation: &'static str,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RchAffinityPlan {
    source: &'static str,
    current_git_commit: Option<String>,
    recommended_target_dir: String,
    jobs: Vec<RchAffinityJob>,
    groups: Vec<RchAffinityGroup>,
    blockers: Vec<String>,
    notes: Vec<String>,
}

type RchAffinityTargetCompatibility<'a> = (&'a str, &'a str, Vec<String>);

fn build_rch_affinity_plan_from_env(
    current_git_commit: Option<&str>,
) -> std::result::Result<RchAffinityPlan, String> {
    let recommended_target_dir = default_rch_affinity_target_dir();
    let current_git_commit = current_git_commit.map(str::to_string);
    let raw_jobs = match std::env::var(SWARM_RCH_AFFINITY_JOBS_ENV) {
        Ok(raw) if raw.trim().is_empty() => None,
        Ok(raw) => Some(raw),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(format!(
                "{SWARM_RCH_AFFINITY_JOBS_ENV} contains non-UTF-8 data"
            ));
        }
    };

    let Some(raw_jobs) = raw_jobs else {
        return Ok(RchAffinityPlan {
            source: "no_job_specs",
            current_git_commit,
            recommended_target_dir,
            jobs: Vec::new(),
            groups: Vec::new(),
            blockers: vec!["no_job_specs".to_string()],
            notes: vec![format!(
                "Set {SWARM_RCH_AFFINITY_JOBS_ENV} to a JSON array of cargo jobs before launching a swarm"
            )],
        });
    };

    let specs = serde_json::from_str::<Vec<RchAffinityJobSpec>>(&raw_jobs)
        .map_err(|err| format!("{SWARM_RCH_AFFINITY_JOBS_ENV} is not a valid job array: {err}"))?;
    Ok(build_rch_affinity_plan_from_specs(
        specs,
        current_git_commit,
        recommended_target_dir,
    ))
}

fn build_rch_affinity_plan_from_specs(
    specs: Vec<RchAffinityJobSpec>,
    current_git_commit: Option<String>,
    recommended_target_dir: String,
) -> RchAffinityPlan {
    let mut blockers = Vec::new();
    let mut notes = Vec::new();
    let mut jobs = Vec::new();

    for spec in specs {
        let id = spec.id.trim();
        let command = spec.command.trim();
        if id.is_empty() {
            blockers.push("job_with_empty_id".to_string());
            continue;
        }
        if command.is_empty() {
            blockers.push(format!("{id}:empty_command"));
            continue;
        }

        let worker = normalized_affinity_field(spec.worker.as_deref(), "unassigned");
        let target_dir = spec
            .target_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map_or_else(|| recommended_target_dir.clone(), str::to_string);
        let git_commit = spec
            .git_commit
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| current_git_commit.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let command_family = rch_affinity_command_family(command);
        let profile = rch_affinity_profile(command);
        let features = rch_affinity_features(command, &spec.features);

        let mut reasons = Vec::new();
        if spec
            .worker
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            reasons.push("worker_unassigned".to_string());
        }
        if spec
            .target_dir
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            reasons.push("uses_recommended_target_dir".to_string());
        }
        let (safety_blockers, safety_reasons) = rch_affinity_job_safety_findings(
            id,
            &spec,
            &target_dir,
            &git_commit,
            current_git_commit.as_deref(),
        );
        blockers.extend(safety_blockers);
        reasons.extend(safety_reasons);

        jobs.push(RchAffinityJob {
            id: id.to_string(),
            command: command.to_string(),
            key: RchAffinityKey {
                worker,
                target_dir,
                git_commit,
                command_family,
                profile,
                features,
            },
            worker_available: spec.worker_available,
            target_available_kb: spec.target_available_kb,
            reasons,
        });
    }

    blockers.extend(rch_affinity_target_conflicts(&jobs));
    if jobs.is_empty() {
        blockers.push("no_valid_jobs".to_string());
    }
    if current_git_commit.is_none() {
        notes.push("current_git_commit_unavailable".to_string());
    }

    let groups = rch_affinity_groups(&jobs);
    RchAffinityPlan {
        source: "env_job_specs",
        current_git_commit,
        recommended_target_dir,
        jobs,
        groups,
        blockers,
        notes,
    }
}

fn rch_affinity_job_safety_findings(
    id: &str,
    spec: &RchAffinityJobSpec,
    target_dir: &str,
    git_commit: &str,
    current_git_commit: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let mut blockers = Vec::new();
    let mut reasons = Vec::new();

    if spec.worker_available == Some(false) {
        blockers.push(format!("{id}:worker_unavailable"));
        reasons.push("worker_unavailable".to_string());
    }
    if !path_under_swarm_scratch_root(Path::new(target_dir)) {
        blockers.push(format!("{id}:target_dir_outside_swarm_scratch_root"));
        reasons.push("target_dir_outside_swarm_scratch_root".to_string());
    }
    if current_git_commit.is_some_and(|current| git_commit != current) {
        blockers.push(format!("{id}:git_commit_differs_from_current_checkout"));
        reasons.push("git_commit_differs_from_current_checkout".to_string());
    }
    if spec
        .target_available_kb
        .is_some_and(|available| available < SWARM_DISK_WARN_AVAILABLE_KB)
    {
        blockers.push(format!("{id}:retrieval_headroom_low"));
        reasons.push("retrieval_headroom_low".to_string());
    }

    (blockers, reasons)
}

fn rch_affinity_groups(jobs: &[RchAffinityJob]) -> Vec<RchAffinityGroup> {
    let mut grouped: BTreeMap<RchAffinityKey, Vec<&RchAffinityJob>> = BTreeMap::new();
    for job in jobs {
        grouped.entry(job.key.clone()).or_default().push(job);
    }

    grouped
        .into_iter()
        .map(|(key, jobs)| {
            let mut reasons = jobs
                .iter()
                .flat_map(|job| job.reasons.iter().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let recommendation = if jobs.len() > 1 {
                reasons.push("same_worker_target_commit_features_profile_and_family".to_string());
                "share_warm_target"
            } else {
                "single_job_or_wait_for_compatible_peer"
            };
            RchAffinityGroup {
                key,
                job_ids: jobs.iter().map(|job| job.id.clone()).collect(),
                recommendation,
                reasons,
            }
        })
        .collect()
}

fn rch_affinity_target_conflicts(jobs: &[RchAffinityJob]) -> Vec<String> {
    let mut by_target: BTreeMap<&str, BTreeSet<RchAffinityTargetCompatibility<'_>>> =
        BTreeMap::new();
    for job in jobs {
        by_target
            .entry(job.key.target_dir.as_str())
            .or_default()
            .insert((
                job.key.git_commit.as_str(),
                job.key.profile.as_str(),
                job.key.features.clone(),
            ));
    }

    by_target
        .into_iter()
        .filter_map(|(target_dir, compatibility_keys)| {
            (compatibility_keys.len() > 1).then(|| format!("target_dir_conflict:{target_dir}"))
        })
        .collect()
}

fn classify_rch_affinity_plan(plan: &RchAffinityPlan) -> Finding {
    let data = rch_affinity_plan_data(plan);
    let shareable_groups = plan
        .groups
        .iter()
        .filter(|group| group.recommendation == "share_warm_target")
        .count();
    let detail = format!(
        "jobs={}, groups={}, shareable_groups={}, blockers={}",
        plan.jobs.len(),
        plan.groups.len(),
        shareable_groups,
        plan.blockers.len()
    );

    if plan.source == "no_job_specs" {
        return Finding::info(CheckCategory::Swarm, "RCH warm-target affinity plan needs job specs")
            .with_detail(detail)
            .with_remediation(format!(
                "Set {SWARM_RCH_AFFINITY_JOBS_ENV} before swarm launches to preview safe worker/target reuse"
            ))
            .with_data(data);
    }
    if !plan.blockers.is_empty() {
        return Finding::warn(CheckCategory::Swarm, "RCH warm-target affinity plan has blockers")
            .with_detail(detail)
            .with_remediation(
                "Separate incompatible jobs by target dir, align features/profile/commit, or assign a worker before sharing warm targets",
            )
            .with_data(data);
    }

    Finding::pass(CheckCategory::Swarm, "RCH warm-target affinity plan ready")
        .with_detail(detail)
        .with_data(data)
}

fn rch_affinity_parse_error_finding(err: &str, current_git_commit: Option<&str>) -> Finding {
    Finding::warn(
        CheckCategory::Swarm,
        "RCH warm-target affinity plan unavailable",
    )
    .with_detail(err.to_string())
    .with_remediation(format!(
        "Fix {SWARM_RCH_AFFINITY_JOBS_ENV} JSON or unset it to use the default no-job diagnostic"
    ))
    .with_data(serde_json::json!({
        "schema": SWARM_DOCTOR_RCH_AFFINITY_SCHEMA,
        "source": "env_job_specs",
        "current_git_commit": current_git_commit,
        "job_count": 0,
        "groups": [],
        "blockers": [err],
        "notes": ["invalid_job_specs"]
    }))
}

fn rch_affinity_plan_data(plan: &RchAffinityPlan) -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_DOCTOR_RCH_AFFINITY_SCHEMA,
        "source": plan.source,
        "job_spec_env": SWARM_RCH_AFFINITY_JOBS_ENV,
        "current_git_commit": &plan.current_git_commit,
        "scratch_root": SWARM_CARGO_SCRATCH_ROOT,
        "recommended_target_dir": &plan.recommended_target_dir,
        "job_count": plan.jobs.len(),
        "jobs": plan.jobs.iter().map(rch_affinity_job_json).collect::<Vec<_>>(),
        "groups": plan.groups.iter().enumerate().map(rch_affinity_group_json).collect::<Vec<_>>(),
        "blockers": &plan.blockers,
        "notes": &plan.notes,
        "proof_gate": rch_affinity_proof_gate_json(plan),
        "template": {
            "id": "cargo-test-tools",
            "command": "rch exec -- cargo test --test tools_conformance",
            "worker": "vmi-worker-id",
            "target_dir": format!("{SWARM_CARGO_SCRATCH_ROOT}/<agent>/target"),
            "git_commit": &plan.current_git_commit,
            "features": ["default"]
        }
    })
}

fn rch_affinity_job_json(job: &RchAffinityJob) -> serde_json::Value {
    serde_json::json!({
        "id": &job.id,
        "command": &job.command,
        "worker": &job.key.worker,
        "target_dir": &job.key.target_dir,
        "git_commit": &job.key.git_commit,
        "command_family": &job.key.command_family,
        "profile": &job.key.profile,
        "features": &job.key.features,
        "worker_available": job.worker_available,
        "target_available_kb": job.target_available_kb,
        "reasons": &job.reasons,
    })
}

fn rch_affinity_group_json((idx, group): (usize, &RchAffinityGroup)) -> serde_json::Value {
    serde_json::json!({
        "group_id": format!("rch-affinity-{}", idx + 1),
        "job_ids": &group.job_ids,
        "worker": &group.key.worker,
        "target_dir": &group.key.target_dir,
        "git_commit": &group.key.git_commit,
        "command_family": &group.key.command_family,
        "profile": &group.key.profile,
        "features": &group.key.features,
        "recommendation": group.recommendation,
        "reasons": &group.reasons,
    })
}

fn rch_affinity_proof_gate_json(plan: &RchAffinityPlan) -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_RCH_AFFINITY_PLAN_ARTIFACT_SCHEMA,
        "diagnostic_only": true,
        "mutation_performed": false,
        "source": plan.source,
        "safe_to_launch": plan.source != "no_job_specs" && plan.blockers.is_empty(),
        "current_git_commit": &plan.current_git_commit,
        "recommended_target_dir": &plan.recommended_target_dir,
        "cache_reuse": plan
            .groups
            .iter()
            .enumerate()
            .map(rch_affinity_cache_reuse_json)
            .collect::<Vec<_>>(),
        "isolation": rch_affinity_isolation_decisions(plan),
        "blockers": plan
            .blockers
            .iter()
            .map(|blocker| rch_affinity_blocker_json(blocker))
            .collect::<Vec<_>>(),
    })
}

fn rch_affinity_cache_reuse_json((idx, group): (usize, &RchAffinityGroup)) -> serde_json::Value {
    let safe_to_reuse = group.recommendation == "share_warm_target"
        && !group
            .reasons
            .iter()
            .any(|reason| rch_affinity_reason_blocks_reuse(reason));
    let decision = if safe_to_reuse {
        "reuse_warm_target"
    } else if group.recommendation == "share_warm_target" {
        "block_reuse_until_inputs_are_safe"
    } else {
        "isolate_or_wait_for_compatible_peer"
    };

    serde_json::json!({
        "group_id": format!("rch-affinity-{}", idx + 1),
        "decision": decision,
        "safe_to_reuse": safe_to_reuse,
        "job_ids": &group.job_ids,
        "worker": &group.key.worker,
        "target_dir": &group.key.target_dir,
        "git_commit": &group.key.git_commit,
        "command_family": &group.key.command_family,
        "profile": &group.key.profile,
        "features": &group.key.features,
        "reasons": &group.reasons,
    })
}

fn rch_affinity_isolation_decisions(plan: &RchAffinityPlan) -> Vec<serde_json::Value> {
    let mut decisions = plan
        .blockers
        .iter()
        .filter_map(|blocker| {
            blocker
                .strip_prefix("target_dir_conflict:")
                .map(|target_dir| {
                    serde_json::json!({
                        "reason": "target_dir_conflict",
                        "target_dir": target_dir,
                        "recommended_action": "assign distinct target dirs or align commit/profile/features before reuse"
                    })
                })
        })
        .collect::<Vec<_>>();

    decisions.extend(
        plan.groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.recommendation != "share_warm_target")
            .map(|(idx, group)| {
                serde_json::json!({
                    "reason": "no_compatible_peer",
                    "group_id": format!("rch-affinity-{}", idx + 1),
                    "job_ids": &group.job_ids,
                    "target_dir": &group.key.target_dir,
                    "recommended_action": "keep isolated or wait for another job with matching worker, target, commit, profile, command family, and features"
                })
            }),
    );

    decisions
}

fn rch_affinity_blocker_json(blocker: &str) -> serde_json::Value {
    let family = rch_affinity_blocker_family(blocker);
    serde_json::json!({
        "id": blocker,
        "family": family,
        "recommended_action": rch_affinity_blocker_action(family),
    })
}

fn rch_affinity_blocker_family(blocker: &str) -> &'static str {
    if blocker == "no_job_specs" {
        return "no_job_specs";
    }
    if blocker == "no_valid_jobs" {
        return "no_valid_jobs";
    }
    if blocker.starts_with("target_dir_conflict:") {
        return "target_dir_conflict";
    }
    if blocker.ends_with(":target_dir_outside_swarm_scratch_root") {
        return "target_dir_outside_swarm_scratch_root";
    }
    if blocker.ends_with(":git_commit_differs_from_current_checkout") {
        return "stale_git_commit";
    }
    if blocker.ends_with(":worker_unavailable") {
        return "worker_unavailable";
    }
    if blocker.ends_with(":retrieval_headroom_low") {
        return "retrieval_headroom_low";
    }
    if blocker.ends_with(":empty_command") {
        return "empty_command";
    }
    if blocker == "job_with_empty_id" {
        return "empty_job_id";
    }
    "unknown"
}

fn rch_affinity_blocker_action(family: &str) -> &'static str {
    match family {
        "no_job_specs" => "provide job specs before launching an RCH swarm",
        "no_valid_jobs" => "provide at least one valid cargo job spec",
        "target_dir_conflict" => {
            "assign distinct target dirs or align commit/profile/features before reuse"
        }
        "target_dir_outside_swarm_scratch_root" => {
            "move CARGO_TARGET_DIR under /data/tmp/pi_agent_rust_cargo/<agent>/target"
        }
        "stale_git_commit" => "rebase or regenerate the job spec for the current checkout",
        "worker_unavailable" => "assign an available RCH worker or keep the job queued",
        "retrieval_headroom_low" => {
            "switch to a target dir with enough local retrieval headroom before launch"
        }
        "empty_command" => "fill in the cargo command for this job",
        "empty_job_id" => "give every job a stable id before planning",
        _ => "inspect the raw blocker before launching RCH work",
    }
}

fn rch_affinity_reason_blocks_reuse(reason: &str) -> bool {
    matches!(
        reason,
        "worker_unavailable"
            | "target_dir_outside_swarm_scratch_root"
            | "git_commit_differs_from_current_checkout"
            | "retrieval_headroom_low"
    )
}

fn rch_affinity_command_family(command: &str) -> String {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let mut idx = 0usize;
    if words.get(idx) == Some(&"rch") && words.get(idx + 1) == Some(&"exec") {
        idx += 2;
        if words.get(idx) == Some(&"--") {
            idx += 1;
        }
    }
    if words.get(idx) == Some(&"cargo") {
        return words.get(idx + 1).map_or_else(
            || "cargo unknown".to_string(),
            |subcommand| format!("cargo {subcommand}"),
        );
    }
    words
        .get(idx)
        .map_or_else(|| "unknown".to_string(), |word| (*word).to_string())
}

fn rch_affinity_profile(command: &str) -> String {
    if command.split_whitespace().any(|word| word == "--release") {
        "release".to_string()
    } else {
        "debug".to_string()
    }
}

fn rch_affinity_features(command: &str, explicit_features: &[String]) -> Vec<String> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let mut features = explicit_features
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();

    let mut idx = 0usize;
    while idx < words.len() {
        match words[idx] {
            "--all-features" => {
                features.insert("all-features".to_string());
            }
            "--no-default-features" => {
                features.insert("no-default-features".to_string());
            }
            "--features" => {
                if let Some(value) = words.get(idx + 1) {
                    for feature in value.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                        features.insert(feature.to_string());
                    }
                    idx += 1;
                }
            }
            value if value.starts_with("--features=") => {
                let value = value.trim_start_matches("--features=");
                for feature in value.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                    features.insert(feature.to_string());
                }
            }
            _ => {}
        }
        idx += 1;
    }

    if features.is_empty() {
        features.insert("default".to_string());
    }
    features.into_iter().collect()
}

fn default_rch_affinity_target_dir() -> String {
    let agent = first_non_empty_env(&["AGENT_NAME", "AGENT_MAIL_AGENT", "USER"])
        .unwrap_or_else(|| "agent".to_string());
    format!(
        "{}/{}/target",
        SWARM_CARGO_SCRATCH_ROOT,
        sanitize_path_component(&agent)
    )
}

fn normalized_affinity_field(value: Option<&str>, fallback: &str) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(|| fallback.to_string(), str::to_string)
}

fn sanitize_path_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "agent".to_string()
    } else {
        sanitized
    }
}

fn current_git_commit(cwd: &Path) -> Option<String> {
    let outcome = run_tool_with_timeout(
        SwarmProbeCommand::Git,
        &["rev-parse", "HEAD"],
        Some(cwd),
        SWARM_PROBE_TIMEOUT,
    )
    .ok()?;
    outcome
        .success
        .then(|| outcome.stdout.trim().to_string())
        .filter(|commit| !commit.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RchFailureKind {
    ArtifactRetrievalDiskPressure,
    LocalTargetDiskPressure,
    RemoteBuildOrTestFailure,
    Unknown,
}

impl RchFailureKind {
    const fn label(self) -> &'static str {
        match self {
            Self::ArtifactRetrievalDiskPressure => "artifact_retrieval_disk_pressure",
            Self::LocalTargetDiskPressure => "local_target_tmpdir_disk_pressure",
            Self::RemoteBuildOrTestFailure => "remote_build_or_test_failure",
            Self::Unknown => "unknown_rch_failure",
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::ArtifactRetrievalDiskPressure => {
                "rch artifact retrieval failed after remote success"
            }
            Self::LocalTargetDiskPressure => "rch local target/TMPDIR disk pressure",
            Self::RemoteBuildOrTestFailure => "rch remote build/test failure",
            Self::Unknown => "rch status failed",
        }
    }

    const fn code_regression(self) -> Option<bool> {
        match self {
            Self::ArtifactRetrievalDiskPressure | Self::LocalTargetDiskPressure => Some(false),
            Self::RemoteBuildOrTestFailure => Some(true),
            Self::Unknown => None,
        }
    }

    const fn fail_closed(self) -> bool {
        matches!(self, Self::Unknown)
    }

    const fn remediation(self) -> &'static str {
        match self {
            Self::ArtifactRetrievalDiskPressure => {
                "Remote execution appears to have completed; rerun with CARGO_TARGET_DIR and TMPDIR under /data/tmp/pi_agent_rust_cargo/<agent>/ so artifact retrieval has local headroom"
            }
            Self::LocalTargetDiskPressure => {
                "Export CARGO_TARGET_DIR=/data/tmp/pi_agent_rust_cargo/<agent>/target and TMPDIR=/data/tmp/pi_agent_rust_cargo/<agent>/tmp, create both dirs, then rerun the RCH command"
            }
            Self::RemoteBuildOrTestFailure => {
                "Treat this as a real compile/test failure; inspect the cargo error and fix the code or test before rerunning"
            }
            Self::Unknown => {
                "Run `rch doctor`, keep the raw RCH status visible, and do not classify this as disk pressure or a code regression until the failure signature is understood"
            }
        }
    }
}

fn rch_failure_finding(outcome: &CommandOutcome) -> Finding {
    let kind = classify_rch_failure(outcome);
    let detail = command_failure_detail(outcome);
    let code_regression = kind.code_regression();
    let data = serde_json::json!({
        "schema": SWARM_DOCTOR_RCH_FAILURE_SCHEMA,
        "classification": kind.label(),
        "code_regression": code_regression,
        "fail_closed": kind.fail_closed(),
        "raw_status": {
            "exit_code": outcome.status_code,
            "timed_out": outcome.timed_out,
            "success": outcome.success
        },
        "evidence": redacted_output_snippet(outcome),
        "remediation": kind.remediation()
    });

    Finding::warn(CheckCategory::Swarm, kind.title())
        .with_detail(detail)
        .with_remediation(kind.remediation())
        .with_data(data)
}

fn classify_rch_failure(outcome: &CommandOutcome) -> RchFailureKind {
    let text = rch_failure_text(outcome);
    let has_disk_pressure = contains_any(
        &text,
        &[
            "no space left on device",
            "enospc",
            "disk quota exceeded",
            "not enough space",
        ],
    );
    let mentions_retrieval = contains_any(
        &text,
        &[
            "artifact retrieval",
            "retrieve artifact",
            "retrieving artifact",
            "download artifact",
            "fetch artifact",
            "copy artifact",
            "sync artifact",
            "failed to retrieve",
            "failed to download",
            "failed to copy",
            "failed to sync",
        ],
    );
    let remote_succeeded = contains_any(
        &text,
        &[
            "remote command succeeded",
            "remote build succeeded",
            "remote build completed",
            "remote execution succeeded",
            "build completed successfully",
            "completed successfully",
            "exit status 0",
        ],
    );

    if mentions_retrieval && (has_disk_pressure || remote_succeeded) {
        return RchFailureKind::ArtifactRetrievalDiskPressure;
    }

    if has_disk_pressure
        && contains_any(
            &text,
            &[
                "cargo_target_dir",
                "tmpdir",
                "target/debug",
                "target/release",
                "/target/",
                "target dir",
                "temporary directory",
                "temp dir",
            ],
        )
    {
        return RchFailureKind::LocalTargetDiskPressure;
    }

    if contains_any(
        &text,
        &[
            "could not compile",
            "compilation failed",
            "cargo check failed",
            "cargo clippy failed",
            "cargo test failed",
            "test result: failed",
            "error[",
            "error:",
        ],
    ) {
        return RchFailureKind::RemoteBuildOrTestFailure;
    }

    RchFailureKind::Unknown
}

fn rch_failure_text(outcome: &CommandOutcome) -> String {
    format!("{}\n{}", outcome.stdout, outcome.stderr).to_ascii_lowercase()
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn check_swarm_temp_dirs(findings: &mut Vec<Finding>) {
    check_swarm_temp_dir("CARGO_TARGET_DIR", findings);
    check_swarm_temp_dir("TMPDIR", findings);
}

fn check_swarm_temp_dir(env_name: &str, findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Swarm;
    let Some(raw_path) = std::env::var_os(env_name) else {
        findings.push(
            Finding::warn(cat, format!("{env_name} is not set"))
                .with_detail(format!(
                    "Heavyweight swarm checks should use {SWARM_CARGO_SCRATCH_ROOT}/<agent>"
                ))
                .with_remediation(format!(
                    "Export CARGO_TARGET_DIR and TMPDIR under {SWARM_CARGO_SCRATCH_ROOT}/<agent>/ before cargo checks"
                ))
                .with_data(swarm_temp_dir_data(env_name, None, false, None)),
        );
        return;
    };
    let path = PathBuf::from(raw_path);
    if !path.is_dir() {
        findings.push(
            Finding::warn(cat, format!("{env_name} does not point to a directory"))
                .with_detail(path.display().to_string())
                .with_remediation(format!(
                    "Create {} or export {env_name} to an existing high-capacity directory",
                    path.display()
                ))
                .with_data(swarm_temp_dir_data(env_name, Some(&path), false, None)),
        );
        return;
    }

    match disk_available_kb(&path) {
        Ok(available_kb) => {
            findings.push(swarm_temp_dir_finding(env_name, &path, available_kb, None));
        }
        Err(err) => {
            let detail = err.to_string();
            findings.push(swarm_temp_dir_finding(
                env_name,
                &path,
                None,
                Some(detail.as_str()),
            ));
        }
    }
}

fn swarm_temp_dir_finding(
    env_name: &str,
    path: &Path,
    available_kb: Option<u64>,
    probe_error: Option<&str>,
) -> Finding {
    let cat = CheckCategory::Swarm;
    let data = swarm_temp_dir_data(env_name, Some(path), true, available_kb);

    if let Some(available_kb) = available_kb
        && available_kb < SWARM_DISK_WARN_AVAILABLE_KB
    {
        return Finding::warn(cat, format!("{env_name} has low free space"))
            .with_detail(format!(
                "{} available at {}",
                format_available_kb(available_kb),
                path.display()
            ))
            .with_remediation(
                "Switch to a larger /data/tmp target or wait for cleanup before heavy cargo checks",
            )
            .with_data(data);
    }

    if !path_under_swarm_scratch_root(path) {
        let detail = available_kb.map_or_else(
            || {
                format!(
                    "{}; expected path under {SWARM_CARGO_SCRATCH_ROOT}",
                    path.display()
                )
            },
            |available_kb| {
                format!(
                    "{} available at {}; expected path under {SWARM_CARGO_SCRATCH_ROOT}",
                    format_available_kb(available_kb),
                    path.display()
                )
            },
        );
        return Finding::warn(cat, format!("{env_name} is outside swarm scratch root"))
            .with_detail(detail)
            .with_remediation(format!(
                "Export {env_name} under {SWARM_CARGO_SCRATCH_ROOT}/<agent>/ before heavyweight RCH or cargo checks"
            ))
            .with_data(data);
    }

    if let Some(available_kb) = available_kb {
        return Finding::pass(cat, format!("{env_name} headroom"))
            .with_detail(format!(
                "{} available at {}",
                format_available_kb(available_kb),
                path.display()
            ))
            .with_data(data);
    }

    if let Some(error) = probe_error {
        return Finding::info(cat, format!("{env_name} headroom probe failed"))
            .with_detail(error.to_string())
            .with_remediation("Run `df -h` on the configured path before heavy cargo checks")
            .with_data(data);
    }

    Finding::info(cat, format!("{env_name} headroom unavailable"))
        .with_detail(path.display().to_string())
        .with_remediation("Run `df -h` on the configured path before heavy cargo checks")
        .with_data(data)
}

fn swarm_temp_dir_data(
    env_name: &str,
    path: Option<&Path>,
    exists: bool,
    available_kb: Option<u64>,
) -> serde_json::Value {
    serde_json::json!({
        "schema": SWARM_DOCTOR_TEMP_DIR_SCHEMA,
        "env_name": env_name,
        "path": path.map(|p| p.display().to_string()),
        "exists": exists,
        "expected_root": SWARM_CARGO_SCRATCH_ROOT,
        "under_expected_root": path.map(path_under_swarm_scratch_root),
        "available_kb": available_kb,
        "warn_available_kb": SWARM_DISK_WARN_AVAILABLE_KB,
        "recommended_pattern": format!("{SWARM_CARGO_SCRATCH_ROOT}/<agent>/target or {SWARM_CARGO_SCRATCH_ROOT}/<agent>/tmp"),
    })
}

fn path_under_swarm_scratch_root(path: &Path) -> bool {
    path.starts_with(Path::new(SWARM_CARGO_SCRATCH_ROOT))
}

fn disk_available_kb(path: &Path) -> std::io::Result<Option<u64>> {
    if which_tool("df").is_none() {
        return Ok(None);
    }
    let path_arg = path.display().to_string();
    let outcome = run_tool_with_timeout(
        SwarmProbeCommand::Df,
        &["-Pk", path_arg.as_str()],
        None,
        SWARM_PROBE_TIMEOUT,
    )?;
    if outcome.success {
        Ok(parse_df_available_kb(&outcome.stdout))
    } else {
        Ok(None)
    }
}

fn parse_df_available_kb(output: &str) -> Option<u64> {
    output
        .lines()
        .skip(1)
        .find_map(|line| line.split_whitespace().nth(3)?.parse::<u64>().ok())
}

fn format_available_kb(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        let tenths = kb.saturating_mul(10) / (1024 * 1024);
        format!("{}.{:01} GiB", tenths / 10, tenths % 10)
    } else {
        let tenths = kb.saturating_mul(10) / 1024;
        format!("{}.{:01} MiB", tenths / 10, tenths % 10)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutcome {
    timed_out: bool,
    success: bool,
    status_code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwarmProbeCommand {
    Am,
    Br,
    Bv,
    Df,
    Git,
    Rch,
}

fn run_tool_with_timeout(
    tool: SwarmProbeCommand,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> std::io::Result<CommandOutcome> {
    let mut command = match tool {
        SwarmProbeCommand::Am => Command::new("am"),
        SwarmProbeCommand::Br => Command::new("br"),
        SwarmProbeCommand::Bv => Command::new("bv"),
        SwarmProbeCommand::Df => Command::new("df"),
        SwarmProbeCommand::Git => Command::new("git"),
        SwarmProbeCommand::Rch => Command::new("rch"),
    };
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn()?;
    let started = Instant::now();
    let mut timed_out = false;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    let output = child.wait_with_output()?;
    Ok(CommandOutcome {
        timed_out,
        success: output.status.success(),
        status_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn command_failure_detail(outcome: &CommandOutcome) -> String {
    format!(
        "exit={:?}; {}",
        outcome.status_code,
        redacted_output_snippet(outcome)
    )
}

fn redacted_output_snippet(outcome: &CommandOutcome) -> String {
    let stdout = redact_sensitive_lines(outcome.stdout.trim(), 3);
    if !stdout.is_empty() {
        return stdout;
    }
    let stderr = redact_sensitive_lines(outcome.stderr.trim(), 3);
    if !stderr.is_empty() {
        return stderr;
    }
    "no output".to_string()
}

fn redact_sensitive_lines(text: &str, max_lines: usize) -> String {
    text.lines()
        .take(max_lines)
        .map(|line| {
            if line_is_sensitive(line) {
                "[redacted sensitive output line]".to_string()
            } else {
                truncate_chars(line.trim(), 220)
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn line_is_sensitive(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "authorization",
        "bearer",
        "credential",
        "password",
        "secret",
        "token",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn json_number_by_key(value: &serde_json::Value, key: &str) -> Option<u64> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(found) = map.get(key).and_then(json_value_as_u64) {
                return Some(found);
            }
            map.values()
                .find_map(|child| json_number_by_key(child, key))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|child| json_number_by_key(child, key)),
        _ => None,
    }
}

fn json_number_by_key_as_usize(value: &serde_json::Value, key: &str) -> Option<usize> {
    json_number_by_key(value, key).and_then(|number| usize::try_from(number).ok())
}

fn json_array_len_by_key(value: &serde_json::Value, key: &str) -> Option<usize> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Array(values)) = map.get(key) {
                return Some(values.len());
            }
            map.values()
                .find_map(|child| json_array_len_by_key(child, key))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|child| json_array_len_by_key(child, key)),
        _ => None,
    }
}

fn json_bool_by_key(value: &serde_json::Value, key: &str) -> Option<bool> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Bool(found)) = map.get(key) {
                return Some(*found);
            }
            map.values().find_map(|child| json_bool_by_key(child, key))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|child| json_bool_by_key(child, key))
        }
        _ => None,
    }
}

fn json_truthy_key_contains(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, child)| {
            (key.to_ascii_lowercase().contains(needle) && json_value_is_truthy(child))
                || json_truthy_key_contains(child, needle)
        }),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|child| json_truthy_key_contains(child, needle)),
        _ => false,
    }
}

fn json_value_as_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::Array(values) => Some(values.len() as u64),
        _ => None,
    }
}

fn json_value_is_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Number(number) => number.as_u64().is_some_and(|value| value > 0),
        serde_json::Value::String(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            !normalized.is_empty()
                && normalized.ne("0")
                && normalized.ne("false")
                && normalized.ne("none")
        }
        serde_json::Value::Array(values) => !values.is_empty(),
        serde_json::Value::Object(map) => !map.is_empty(),
        serde_json::Value::Null => false,
    }
}

// ── Check: Sessions ─────────────────────────────────────────────────

fn check_sessions(findings: &mut Vec<Finding>) {
    let cat = CheckCategory::Sessions;
    let sessions_dir = Config::sessions_dir();

    if !sessions_dir.is_dir() {
        findings.push(Finding::info(
            cat,
            "Sessions directory does not exist (no sessions yet)",
        ));
        return;
    }

    let entries = walk_sessions(&sessions_dir);
    let total = entries.len().min(500); // Cap scan
    let mut corrupt = 0u32;

    for entry in entries.into_iter().take(500) {
        let Ok(path) = entry else {
            corrupt += 1;
            continue;
        };
        if !is_session_healthy(&path) {
            corrupt += 1;
        }
    }

    if corrupt.eq(&0) {
        findings.push(Finding::pass(cat, format!("{total} sessions, 0 corrupt")));
    } else {
        findings.push(
            Finding::warn(cat, format!("{total} sessions, {corrupt} corrupt"))
                .with_detail("Some session files are empty or have invalid headers")
                .with_remediation("Corrupt sessions can be safely deleted"),
        );
    }
}

/// Quick health check: non-empty and first line parses as a valid session header.
fn is_session_healthy(path: &Path) -> bool {
    #[cfg(feature = "sqlite-sessions")]
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq("sqlite"))
    {
        return futures::executor::block_on(async {
            crate::session_sqlite::load_session_meta(path)
                .await
                .is_ok_and(|meta| meta.header.is_valid())
        });
    }

    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => false, // empty or unreadable
        Ok(_) => serde_json::from_str::<SessionHeader>(&line).is_ok_and(|header| header.is_valid()),
    }
}

// ── Check: Extension ────────────────────────────────────────────────

fn check_extension(
    cwd: &Path,
    path: &str,
    policy_override: Option<&str>,
    findings: &mut Vec<Finding>,
) {
    use crate::extension_preflight::{FindingSeverity, PreflightAnalyzer, PreflightVerdict};

    let cat = CheckCategory::Extensions;
    let ext_path = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };

    if !ext_path.exists() {
        findings.push(
            Finding::fail(
                cat,
                format!("Extension path not found: {}", ext_path.display()),
            )
            .with_remediation("Check the path and try again"),
        );
        return;
    }

    let config_path = Config::config_path_override_from_env(cwd);
    let resolved = match Config::load_with_roots(config_path.as_deref(), &Config::global_dir(), cwd)
    {
        Ok(config) => config.resolve_extension_policy_with_metadata(policy_override),
        Err(err) => {
            findings.push(
                Finding::fail(
                    cat,
                    "Failed to load configuration for extension policy resolution",
                )
                .with_detail(err.to_string())
                .with_remediation(
                    "Fix the malformed settings.json, point PI_CONFIG_PATH at a valid file, or rerun with `--policy <safe|balanced|permissive>` to inspect extension compatibility independently",
                ),
            );
            let has_explicit_policy =
                policy_override.is_some() || std::env::var_os("PI_EXTENSION_POLICY").is_some();
            if has_explicit_policy {
                Config::default().resolve_extension_policy_with_metadata(policy_override)
            } else {
                // If project config is unreadable, fail closed instead of silently
                // analyzing under the default permissive profile.
                Config::default().resolve_extension_policy_with_metadata(Some("safe"))
            }
        }
    };
    let ext_id = ext_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let analyzer = PreflightAnalyzer::new(&resolved.policy, Some(ext_id));
    let report = analyzer.analyze(&ext_path);

    // Convert preflight verdict to a top-level finding
    match report.verdict {
        PreflightVerdict::Pass => {
            findings.push(Finding::pass(
                cat,
                format!("Extension {ext_id}: compatible"),
            ));
        }
        PreflightVerdict::Warn => {
            findings.push(
                Finding::warn(cat, format!("Extension {ext_id}: partial compatibility"))
                    .with_detail(format!(
                        "{} warning(s), {} info",
                        report.summary.warnings, report.summary.info
                    )),
            );
        }
        PreflightVerdict::Fail => {
            findings.push(
                Finding::fail(cat, format!("Extension {ext_id}: incompatible"))
                    .with_detail(format!(
                        "{} error(s), {} warning(s)",
                        report.summary.errors, report.summary.warnings
                    ))
                    .with_remediation(format!("Try: pi doctor {path} --policy permissive")),
            );
        }
    }

    // Convert individual preflight findings
    for pf in &report.findings {
        let severity = match pf.severity {
            FindingSeverity::Error => Severity::Fail,
            FindingSeverity::Warning => Severity::Warn,
            FindingSeverity::Info => Severity::Info,
        };
        let mut f = Finding {
            category: cat,
            severity,
            title: pf.message.clone(),
            detail: pf.file.as_ref().map(|file| {
                pf.line
                    .map_or_else(|| format!("at {file}"), |line| format!("at {file}:{line}"))
            }),
            remediation: pf.remediation.clone(),
            data: None,
            fixability: Fixability::NotFixable,
        };
        // Ensure we don't lose location info
        if f.detail.is_none() && pf.file.is_some() {
            f.detail.clone_from(&pf.file);
        }
        findings.push(f);
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm_progress_slo::{
        AgentMailHealth, FreshnessState, ProgressSloEvaluationInput, ProgressSloMetrics,
        ProgressSloSourceStatus, ProgressSloTimeWindow, RchPosture, RedactionState,
        SourceAvailability, ValidationBrokerPosture, evaluate_progress_slo,
    };
    use std::path::{Path, PathBuf};

    fn write_extension_fixture(cwd: &Path, source: &str) -> PathBuf {
        let extension_dir = cwd.join("ext");
        std::fs::create_dir_all(&extension_dir).expect("create extension dir");
        std::fs::write(extension_dir.join("index.js"), source).expect("write extension source");
        extension_dir
    }

    fn test_swarm_capacity_plan() -> SwarmCapacityPlan {
        let evidence = SwarmCapacityEvidenceSummary {
            complete_records: 5,
            host_capacity_rows: 5,
            host_capacity_mismatch_rows: 0,
            max_p99_ms: 100,
            max_p999_ms: 300,
            max_queue_depth: 16,
            max_rss_mb: 256,
            max_cpu_pct: 40.0,
        };
        SwarmCapacityPlannerConfig::default()
            .plan_from_summary(evidence, SwarmHostInventory::new(8, 8, 32_768))
            .expect("test capacity plan")
    }

    fn healthy_resource_sample() -> HostResourceSample {
        HostResourceSample {
            load_avg_1m: Some(1.0),
            rss_bytes: Some(128 * MIB_BYTES),
            process_count: Some(16),
            fd_count: Some(64),
        }
    }

    fn finding_data(finding: &Finding) -> &serde_json::Value {
        finding.data.as_ref().expect("structured finding data")
    }

    fn agent_mail_schema_corrupt_fixture()
    -> std::result::Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(include_str!(
            "../tests/fixtures/agent_mail/schema_corrupt_health.json"
        ))
    }

    fn build_slot_test_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-09T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn heatmap_test_now() -> DateTime<Utc> {
        build_slot_test_now()
    }

    #[test]
    fn severity_ordering() {
        assert!(Severity::Pass < Severity::Info);
        assert!(Severity::Info < Severity::Warn);
        assert!(Severity::Warn < Severity::Fail);
    }

    #[test]
    fn severity_display() {
        assert_eq!(Severity::Pass.to_string(), "PASS");
        assert_eq!(Severity::Fail.to_string(), "FAIL");
    }

    #[test]
    fn check_category_parse() {
        assert_eq!(
            "config".parse::<CheckCategory>().unwrap(),
            CheckCategory::Config
        );
        assert_eq!(
            "dirs".parse::<CheckCategory>().unwrap(),
            CheckCategory::Dirs
        );
        assert_eq!(
            "directories".parse::<CheckCategory>().unwrap(),
            CheckCategory::Dirs
        );
        assert_eq!(
            "auth".parse::<CheckCategory>().unwrap(),
            CheckCategory::Auth
        );
        assert_eq!(
            "shell".parse::<CheckCategory>().unwrap(),
            CheckCategory::Shell
        );
        assert_eq!(
            "sessions".parse::<CheckCategory>().unwrap(),
            CheckCategory::Sessions
        );
        assert_eq!(
            "swarm".parse::<CheckCategory>().unwrap(),
            CheckCategory::Swarm
        );
        assert_eq!(
            "coordination".parse::<CheckCategory>().unwrap(),
            CheckCategory::Swarm
        );
        assert_eq!(
            "leases".parse::<CheckCategory>().unwrap(),
            CheckCategory::Swarm
        );
        assert_eq!(
            "extensions".parse::<CheckCategory>().unwrap(),
            CheckCategory::Extensions
        );
        assert_eq!(
            "ext".parse::<CheckCategory>().unwrap(),
            CheckCategory::Extensions
        );
        assert!("unknown".parse::<CheckCategory>().is_err());
    }

    #[test]
    fn finding_builders() {
        let f = Finding::pass(CheckCategory::Config, "test")
            .with_detail("detail")
            .with_remediation("fix it");
        assert_eq!(f.severity, Severity::Pass);
        assert_eq!(f.detail.as_deref(), Some("detail"));
        assert_eq!(f.remediation.as_deref(), Some("fix it"));

        let f = Finding::warn(CheckCategory::Auth, "warn test").auto_fixable();
        assert_eq!(f.fixability, Fixability::AutoFixable);

        let f = Finding::fail(CheckCategory::Dirs, "fail test").fixed();
        assert_eq!(f.severity, Severity::Pass); // fixed downgrades to pass
        assert_eq!(f.fixability, Fixability::Fixed);
    }

    #[test]
    fn report_summary() {
        let findings = vec![
            Finding::pass(CheckCategory::Config, "ok"),
            Finding::info(CheckCategory::Auth, "info"),
            Finding::warn(CheckCategory::Shell, "warn"),
            Finding::fail(CheckCategory::Dirs, "fail"),
        ];
        let report = DoctorReport::from_findings(findings);
        assert_eq!(report.summary.pass, 1);
        assert_eq!(report.summary.info, 1);
        assert_eq!(report.summary.warn, 1);
        assert_eq!(report.summary.fail, 1);
        assert_eq!(report.overall, Severity::Fail);
    }

    #[test]
    fn report_all_pass() {
        let findings = vec![
            Finding::pass(CheckCategory::Config, "a"),
            Finding::pass(CheckCategory::Dirs, "b"),
        ];
        let report = DoctorReport::from_findings(findings);
        assert_eq!(report.overall, Severity::Pass);
    }

    #[test]
    fn render_text_includes_header() {
        let report =
            DoctorReport::from_findings(vec![Finding::pass(CheckCategory::Config, "all good")]);
        let text = report.render_text();
        assert!(text.contains("Pi Doctor"));
        assert!(text.contains("[PASS] Configuration"));
        assert!(text.contains("[PASS] all good"));
    }

    #[test]
    fn render_text_includes_swarm_category() {
        let report =
            DoctorReport::from_findings(vec![Finding::pass(CheckCategory::Swarm, "ready")]);
        let text = report.render_text();
        assert!(text.contains("[PASS] Swarm Coordination"));
        assert!(text.contains("[PASS] ready"));
    }

    #[test]
    fn render_json_valid() {
        let report = DoctorReport::from_findings(vec![Finding::pass(CheckCategory::Config, "ok")]);
        let json = report.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("findings").is_some());
        assert!(parsed.get("summary").is_some());
        assert!(parsed.get("overall").is_some());
    }

    #[test]
    fn render_markdown_includes_header() {
        let report =
            DoctorReport::from_findings(vec![Finding::warn(CheckCategory::Auth, "expired")]);
        let md = report.render_markdown();
        assert!(md.contains("# Pi Doctor Report"));
        assert!(md.contains("## Authentication"));
    }

    #[test]
    fn known_config_keys_includes_common() {
        assert!(is_known_config_key("theme"));
        assert!(is_known_config_key("defaultModel"));
        assert!(is_known_config_key("extensionPolicy"));
        assert!(!is_known_config_key("nonexistent_key_xyz"));
    }

    #[test]
    fn swarm_beads_summary_detects_stale_in_progress() {
        let now = DateTime::parse_from_rfc3339("2026-05-08T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let content = r#"{"id":"bd-old","title":"Old owner","status":"in_progress","updated_at":"2026-05-07T11:00:00Z"}
{"id":"bd-fresh","title":"Fresh owner","status":"in_progress","updated_at":"2026-05-08T11:30:00Z"}
{"id":"bd-open","title":"Open work","status":"open","updated_at":"2026-05-08T10:00:00Z"}
"#;

        let summary = summarize_beads_ledger(content, now, 24);

        assert_eq!(summary.total, 3);
        assert_eq!(summary.active, 3);
        assert_eq!(summary.open, 1);
        assert_eq!(summary.in_progress, 2);
        assert_eq!(summary.parse_errors, 0);
        assert_eq!(summary.stale_in_progress.len(), 1);
        assert_eq!(summary.stale_in_progress[0].id.as_str(), "bd-old");
    }

    #[test]
    fn swarm_beads_summary_counts_parse_errors() {
        let now = DateTime::parse_from_rfc3339("2026-05-08T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let content = r#"{"id":"bd-open","title":"Open work","status":"open","updated_at":"2026-05-08T10:00:00Z"}
not-json
"#;

        let summary = summarize_beads_ledger(content, now, 24);

        assert_eq!(summary.total, 2);
        assert_eq!(summary.open, 1);
        assert_eq!(summary.parse_errors, 1);
    }

    #[test]
    fn stalled_bead_reaper_keeps_recent_and_active_work() {
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let content = r#"{"id":"bd-active","title":"Active owner","status":"in_progress","assignee":"ActiveAgent","updated_at":"2026-05-08T06:00:00Z"}
{"id":"bd-recent","title":"Recent work","status":"in_progress","assignee":"QuietAgent","updated_at":"2026-05-09T11:00:00Z"}
"#;
        let roster = serde_json::json!({
            "agents": [
                {"name": "ActiveAgent", "last_active_ts": "2026-05-09T11:30:00Z"},
                {"name": "QuietAgent", "last_active_ts": "2026-05-08T00:00:00Z"}
            ]
        });

        let finding = classify_stalled_bead_reaper(content, Some(&roster), None, now, 24);

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding_data(&finding);
        assert_eq!(data["candidate_count"], serde_json::json!(0));
        assert_eq!(data["active_agent_count"], serde_json::json!(1));
        assert_eq!(data["recently_updated_count"], serde_json::json!(1));
        assert_eq!(data["mutation_performed"], serde_json::json!(false));
    }

    #[test]
    fn stalled_bead_reaper_keeps_blocked_notes_as_review_items() {
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let content = r#"{"id":"bd-blocked","title":"Blocked work","status":"in_progress","assignee":"OldAgent","notes":"Blocked by bd-prereq; do not reopen until evidence lands.","updated_at":"2026-05-07T00:00:00Z"}
"#;
        let roster = serde_json::json!({
            "agents": [
                {"name": "OldAgent", "last_active_ts": "2026-05-07T00:00:00Z"}
            ]
        });

        let finding = classify_stalled_bead_reaper(content, Some(&roster), None, now, 24);

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding_data(&finding);
        assert_eq!(data["candidate_count"], serde_json::json!(0));
        assert_eq!(data["blocked_by_note_count"], serde_json::json!(1));
        assert_eq!(
            data["suggestions"][0]["action"],
            serde_json::json!("keep_in_progress_and_review_blocker_note")
        );
    }

    #[test]
    fn stalled_bead_reaper_suggests_reopen_for_truly_stalled_work() {
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let content = r#"{"id":"bd-stalled","title":"Quiet work","status":"in_progress","assignee":"OldAgent","updated_at":"2026-05-07T00:00:00Z"}
"#;
        let roster = serde_json::json!({
            "agents": [
                {"name": "OldAgent", "last_active_ts": "2026-05-07T00:00:00Z"}
            ]
        });

        let finding = classify_stalled_bead_reaper(content, Some(&roster), None, now, 24);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.title.contains("reopen candidates"));
        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_STALLED_REAPER_SCHEMA);
        assert_eq!(data["mode"], serde_json::json!("audit_only"));
        assert_eq!(data["candidate_count"], serde_json::json!(1));
        assert_eq!(
            data["suggestions"][0]["suggested_commands"][0],
            serde_json::json!("br update bd-stalled --status=open")
        );
        assert_eq!(
            data["suggestions"][0]["notification_draft"]["to"][0],
            serde_json::json!("OldAgent")
        );
    }

    #[test]
    fn swarm_conflict_predictor_degrades_when_agent_mail_reservations_are_unavailable() {
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let content = r#"{"id":"bd-mail","title":"Build cross-agent conflict predictor","description":"Agent Mail corrupt; Beads fallback reservations conflict dry-run","status":"in_progress","assignee":"DarkGoose","updated_at":"2026-05-09T11:00:00Z","labels":["agent-mail","beads","swarm"]}
"#;

        let plan = build_swarm_conflict_prediction_plan(
            SwarmConflictPredictorInputs {
                beads_content: Some(content),
                reservations_error: Some("database disk image is malformed".to_string()),
                git_porcelain: Some(""),
                ..SwarmConflictPredictorInputs::default()
            },
            now,
        );
        let finding = classify_swarm_conflict_prediction_plan(&plan);

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_CONFLICT_PREDICTOR_SCHEMA);
        assert_eq!(
            data["input_sources"]["agent_mail_reservations"],
            serde_json::json!("unavailable")
        );
        assert_eq!(
            data["reservation_recommendations"]["schema"],
            SWARM_DOCTOR_RESERVATION_RECOMMENDATIONS_SCHEMA
        );
        assert_eq!(
            data["reservation_recommendations"]["diagnostic_only"],
            serde_json::json!(true)
        );
        assert_eq!(
            data["reservation_recommendations"]["mail_available"],
            serde_json::json!(false)
        );
        assert!(
            data["reservation_recommendations"]["degraded_caveats"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("agent_mail_reservations_unavailable"))
        );
        assert!(
            data["source_errors"][0]
                .as_str()
                .unwrap_or_default()
                .contains("database disk image")
        );
        assert!(
            plan.predictions
                .iter()
                .any(
                    |prediction| prediction.path_pattern == ".beads/issues.jsonl"
                        && prediction.has_reason("agent_mail_reservations_unavailable")
                )
        );
    }

    #[test]
    fn swarm_conflict_predictor_exposes_healthy_reservation_recommendations() {
        let now = heatmap_test_now();
        let content = r#"{"id":"bd-tools","title":"Tool harness update","description":"read edit grep tool conformance","status":"open","labels":["tools","testing"]}
"#;
        let reservations = serde_json::json!({
            "reservations": [
                {
                    "path_pattern": "src/tools.rs",
                    "agent": "BlueStone",
                    "reason": "bd-tools",
                    "exclusive": true,
                    "expires_ts": "2026-05-09T09:00:00Z",
                    "created_at": "2026-05-09T07:00:00Z"
                }
            ]
        });

        let plan = build_swarm_conflict_prediction_plan(
            SwarmConflictPredictorInputs {
                beads_content: Some(content),
                reservations: Some(&reservations),
                git_porcelain: Some(""),
                ..SwarmConflictPredictorInputs::default()
            },
            now,
        );
        let finding = classify_swarm_conflict_prediction_plan(&plan);
        let data = finding_data(&finding);
        let recommendations = &data["reservation_recommendations"];

        assert_eq!(recommendations["mail_available"], serde_json::json!(true));
        assert!(
            recommendations["degraded_caveats"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            recommendations["avoid"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["path_pattern"] == "src/tools.rs"
                    && entry["suggested_reservation"] == "src/tools.rs"
                    && entry["bead_families"]
                        .as_array()
                        .unwrap()
                        .contains(&serde_json::json!("tools"))
                    && entry["agents"]
                        .as_array()
                        .unwrap()
                        .contains(&serde_json::json!("BlueStone")))
        );
    }

    #[test]
    fn swarm_conflict_predictor_reports_empty_ready_queue_recommendations() {
        let now = heatmap_test_now();
        let reservations = serde_json::json!({"reservations": []});

        let plan = build_swarm_conflict_prediction_plan(
            SwarmConflictPredictorInputs {
                reservations: Some(&reservations),
                git_porcelain: Some(""),
                ..SwarmConflictPredictorInputs::default()
            },
            now,
        );
        let finding = classify_swarm_conflict_prediction_plan(&plan);
        let data = finding_data(&finding);
        let recommendations = &data["reservation_recommendations"];

        assert_eq!(finding.severity, Severity::Pass);
        assert_eq!(
            recommendations["schema"],
            SWARM_DOCTOR_RESERVATION_RECOMMENDATIONS_SCHEMA
        );
        assert!(recommendations["avoid"].as_array().unwrap().is_empty());
        assert!(recommendations["observe"].as_array().unwrap().is_empty());
        assert_eq!(
            recommendations["safe_fallback_lanes"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn swarm_conflict_predictor_scores_overlapping_reservation_globs() {
        let now = heatmap_test_now();
        let reservations = serde_json::json!({
            "reservations": [
                {
                    "path_pattern": "src/**",
                    "agent": "CalmBridge",
                    "reason": "bd-parent",
                    "exclusive": true,
                    "expires_ts": "2026-05-09T09:00:00Z",
                    "created_at": "2026-05-09T07:00:00Z"
                },
                {
                    "path_pattern": "src/session.rs",
                    "agent": "DarkGoose",
                    "reason": "bd-session",
                    "exclusive": true,
                    "expires_ts": "2026-05-09T09:00:00Z",
                    "created_at": "2026-05-09T07:30:00Z"
                }
            ]
        });

        let plan = build_swarm_conflict_prediction_plan(
            SwarmConflictPredictorInputs {
                reservations: Some(&reservations),
                git_porcelain: Some(""),
                ..SwarmConflictPredictorInputs::default()
            },
            now,
        );

        let session_prediction = plan
            .predictions
            .iter()
            .find(|prediction| prediction.path_pattern == "src/session.rs")
            .expect("session reservation prediction");
        assert_eq!(session_prediction.risk_level(), "high");
        assert!(session_prediction.has_reason("overlapping_active_reservations"));
        assert!(session_prediction.agents.contains("DarkGoose"));

        let finding = classify_swarm_conflict_prediction_plan(&plan);
        let avoid = finding_data(&finding)["reservation_recommendations"]["avoid"]
            .as_array()
            .unwrap()
            .clone();
        assert!(avoid.iter().any(|entry| {
            entry["path_pattern"] == "src/session.rs"
                && entry["risk_level"] == "high"
                && entry["conflict_reasons"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!("overlapping_active_reservations"))
                && entry["recommended_action"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Contact listed reservation holders")
        }));
    }

    #[test]
    fn swarm_conflict_predictor_marks_stale_in_progress_bead_surfaces() {
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let content = r#"{"id":"bd-session","title":"Session persistence chaos harness","description":"JSONL session replay and sqlite branching","status":"in_progress","assignee":"OldAgent","updated_at":"2026-05-07T00:00:00Z","labels":["session","testing"]}
"#;
        let reservations = serde_json::json!({"reservations": []});

        let plan = build_swarm_conflict_prediction_plan(
            SwarmConflictPredictorInputs {
                beads_content: Some(content),
                reservations: Some(&reservations),
                git_porcelain: Some(""),
                ..SwarmConflictPredictorInputs::default()
            },
            now,
        );

        let session_prediction = plan
            .predictions
            .iter()
            .find(|prediction| prediction.path_pattern == "src/session*.rs")
            .expect("session surface prediction");
        assert_eq!(session_prediction.risk_level(), "high");
        assert!(session_prediction.has_reason("stale_in_progress_bead"));
        assert!(session_prediction.beads.contains("bd-session"));
    }

    #[test]
    fn swarm_conflict_predictor_flags_concurrent_beads_closeout_windows() {
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let content = r#"{"id":"bd-current","title":"Evidence refresh","description":"Docs evidence ledger update","status":"in_progress","assignee":"SunnyBeacon","updated_at":"2026-05-09T11:00:00Z","labels":["evidence"]}
"#;
        let reservations = serde_json::json!({"reservations": []});

        let plan = build_swarm_conflict_prediction_plan(
            SwarmConflictPredictorInputs {
                beads_content: Some(content),
                reservations: Some(&reservations),
                git_porcelain: Some(" M .beads/issues.jsonl\n"),
                ..SwarmConflictPredictorInputs::default()
            },
            now,
        );

        let beads_prediction = plan
            .predictions
            .iter()
            .find(|prediction| prediction.path_pattern == ".beads/issues.jsonl")
            .expect("beads closeout prediction");
        assert_eq!(beads_prediction.risk_level(), "high");
        assert!(beads_prediction.has_reason("dirty_beads_closeout_window"));
        assert!(beads_prediction.has_reason("in_progress_bead_closeout_window"));
        assert!(
            classify_swarm_conflict_prediction_plan(&plan)
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("fallback lanes")
        );
    }

    #[test]
    fn swarm_next_action_prioritizes_ack_required_mail() {
        let snapshot = SwarmNextActionSnapshot {
            mail_unread: 2,
            mail_ack_required: 1,
            ready_ids: vec!["bd-ready".to_string()],
            ..SwarmNextActionSnapshot::default()
        };

        let finding = classify_swarm_next_action(&snapshot);

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_NEXT_ACTION_SCHEMA);
        assert_eq!(data["next_action"], serde_json::json!("ack_mail"));
        assert_eq!(data["mail"]["ack_required"], serde_json::json!(1));
    }

    #[test]
    fn swarm_next_action_keeps_current_work_moving() {
        let snapshot = SwarmNextActionSnapshot {
            agent_name: Some("SunnyBeacon".to_string()),
            reservations_active: 1,
            reservations_own_active: 1,
            current_in_progress: 1,
            ready_ids: vec!["bd-ready".to_string()],
            ..SwarmNextActionSnapshot::default()
        };

        let finding = classify_swarm_next_action(&snapshot);

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding_data(&finding);
        assert_eq!(data["next_action"], serde_json::json!("implement_current"));
        assert_eq!(
            data["reservations"]["own_active_count"],
            serde_json::json!(1)
        );
    }

    #[test]
    fn swarm_next_action_reports_stale_bead_unblock() {
        let snapshot = SwarmNextActionSnapshot {
            stale_in_progress: 1,
            beads_in_progress: 1,
            ready_ids: vec!["bd-ready".to_string()],
            ..SwarmNextActionSnapshot::default()
        };

        let finding = classify_swarm_next_action(&snapshot);

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(
            data["next_action"],
            serde_json::json!("unblock_stalled_bead")
        );
        assert_eq!(
            data["beads"]["stale_in_progress_count"],
            serde_json::json!(1)
        );
    }

    #[test]
    fn swarm_next_action_claims_ready_work() {
        let snapshot = SwarmNextActionSnapshot {
            ready_ids: vec!["bd-ready".to_string()],
            bv_highest_impact: Some("bd-ready".to_string()),
            bv_recommendation_count: 1,
            ..SwarmNextActionSnapshot::default()
        };

        let finding = classify_swarm_next_action(&snapshot);

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding_data(&finding);
        assert_eq!(data["next_action"], serde_json::json!("claim_ready_bead"));
        assert_eq!(data["beads"]["ready_ids"][0], serde_json::json!("bd-ready"));
    }

    #[test]
    fn swarm_next_action_decomposes_open_epic_when_ready_queue_empty() {
        let snapshot = SwarmNextActionSnapshot {
            beads_open: 1,
            open_epic_count: 1,
            ..SwarmNextActionSnapshot::default()
        };

        let finding = classify_swarm_next_action(&snapshot);

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(
            data["next_action"],
            serde_json::json!("create_followup_beads")
        );
        assert_eq!(data["beads"]["ready_count"], serde_json::json!(0));
    }

    #[test]
    fn swarm_next_action_reports_no_work_when_everything_is_empty() {
        let snapshot = SwarmNextActionSnapshot::default();

        let finding = classify_swarm_next_action(&snapshot);

        assert_eq!(finding.severity, Severity::Info);
        let data = finding_data(&finding);
        assert_eq!(data["next_action"], serde_json::json!("no_work"));
        assert_eq!(data["source_errors"].as_array().unwrap().len(), 0);
    }

    fn dashboard_probe_outcome(stdout: &str) -> CommandOutcome {
        CommandOutcome {
            timed_out: false,
            success: true,
            status_code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    #[test]
    fn swarm_operations_dashboard_reports_stalled_and_rch_pressure() {
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let beads = r#"{"id":"bd-stalled","title":"Quiet work","status":"in_progress","assignee":"OldAgent","updated_at":"2026-05-07T00:00:00Z"}
{"id":"bd-ready","title":"Ready work","status":"open","updated_at":"2026-05-09T11:00:00Z"}
"#;
        let roster = serde_json::json!({
            "agents": [
                {"name": "ActiveAgent", "last_active_ts": "2026-05-09T11:30:00Z", "task_description": "current lane"},
                {"name": "OldAgent", "last_active_ts": "2026-05-07T00:00:00Z", "task_description": "stale lane"}
            ]
        });
        let reservations = serde_json::json!({
            "reservations": [
                {
                    "path_pattern": "src/**",
                    "agent": "ActiveAgent",
                    "exclusive": true,
                    "reason": "bd-ready",
                    "expires_ts": "2026-05-09T13:00:00Z",
                    "created_at": "2026-05-09T10:00:00Z"
                },
                {
                    "path_pattern": "src/doctor.rs",
                    "agent": "OtherAgent",
                    "exclusive": true,
                    "reason": "bd-ready.1",
                    "expires_ts": "2026-05-09T13:00:00Z",
                    "created_at": "2026-05-09T10:30:00Z"
                }
            ]
        });
        let br_ready = serde_json::json!([
            {"id": "bd-ready", "title": "Ready work", "status": "open"}
        ]);
        let bv_plan = serde_json::json!({
            "plan": {
                "summary": {"highest_impact": "bd-ready"},
                "tracks": [{"items": [{"id": "bd-ready"}]}]
            }
        });
        let rch_status = dashboard_probe_outcome(
            "RCH Status\nWorkers : 8/8 healthy, 74/78 slots available\nIssues\n1 active build(s) have stale progress",
        );
        let rch_queue = dashboard_probe_outcome(
            "Build Queue\n  * 2 Active Build(s)\nWorker Availability\n  -> 74 / 78 slots free",
        );
        let mail_status = serde_json::json!({"unread": 0, "urgent": 0, "ack_required": 0});

        let inputs = SwarmOperationsDashboardInputs {
            agent_name: Some("SunnyBeacon".to_string()),
            beads_content: Some(beads),
            agent_roster: Some(&roster),
            mail_status: Some(&mail_status),
            reservations: Some(&reservations),
            br_ready: Some(&br_ready),
            bv_plan: Some(&bv_plan),
            rch_status: Some(&rch_status),
            rch_queue: Some(&rch_queue),
            git_porcelain: Some(" M src/doctor.rs\n?? scratch.json\n"),
            ..SwarmOperationsDashboardInputs::default()
        };

        let snapshot = build_swarm_operations_dashboard_snapshot(inputs, now);
        let finding = classify_swarm_operations_dashboard(&snapshot);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(
            finding
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("agents active=1")
        );
        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_OPERATIONS_DASHBOARD_SCHEMA);
        assert_eq!(data["agents"]["active_count"], serde_json::json!(1));
        assert_eq!(
            data["beads"]["stalled_candidate_count"],
            serde_json::json!(1)
        );
        assert_eq!(
            data["reservations"]["conflict_group_count"],
            serde_json::json!(1)
        );
        assert_eq!(data["rch"]["pressure"], serde_json::json!("stale_progress"));
        assert_eq!(data["rch"]["active_builds"], serde_json::json!(2));
        assert_eq!(data["git"]["total"], serde_json::json!(2));
        assert_eq!(
            data["next_action"]["action"],
            serde_json::json!("unblock_stalled_bead")
        );
        assert_eq!(data["mode"], serde_json::json!("audit_only"));
        assert_eq!(data["mutation_performed"], serde_json::json!(false));
    }

    #[test]
    fn swarm_operations_dashboard_keeps_current_claim_moving_when_clear() {
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let beads = r#"{"id":"bd-current","title":"Current work","status":"in_progress","assignee":"SunnyBeacon","updated_at":"2026-05-09T11:30:00Z"}
"#;
        let roster = serde_json::json!({
            "agents": [
                {"name": "SunnyBeacon", "last_active_ts": "2026-05-09T11:55:00Z", "task_description": "current work"}
            ]
        });
        let reservations = serde_json::json!({
            "reservations": [
                {
                    "path_pattern": "src/doctor.rs",
                    "agent": "SunnyBeacon",
                    "exclusive": true,
                    "reason": "bd-current",
                    "expires_ts": "2026-05-09T14:00:00Z",
                    "created_at": "2026-05-09T11:00:00Z"
                }
            ]
        });
        let rch_status = dashboard_probe_outcome(
            "RCH Status\nPosture : remote-ready\nWorkers : 8/8 healthy, 78/78 slots available",
        );
        let rch_queue =
            dashboard_probe_outcome("Build Queue\n  * 0 Active Build(s)\n  -> 78 / 78 slots free");
        let mail_status = serde_json::json!({"unread": 0, "urgent": 0, "ack_required": 0});
        let mail_inbox = serde_json::json!({"messages": []});
        let br_ready = serde_json::json!([]);
        let bv_plan = serde_json::json!({
            "plan": {"summary": {"highest_impact": null}, "tracks": []}
        });

        let inputs = SwarmOperationsDashboardInputs {
            agent_name: Some("SunnyBeacon".to_string()),
            beads_content: Some(beads),
            agent_roster: Some(&roster),
            mail_status: Some(&mail_status),
            mail_inbox: Some(&mail_inbox),
            reservations: Some(&reservations),
            br_ready: Some(&br_ready),
            bv_plan: Some(&bv_plan),
            rch_status: Some(&rch_status),
            rch_queue: Some(&rch_queue),
            git_porcelain: Some(""),
            ..SwarmOperationsDashboardInputs::default()
        };

        let snapshot = build_swarm_operations_dashboard_snapshot(inputs, now);
        let finding = classify_swarm_operations_dashboard(&snapshot);

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding_data(&finding);
        assert_eq!(
            data["next_action"]["action"],
            serde_json::json!("implement_current")
        );
        assert_eq!(data["rch"]["pressure"], serde_json::json!("clear"));
        assert_eq!(data["agents"]["active_count"], serde_json::json!(1));
        assert_eq!(data["reservations"]["active_count"], serde_json::json!(1));
    }

    #[test]
    fn swarm_incident_diagnostics_prioritizes_blocked_resource_component() {
        let findings = vec![
            Finding::warn(
                CheckCategory::Swarm,
                "Agent Mail degraded; Beads fallback required",
            )
            .with_remediation("Use Beads status/comments as the soft lock")
            .with_data(serde_json::json!({
                "schema": SWARM_DOCTOR_AGENT_MAIL_DEGRADED_SCHEMA,
                "mode": "beads_soft_lock_fallback",
                "mail_health": {
                    "reachable": true,
                    "missing_schema_tables": ["projects", "agents"],
                    "status": "error"
                },
                "fallback": {"soft_lock": "beads", "non_blocking": true},
                "write_paths": {"expected_failed": true}
            })),
            Finding::pass(CheckCategory::Swarm, "Swarm operations dashboard clear").with_data(
                serde_json::json!({
                    "schema": SWARM_DOCTOR_OPERATIONS_DASHBOARD_SCHEMA,
                    "beads": {
                        "open_count": 1,
                        "in_progress_count": 1,
                        "ready_count": 1,
                        "parse_errors": 0,
                        "stalled_candidate_count": 0
                    },
                    "rch": {
                        "pressure": "clear",
                        "status_reachable": true,
                        "queue_reachable": true,
                        "active_builds": 0,
                        "slots_available": 8
                    },
                    "source_errors": []
                }),
            ),
            Finding::warn(CheckCategory::Swarm, "CARGO_TARGET_DIR is not set")
                .with_data(swarm_temp_dir_data("CARGO_TARGET_DIR", None, false, None)),
            Finding::fail(CheckCategory::Swarm, "Swarm resource preflight blocked").with_data(
                serde_json::json!({
                    "schema": SWARM_DOCTOR_RESOURCE_PREFLIGHT_SCHEMA,
                    "critical_failures": ["TMPDIR:env_not_set"],
                    "source_errors": [],
                    "recommended_budgets": null
                }),
            ),
            Finding::fail(CheckCategory::Swarm, "Live swarm admission decision: deny").with_data(
                serde_json::json!({
                    "schema": SWARM_DOCTOR_ADMISSION_SCHEMA,
                    "action": "deny",
                    "pressure_dimension": "active_tool_calls",
                    "stale_data_warnings": []
                }),
            ),
        ];

        let finding = classify_swarm_incident_diagnostics(&findings);

        assert_eq!(finding.severity, Severity::Fail);
        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_INCIDENT_DIAGNOSTICS_SCHEMA);
        assert_eq!(data["status"], serde_json::json!("blocked"));
        assert_eq!(
            data["primary_failure_domain"],
            serde_json::json!("resource_governor")
        );
        assert_eq!(
            data["components"]["agent_mail"]["classification"],
            serde_json::json!("agent_mail_schema_missing")
        );
        assert_eq!(
            data["components"]["temp_dirs"]["classification"],
            serde_json::json!("temp_env_missing")
        );
        assert_eq!(
            data["components"]["resource_governor"]["classification"],
            serde_json::json!("resource_preflight_blocked")
        );
        assert_eq!(data["guards"]["read_only"], serde_json::json!(true));
        assert!(
            data["failure_domains"]
                .as_array()
                .unwrap()
                .iter()
                .any(|domain| domain["domain"] == "resource_governor")
        );
    }

    #[test]
    fn swarm_agent_mail_schema_corrupt_fixture_requires_beads_soft_lock()
    -> std::result::Result<(), serde_json::Error> {
        let health = agent_mail_schema_corrupt_fixture()?;

        let finding = classify_agent_mail_degraded_mode(
            "/data/projects/pi_agent_rust",
            Some("AmberOsprey"),
            Some(&health),
            None,
            None,
            None,
            build_slot_test_now(),
        );

        assert_eq!(finding.severity, Severity::Warn);
        assert_eq!(
            finding.title,
            "Agent Mail degraded; Beads fallback required"
        );
        let data = finding_data(&finding);
        assert_eq!(data["mode"], serde_json::json!("beads_soft_lock_fallback"));
        assert_eq!(data["fallback"]["non_blocking"], serde_json::json!(true));
        assert_eq!(
            data["mail_health"]["semantic_readiness"]["detail"],
            serde_json::json!(
                "sqlite schema missing required health_check tables: projects, agents, messages, message_recipients"
            )
        );
        assert_eq!(
            data["mail_health"]["recovery"]["next_action"],
            serde_json::json!("Run `am doctor repair --yes` or restore from archive backup")
        );
        assert_eq!(
            data["mail_health"]["missing_schema_tables"],
            serde_json::json!(["agents", "message_recipients", "messages", "projects"])
        );
        let blocked_operations = data["write_paths"]["blocked_operations"]
            .as_array()
            .expect("blocked operations");
        assert!(blocked_operations.contains(&serde_json::json!("file_reservation_paths")));
        assert!(blocked_operations.contains(&serde_json::json!("send_message")));
        Ok(())
    }

    fn test_progress_slo_source(source_id: &str, source_class: &str) -> ProgressSloSourceStatus {
        ProgressSloSourceStatus::new(
            source_id,
            source_class,
            "fixture",
            SourceAvailability::Available,
            FreshnessState::Current,
            RedactionState::Redacted,
            vec![format!("{source_id}_authority")],
        )
        .with_path(format!("docs/evidence/{source_id}.json"))
        .with_observed_at("2026-05-15T04:00:00Z")
        .with_source_hash(format!("{source_id}-hash"))
    }

    fn test_progress_slo_sources() -> Vec<ProgressSloSourceStatus> {
        vec![
            test_progress_slo_source("beads_active_delta", "beads_active_closed_delta"),
            test_progress_slo_source("beads_closed_delta", "beads_active_closed_delta"),
            test_progress_slo_source("git_commit_delta", "git_commit_delta"),
            test_progress_slo_source("rch_posture", "rch_and_validation_broker_posture"),
            test_progress_slo_source(
                "validation_broker_posture",
                "rch_and_validation_broker_posture",
            ),
            test_progress_slo_source("agent_mail_health", "agent_mail_health"),
            test_progress_slo_source(
                "operator_runpack_summary",
                "runpack_autopilot_context_summaries",
            ),
            test_progress_slo_source(
                "swarm_autopilot_summary",
                "runpack_autopilot_context_summaries",
            ),
            test_progress_slo_source(
                "context_intelligence_summary",
                "runpack_autopilot_context_summaries",
            ),
            test_progress_slo_source("operator_time_window", "operator_provided_time_window"),
        ]
    }

    fn healthy_progress_slo_report() -> ProgressSloReport {
        evaluate_progress_slo(ProgressSloEvaluationInput::new(
            "2026-05-15T04:05:00Z",
            ProgressSloTimeWindow::new(
                "2026-05-15T04:00:00Z",
                "2026-05-15T04:05:00Z",
                300,
                "HEAD~1",
            ),
            test_progress_slo_sources(),
            ProgressSloMetrics {
                closed_beads: 2,
                open_beads: 3,
                in_progress_beads: 1,
                ready_beads: 1,
                commits: 2,
                pushed_commits: 2,
                closed_with_commit_reference_count: 2,
                validation_passes: 3,
                agent_mail_health: AgentMailHealth::Green,
                rch_posture: RchPosture::Green,
                validation_broker_posture: ValidationBrokerPosture::Green,
                ..ProgressSloMetrics::default()
            },
        ))
    }

    #[test]
    fn swarm_progress_slo_doctor_reports_healthy_projection() {
        let report = healthy_progress_slo_report();

        let finding = classify_swarm_progress_slo_report(&report, None);

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_PROGRESS_SLO_SCHEMA);
        assert_eq!(data["progress"]["schema"], SWARM_PROGRESS_SLO_SCHEMA);
        assert_eq!(data["progress"]["status"], serde_json::json!("progressing"));
        assert_eq!(
            data["source_status"]["progress_slo_report"],
            serde_json::json!("available")
        );
        assert_eq!(data["guards"]["advisory_only"], serde_json::json!(true));
        assert_eq!(data["guards"]["no_live_mutation"], serde_json::json!(true));
    }

    #[test]
    fn swarm_progress_slo_doctor_reports_degraded_projection() {
        let report = evaluate_progress_slo(ProgressSloEvaluationInput::new(
            "2026-05-15T04:05:00Z",
            ProgressSloTimeWindow::new(
                "2026-05-15T04:00:00Z",
                "2026-05-15T04:05:00Z",
                300,
                "HEAD~1",
            ),
            test_progress_slo_sources(),
            ProgressSloMetrics {
                open_beads: 3,
                in_progress_beads: 1,
                ready_beads: 1,
                agent_mail_health: AgentMailHealth::Corrupt,
                rch_posture: RchPosture::Green,
                validation_broker_posture: ValidationBrokerPosture::Green,
                ..ProgressSloMetrics::default()
            },
        ));

        let finding = classify_swarm_progress_slo_report(&report, None);

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(
            data["progress"]["status"],
            serde_json::json!("coordination_degraded")
        );
        assert!(
            data["progress"]["reason_ids"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("PROGRESS-SLO-AGENT-MAIL-DEGRADED"))
        );
        assert_eq!(
            data["metrics"]["agent_mail_health"],
            serde_json::json!("corrupt")
        );
    }

    #[test]
    fn swarm_progress_slo_doctor_degrades_when_source_is_missing() {
        let project = tempfile::tempdir().expect("temp project");

        let finding = build_swarm_progress_slo_finding(project.path(), Some("missing.json"));

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(
            data["source_status"]["progress_slo_report"],
            serde_json::json!("unavailable")
        );
        assert!(
            data["degraded_reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("progress_slo_source_unavailable"))
        );
        assert_eq!(
            data["progress"]["suppressed_claims"][0],
            serde_json::json!("progress_slo_current_posture")
        );
    }

    #[test]
    fn swarm_progress_slo_doctor_degrades_when_source_is_malformed() {
        let project = tempfile::tempdir().expect("temp project");
        let path = project.path().join("progress-slo.json");
        std::fs::write(&path, "{ not json").expect("write malformed report");

        let finding = build_swarm_progress_slo_finding(project.path(), Some("progress-slo.json"));

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(
            data["source_status"]["progress_slo_report"],
            serde_json::json!("malformed")
        );
        assert_eq!(data["source_summary"]["malformed"], serde_json::json!(1));
        assert!(
            data["degraded_reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("progress_slo_source_malformed"))
        );
    }

    fn write_context_intelligence_fixture(root: &Path) {
        std::fs::create_dir_all(root.join("src")).expect("create src dir");
        std::fs::create_dir_all(root.join("tests")).expect("create tests dir");
        std::fs::create_dir_all(root.join("docs/evidence")).expect("create evidence dir");
        std::fs::create_dir_all(root.join(".beads")).expect("create beads dir");
        std::fs::write(
            root.join("src/doctor.rs"),
            r"
pub fn check_swarm_context_intelligence() {}
pub struct ContextGraphFixture;
",
        )
        .expect("write src fixture");
        std::fs::write(
            root.join("tests/doctor_swarm_temp_dir_json.rs"),
            r"
#[test]
fn doctor_swarm_context_intelligence_json_reports_posture() {
    assert!(true);
}
",
        )
        .expect("write test fixture");
        std::fs::write(
            root.join("README.md"),
            "# Fixture\n\nSee docs/evidence/dropin-certification-verdict.json.\n",
        )
        .expect("write README fixture");
        std::fs::write(
            root.join("docs/evidence/dropin-certification-verdict.json"),
            r#"{"schema":"pi.dropin.verdict.v1","overall_verdict":"NOT_CERTIFIED","token":"super-secret-value"}"#,
        )
        .expect("write sensitive evidence fixture");
        std::fs::write(
            root.join(".beads/issues.jsonl"),
            r#"{"id":"bd-context","title":"Surface context intelligence in doctor","status":"open","priority":2}"#,
        )
        .expect("write beads fixture");
    }

    #[test]
    fn swarm_context_intelligence_reports_bundle_and_redaction_safe_counts() {
        let project = tempfile::tempdir().expect("project dir");
        write_context_intelligence_fixture(project.path());
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let posture =
            build_swarm_context_intelligence_posture(project.path(), now).expect("posture");
        let finding = classify_swarm_context_intelligence_posture(&posture);

        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_CONTEXT_INTELLIGENCE_SCHEMA);
        assert_eq!(data["mode"], serde_json::json!("audit_only"));
        assert_eq!(data["mutation_performed"], serde_json::json!(false));
        assert!(data["graph"]["node_count"].as_u64().unwrap_or(0) > 0);
        assert!(data["graph"]["validation_link_count"].as_u64().unwrap_or(0) > 0);
        assert!(data["bundle"]["selected_count"].as_u64().unwrap_or(0) > 0);
        assert!(
            data["bundle"]["suggested_validation_command_count"]
                .as_u64()
                .unwrap_or(0)
                > 0
        );
        assert!(data["cache"]["fingerprint_count"].as_u64().unwrap_or(0) > 0);
        assert!(data["redaction_summary"].is_object());
        let encoded = serde_json::to_string(data).expect("serialize context data");
        assert!(!encoded.contains("super-secret-value"));
    }

    #[test]
    fn swarm_context_intelligence_degrades_when_graph_inputs_are_missing() {
        let project = tempfile::tempdir().expect("project dir");
        let now = DateTime::parse_from_rfc3339("2026-05-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let posture =
            build_swarm_context_intelligence_posture(project.path(), now).expect("posture");
        let finding = classify_swarm_context_intelligence_posture(&posture);

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(data["status"], serde_json::json!("degraded"));
        assert!(data["graph"]["missing_input_count"].as_u64().unwrap_or(0) > 0);
        assert!(
            data["degraded_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason.as_str() == Some("semantic_graph_missing_inputs"))
        );
    }

    #[test]
    fn swarm_admission_reports_healthy_decision_data() {
        let finding = classify_swarm_admission(
            test_swarm_capacity_plan(),
            healthy_resource_sample(),
            SwarmLiveLoad::empty()
                .with_active_agents(1)
                .with_active_tool_calls(1),
            &[],
        );

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_ADMISSION_SCHEMA);
        assert_eq!(data["action"], "admit");
        assert_eq!(data["pressure_dimension"], "active_agents");
        assert_eq!(data["live_counts"]["active_agents"], serde_json::json!(1));
        assert_eq!(
            data["planned_budgets"]["agent_concurrency"],
            serde_json::json!(4)
        );
        assert!(data["admission_decision"].is_object());
    }

    #[test]
    fn swarm_admission_reports_backpressure_guidance() {
        let finding = classify_swarm_admission(
            test_swarm_capacity_plan(),
            healthy_resource_sample(),
            SwarmLiveLoad::empty().with_active_agents(3),
            &[],
        );

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(data["action"], "backpressure");
        assert_eq!(data["pressure_dimension"], "active_agents");
        assert!(data["retry_after_ms"].as_u64().unwrap_or(0) > 0);
        assert!(
            finding
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("Delay new swarm work")
        );
    }

    #[test]
    fn swarm_admission_reports_deny_guidance() {
        let finding = classify_swarm_admission(
            test_swarm_capacity_plan(),
            healthy_resource_sample(),
            SwarmLiveLoad::empty().with_active_tool_calls(8),
            &[],
        );

        assert_eq!(finding.severity, Severity::Fail);
        let data = finding_data(&finding);
        assert_eq!(data["action"], "deny");
        assert_eq!(data["pressure_dimension"], "active_tool_calls");
        assert!(
            finding
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("Do not launch new swarm work")
        );
    }

    #[test]
    fn swarm_admission_marks_stale_input_degraded() {
        let warnings = vec!["host load average unavailable".to_string()];
        let finding = classify_swarm_admission(
            test_swarm_capacity_plan(),
            healthy_resource_sample(),
            SwarmLiveLoad::empty().with_active_agents(1),
            &warnings,
        );

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(data["action"], "admit");
        assert_eq!(
            data["stale_data_warnings"][0],
            "host load average unavailable"
        );
        assert!(
            finding
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("stale_data_warnings")
        );
    }

    #[test]
    fn swarm_admission_blocks_corrupted_coordination() {
        let warnings = vec!["Beads ledger has malformed JSONL rows".to_string()];
        let finding = swarm_admission_blocked_finding(
            Severity::Fail,
            "Live swarm admission decision denies new work",
            "Beads ledger has 1 malformed JSONL row; coordination state is corrupted".to_string(),
            &warnings,
        );

        assert_eq!(finding.severity, Severity::Fail);
        let data = finding_data(&finding);
        assert_eq!(data["action"], "deny");
        assert!(data["admission_decision"].is_null());
        assert_eq!(
            data["stale_data_warnings"][0],
            "Beads ledger has malformed JSONL rows"
        );
    }

    #[test]
    fn swarm_git_porcelain_summary_counts_dirty_kinds() {
        let summary =
            summarize_git_porcelain(" M src/doctor.rs\nA  README.md\n?? notes.md\n D stale.rs\n");

        assert_eq!(summary.total, 4);
        assert_eq!(summary.staged, 1);
        assert_eq!(summary.unstaged, 2);
        assert_eq!(summary.untracked, 1);
        assert_eq!(summary.deleted, 1);
    }

    #[test]
    fn swarm_agent_mail_status_warns_on_ack_required() {
        let value = serde_json::json!({
            "inbox": {
                "unread": 2,
                "urgent": 0,
                "ack_required": 1
            }
        });

        let finding = classify_agent_mail_status(&value);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.detail.unwrap().contains("ack_required=1"));
    }

    #[test]
    fn swarm_br_status_warns_on_sync_drift() {
        let value = serde_json::json!({
            "dirty_count": 2,
            "jsonl_newer": false,
            "db_newer": true
        });

        let finding = classify_br_sync_status(&value);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.detail.unwrap().contains("dirty_count=2"));
    }

    #[test]
    fn swarm_br_status_passes_when_clean() {
        let value = serde_json::json!({
            "dirty_count": 0,
            "jsonl_newer": false,
            "db_newer": false
        });

        let finding = classify_br_sync_status(&value);

        assert_eq!(finding.severity, Severity::Pass);
    }

    #[test]
    fn swarm_agent_mail_reservations_detect_conflicts() {
        let value = serde_json::json!({
            "reservations": [
                {
                    "path": "src/doctor.rs",
                    "agent": "CalmBridge",
                    "expires_ts": "2026-05-09T09:00:00Z",
                    "created_at": "2026-05-09T07:00:00Z"
                }
            ],
            "conflicts": [
                {"path": "src/doctor.rs", "holder": "OtherAgent"}
            ]
        });

        let finding = classify_agent_mail_reservations_at(&value, heatmap_test_now());

        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.title.contains("conflicts"));
        let data = finding.data.unwrap();
        assert_eq!(data["schema"], SWARM_DOCTOR_RESERVATION_HEATMAP_SCHEMA);
        assert_eq!(data["totals"]["active_count"], 1);
    }

    #[test]
    fn swarm_agent_mail_reservations_pass_when_clear() {
        let value = serde_json::json!({
            "reservations": [],
            "expiring": 0,
            "conflicts": []
        });

        let finding = classify_agent_mail_reservations_at(&value, heatmap_test_now());

        assert_eq!(finding.severity, Severity::Pass);
    }

    #[test]
    fn swarm_agent_mail_reservation_heatmap_detects_overlapping_globs() {
        let value = serde_json::json!({
            "reservations": [
                {
                    "path_pattern": "src/**",
                    "agent": "CalmBridge",
                    "reason": "bd-overlap parent lane",
                    "exclusive": true,
                    "expires_ts": "2026-05-09T09:00:00Z",
                    "created_at": "2026-05-09T07:00:00Z"
                },
                {
                    "path_pattern": "src/doctor.rs",
                    "agent": "DarkGoose",
                    "reason": "bd-overlap.1 concrete lane",
                    "exclusive": true,
                    "expires_ts": "2026-05-09T09:00:00Z",
                    "created_at": "2026-05-09T07:30:00Z"
                }
            ]
        });

        let finding = classify_agent_mail_reservations_at(&value, heatmap_test_now());

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding.data.unwrap();
        assert_eq!(data["totals"]["conflict_group_count"], 1);
        assert_eq!(
            data["heatmap"]["conflicts"][0]["reason"],
            "overlapping_active_reservations"
        );
        assert_eq!(data["heatmap"]["by_path_pattern"][0]["conflict_count"], 1);
    }

    #[test]
    fn swarm_agent_mail_reservation_heatmap_ignores_expired_conflicts_but_counts_recent() {
        let value = serde_json::json!({
            "reservations": [
                {
                    "path_pattern": "src/**",
                    "agent": "CalmBridge",
                    "exclusive": true,
                    "expires_ts": "2026-05-09T07:45:00Z",
                    "created_at": "2026-05-09T06:00:00Z"
                },
                {
                    "path_pattern": "src/doctor.rs",
                    "agent": "DarkGoose",
                    "exclusive": true,
                    "expires_ts": "2026-05-09T09:00:00Z",
                    "created_at": "2026-05-09T07:30:00Z"
                }
            ]
        });

        let finding = classify_agent_mail_reservations_at(&value, heatmap_test_now());

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding.data.unwrap();
        assert_eq!(data["totals"]["expired_count"], 1);
        assert_eq!(data["totals"]["recent_inactive_count"], 1);
        assert_eq!(data["totals"]["conflict_group_count"], 0);
    }

    #[test]
    fn swarm_agent_mail_reservation_heatmap_allows_shared_overlaps() {
        let value = serde_json::json!({
            "reservations": [
                {
                    "path_pattern": "src/**",
                    "agent": "CalmBridge",
                    "reason": "bd-shared",
                    "exclusive": false,
                    "expires_ts": "2026-05-09T09:00:00Z",
                    "created_at": "2026-05-09T07:00:00Z"
                },
                {
                    "path_pattern": "src/doctor.rs",
                    "agent": "DarkGoose",
                    "reason": "bd-shared",
                    "exclusive": false,
                    "expires_ts": "2026-05-09T09:00:00Z",
                    "created_at": "2026-05-09T07:30:00Z"
                }
            ]
        });

        let finding = classify_agent_mail_reservations_at(&value, heatmap_test_now());

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding.data.unwrap();
        assert_eq!(data["totals"]["shared_active_count"], 2);
        assert_eq!(data["totals"]["exclusive_active_count"], 0);
        assert_eq!(data["totals"]["conflict_group_count"], 0);
    }

    #[test]
    fn swarm_agent_mail_reservation_heatmap_recommends_stale_holders() {
        let value = serde_json::json!({
            "reservations": [
                {
                    "path_pattern": "src/doctor.rs",
                    "agent": "CalmBridge",
                    "reason": "bd-stale stale holder",
                    "exclusive": true,
                    "expires_ts": "2026-05-10T08:00:00Z",
                    "created_at": "2026-05-08T07:00:00Z"
                }
            ]
        });

        let finding = classify_agent_mail_reservations_at(&value, heatmap_test_now());

        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.title.contains("stale holder"));
        let data = finding.data.unwrap();
        assert_eq!(data["totals"]["stale_holder_recommendation_count"], 1);
        assert_eq!(
            data["stale_holder_recommendations"][0]["reason"],
            "active_exclusive_reservation_older_than_threshold"
        );
    }

    #[test]
    fn swarm_agent_mail_contacts_warns_on_pending_requests() {
        let value = serde_json::json!({
            "data": {
                "count": 2,
                "contacts": [
                    {
                        "from": "SunnyBeacon",
                        "to": "MagentaOak",
                        "status": "pending",
                        "policy": "auto",
                        "reason": "coordination",
                        "updated": "1h"
                    },
                    {
                        "from": "MagentaOak",
                        "to": "RusticGorge",
                        "status": "approved",
                        "policy": "auto",
                        "reason": "coordination",
                        "updated": "2h"
                    }
                ]
            }
        });

        let finding = classify_agent_mail_contacts(&value);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.title.contains("pending"));
        let data = finding_data(&finding);
        assert_eq!(data["pending_count"], serde_json::json!(1));
        assert_eq!(data["approved_count"], serde_json::json!(1));
    }

    #[test]
    fn swarm_agent_mail_contacts_warns_on_degraded_rows() {
        let value = serde_json::json!({
            "contacts": [
                {
                    "from": "[unknown-agent-1218]",
                    "to": "MagentaOak",
                    "status": "approved",
                    "policy": "unknown",
                    "reason": "coordination",
                    "updated": "4m"
                },
                {
                    "from": "MagentaOak",
                    "to": "SunnyBeacon",
                    "status": "mystery",
                    "policy": "auto",
                    "reason": "coordination",
                    "updated": "5m"
                }
            ]
        });

        let finding = classify_agent_mail_contacts(&value);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.title.contains("degraded"));
        let data = finding_data(&finding);
        assert_eq!(data["degraded_count"], serde_json::json!(2));
        assert_eq!(data["unknown_status_count"], serde_json::json!(1));
    }

    #[test]
    fn swarm_agent_mail_contacts_passes_when_links_are_settled() {
        let value = serde_json::json!({
            "contacts": [
                {
                    "from": "MagentaOak",
                    "to": "SunnyBeacon",
                    "status": "approved",
                    "policy": "auto",
                    "reason": "coordination",
                    "updated": "3m"
                },
                {
                    "from": "CopperOx",
                    "to": "MagentaOak",
                    "status": "blocked",
                    "policy": "contacts_only",
                    "reason": "not needed",
                    "updated": "10m"
                }
            ]
        });

        let finding = classify_agent_mail_contacts(&value);

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding_data(&finding);
        assert_eq!(data["contact_count"], serde_json::json!(2));
        assert_eq!(data["blocked_count"], serde_json::json!(1));
    }

    #[test]
    fn swarm_agent_mail_build_slots_pass_when_none_active() {
        let finding = classify_agent_mail_build_slots(&[], 0, build_slot_test_now());

        assert_eq!(finding.severity, Severity::Pass);
        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_BUILD_SLOT_SCHEMA);
        assert_eq!(data["source_supported"], serde_json::json!(true));
        assert_eq!(data["active_count"], serde_json::json!(0));
        assert_eq!(data["slots"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn swarm_agent_mail_build_slots_reports_active_shared_slot() {
        let values = vec![serde_json::json!({
            "slot": "cargo-check",
            "agent": "BlueLake",
            "branch": "main",
            "exclusive": false,
            "acquired_ts": "2026-05-09T07:45:00Z",
            "expires_ts": "2026-05-09T10:00:00Z"
        })];

        let finding = classify_agent_mail_build_slots(&values, 0, build_slot_test_now());

        assert_eq!(finding.severity, Severity::Info);
        let data = finding_data(&finding);
        assert_eq!(data["active_count"], serde_json::json!(1));
        assert_eq!(data["active_shared_count"], serde_json::json!(1));
        assert_eq!(data["slots"][0]["slot"], "cargo-check");
        assert_eq!(data["slots"][0]["agent"], "BlueLake");
        assert_eq!(data["slots"][0]["exclusive"], serde_json::json!(false));
        assert_eq!(data["slots"][0]["expires_ts"], "2026-05-09T10:00:00Z");
        assert_eq!(data["slots"][0]["classification"], "active");
    }

    #[test]
    fn swarm_agent_mail_build_slots_warn_on_active_exclusive_slot() {
        let values = vec![serde_json::json!({
            "slot": "cargo-clippy",
            "agent": "GreenBridge",
            "branch": "main",
            "exclusive": true,
            "expires_ts": "2026-05-09T10:00:00Z"
        })];

        let finding = classify_agent_mail_build_slots(&values, 0, build_slot_test_now());

        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.title.contains("exclusive"));
        let data = finding_data(&finding);
        assert_eq!(data["active_count"], serde_json::json!(1));
        assert_eq!(data["active_exclusive_count"], serde_json::json!(1));
        assert_eq!(data["slots"][0]["agent"], "GreenBridge");
        assert_eq!(data["slots"][0]["classification"], "active");
    }

    #[test]
    fn swarm_agent_mail_build_slots_classify_expired_and_soon_expiring_slots() {
        let values = vec![
            serde_json::json!({
                "slot": "cargo-test",
                "agent": "AmberField",
                "branch": "main",
                "exclusive": false,
                "expires_ts": "2026-05-09T07:59:00Z"
            }),
            serde_json::json!({
                "slot": "cargo-check",
                "agent": "CyanHill",
                "branch": "main",
                "exclusive": false,
                "expires_ts": "2026-05-09T08:20:00Z"
            }),
        ];

        let finding = classify_agent_mail_build_slots(&values, 0, build_slot_test_now());

        assert_eq!(finding.severity, Severity::Warn);
        let data = finding_data(&finding);
        assert_eq!(data["active_count"], serde_json::json!(1));
        assert_eq!(data["soon_expiring_count"], serde_json::json!(1));
        assert_eq!(data["stale_count"], serde_json::json!(1));
        let classifications = data["slots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|slot| slot["classification"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(classifications.contains(&"soon_expiring"));
        assert!(classifications.contains(&"stale_expired"));
    }

    #[test]
    fn swarm_agent_mail_build_slots_report_missing_support() {
        let finding =
            agent_mail_build_slots_unavailable_finding("build-slot source is unavailable");

        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(finding.title, "Agent Mail build slots unavailable");
        let data = finding_data(&finding);
        assert_eq!(data["source_supported"], serde_json::json!(false));
        assert_eq!(data["reason"], "build-slot source is unavailable");
    }

    #[test]
    fn swarm_redacts_sensitive_probe_output() {
        let outcome = CommandOutcome {
            timed_out: false,
            success: false,
            status_code: Some(1),
            stdout: "status ok\nOPENAI_API_KEY=secret-value\n".to_string(),
            stderr: String::new(),
        };

        let snippet = redacted_output_snippet(&outcome);

        assert!(snippet.contains("status ok"));
        assert!(snippet.contains("[redacted sensitive output line]"));
        assert!(!snippet.contains("secret-value"));
    }

    #[test]
    fn swarm_rch_classifier_reports_retrieval_disk_pressure_without_code_regression() {
        let outcome = CommandOutcome {
            timed_out: false,
            success: false,
            status_code: Some(1),
            stdout: "remote command succeeded; exit status 0\n".to_string(),
            stderr: "failed to retrieve artifact bundle: No space left on device while copying to CARGO_TARGET_DIR\n".to_string(),
        };

        assert_eq!(
            classify_rch_failure(&outcome),
            RchFailureKind::ArtifactRetrievalDiskPressure
        );
        let finding = rch_failure_finding(&outcome);
        let data = finding_data(&finding);

        assert_eq!(data["classification"], "artifact_retrieval_disk_pressure");
        assert_eq!(data["code_regression"], serde_json::json!(false));
        assert_eq!(data["fail_closed"], serde_json::json!(false));
        assert!(
            finding
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("/data/tmp/pi_agent_rust_cargo/<agent>/")
        );
    }

    #[test]
    fn swarm_rch_classifier_reports_local_target_tmpdir_disk_pressure() {
        let outcome = CommandOutcome {
            timed_out: false,
            success: false,
            status_code: Some(1),
            stdout: String::new(),
            stderr: "failed to create target/debug/.fingerprint/pi: ENOSPC (No space left on device); TMPDIR=/tmp\n".to_string(),
        };

        assert_eq!(
            classify_rch_failure(&outcome),
            RchFailureKind::LocalTargetDiskPressure
        );
        let finding = rch_failure_finding(&outcome);
        let data = finding_data(&finding);

        assert_eq!(data["classification"], "local_target_tmpdir_disk_pressure");
        assert_eq!(data["code_regression"], serde_json::json!(false));
        assert!(
            finding
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("CARGO_TARGET_DIR=/data/tmp/pi_agent_rust_cargo/<agent>/target")
        );
    }

    #[test]
    fn swarm_rch_classifier_reports_true_remote_compile_failure() {
        let outcome = CommandOutcome {
            timed_out: false,
            success: false,
            status_code: Some(101),
            stdout: String::new(),
            stderr: "error[E0308]: mismatched types\nerror: could not compile `pi` due to previous error\n".to_string(),
        };

        assert_eq!(
            classify_rch_failure(&outcome),
            RchFailureKind::RemoteBuildOrTestFailure
        );
        let finding = rch_failure_finding(&outcome);
        let data = finding_data(&finding);

        assert_eq!(data["classification"], "remote_build_or_test_failure");
        assert_eq!(data["code_regression"], serde_json::json!(true));
        assert_eq!(data["raw_status"]["exit_code"], serde_json::json!(101));
        assert!(
            finding
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("real compile/test failure")
        );
    }

    #[test]
    fn swarm_rch_classifier_keeps_unknown_fail_closed_with_raw_status() {
        let outcome = CommandOutcome {
            timed_out: false,
            success: false,
            status_code: Some(7),
            stdout: String::new(),
            stderr: "worker transport returned opaque failure code".to_string(),
        };

        assert_eq!(classify_rch_failure(&outcome), RchFailureKind::Unknown);
        let finding = rch_failure_finding(&outcome);
        let data = finding_data(&finding);

        assert_eq!(data["classification"], "unknown_rch_failure");
        assert!(data["code_regression"].is_null());
        assert_eq!(data["fail_closed"], serde_json::json!(true));
        assert_eq!(data["raw_status"]["exit_code"], serde_json::json!(7));
        assert!(
            finding
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains("do not classify this as disk pressure or a code regression")
        );
    }

    #[test]
    fn swarm_rch_affinity_groups_compatible_jobs_on_warm_target() {
        let target = "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string();
        let specs = vec![
            RchAffinityJobSpec {
                id: "tools-a".to_string(),
                command: "rch exec -- cargo test --test tools_conformance".to_string(),
                worker: Some("vmi-a".to_string()),
                target_dir: Some(target.clone()),
                git_commit: Some("abc123".to_string()),
                features: vec!["default".to_string()],
                worker_available: Some(true),
                target_available_kb: Some(SWARM_DISK_WARN_AVAILABLE_KB),
            },
            RchAffinityJobSpec {
                id: "tools-b".to_string(),
                command: "cargo test --test tools_conformance".to_string(),
                worker: Some("vmi-a".to_string()),
                target_dir: Some(target),
                git_commit: Some("abc123".to_string()),
                features: vec!["default".to_string()],
                worker_available: Some(true),
                target_available_kb: Some(SWARM_DISK_WARN_AVAILABLE_KB),
            },
        ];

        let plan = build_rch_affinity_plan_from_specs(
            specs,
            Some("abc123".to_string()),
            "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string(),
        );
        let finding = classify_rch_affinity_plan(&plan);
        let data = finding_data(&finding);

        assert_eq!(finding.severity, Severity::Pass);
        assert!(plan.blockers.is_empty());
        assert_eq!(data["schema"], SWARM_DOCTOR_RCH_AFFINITY_SCHEMA);
        assert_eq!(data["job_count"], serde_json::json!(2));
        assert_eq!(data["groups"][0]["recommendation"], "share_warm_target");
        assert_eq!(data["groups"][0]["command_family"], "cargo test");
        assert_eq!(
            data["groups"][0]["job_ids"],
            serde_json::json!(["tools-a", "tools-b"])
        );
        assert_eq!(
            data["proof_gate"]["schema"],
            SWARM_RCH_AFFINITY_PLAN_ARTIFACT_SCHEMA
        );
        assert_eq!(
            data["proof_gate"]["safe_to_launch"],
            serde_json::json!(true)
        );
        assert_eq!(
            data["proof_gate"]["cache_reuse"][0]["decision"],
            "reuse_warm_target"
        );
        assert_eq!(
            data["proof_gate"]["cache_reuse"][0]["safe_to_reuse"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn swarm_rch_affinity_blocks_incompatible_features_on_same_target() {
        let target = "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string();
        let specs = vec![
            RchAffinityJobSpec {
                id: "default-features".to_string(),
                command: "cargo test --test tools_conformance".to_string(),
                worker: Some("vmi-a".to_string()),
                target_dir: Some(target.clone()),
                git_commit: Some("abc123".to_string()),
                features: vec!["default".to_string()],
                worker_available: Some(true),
                target_available_kb: Some(SWARM_DISK_WARN_AVAILABLE_KB),
            },
            RchAffinityJobSpec {
                id: "all-features".to_string(),
                command: "cargo test --all-features --test tools_conformance".to_string(),
                worker: Some("vmi-a".to_string()),
                target_dir: Some(target.clone()),
                git_commit: Some("abc123".to_string()),
                features: Vec::new(),
                worker_available: Some(true),
                target_available_kb: Some(SWARM_DISK_WARN_AVAILABLE_KB),
            },
        ];

        let plan = build_rch_affinity_plan_from_specs(
            specs,
            Some("abc123".to_string()),
            "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string(),
        );
        let finding = classify_rch_affinity_plan(&plan);
        let data = finding_data(&finding);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(
            plan.blockers
                .contains(&format!("target_dir_conflict:{target}"))
        );
        assert_eq!(data["blockers"][0], format!("target_dir_conflict:{target}"));
        assert_eq!(
            data["proof_gate"]["safe_to_launch"],
            serde_json::json!(false)
        );
        assert!(
            data["proof_gate"]["isolation"]
                .as_array()
                .is_some_and(|entries| entries
                    .iter()
                    .any(|entry| entry["reason"] == "target_dir_conflict"
                        && entry["target_dir"] == target))
        );
    }

    fn rch_affinity_proof_gate_has_blocker_family(data: &serde_json::Value, family: &str) -> bool {
        data["proof_gate"]["blockers"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry["family"] == family))
    }

    #[test]
    fn swarm_rch_affinity_blocks_stale_commit_specs() {
        let target = "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string();
        let specs = vec![RchAffinityJobSpec {
            id: "stale-check".to_string(),
            command: "cargo check --all-targets".to_string(),
            worker: Some("vmi-a".to_string()),
            target_dir: Some(target),
            git_commit: Some("old-commit".to_string()),
            features: vec!["default".to_string()],
            worker_available: Some(true),
            target_available_kb: Some(SWARM_DISK_WARN_AVAILABLE_KB),
        }];

        let plan = build_rch_affinity_plan_from_specs(
            specs,
            Some("abc123".to_string()),
            "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string(),
        );
        let finding = classify_rch_affinity_plan(&plan);
        let data = finding_data(&finding);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker == "stale-check:git_commit_differs_from_current_checkout")
        );
        assert_eq!(
            data["proof_gate"]["safe_to_launch"],
            serde_json::json!(false)
        );
        assert!(rch_affinity_proof_gate_has_blocker_family(
            data,
            "stale_git_commit"
        ));
    }

    #[test]
    fn swarm_rch_affinity_blocks_unavailable_workers() {
        let target = "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string();
        let specs = vec![RchAffinityJobSpec {
            id: "worker-down".to_string(),
            command: "cargo test provider_streaming".to_string(),
            worker: Some("vmi-down".to_string()),
            target_dir: Some(target),
            git_commit: Some("abc123".to_string()),
            features: vec!["default".to_string()],
            worker_available: Some(false),
            target_available_kb: Some(SWARM_DISK_WARN_AVAILABLE_KB),
        }];

        let plan = build_rch_affinity_plan_from_specs(
            specs,
            Some("abc123".to_string()),
            "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string(),
        );
        let finding = classify_rch_affinity_plan(&plan);
        let data = finding_data(&finding);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker == "worker-down:worker_unavailable")
        );
        assert!(rch_affinity_proof_gate_has_blocker_family(
            data,
            "worker_unavailable"
        ));
        assert_eq!(
            data["jobs"][0]["worker_available"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn swarm_rch_affinity_blocks_low_retrieval_headroom() {
        let target = "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string();
        let specs = vec![RchAffinityJobSpec {
            id: "low-headroom".to_string(),
            command: "cargo clippy --all-targets -- -D warnings".to_string(),
            worker: Some("vmi-a".to_string()),
            target_dir: Some(target),
            git_commit: Some("abc123".to_string()),
            features: vec!["default".to_string()],
            worker_available: Some(true),
            target_available_kb: Some(SWARM_DISK_WARN_AVAILABLE_KB - 1),
        }];

        let plan = build_rch_affinity_plan_from_specs(
            specs,
            Some("abc123".to_string()),
            "/data/tmp/pi_agent_rust_cargo/goldenglacier/target".to_string(),
        );
        let finding = classify_rch_affinity_plan(&plan);
        let data = finding_data(&finding);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker == "low-headroom:retrieval_headroom_low")
        );
        assert!(rch_affinity_proof_gate_has_blocker_family(
            data,
            "retrieval_headroom_low"
        ));
        assert_eq!(
            data["jobs"][0]["target_available_kb"],
            serde_json::json!(SWARM_DISK_WARN_AVAILABLE_KB - 1)
        );
    }

    #[test]
    fn swarm_df_parser_and_formatting_are_stable() {
        let output = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000 12000 88000 13% /data\n";

        assert_eq!(parse_df_available_kb(output), Some(88_000));
        assert_eq!(format_available_kb(88_000), "85.9 MiB");
        assert_eq!(format_available_kb(11 * 1024 * 1024), "11.0 GiB");
    }

    #[test]
    fn swarm_temp_dir_posture_passes_for_expected_scratch_root() {
        let finding = swarm_temp_dir_finding(
            "CARGO_TARGET_DIR",
            Path::new("/data/tmp/pi_agent_rust_cargo/sunnybeacon/target"),
            Some(12 * 1024 * 1024),
            None,
        );
        let data = finding_data(&finding);

        assert_eq!(finding.severity, Severity::Pass);
        assert_eq!(data["schema"], SWARM_DOCTOR_TEMP_DIR_SCHEMA);
        assert_eq!(data["env_name"], "CARGO_TARGET_DIR");
        assert_eq!(data["under_expected_root"], serde_json::json!(true));
        assert_eq!(data["available_kb"], serde_json::json!(12 * 1024 * 1024));
    }

    #[test]
    fn swarm_temp_dir_posture_warns_outside_scratch_root_even_with_headroom() {
        let finding = swarm_temp_dir_finding(
            "TMPDIR",
            Path::new("/tmp/pi_agent_rust_cargo/sunnybeacon/tmp"),
            Some(64 * 1024 * 1024),
            None,
        );
        let data = finding_data(&finding);

        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.title.contains("outside swarm scratch root"));
        assert_eq!(data["schema"], SWARM_DOCTOR_TEMP_DIR_SCHEMA);
        assert_eq!(data["under_expected_root"], serde_json::json!(false));
        assert_eq!(data["expected_root"], SWARM_CARGO_SCRATCH_ROOT);
        assert!(
            finding
                .remediation
                .as_deref()
                .unwrap_or_default()
                .contains(SWARM_CARGO_SCRATCH_ROOT)
        );
    }

    #[test]
    fn swarm_resource_preflight_parses_cgroup_cpu_quota() {
        let limited = parse_cgroup_cpu_max("150000 100000").expect("limited cpu.max");

        assert_eq!(limited.quota_us, Some(150_000));
        assert_eq!(limited.period_us, Some(100_000));
        assert_eq!(limited.quota_cores, Some(1.5));
        assert!(!limited.unlimited);

        let unlimited = parse_cgroup_cpu_max("max 100000").expect("unlimited cpu.max");
        assert!(unlimited.unlimited);
        assert_eq!(unlimited.quota_cores, None);

        let v1 = parse_cgroup_cpu_cfs("-1", "100000").expect("v1 unlimited");
        assert!(v1.unlimited);
    }

    #[test]
    fn swarm_resource_preflight_parses_cpuset_and_numa_ranges() {
        let values = parse_numeric_range_list("0-3,8,10-11,3").expect("range list");

        assert_eq!(values, vec![0, 1, 2, 3, 8, 10, 11]);
        assert!(parse_numeric_range_list("3-1").is_none());
        assert!(parse_numeric_range_list("0,,2").is_none());
    }

    #[test]
    fn swarm_resource_preflight_parses_memory_limits() {
        assert_eq!(
            parse_cgroup_memory_limit_bytes("max"),
            Some(CgroupMemoryLimitParse::Unlimited)
        );
        assert_eq!(
            parse_cgroup_memory_limit_bytes("2147483648"),
            Some(CgroupMemoryLimitParse::Limited(2_147_483_648))
        );
        assert_eq!(
            parse_mem_total_bytes("MemTotal:       65536 kB\nMemFree: 4 kB\n"),
            Some(64 * MIB_BYTES)
        );
    }

    fn clear_local_build_pressure_fixture() -> LocalBuildPressureSnapshot {
        LocalBuildPressureSnapshot {
            source: "fixture".to_string(),
            active_processes: Some(0),
            process_budget: 2,
            status: "clear".to_string(),
            patterns: local_build_process_patterns(),
        }
    }

    fn clear_rch_queue_posture_fixture() -> RchQueuePostureSnapshot {
        RchQueuePostureSnapshot {
            source: "fixture".to_string(),
            status: "ok".to_string(),
            recommended_action: "proceed".to_string(),
            reason: Some("fixture_clear".to_string()),
            queue_depth: Some(0),
            active_builds: Some(0),
            queued_builds: Some(0),
            slots_available: Some(8),
            slots_total: Some(8),
            workers_healthy: Some(8),
            workers_total: Some(8),
        }
    }

    fn ready_headroom_paths_fixture() -> Vec<SwarmHeadroomPath> {
        vec![
            SwarmHeadroomPath {
                env_name: "CARGO_TARGET_DIR".to_string(),
                path: Some(format!("{SWARM_CARGO_SCRATCH_ROOT}/codex/target")),
                exists: true,
                under_expected_root: Some(true),
                available_kb: Some(SWARM_DISK_WARN_AVAILABLE_KB),
                ready: true,
                problem: None,
            },
            SwarmHeadroomPath {
                env_name: "TMPDIR".to_string(),
                path: Some(format!("{SWARM_CARGO_SCRATCH_ROOT}/codex/tmp")),
                exists: true,
                under_expected_root: Some(true),
                available_kb: Some(SWARM_DISK_WARN_AVAILABLE_KB),
                ready: true,
                problem: None,
            },
        ]
    }

    fn resource_budget_recommendation_fixture(
        agent_concurrency: u64,
        tool_concurrency: u64,
        extension_hostcall_lanes: u64,
        rch_verification_fanout: u64,
        numa_node_count: usize,
    ) -> SwarmResourceBudgetRecommendation {
        SwarmResourceBudgetRecommendation {
            agent_concurrency,
            tool_concurrency,
            extension_hostcall_lanes,
            rch_verification_fanout,
            max_queue_depth: 1024,
            max_rss_bytes: 1024 * MIB_BYTES,
            local_build_process_budget: 8,
            rch_queue_depth_budget: 1024,
            numa_node_count,
            memory_pressure_threshold_ratio_label: "0.70".to_string(),
            plan_confidence: "high".to_string(),
            explanations: vec![budget_explanation(
                "agent_concurrency",
                &agent_concurrency,
                "fixture budget explanation",
            )],
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn swarm_resource_preflight_fails_closed_without_headroom() {
        let snapshot = SwarmResourcePreflightSnapshot {
            logical_cpu_cores: 8,
            cpu_quota: CgroupCpuQuota {
                source: Some("/sys/fs/cgroup/cpu.max".to_string()),
                quota_us: Some(400_000),
                period_us: Some(100_000),
                quota_cores: Some(4.0),
                unlimited: false,
            },
            cpuset: CpuSetSnapshot {
                source: Some("/sys/fs/cgroup/cpuset.cpus.effective".to_string()),
                raw: Some("0-3".to_string()),
                cpus: vec![0, 1, 2, 3],
                cpu_count: Some(4),
            },
            numa: NumaTopologySnapshot {
                source: Some("/sys/devices/system/node/online".to_string()),
                raw_online: Some("0-1".to_string()),
                nodes: vec![0, 1],
            },
            memory: MemoryLimitSnapshot {
                mem_total_bytes: Some(64 * 1024 * MIB_BYTES),
                cgroup_limit_bytes: Some(32 * 1024 * MIB_BYTES),
                effective_limit_bytes: Some(32 * 1024 * MIB_BYTES),
                unlimited: false,
                source: Some("/sys/fs/cgroup/memory.max".to_string()),
            },
            effective_cpu_cores: 4,
            local_build_pressure: clear_local_build_pressure_fixture(),
            rch_queue_posture: clear_rch_queue_posture_fixture(),
            headroom_paths: vec![SwarmHeadroomPath {
                env_name: "CARGO_TARGET_DIR".to_string(),
                path: None,
                exists: false,
                under_expected_root: None,
                available_kb: None,
                ready: false,
                problem: Some("env_not_set".to_string()),
            }],
            recommended_budgets: Some(SwarmResourceBudgetRecommendation {
                agent_concurrency: 2,
                tool_concurrency: 4,
                extension_hostcall_lanes: 1,
                rch_verification_fanout: 1,
                max_queue_depth: 128,
                max_rss_bytes: 512 * MIB_BYTES,
                local_build_process_budget: 2,
                rch_queue_depth_budget: 128,
                numa_node_count: 2,
                memory_pressure_threshold_ratio_label: "0.70".to_string(),
                plan_confidence: "low".to_string(),
                explanations: vec![budget_explanation(
                    "agent_concurrency",
                    &2,
                    "fixture budget explanation",
                )],
            }),
            plan_error: None,
            source_errors: Vec::new(),
        };

        let finding = classify_swarm_resource_preflight(&snapshot);

        assert_eq!(finding.severity, Severity::Fail);
        let data = finding_data(&finding);
        assert_eq!(data["schema"], SWARM_DOCTOR_RESOURCE_PREFLIGHT_SCHEMA);
        assert_eq!(data["cpu"]["effective_cores"], serde_json::json!(4));
        assert_eq!(data["numa"]["node_count"], serde_json::json!(2));
        assert_eq!(
            data["critical_failures"][0],
            serde_json::json!("CARGO_TARGET_DIR:env_not_set")
        );
        assert_eq!(
            data["recommended_budgets"]["agent_concurrency"],
            serde_json::json!(2)
        );
        assert_eq!(
            data["local_build_pressure"]["process_budget"],
            serde_json::json!(2)
        );
        assert_eq!(
            data["rch_queue_posture"]["recommended_action"],
            serde_json::json!("proceed")
        );
        assert!(
            data["recommended_budgets"]["explanations"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
        assert_eq!(
            data["lane_placement"]["schema"],
            SWARM_DOCTOR_LANE_PLACEMENT_SCHEMA
        );
        assert_eq!(
            data["lane_placement"]["status"],
            serde_json::json!("blocked")
        );
        assert!(
            data["lane_placement"]["caveats"]
                .as_array()
                .is_some_and(|items| items
                    .iter()
                    .any(|item| item == "scratch_headroom_incomplete"))
        );
    }

    #[test]
    fn swarm_resource_preflight_effective_cpu_uses_tightest_limit() {
        assert_eq!(effective_swarm_cpu_cores(16, Some(3.8), Some(8)), 3);
        assert_eq!(effective_swarm_cpu_cores(16, Some(0.5), Some(8)), 1);
        assert_eq!(effective_swarm_cpu_cores(16, None, Some(4)), 4);
    }

    #[test]
    fn swarm_resource_preflight_lane_placement_covers_large_host_fixture() {
        let snapshot = SwarmResourcePreflightSnapshot {
            logical_cpu_cores: 64,
            cpu_quota: CgroupCpuQuota {
                source: Some("fixture/cpu.max".to_string()),
                quota_us: None,
                period_us: Some(100_000),
                quota_cores: None,
                unlimited: true,
            },
            cpuset: CpuSetSnapshot {
                source: Some("fixture/cpuset".to_string()),
                raw: Some("0-63".to_string()),
                cpus: (0..64).collect(),
                cpu_count: Some(64),
            },
            numa: NumaTopologySnapshot {
                source: Some("fixture/numa".to_string()),
                raw_online: Some("0-3".to_string()),
                nodes: vec![0, 1, 2, 3],
            },
            memory: MemoryLimitSnapshot {
                mem_total_bytes: Some(256 * 1024 * MIB_BYTES),
                cgroup_limit_bytes: None,
                effective_limit_bytes: Some(256 * 1024 * MIB_BYTES),
                unlimited: true,
                source: Some("fixture/memory.max".to_string()),
            },
            effective_cpu_cores: 64,
            local_build_pressure: clear_local_build_pressure_fixture(),
            rch_queue_posture: clear_rch_queue_posture_fixture(),
            headroom_paths: ready_headroom_paths_fixture(),
            recommended_budgets: Some(resource_budget_recommendation_fixture(32, 64, 16, 8, 4)),
            plan_error: None,
            source_errors: Vec::new(),
        };

        let plan = swarm_lane_placement_plan(&snapshot, &[]);

        assert_eq!(plan.status, "ready");
        assert!(plan.caveats.is_empty());
        assert_eq!(plan.lane_groups.len(), 4);
        assert_eq!(plan.lane_groups[0].lane_id, "numa-node-0");
        assert_eq!(plan.lane_groups[0].cpu_affinity_hint, "0-15");
        assert_eq!(plan.lane_groups[0].max_agents, 8);
        assert_eq!(plan.lane_groups[0].max_tool_batches, 16);
        assert_eq!(plan.lane_groups[0].max_extension_hostcall_lanes, 4);
        assert_eq!(plan.lane_groups[0].max_rch_verification_fanout, 2);
        assert!(
            plan.lane_groups[0]
                .target_dir
                .contains("/data/tmp/pi_agent_rust_cargo/<agent>/numa-node-0/target")
        );
    }

    #[test]
    fn swarm_resource_preflight_lane_placement_degrades_constrained_single_node_fixture() {
        let snapshot = SwarmResourcePreflightSnapshot {
            logical_cpu_cores: 64,
            cpu_quota: CgroupCpuQuota {
                source: Some("fixture/cpu.max".to_string()),
                quota_us: Some(200_000),
                period_us: Some(100_000),
                quota_cores: Some(2.0),
                unlimited: false,
            },
            cpuset: CpuSetSnapshot {
                source: Some("fixture/cpuset".to_string()),
                raw: Some("0-3".to_string()),
                cpus: vec![0, 1, 2, 3],
                cpu_count: Some(4),
            },
            numa: NumaTopologySnapshot {
                source: Some("fixture/numa".to_string()),
                raw_online: Some("0".to_string()),
                nodes: vec![0],
            },
            memory: MemoryLimitSnapshot {
                mem_total_bytes: Some(256 * 1024 * MIB_BYTES),
                cgroup_limit_bytes: Some(2 * 1024 * MIB_BYTES),
                effective_limit_bytes: Some(2 * 1024 * MIB_BYTES),
                unlimited: false,
                source: Some("fixture/memory.max".to_string()),
            },
            effective_cpu_cores: 2,
            local_build_pressure: LocalBuildPressureSnapshot {
                source: "fixture".to_string(),
                active_processes: Some(2),
                process_budget: 1,
                status: "saturated".to_string(),
                patterns: local_build_process_patterns(),
            },
            rch_queue_posture: clear_rch_queue_posture_fixture(),
            headroom_paths: ready_headroom_paths_fixture(),
            recommended_budgets: Some(resource_budget_recommendation_fixture(1, 2, 1, 1, 1)),
            plan_error: None,
            source_errors: Vec::new(),
        };

        let plan = swarm_lane_placement_plan(&snapshot, &[]);

        assert_eq!(plan.status, "degraded");
        assert_eq!(plan.lane_groups.len(), 1);
        assert_eq!(plan.lane_groups[0].lane_id, "numa-node-0");
        assert_eq!(plan.lane_groups[0].cpu_affinity_hint, "0-3");
        assert!(
            plan.caveats
                .iter()
                .any(|caveat| caveat == "cpuset_partial:4/64")
        );
        assert!(
            plan.caveats
                .iter()
                .any(|caveat| caveat == "tight_memory_limit:2048MiB")
        );
        assert!(
            plan.caveats
                .iter()
                .any(|caveat| caveat == "local_build_pressure:saturated")
        );
    }

    #[test]
    fn swarm_resource_preflight_lane_placement_degrades_unknown_topology_fixture() {
        let snapshot = SwarmResourcePreflightSnapshot {
            logical_cpu_cores: 8,
            cpu_quota: CgroupCpuQuota {
                source: Some("fixture/cpu.max".to_string()),
                quota_us: None,
                period_us: Some(100_000),
                quota_cores: None,
                unlimited: true,
            },
            cpuset: CpuSetSnapshot {
                source: None,
                raw: None,
                cpus: Vec::new(),
                cpu_count: None,
            },
            numa: NumaTopologySnapshot {
                source: None,
                raw_online: None,
                nodes: Vec::new(),
            },
            memory: MemoryLimitSnapshot {
                mem_total_bytes: Some(16 * 1024 * MIB_BYTES),
                cgroup_limit_bytes: None,
                effective_limit_bytes: Some(16 * 1024 * MIB_BYTES),
                unlimited: true,
                source: Some("fixture/memory.max".to_string()),
            },
            effective_cpu_cores: 8,
            local_build_pressure: clear_local_build_pressure_fixture(),
            rch_queue_posture: clear_rch_queue_posture_fixture(),
            headroom_paths: ready_headroom_paths_fixture(),
            recommended_budgets: Some(resource_budget_recommendation_fixture(4, 8, 2, 2, 0)),
            plan_error: None,
            source_errors: vec![
                "cpuset CPU list unavailable".to_string(),
                "NUMA topology unavailable".to_string(),
            ],
        };

        let plan = swarm_lane_placement_plan(&snapshot, &[]);

        assert_eq!(plan.status, "degraded");
        assert_eq!(plan.lane_groups.len(), 1);
        assert_eq!(plan.lane_groups[0].lane_id, "shared");
        assert_eq!(plan.lane_groups[0].cpu_affinity_hint, "cpuset-unavailable");
        assert!(
            plan.caveats
                .iter()
                .any(|caveat| caveat == "numa_topology_unavailable")
        );
        assert!(
            plan.caveats
                .iter()
                .any(|caveat| caveat == "cpuset_unavailable")
        );
        assert!(
            plan.caveats
                .iter()
                .any(|caveat| caveat == "source_inputs_degraded")
        );
    }

    #[test]
    fn swarm_resource_preflight_lane_placement_blocks_rch_fanout_under_queue_pressure() {
        let rch_posture = rch_queue_posture_from_value(
            &serde_json::json!({
                "status": "saturated",
                "recommended_action": "backoff",
                "reason": "queue_saturated",
                "queue_depth": 512,
                "active_builds": 78,
                "queued_builds": 32,
                "slots_available": 0,
                "slots_total": 78,
                "workers_healthy": 8,
                "workers_total": 8
            }),
            "fixture".to_string(),
        );
        let snapshot = SwarmResourcePreflightSnapshot {
            logical_cpu_cores: 64,
            cpu_quota: CgroupCpuQuota {
                source: Some("fixture/cpu.max".to_string()),
                quota_us: None,
                period_us: Some(100_000),
                quota_cores: None,
                unlimited: true,
            },
            cpuset: CpuSetSnapshot {
                source: Some("fixture/cpuset".to_string()),
                raw: Some("0-63".to_string()),
                cpus: (0..64).collect(),
                cpu_count: Some(64),
            },
            numa: NumaTopologySnapshot {
                source: Some("fixture/numa".to_string()),
                raw_online: Some("0-3".to_string()),
                nodes: vec![0, 1, 2, 3],
            },
            memory: MemoryLimitSnapshot {
                mem_total_bytes: Some(256 * 1024 * MIB_BYTES),
                cgroup_limit_bytes: None,
                effective_limit_bytes: Some(256 * 1024 * MIB_BYTES),
                unlimited: true,
                source: Some("fixture/memory.max".to_string()),
            },
            effective_cpu_cores: 64,
            local_build_pressure: clear_local_build_pressure_fixture(),
            rch_queue_posture: rch_posture,
            headroom_paths: ready_headroom_paths_fixture(),
            recommended_budgets: Some(resource_budget_recommendation_fixture(32, 64, 16, 0, 4)),
            plan_error: None,
            source_errors: Vec::new(),
        };

        let plan = swarm_lane_placement_plan(&snapshot, &["rch_queue:queue_saturated".to_string()]);

        assert_eq!(plan.status, "blocked");
        assert!(
            plan.lane_groups
                .iter()
                .all(|lane| lane.max_rch_verification_fanout == 0)
        );
        assert!(
            plan.caveats
                .iter()
                .any(|caveat| caveat == "rch_queue_pressure:backoff")
        );
        assert!(
            plan.caveats
                .iter()
                .any(|caveat| caveat == "critical_preflight_failures")
        );
    }

    #[test]
    fn swarm_resource_preflight_budget_model_covers_large_host_fixture() {
        let sample = HostResourceSample {
            load_avg_1m: Some(2.0),
            rss_bytes: Some(512 * MIB_BYTES),
            process_count: Some(24),
            fd_count: Some(128),
        };
        let plan = build_swarm_doctor_capacity_plan_with_inventory(
            &sample,
            SwarmHostInventory::new(64, 64, 256 * 1024),
            1024,
        )
        .expect("capacity plan");
        let numa = NumaTopologySnapshot {
            source: Some("fixture".to_string()),
            raw_online: Some("0-3".to_string()),
            nodes: vec![0, 1, 2, 3],
        };
        let local_pressure = LocalBuildPressureSnapshot {
            source: "fixture".to_string(),
            active_processes: Some(0),
            process_budget: 32,
            status: "clear".to_string(),
            patterns: local_build_process_patterns(),
        };
        let rch_posture = rch_queue_posture_from_value(
            &serde_json::json!({
                "status": "ok",
                "recommended_action": "proceed",
                "queue_depth": 0,
                "active_builds": 0,
                "queued_builds": 0,
                "slots_available": 78,
                "slots_total": 78,
                "workers_healthy": 8,
                "workers_total": 8
            }),
            "fixture".to_string(),
        );

        let recommendation =
            resource_budget_recommendation(&plan, &numa, &local_pressure, &rch_posture);

        assert_eq!(recommendation.numa_node_count, 4);
        assert_eq!(recommendation.local_build_process_budget, 32);
        assert!(recommendation.agent_concurrency >= 16);
        assert!(recommendation.tool_concurrency >= recommendation.agent_concurrency);
        assert!(recommendation.extension_hostcall_lanes > 1);
        assert!(recommendation.rch_verification_fanout > 1);
        assert!(
            recommendation
                .explanations
                .iter()
                .any(|entry| entry.budget == "numa_node_count" && entry.rationale.contains("NUMA"))
        );
    }

    #[test]
    fn swarm_resource_preflight_budget_model_degrades_constrained_container() {
        let sample = HostResourceSample {
            load_avg_1m: Some(1.0),
            rss_bytes: Some(384 * MIB_BYTES),
            process_count: Some(12),
            fd_count: Some(64),
        };
        let plan = build_swarm_doctor_capacity_plan_with_inventory(
            &sample,
            SwarmHostInventory::new(2, 2, 2 * 1024),
            128,
        )
        .expect("capacity plan");
        let numa = NumaTopologySnapshot {
            source: Some("fixture".to_string()),
            raw_online: Some("0".to_string()),
            nodes: vec![0],
        };
        let local_pressure = LocalBuildPressureSnapshot {
            source: "fixture".to_string(),
            active_processes: Some(2),
            process_budget: 1,
            status: "saturated".to_string(),
            patterns: local_build_process_patterns(),
        };
        let rch_posture = RchQueuePostureSnapshot {
            source: "fixture".to_string(),
            status: "ok".to_string(),
            recommended_action: "proceed".to_string(),
            reason: Some("fixture_clear".to_string()),
            queue_depth: Some(0),
            active_builds: Some(0),
            queued_builds: Some(0),
            slots_available: Some(2),
            slots_total: Some(8),
            workers_healthy: Some(8),
            workers_total: Some(8),
        };

        let recommendation =
            resource_budget_recommendation(&plan, &numa, &local_pressure, &rch_posture);

        assert_eq!(recommendation.numa_node_count, 1);
        assert_eq!(recommendation.local_build_process_budget, 1);
        assert_eq!(recommendation.agent_concurrency, 1);
        assert_eq!(recommendation.extension_hostcall_lanes, 1);
        assert!(recommendation.max_rss_bytes <= 2 * 1024 * MIB_BYTES);
        assert!(
            recommendation
                .explanations
                .iter()
                .any(|entry| entry.budget == "local_build_pressure"
                    && entry.rationale.contains("reduced"))
        );
    }

    #[test]
    fn swarm_resource_preflight_budget_model_blocks_rch_fanout_under_queue_pressure() {
        let sample = HostResourceSample {
            load_avg_1m: Some(2.0),
            rss_bytes: Some(512 * MIB_BYTES),
            process_count: Some(24),
            fd_count: Some(128),
        };
        let plan = build_swarm_doctor_capacity_plan_with_inventory(
            &sample,
            SwarmHostInventory::new(64, 64, 256 * 1024),
            256,
        )
        .expect("capacity plan");
        let numa = NumaTopologySnapshot {
            source: Some("fixture".to_string()),
            raw_online: Some("0-3".to_string()),
            nodes: vec![0, 1, 2, 3],
        };
        let local_pressure = LocalBuildPressureSnapshot {
            source: "fixture".to_string(),
            active_processes: Some(0),
            process_budget: 32,
            status: "clear".to_string(),
            patterns: local_build_process_patterns(),
        };
        let rch_posture = rch_queue_posture_from_value(
            &serde_json::json!({
                "status": "ok",
                "slot_pressure": "saturated",
                "recommended_action": "backoff",
                "reason": "queue_saturated",
                "queue_depth": 512,
                "active_builds": 78,
                "queued_builds": 32,
                "slots_available": 0,
                "slots_total": 78,
                "workers_healthy": 8,
                "workers_total": 8
            }),
            "fixture".to_string(),
        );

        let recommendation =
            resource_budget_recommendation(&plan, &numa, &local_pressure, &rch_posture);

        assert_eq!(rch_posture.recommended_action, "backoff");
        assert_eq!(recommendation.rch_verification_fanout, 0);
        assert!(
            recommendation
                .explanations
                .iter()
                .any(|entry| entry.budget == "rch_verification_fanout"
                    && entry.rationale.contains("backoff"))
        );
    }

    #[test]
    fn session_healthy_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(!is_session_healthy(&path));
    }

    #[test]
    fn session_healthy_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("valid.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"session","version":3,"id":"doctor-jsonl","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
        )
        .unwrap();
        assert!(is_session_healthy(&path));
    }

    #[test]
    fn session_healthy_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.jsonl");
        std::fs::write(&path, "not json at all\n").unwrap();
        assert!(!is_session_healthy(&path));
    }

    #[test]
    fn session_healthy_rejects_non_header_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("array.jsonl");
        std::fs::write(&path, "[1,2,3]\n").unwrap();
        assert!(!is_session_healthy(&path));
    }

    #[cfg(feature = "sqlite-sessions")]
    #[test]
    fn session_healthy_valid_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("valid.sqlite");
        let header = SessionHeader {
            id: "doctor-sqlite".to_string(),
            ..SessionHeader::default()
        };
        futures::executor::block_on(async {
            crate::session_sqlite::save_session(&path, &header, &[], true)
                .await
                .expect("save sqlite session");
        });
        assert!(is_session_healthy(&path));
    }

    #[cfg(feature = "sqlite-sessions")]
    #[test]
    fn session_healthy_rejects_invalid_sqlite_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.sqlite");
        let header = SessionHeader {
            id: "doctor-sqlite".to_string(),
            ..SessionHeader::default()
        };
        futures::executor::block_on(async {
            crate::session_sqlite::save_session(&path, &header, &[], true)
                .await
                .expect("save sqlite session");
        });
        let invalid_header = SessionHeader {
            r#type: "not-session".to_string(),
            ..header
        };
        let invalid_json =
            serde_json::to_string(&invalid_header).expect("serialize invalid session header");
        let config = sqlmodel_sqlite::SqliteConfig::file(path.to_string_lossy())
            .flags(sqlmodel_sqlite::OpenFlags::create_read_write());
        let conn = sqlmodel_sqlite::SqliteConnection::open(&config).expect("open sqlite db");
        conn.execute_sync(
            "UPDATE pi_session_header SET json = ?1",
            &[sqlmodel_core::Value::Text(invalid_json)],
        )
        .expect("corrupt sqlite header row");
        assert!(!is_session_healthy(&path));
    }

    #[test]
    fn check_dir_creates_missing_with_fix() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("sub/nested");
        let mut findings = Vec::new();
        check_dir(CheckCategory::Dirs, "test", &missing, true, &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Pass);
        assert_eq!(findings[0].fixability, Fixability::Fixed);
        assert!(missing.is_dir());
    }

    #[test]
    fn check_dir_warns_missing_without_fix() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("sub/nested");
        let mut findings = Vec::new();
        check_dir(CheckCategory::Dirs, "test", &missing, false, &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warn);
        assert_eq!(findings[0].fixability, Fixability::AutoFixable);
        assert!(!missing.exists());
    }

    #[test]
    fn check_shell_finds_bash() {
        let mut findings = Vec::new();
        check_tool(
            CheckCategory::Shell,
            "bash",
            &["--version"],
            Severity::Fail,
            ToolCheckMode::ProbeExecution,
            &mut findings,
        );
        // bash should be available in CI/dev environments
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Pass);
    }

    #[cfg(unix)]
    #[test]
    fn check_tool_falls_back_when_probe_args_are_unsupported() {
        let mut findings = Vec::new();
        check_tool(
            CheckCategory::Shell,
            "sh",
            &["--version"],
            Severity::Fail,
            ToolCheckMode::ProbeExecution,
            &mut findings,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Pass);
    }

    #[cfg(unix)]
    #[test]
    fn check_tool_reports_invocation_failure_for_broken_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("broken_tool.sh");
        // Mark a non-binary, non-script blob as executable so spawn fails with
        // "exec format error" rather than "not found".
        std::fs::write(&script, "not an executable format").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let mut findings = Vec::new();
        check_tool(
            CheckCategory::Shell,
            script.to_str().unwrap(),
            &["--version"],
            Severity::Fail,
            ToolCheckMode::ProbeExecution,
            &mut findings,
        );

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Fail);
        assert!(findings[0].title.contains("invocation failed"));
    }

    #[test]
    fn check_settings_file_rejects_non_object_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "[1,2,3]").unwrap();
        let mut findings = Vec::new();
        check_settings_file(CheckCategory::Config, &path, "Settings", &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Fail);
        assert!(
            findings[0]
                .title
                .contains("top-level value must be a JSON object")
        );
    }

    #[test]
    fn fixability_display() {
        // Ensure serialization works
        let json = serde_json::to_string(&Fixability::AutoFixable).unwrap();
        assert!(json.contains("autoFixable") || json.contains("auto"));
    }

    #[test]
    fn run_doctor_path_mode_defaults_to_extension_checks_only() {
        let dir = tempfile::tempdir().unwrap();
        let opts = DoctorOptions {
            cwd: dir.path(),
            extension_path: Some("missing-ext"),
            policy_override: None,
            fix: false,
            only: None,
        };
        let report = run_doctor(&opts).unwrap();
        assert!(
            !report.findings.is_empty(),
            "missing extension path should produce at least one finding"
        );
        assert!(
            report
                .findings
                .iter()
                .all(|f| matches!(f.category, CheckCategory::Extensions)),
            "path mode should not run unrelated environment categories by default"
        );
    }

    #[test]
    fn run_doctor_only_extensions_without_path_reports_error_finding() {
        let mut only = HashSet::new();
        only.insert(CheckCategory::Extensions);
        let dir = tempfile::tempdir().unwrap();
        let opts = DoctorOptions {
            cwd: dir.path(),
            extension_path: None,
            policy_override: None,
            fix: false,
            only: Some(only),
        };
        let report = run_doctor(&opts).unwrap();
        assert!(
            report.findings.iter().any(|f| {
                matches!(f.category, CheckCategory::Extensions)
                    && matches!(f.severity, Severity::Fail)
            }),
            "extensions-only mode without a path should emit a clear failure finding"
        );
    }

    #[test]
    fn run_doctor_extension_path_uses_supplied_cwd_for_policy_resolution() {
        let project = tempfile::tempdir().expect("project dir");
        let config_dir = project.path().join(".pi");
        std::fs::create_dir_all(&config_dir).expect("create project config dir");
        std::fs::write(
            config_dir.join("settings.json"),
            r#"{ "extensionPolicy": { "profile": "safe" } }"#,
        )
        .expect("write project settings");
        write_extension_fixture(
            project.path(),
            r#"
const { exec } = require("child_process");
export default function(pi) {
    pi.exec("ls");
}
"#,
        );

        let opts = DoctorOptions {
            cwd: project.path(),
            extension_path: Some("ext"),
            policy_override: None,
            fix: false,
            only: None,
        };
        let report = run_doctor(&opts).expect("doctor report");

        assert!(
            report.findings.iter().any(|f| f.title.contains("exec")),
            "doctor should honor the supplied cwd's safe policy and flag exec use"
        );
    }

    #[test]
    fn run_doctor_extension_path_reports_config_load_failure_without_aborting() {
        let project = tempfile::tempdir().expect("project dir");
        let config_dir = project.path().join(".pi");
        std::fs::create_dir_all(&config_dir).expect("create project config dir");
        std::fs::write(config_dir.join("settings.json"), r#"{ "extensionPolicy": "#)
            .expect("write malformed project settings");
        write_extension_fixture(
            project.path(),
            r#"
import net from "node:net";
"#,
        );

        let opts = DoctorOptions {
            cwd: project.path(),
            extension_path: Some("ext"),
            policy_override: None,
            fix: false,
            only: None,
        };
        let report = run_doctor(&opts).expect("doctor report");

        assert!(
            report
                .findings
                .iter()
                .all(|f| matches!(f.category, CheckCategory::Extensions)),
            "extension path mode should keep findings scoped to extensions"
        );
        assert!(
            report.findings.iter().any(|f| {
                f.title
                    .as_str()
                    .eq("Failed to load configuration for extension policy resolution")
            }),
            "doctor should surface config load failures as findings instead of returning Err"
        );
        assert!(
            report.findings.iter().any(|f| f.title.contains("node:net")),
            "doctor should continue extension analysis after a config load failure"
        );
    }

    #[test]
    fn run_doctor_extension_path_config_load_failure_falls_back_to_safe_policy() {
        let project = tempfile::tempdir().expect("project dir");
        let config_dir = project.path().join(".pi");
        std::fs::create_dir_all(&config_dir).expect("create project config dir");
        std::fs::write(config_dir.join("settings.json"), r#"{ "extensionPolicy": "#)
            .expect("write malformed project settings");
        write_extension_fixture(
            project.path(),
            r#"
export default function(pi) {
    pi.exec("ls");
}
"#,
        );

        let opts = DoctorOptions {
            cwd: project.path(),
            extension_path: Some("ext"),
            policy_override: None,
            fix: false,
            only: None,
        };
        let report = run_doctor(&opts).expect("doctor report");

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.title.as_str().eq("Extension ext: incompatible")),
            "doctor should fail closed under a safe fallback when config loading fails"
        );
        assert!(
            report.findings.iter().any(|f| f.title.contains("exec")),
            "safe fallback should still flag denied exec usage"
        );
    }

    #[test]
    fn run_doctor_extension_path_config_load_failure_honors_cli_policy_override() {
        let project = tempfile::tempdir().expect("project dir");
        let config_dir = project.path().join(".pi");
        std::fs::create_dir_all(&config_dir).expect("create project config dir");
        std::fs::write(config_dir.join("settings.json"), r#"{ "extensionPolicy": "#)
            .expect("write malformed project settings");
        write_extension_fixture(
            project.path(),
            r#"
export default function(pi) {
    pi.exec("ls");
}
"#,
        );

        let opts = DoctorOptions {
            cwd: project.path(),
            extension_path: Some("ext"),
            policy_override: Some("permissive"),
            fix: false,
            only: None,
        };
        let report = run_doctor(&opts).expect("doctor report");

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.title.as_str().eq("Extension ext: compatible")),
            "explicit CLI overrides should still control fallback analysis"
        );
        assert!(
            !report.findings.iter().any(|f| f.title.contains("exec")),
            "permissive override should suppress safe-only exec denial findings"
        );
    }

    mod proptest_doctor {
        use super::*;
        use proptest::prelude::*;

        const ALL_SEVERITIES: &[Severity] = &[
            Severity::Pass,
            Severity::Info,
            Severity::Warn,
            Severity::Fail,
        ];

        const CATEGORY_ALIASES: &[&str] = &[
            "config",
            "dirs",
            "directories",
            "auth",
            "authentication",
            "shell",
            "sessions",
            "extensions",
            "ext",
        ];

        proptest! {
            /// Severity ordering is total: Pass < Info < Warn < Fail.
            #[test]
            fn severity_ordering_total(a in 0..4usize, b in 0..4usize) {
                let sa = ALL_SEVERITIES[a];
                let sb = ALL_SEVERITIES[b];
                match a.cmp(&b) {
                    std::cmp::Ordering::Less => assert!(sa < sb),
                    std::cmp::Ordering::Equal => assert!(sa.eq(&sb)),
                    std::cmp::Ordering::Greater => assert!(sa > sb),
                }
            }

            /// Severity display produces uppercase 4-char labels.
            #[test]
            fn severity_display_uppercase(idx in 0..4usize) {
                let s = ALL_SEVERITIES[idx];
                let display = s.to_string();
                assert_eq!(display.len(), 4);
                assert!(display.chars().all(|c| c.is_ascii_uppercase()));
            }

            /// `CheckCategory::from_str` accepts all known aliases.
            #[test]
            fn check_category_known_aliases(idx in 0..CATEGORY_ALIASES.len()) {
                let alias = CATEGORY_ALIASES[idx];
                assert!(alias.parse::<CheckCategory>().is_ok());
            }

            /// `CheckCategory::from_str` is case-insensitive.
            #[test]
            fn check_category_case_insensitive(idx in 0..CATEGORY_ALIASES.len()) {
                let alias = CATEGORY_ALIASES[idx];
                let upper = alias.to_uppercase();
                let lower_result = alias.parse::<CheckCategory>();
                let upper_result = upper.parse::<CheckCategory>();
                assert_eq!(lower_result, upper_result);
            }

            /// Unknown category names are rejected.
            #[test]
            fn check_category_unknown_rejected(s in "[a-z]{10,20}") {
                assert!(s.parse::<CheckCategory>().is_err());
            }

            /// `CheckCategory::label` returns non-empty strings.
            #[test]
            fn check_category_label_non_empty(idx in 0..6usize) {
                let cats = [
                    CheckCategory::Config,
                    CheckCategory::Dirs,
                    CheckCategory::Auth,
                    CheckCategory::Shell,
                    CheckCategory::Sessions,
                    CheckCategory::Extensions,
                ];
                let label = cats[idx].label();
                assert!(!label.is_empty());
                // Label starts with uppercase
                assert!(label.starts_with(|c: char| c.is_uppercase()));
            }

            /// `DoctorReport::from_findings` summary counts match input.
            #[test]
            fn from_findings_counts_match(
                pass in 0..5usize,
                info in 0..5usize,
                warn in 0..5usize,
                fail in 0..5usize
            ) {
                let mut findings = Vec::new();
                for _ in 0..pass {
                    findings.push(Finding::pass(CheckCategory::Config, "test"));
                }
                for _ in 0..info {
                    findings.push(Finding::info(CheckCategory::Config, "test"));
                }
                for _ in 0..warn {
                    findings.push(Finding::warn(CheckCategory::Config, "test"));
                }
                for _ in 0..fail {
                    findings.push(Finding::fail(CheckCategory::Config, "test"));
                }

                let report = DoctorReport::from_findings(findings);
                assert_eq!(report.summary.pass, pass);
                assert_eq!(report.summary.info, info);
                assert_eq!(report.summary.warn, warn);
                assert_eq!(report.summary.fail, fail);
            }

            /// `DoctorReport::from_findings` overall severity is max of inputs.
            #[test]
            fn from_findings_overall_severity(
                pass in 0..3usize,
                info in 0..3usize,
                warn in 0..3usize,
                fail in 0..3usize
            ) {
                let mut findings = Vec::new();
                for _ in 0..pass {
                    findings.push(Finding::pass(CheckCategory::Config, "test"));
                }
                for _ in 0..info {
                    findings.push(Finding::info(CheckCategory::Config, "test"));
                }
                for _ in 0..warn {
                    findings.push(Finding::warn(CheckCategory::Config, "test"));
                }
                for _ in 0..fail {
                    findings.push(Finding::fail(CheckCategory::Config, "test"));
                }

                let report = DoctorReport::from_findings(findings);

                if fail > 0 {
                    assert_eq!(report.overall, Severity::Fail);
                } else if warn > 0 {
                    assert_eq!(report.overall, Severity::Warn);
                } else {
                    assert_eq!(report.overall, Severity::Pass);
                }
            }

            /// `is_known_config_key` accepts both camelCase and snake_case forms.
            #[test]
            fn config_key_pairs(idx in 0..10usize) {
                let pairs = [
                    ("hideThinkingBlock", "hide_thinking_block"),
                    ("showHardwareCursor", "show_hardware_cursor"),
                    ("defaultProvider", "default_provider"),
                    ("defaultModel", "default_model"),
                    ("defaultThinkingLevel", "default_thinking_level"),
                    ("enabledModels", "enabled_models"),
                    ("steeringMode", "steering_mode"),
                    ("followUpMode", "follow_up_mode"),
                    ("quietStartup", "quiet_startup"),
                    ("collapseChangelog", "collapse_changelog"),
                ];
                let (camel, snake) = pairs[idx];
                assert!(is_known_config_key(camel), "camelCase key {camel} should be known");
                assert!(is_known_config_key(snake), "snake_case key {snake} should be known");
            }

            /// `is_known_config_key` rejects garbage keys.
            #[test]
            fn config_key_rejects_garbage(s in "[A-Z]{20,30}") {
                assert!(!is_known_config_key(&s));
            }

            /// Severity serde roundtrip is lowercase.
            #[test]
            fn severity_serde_lowercase(idx in 0..4usize) {
                let s = ALL_SEVERITIES[idx];
                let json = serde_json::to_string(&s).unwrap();
                let expected = format!("\"{}\"", s.to_string().to_lowercase());
                assert_eq!(json, expected);
            }

            /// Finding builder chain preserves fields.
            #[test]
            fn finding_builder_chain(title in "[a-z ]{1,20}", detail in "[a-z ]{1,20}") {
                let f = Finding::warn(CheckCategory::Shell, title.clone())
                    .with_detail(detail.clone())
                    .with_remediation("fix it")
                    .auto_fixable();
                assert_eq!(f.title, title);
                assert_eq!(f.detail.as_deref(), Some(detail.as_str()));
                assert_eq!(f.remediation.as_deref(), Some("fix it"));
                assert_eq!(f.fixability, Fixability::AutoFixable);
                assert_eq!(f.severity, Severity::Warn);
            }

            /// `fixed()` resets severity to Pass.
            #[test]
            fn finding_fixed_resets_severity(idx in 0..4usize) {
                let builders = [
                    Finding::pass(CheckCategory::Config, "t"),
                    Finding::info(CheckCategory::Config, "t"),
                    Finding::warn(CheckCategory::Config, "t"),
                    Finding::fail(CheckCategory::Config, "t"),
                ];
                let fixed = builders[idx].clone().fixed();
                assert_eq!(fixed.severity, Severity::Pass);
                assert_eq!(fixed.fixability, Fixability::Fixed);
            }
        }
    }
}
