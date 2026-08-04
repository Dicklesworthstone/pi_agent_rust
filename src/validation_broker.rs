//! Durable validation slot lease store for the live validation broker.
//!
//! The store is append-only JSONL. Loading is fail-closed: malformed or
//! unavailable records produce a degraded snapshot instead of inventing a green
//! validation state.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

pub const VALIDATION_BROKER_SLOT_SCHEMA: &str = "pi.validation_broker.slot.v1";
pub const VALIDATION_BROKER_SLOT_STORE_SCHEMA: &str = "pi.validation_broker.slot_store.v1";
pub const VALIDATION_BROKER_SLOT_RECORD_SCHEMA: &str = "pi.validation_broker.slot_store.record.v1";
pub const VALIDATION_BROKER_REQUEST_SCHEMA: &str = "pi.validation_broker.request.v1";
pub const VALIDATION_BROKER_DECISION_SCHEMA: &str = "pi.validation_broker.decision.v1";
pub const VALIDATION_BROKER_INPUT_SCHEMA: &str = "pi.validation_broker.input_snapshot.v1";
pub const VALIDATION_BROKER_SOURCE_PROVENANCE_SCHEMA: &str =
    "pi.validation_broker.source_provenance.v1";
pub const VALIDATION_BROKER_RCH_INPUT_SCHEMA: &str = "pi.validation_broker.rch_input.v1";
pub const VALIDATION_BROKER_HEADROOM_INPUT_SCHEMA: &str = "pi.validation_broker.headroom_input.v1";
pub const VALIDATION_BROKER_DOCTOR_INPUT_SCHEMA: &str = "pi.validation_broker.doctor_input.v1";
pub const VALIDATION_BROKER_GIT_INPUT_SCHEMA: &str = "pi.validation_broker.git_input.v1";
pub const VALIDATION_BROKER_BEADS_INPUT_SCHEMA: &str = "pi.validation_broker.beads_input.v1";
pub const VALIDATION_BROKER_CLI_STATUS_SCHEMA: &str = "pi.validation_broker.cli_status.v1";
pub const VALIDATION_BROKER_CLI_PLAN_SCHEMA: &str = "pi.validation_broker.cli_plan.v1";
pub const VALIDATION_BROKER_CLI_LEASE_MUTATION_SCHEMA: &str =
    "pi.validation_broker.cli_lease_mutation.v1";
pub const VALIDATION_BROKER_STRESS_BUDGET_REPORT_SCHEMA: &str =
    "pi.validation_broker.stress_budget_report.v1";
pub const VALIDATION_BROKER_STRESS_EVIDENCE_SCHEMA: &str =
    "pi.validation_broker.stress_evidence.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationSlotState {
    Requested,
    Active,
    Reusable,
    Stale,
    Failed,
    Released,
    Expired,
    Degraded,
}

impl ValidationSlotState {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Failed | Self::Released | Self::Expired | Self::Degraded
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationSlotArtifact {
    pub path: String,
    pub sha256: Option<String>,
    pub schema: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationSlotRequest {
    pub slot_id: String,
    pub owner_agent: String,
    pub bead_id: String,
    pub command: Vec<String>,
    pub command_class: String,
    pub cwd: String,
    pub git_head: String,
    pub feature_flags: Vec<String>,
    pub target_dir: String,
    pub tmpdir: String,
    pub runner: String,
    pub rust_toolchain: Option<String>,
    pub rch_job_id: Option<String>,
    pub environment: BTreeMap<String, String>,
    pub expected_artifacts: Vec<ValidationSlotArtifact>,
    pub artifact_schema: Option<String>,
    pub artifact_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationSlotLease {
    pub schema: String,
    pub slot_id: String,
    pub state: ValidationSlotState,
    pub owner_agent: String,
    pub bead_id: String,
    pub command: Vec<String>,
    pub command_class: String,
    pub cwd: String,
    pub command_fingerprint: String,
    pub environment_fingerprint: String,
    pub git_head: String,
    pub feature_flags: Vec<String>,
    pub target_dir: String,
    pub tmpdir: String,
    pub runner: String,
    pub rust_toolchain: Option<String>,
    pub rch_job_id: Option<String>,
    pub started_at_utc: String,
    pub heartbeat_at_utc: String,
    pub expires_at_utc: String,
    pub expected_artifacts: Vec<ValidationSlotArtifact>,
    pub artifacts: Vec<ValidationSlotArtifact>,
    pub artifact_schema: Option<String>,
    pub artifact_hash: Option<String>,
    pub release_reason: Option<String>,
    pub state_reason: Option<String>,
}

impl ValidationSlotLease {
    pub fn acquire(
        request: ValidationSlotRequest,
        started_at_utc: impl Into<String>,
        expires_at_utc: impl Into<String>,
    ) -> Result<Self> {
        validate_request(&request)?;
        let started_at_utc = started_at_utc.into();
        let expires_at_utc = expires_at_utc.into();
        ensure_future_expiry(&started_at_utc, &expires_at_utc)?;
        let command_fingerprint = command_fingerprint(&request)?;
        let environment_fingerprint = environment_fingerprint(&request.environment)?;

        Ok(Self {
            schema: VALIDATION_BROKER_SLOT_SCHEMA.to_string(),
            slot_id: request.slot_id,
            state: ValidationSlotState::Active,
            owner_agent: request.owner_agent,
            bead_id: request.bead_id,
            command_fingerprint,
            environment_fingerprint,
            command: request.command,
            command_class: request.command_class,
            cwd: request.cwd,
            git_head: request.git_head,
            feature_flags: request.feature_flags,
            target_dir: request.target_dir,
            tmpdir: request.tmpdir,
            runner: request.runner,
            rust_toolchain: request.rust_toolchain,
            rch_job_id: request.rch_job_id,
            heartbeat_at_utc: started_at_utc.clone(),
            started_at_utc,
            expires_at_utc,
            expected_artifacts: request.expected_artifacts,
            artifacts: Vec::new(),
            artifact_schema: request.artifact_schema,
            artifact_hash: request.artifact_hash,
            release_reason: None,
            state_reason: None,
        })
    }

    pub fn renew(
        &mut self,
        owner_agent: &str,
        heartbeat_at_utc: impl Into<String>,
        expires_at_utc: impl Into<String>,
    ) -> Result<()> {
        self.ensure_owner(owner_agent)?;
        if self.state.is_terminal() {
            return Err(Error::validation(format!(
                "cannot renew terminal slot {} in state {:?}",
                self.slot_id, self.state
            )));
        }
        let heartbeat_at_utc = heartbeat_at_utc.into();
        let expires_at_utc = expires_at_utc.into();
        ensure_future_expiry(&heartbeat_at_utc, &expires_at_utc)?;
        self.heartbeat_at_utc = heartbeat_at_utc;
        self.expires_at_utc = expires_at_utc;
        self.state = ValidationSlotState::Active;
        self.state_reason = None;
        Ok(())
    }

    pub fn mark_reusable(
        &mut self,
        owner_agent: &str,
        heartbeat_at_utc: impl Into<String>,
        artifacts: Vec<ValidationSlotArtifact>,
    ) -> Result<()> {
        self.ensure_owner(owner_agent)?;
        if self.state.is_terminal() {
            return Err(Error::validation(format!(
                "cannot reuse terminal slot {} in state {:?}",
                self.slot_id, self.state
            )));
        }
        if artifacts.is_empty() {
            return Err(Error::validation("reusable slots require artifacts"));
        }
        self.heartbeat_at_utc = heartbeat_at_utc.into();
        parse_utc(&self.heartbeat_at_utc)?;
        self.artifacts = artifacts;
        self.state = ValidationSlotState::Reusable;
        self.state_reason = Some("validation_succeeded".to_string());
        Ok(())
    }

    pub fn mark_stale(
        &mut self,
        now_utc: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<()> {
        let now_utc = now_utc.into();
        parse_utc(&now_utc)?;
        let reason = non_empty(reason.into(), "stale reason")?;
        if !self.is_stale_at(&now_utc)? {
            return Err(Error::validation(format!(
                "slot {} is not stale at {now_utc}",
                self.slot_id
            )));
        }
        if self.state.is_terminal() {
            return Err(Error::validation(format!(
                "cannot mark terminal slot {} stale",
                self.slot_id
            )));
        }
        self.state = ValidationSlotState::Stale;
        self.heartbeat_at_utc = now_utc;
        self.state_reason = Some(reason);
        Ok(())
    }

    pub fn release(
        &mut self,
        owner_agent: &str,
        heartbeat_at_utc: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<()> {
        self.ensure_owner(owner_agent)?;
        let reason = non_empty(reason.into(), "release reason")?;
        self.heartbeat_at_utc = heartbeat_at_utc.into();
        parse_utc(&self.heartbeat_at_utc)?;
        self.state = ValidationSlotState::Released;
        self.release_reason = Some(reason);
        self.state_reason = Some("released_by_owner".to_string());
        Ok(())
    }

    pub fn fail(
        &mut self,
        owner_agent: &str,
        heartbeat_at_utc: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<()> {
        self.ensure_owner(owner_agent)?;
        let reason = non_empty(reason.into(), "failure reason")?;
        self.heartbeat_at_utc = heartbeat_at_utc.into();
        parse_utc(&self.heartbeat_at_utc)?;
        self.state = ValidationSlotState::Failed;
        self.state_reason = Some(reason);
        Ok(())
    }

    pub fn is_stale_at(&self, now_utc: &str) -> Result<bool> {
        let now = parse_utc(now_utc)?;
        let expires = parse_utc(&self.expires_at_utc)?;
        Ok(now > expires)
    }

    pub fn matches_request_equivalence(&self, request: &ValidationSlotRequest) -> Result<bool> {
        Ok(self.command_fingerprint == command_fingerprint(request)?
            && self.environment_fingerprint == environment_fingerprint(&request.environment)?
            && self.cwd == request.cwd
            && self.git_head == request.git_head
            && self.feature_flags == request.feature_flags
            && self.target_dir == request.target_dir
            && self.tmpdir == request.tmpdir
            && self.runner == request.runner
            && self.rust_toolchain == request.rust_toolchain
            && self.artifact_schema == request.artifact_schema
            && self.artifact_hash == request.artifact_hash)
    }

    fn ensure_owner(&self, owner_agent: &str) -> Result<()> {
        if self.owner_agent == owner_agent {
            Ok(())
        } else {
            Err(Error::validation(format!(
                "slot {} is owned by {}, not {owner_agent}",
                self.slot_id, self.owner_agent
            )))
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schema != VALIDATION_BROKER_SLOT_SCHEMA {
            return Err(Error::validation(format!(
                "slot {} has unexpected schema {}",
                self.slot_id, self.schema
            )));
        }
        require_non_empty(&self.slot_id, "slot_id")?;
        require_non_empty(&self.owner_agent, "owner_agent")?;
        require_non_empty(&self.bead_id, "bead_id")?;
        require_non_empty(&self.command_fingerprint, "command_fingerprint")?;
        require_non_empty(&self.environment_fingerprint, "environment_fingerprint")?;
        require_non_empty(&self.git_head, "git_head")?;
        require_non_empty(&self.target_dir, "target_dir")?;
        require_non_empty(&self.tmpdir, "tmpdir")?;
        require_non_empty(&self.runner, "runner")?;
        parse_utc(&self.started_at_utc)?;
        parse_utc(&self.heartbeat_at_utc)?;
        parse_utc(&self.expires_at_utc)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationSlotStoreRecord {
    pub schema: String,
    pub event: String,
    pub recorded_at_utc: String,
    pub lease: ValidationSlotLease,
}

impl ValidationSlotStoreRecord {
    pub fn new(
        event: impl Into<String>,
        recorded_at_utc: impl Into<String>,
        lease: ValidationSlotLease,
    ) -> Result<Self> {
        let event = non_empty(event.into(), "event")?;
        let recorded_at_utc = recorded_at_utc.into();
        parse_utc(&recorded_at_utc)?;
        lease.validate()?;
        Ok(Self {
            schema: VALIDATION_BROKER_SLOT_RECORD_SCHEMA.to_string(),
            event,
            recorded_at_utc,
            lease,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.schema != VALIDATION_BROKER_SLOT_RECORD_SCHEMA {
            return Err(Error::validation(format!(
                "record has unexpected schema {}",
                self.schema
            )));
        }
        require_non_empty(&self.event, "event")?;
        parse_utc(&self.recorded_at_utc)?;
        self.lease.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationSlotStoreStatus {
    Available,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationSlotStoreSnapshot {
    pub schema: String,
    pub status: ValidationSlotStoreStatus,
    pub leases: Vec<ValidationSlotLease>,
    pub latest_by_slot_id: BTreeMap<String, ValidationSlotLease>,
    pub degraded_reasons: Vec<String>,
}

impl ValidationSlotStoreSnapshot {
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.status == ValidationSlotStoreStatus::Degraded
    }
}

#[derive(Debug, Clone)]
pub struct ValidationSlotStore {
    path: PathBuf,
}

impl ValidationSlotStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_record(&self, record: &ValidationSlotStoreRecord) -> Result<()> {
        record.validate()?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    pub fn append_lease(
        &self,
        event: impl Into<String>,
        recorded_at_utc: impl Into<String>,
        lease: &ValidationSlotLease,
    ) -> Result<()> {
        let record = ValidationSlotStoreRecord::new(event, recorded_at_utc, lease.clone())?;
        self.append_record(&record)
    }

    #[must_use]
    pub fn load_snapshot(&self) -> ValidationSlotStoreSnapshot {
        let mut leases = Vec::new();
        let mut latest_by_slot_id = BTreeMap::new();
        let mut degraded_reasons = Vec::new();

        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return snapshot(leases, latest_by_slot_id, degraded_reasons);
            }
            Err(err) => {
                degraded_reasons.push(format!("store_unavailable: {err}"));
                return snapshot(leases, latest_by_slot_id, degraded_reasons);
            }
        };

        for (line_index, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ValidationSlotStoreRecord>(line) {
                Ok(record) => match record.validate() {
                    Ok(()) => {
                        leases.push(record.lease);
                    }
                    Err(err) => {
                        degraded_reasons.push(line_degraded_reason(
                            line_index,
                            "invalid lease",
                            err,
                        ));
                    }
                },
                Err(err) => {
                    degraded_reasons.push(line_degraded_reason(
                        line_index,
                        "malformed record",
                        err,
                    ));
                }
            }
        }

        latest_by_slot_id = leases
            .iter()
            .map(|lease| (lease.slot_id.clone(), lease.clone()))
            .collect();
        snapshot(leases, latest_by_slot_id, degraded_reasons)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationSourceState {
    Available,
    Degraded,
    Unavailable,
}

impl ValidationSourceState {
    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded | Self::Unavailable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationSourceProvenance {
    pub schema: String,
    pub source: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub captured_at_utc: String,
    pub artifact_path: Option<String>,
}

impl ValidationSourceProvenance {
    pub fn new(
        source: impl Into<String>,
        command: Vec<String>,
        cwd: impl Into<String>,
        captured_at_utc: impl Into<String>,
        artifact_path: Option<String>,
    ) -> Result<Self> {
        let provenance = Self {
            schema: VALIDATION_BROKER_SOURCE_PROVENANCE_SCHEMA.to_string(),
            source: source.into(),
            command,
            cwd: cwd.into(),
            captured_at_utc: captured_at_utc.into(),
            artifact_path,
        };
        provenance.validate()?;
        Ok(provenance)
    }

    fn validate(&self) -> Result<()> {
        if self.schema != VALIDATION_BROKER_SOURCE_PROVENANCE_SCHEMA {
            return Err(Error::validation(format!(
                "source provenance has unexpected schema {}",
                self.schema
            )));
        }
        require_non_empty(&self.source, "source")?;
        require_non_empty(&self.cwd, "source cwd")?;
        parse_utc(&self.captured_at_utc)?;
        for segment in &self.command {
            require_non_empty(segment, "source command segment")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationSourceHealth {
    pub state: ValidationSourceState,
    pub provenance: ValidationSourceProvenance,
    pub degraded_reasons: Vec<String>,
}

impl ValidationSourceHealth {
    const fn available(provenance: ValidationSourceProvenance) -> Self {
        Self {
            state: ValidationSourceState::Available,
            provenance,
            degraded_reasons: Vec::new(),
        }
    }

    const fn degraded(provenance: ValidationSourceProvenance, reasons: Vec<String>) -> Self {
        Self {
            state: ValidationSourceState::Degraded,
            provenance,
            degraded_reasons: reasons,
        }
    }

    fn unavailable(provenance: ValidationSourceProvenance, reason: String) -> Self {
        Self {
            state: ValidationSourceState::Unavailable,
            provenance,
            degraded_reasons: vec![reason],
        }
    }

    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        self.state.is_degraded()
    }
}

pub fn normalize_available_source(
    provenance: ValidationSourceProvenance,
) -> Result<ValidationSourceHealth> {
    provenance.validate()?;
    Ok(ValidationSourceHealth::available(provenance))
}

pub fn normalize_unavailable_source(
    provenance: ValidationSourceProvenance,
    reason: impl Into<String>,
) -> Result<ValidationSourceHealth> {
    provenance.validate()?;
    let reason = non_empty(reason.into(), "unavailable reason")?;
    Ok(ValidationSourceHealth::unavailable(provenance, reason))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationRchInput {
    pub schema: String,
    pub health: ValidationSourceHealth,
    pub active_builds: Option<u64>,
    pub queued_builds: Option<u64>,
    pub free_slots: Option<u64>,
    pub total_slots: Option<u64>,
    pub local_fallback: bool,
    pub saturated: bool,
}

pub fn normalize_rch_queue_text(
    provenance: ValidationSourceProvenance,
    raw: &str,
) -> Result<ValidationRchInput> {
    provenance.validate()?;
    let mut degraded_reasons = Vec::new();
    if raw.trim().is_empty() {
        degraded_reasons.push("rch_queue_output_missing".to_string());
    }

    let active_builds = count_from_line(raw, "Active Build");
    let queued_builds = count_from_line(raw, "Queued Build").or(Some(0));
    let (free_slots, total_slots) = worker_slots(raw);
    let local_fallback = contains_any(raw, &["fail open", "fails open", "local fallback"]);

    if active_builds.is_none() {
        degraded_reasons.push("rch_active_build_count_missing".to_string());
    }
    if free_slots.is_none() || total_slots.is_none() {
        degraded_reasons.push("rch_worker_slot_count_missing".to_string());
    }
    if local_fallback {
        degraded_reasons.push("rch_local_fallback_detected".to_string());
    }

    let saturated = queued_builds.unwrap_or_default() > 0 || free_slots == Some(0);
    Ok(ValidationRchInput {
        schema: VALIDATION_BROKER_RCH_INPUT_SCHEMA.to_string(),
        health: source_health(provenance, degraded_reasons),
        active_builds,
        queued_builds,
        free_slots,
        total_slots,
        local_fallback,
        saturated,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationHeadroomInput {
    pub schema: String,
    pub health: ValidationSourceHealth,
    pub available_bytes: Option<u64>,
    pub required_bytes: Option<u64>,
    pub low_headroom: bool,
}

pub fn normalize_headroom_json(
    provenance: ValidationSourceProvenance,
    value: &Value,
) -> Result<ValidationHeadroomInput> {
    provenance.validate()?;
    let mut degraded_reasons = Vec::new();
    if !value.is_object() {
        degraded_reasons.push("headroom_source_not_object".to_string());
    }
    let available_bytes = u64_field(value, &["available_bytes", "free_bytes", "free"]);
    let required_bytes = u64_field(
        value,
        &[
            "required_bytes",
            "min_required_bytes",
            "minimum_required_bytes",
        ],
    );
    if available_bytes.is_none() {
        degraded_reasons.push("headroom_available_bytes_missing".to_string());
    }
    if required_bytes.is_none() {
        degraded_reasons.push("headroom_required_bytes_missing".to_string());
    }
    let low_headroom = matches!(
        (available_bytes, required_bytes),
        (Some(available), Some(required)) if available < required
    );

    Ok(ValidationHeadroomInput {
        schema: VALIDATION_BROKER_HEADROOM_INPUT_SCHEMA.to_string(),
        health: source_health(provenance, degraded_reasons),
        available_bytes,
        required_bytes,
        low_headroom,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationDoctorCheck {
    pub name: String,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationDoctorInput {
    pub schema: String,
    pub health: ValidationSourceHealth,
    pub checks: Vec<ValidationDoctorCheck>,
    pub has_failures: bool,
}

pub fn normalize_doctor_json(
    provenance: ValidationSourceProvenance,
    value: &Value,
) -> Result<ValidationDoctorInput> {
    provenance.validate()?;
    let mut degraded_reasons = Vec::new();
    let checks_value = value.get("checks").or_else(|| {
        value
            .get("preflight")
            .and_then(|preflight| preflight.get("checks"))
    });
    let mut checks = Vec::new();

    if let Some(raw_checks) = checks_value.and_then(Value::as_array) {
        for (index, raw_check) in raw_checks.iter().enumerate() {
            let name = string_field(raw_check, &["name", "id"]).unwrap_or_else(|| {
                degraded_reasons.push(format!("doctor_check_{}_name_missing", index + 1));
                format!("unnamed_check_{}", index + 1)
            });
            let status = string_field(raw_check, &["status", "result"]).unwrap_or_else(|| {
                degraded_reasons.push(format!("doctor_check_{}_status_missing", index + 1));
                "unknown".to_string()
            });
            checks.push(ValidationDoctorCheck {
                name,
                status,
                message: string_field(raw_check, &["message", "reason"]),
            });
        }
    } else {
        degraded_reasons.push("doctor_checks_missing".to_string());
    }

    if checks.is_empty() {
        degraded_reasons.push("doctor_checks_empty".to_string());
    }

    let has_failures = checks.iter().any(|check| !is_success_status(&check.status));
    Ok(ValidationDoctorInput {
        schema: VALIDATION_BROKER_DOCTOR_INPUT_SCHEMA.to_string(),
        health: source_health(provenance, degraded_reasons),
        checks,
        has_failures,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationGitInput {
    pub schema: String,
    pub health: ValidationSourceHealth,
    pub head: String,
    pub branch: Option<String>,
    pub dirty: bool,
    pub staged_paths: Vec<String>,
    pub unstaged_paths: Vec<String>,
    pub untracked_paths: Vec<String>,
}

pub fn normalize_git_status_text(
    provenance: ValidationSourceProvenance,
    head: impl Into<String>,
    status: &str,
) -> Result<ValidationGitInput> {
    provenance.validate()?;
    let head = non_empty(head.into(), "git head")?;
    let mut branch = None;
    let mut staged_paths = Vec::new();
    let mut unstaged_paths = Vec::new();
    let mut untracked_paths = Vec::new();
    let mut degraded_reasons = Vec::new();

    for line in status.lines() {
        if let Some(raw_branch) = line.strip_prefix("## ") {
            branch = raw_branch
                .split("...")
                .next()
                .and_then(|candidate| candidate.split_whitespace().next())
                .filter(|candidate| !candidate.is_empty())
                .map(ToOwned::to_owned);
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let Some(code) = line.get(..2) else {
            degraded_reasons.push(format!("git_status_line_malformed: {line}"));
            continue;
        };
        let Some(separator) = line.get(2..3) else {
            degraded_reasons.push(format!("git_status_line_malformed: {line}"));
            continue;
        };
        let Some(raw_path) = line.get(3..) else {
            degraded_reasons.push(format!("git_status_line_malformed: {line}"));
            continue;
        };
        if separator != " " || !code.bytes().all(is_git_short_status_code) {
            degraded_reasons.push(format!("git_status_line_malformed: {line}"));
            continue;
        }
        let path = raw_path.trim().to_string();
        if path.is_empty() {
            degraded_reasons.push("git_status_path_missing".to_string());
            continue;
        }
        if code == "??" {
            untracked_paths.push(path);
        } else {
            let mut chars = code.chars();
            let staged = chars.next().is_some_and(|state| state != ' ');
            let unstaged = chars.next().is_some_and(|state| state != ' ');
            if staged {
                staged_paths.push(path.clone());
            }
            if unstaged {
                unstaged_paths.push(path);
            }
        }
    }

    if branch.is_none() {
        degraded_reasons.push("git_branch_missing".to_string());
    }
    let dirty =
        !staged_paths.is_empty() || !unstaged_paths.is_empty() || !untracked_paths.is_empty();

    Ok(ValidationGitInput {
        schema: VALIDATION_BROKER_GIT_INPUT_SCHEMA.to_string(),
        health: source_health(provenance, degraded_reasons),
        head,
        branch,
        dirty,
        staged_paths,
        unstaged_paths,
        untracked_paths,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBeadInput {
    pub id: String,
    pub status: String,
    pub assignee: Option<String>,
    pub updated_at_utc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBeadsInput {
    pub schema: String,
    pub health: ValidationSourceHealth,
    pub ready_count: usize,
    pub in_progress: Vec<ValidationBeadInput>,
    pub stale_in_progress_ids: Vec<String>,
}

pub fn normalize_beads_json(
    provenance: ValidationSourceProvenance,
    value: &Value,
    now_utc: &str,
    stale_after_seconds: i64,
) -> Result<ValidationBeadsInput> {
    provenance.validate()?;
    let now = parse_utc(now_utc)?;
    if stale_after_seconds < 0 {
        return Err(Error::validation(
            "stale_after_seconds must be non-negative",
        ));
    }
    let mut degraded_reasons = Vec::new();
    let issue_values = value
        .as_array()
        .or_else(|| value.get("issues").and_then(Value::as_array));
    let mut ready_count = 0;
    let mut in_progress = Vec::new();
    let mut stale_in_progress_ids = Vec::new();

    if let Some(issues) = issue_values {
        for (index, issue) in issues.iter().enumerate() {
            let Some(id) = string_field(issue, &["id"]) else {
                degraded_reasons.push(format!("bead_{}_id_missing", index + 1));
                continue;
            };
            let Some(status) = string_field(issue, &["status"]) else {
                degraded_reasons.push(format!("bead_{id}_status_missing"));
                continue;
            };
            if status == "open" {
                ready_count += 1;
            }
            if status == "in_progress" {
                let updated_at_utc = string_field(issue, &["updated_at", "updated_at_utc"]);
                if let Some(updated_at) = &updated_at_utc {
                    match parse_utc(updated_at) {
                        Ok(updated) => {
                            if now.signed_duration_since(updated).num_seconds()
                                > stale_after_seconds
                            {
                                stale_in_progress_ids.push(id.clone());
                            }
                        }
                        Err(err) => {
                            degraded_reasons.push(format!("bead_{id}_updated_at_invalid: {err}"));
                        }
                    }
                } else {
                    degraded_reasons.push(format!("bead_{id}_updated_at_missing"));
                }
                in_progress.push(ValidationBeadInput {
                    id,
                    status,
                    assignee: string_field(issue, &["assignee"]),
                    updated_at_utc,
                });
            }
        }
    } else {
        degraded_reasons.push("beads_issue_array_missing".to_string());
    }

    Ok(ValidationBeadsInput {
        schema: VALIDATION_BROKER_BEADS_INPUT_SCHEMA.to_string(),
        health: source_health(provenance, degraded_reasons),
        ready_count,
        in_progress,
        stale_in_progress_ids,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBrokerInputParts {
    pub captured_at_utc: String,
    pub rch: ValidationRchInput,
    pub cargo_headroom: ValidationHeadroomInput,
    pub doctor: ValidationDoctorInput,
    pub git: ValidationGitInput,
    pub beads: ValidationBeadsInput,
    pub scratch_headroom: ValidationHeadroomInput,
    pub agent_mail: ValidationSourceHealth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBrokerInputSnapshot {
    pub schema: String,
    pub captured_at_utc: String,
    pub rch: ValidationRchInput,
    pub cargo_headroom: ValidationHeadroomInput,
    pub doctor: ValidationDoctorInput,
    pub git: ValidationGitInput,
    pub beads: ValidationBeadsInput,
    pub scratch_headroom: ValidationHeadroomInput,
    pub agent_mail: ValidationSourceHealth,
    pub degraded_reasons: Vec<String>,
}

impl ValidationBrokerInputSnapshot {
    pub fn from_parts(parts: ValidationBrokerInputParts) -> Result<Self> {
        parse_utc(&parts.captured_at_utc)?;
        let mut degraded_reasons = Vec::new();
        collect_source_reasons(&mut degraded_reasons, &parts.rch.health);
        collect_source_reasons(&mut degraded_reasons, &parts.cargo_headroom.health);
        collect_source_reasons(&mut degraded_reasons, &parts.doctor.health);
        collect_source_reasons(&mut degraded_reasons, &parts.git.health);
        collect_source_reasons(&mut degraded_reasons, &parts.beads.health);
        collect_source_reasons(&mut degraded_reasons, &parts.scratch_headroom.health);
        collect_source_reasons(&mut degraded_reasons, &parts.agent_mail);

        Ok(Self {
            schema: VALIDATION_BROKER_INPUT_SCHEMA.to_string(),
            captured_at_utc: parts.captured_at_utc,
            rch: parts.rch,
            cargo_headroom: parts.cargo_headroom,
            doctor: parts.doctor,
            git: parts.git,
            beads: parts.beads,
            scratch_headroom: parts.scratch_headroom,
            agent_mail: parts.agent_mail,
            degraded_reasons,
        })
    }

    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        !self.degraded_reasons.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationAdmissionDecision {
    Allow,
    Wait,
    Coalesce,
    Narrow,
    DenyLocalFallback,
    StaleRecover,
    DegradedBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationAdmissionPolicy {
    pub max_active_builds: u64,
    pub max_queued_builds: u64,
    pub min_free_slots_for_broad_gate: u64,
    pub max_active_broad_gates: usize,
    pub reusable_artifact_freshness_seconds: i64,
    pub request_age_boost_seconds: i64,
    pub reuse_required: bool,
    pub allow_narrow_scope: bool,
}

impl Default for ValidationAdmissionPolicy {
    fn default() -> Self {
        Self {
            max_active_builds: 4,
            max_queued_builds: 0,
            min_free_slots_for_broad_gate: 2,
            max_active_broad_gates: 1,
            reusable_artifact_freshness_seconds: 86_400,
            request_age_boost_seconds: 3_600,
            reuse_required: false,
            allow_narrow_scope: true,
        }
    }
}

impl ValidationAdmissionPolicy {
    fn validate(&self) -> Result<()> {
        if self.reusable_artifact_freshness_seconds < 0 {
            return Err(Error::validation(
                "reusable_artifact_freshness_seconds must be non-negative",
            ));
        }
        if self.request_age_boost_seconds < 0 {
            return Err(Error::validation(
                "request_age_boost_seconds must be non-negative",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBrokerStressBudgets {
    pub max_plan_latency_ms: u64,
    pub max_status_latency_ms: u64,
    pub max_stale_scan_ms: u64,
    pub max_slot_store_bytes: u64,
    pub max_memory_growth_bytes: u64,
    pub min_request_throughput_per_minute: u64,
}

impl ValidationBrokerStressBudgets {
    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("max_plan_latency_ms", self.max_plan_latency_ms),
            ("max_status_latency_ms", self.max_status_latency_ms),
            ("max_stale_scan_ms", self.max_stale_scan_ms),
            ("max_slot_store_bytes", self.max_slot_store_bytes),
            ("max_memory_growth_bytes", self.max_memory_growth_bytes),
            (
                "min_request_throughput_per_minute",
                self.min_request_throughput_per_minute,
            ),
        ] {
            if value == 0 {
                return Err(Error::validation(format!("{name} must be positive")));
            }
        }
        Ok(())
    }
}

impl Default for ValidationBrokerStressBudgets {
    fn default() -> Self {
        Self {
            max_plan_latency_ms: 20,
            max_status_latency_ms: 10,
            max_stale_scan_ms: 16,
            max_slot_store_bytes: 4 * 1024 * 1024,
            max_memory_growth_bytes: 8 * 1024 * 1024,
            min_request_throughput_per_minute: 240,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBrokerStressProfile {
    pub profile_id: String,
    pub source_kind: String,
    pub cpu_count: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub active_agents: Option<u64>,
    pub requested_validations_per_minute: Option<u64>,
    pub total_slots: Option<u64>,
    pub active_slots: Option<u64>,
    pub reusable_slots: Option<u64>,
    pub stale_slots: Option<u64>,
    pub slot_store_records: Option<u64>,
    pub slot_store_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBrokerStressMeasurements {
    pub plan_latency_ms: Option<u64>,
    pub status_latency_ms: Option<u64>,
    pub stale_scan_ms: Option<u64>,
    pub request_throughput_per_minute: Option<u64>,
    pub slot_store_bytes: Option<u64>,
    pub memory_growth_bytes: Option<u64>,
    pub duplicate_gate_opportunities: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationBrokerStressVerdict {
    Pass,
    Fail,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBrokerStressCacheProvenance {
    pub cache_key: String,
    pub cache_status: String,
    pub ttl_seconds: u64,
    pub input_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBrokerStressGuards {
    pub synthetic_data: bool,
    pub no_live_rch_mutation: bool,
    pub provider_calls: u8,
    pub live_mutations: u8,
    pub release_claim_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBrokerStressEvidence {
    pub schema: String,
    pub generated_at_utc: String,
    pub profile: ValidationBrokerStressProfile,
    pub budgets: ValidationBrokerStressBudgets,
    pub measurements: ValidationBrokerStressMeasurements,
    pub verdict: ValidationBrokerStressVerdict,
    pub budget_failures: Vec<String>,
    pub missing_data: Vec<String>,
    pub caveats: Vec<String>,
    pub provenance: ValidationSourceProvenance,
    pub cache: ValidationBrokerStressCacheProvenance,
    pub guards: ValidationBrokerStressGuards,
    pub suppressed_claims: Vec<String>,
    pub no_claims: Vec<String>,
}

pub fn evaluate_validation_broker_stress_budget(
    profile: ValidationBrokerStressProfile,
    budgets: ValidationBrokerStressBudgets,
    provenance: ValidationSourceProvenance,
    generated_at_utc: &str,
) -> Result<ValidationBrokerStressEvidence> {
    require_non_empty(&profile.profile_id, "stress profile_id")?;
    require_non_empty(&profile.source_kind, "stress source_kind")?;
    budgets.validate()?;
    provenance.validate()?;
    parse_utc(generated_at_utc)?;

    let input_fingerprint = validation_stress_input_fingerprint(&profile, &budgets, &provenance)?;
    let mut caveats = vec![
        "synthetic_large_host_profile_not_release_performance_evidence".to_string(),
        "does_not_replace_rch_doctor_cargo_headroom_ci_or_required_repo_gates".to_string(),
    ];
    if profile.source_kind != "synthetic" {
        caveats.push("profile_source_is_not_synthetic_fixture".to_string());
    }
    let synthetic_data = profile.source_kind == "synthetic";

    let missing_data = validation_stress_missing_data(&profile);
    let (measurements, budget_failures, verdict) = if missing_data.is_empty() {
        let measurements = validation_stress_measurements(&profile)?;
        let budget_failures = validation_stress_budget_failures(&measurements, &budgets);
        let verdict = if budget_failures.is_empty() {
            ValidationBrokerStressVerdict::Pass
        } else {
            ValidationBrokerStressVerdict::Fail
        };
        (measurements, budget_failures, verdict)
    } else {
        (
            blocked_validation_stress_measurements(),
            Vec::new(),
            ValidationBrokerStressVerdict::Blocked,
        )
    };

    Ok(ValidationBrokerStressEvidence {
        schema: VALIDATION_BROKER_STRESS_EVIDENCE_SCHEMA.to_string(),
        generated_at_utc: generated_at_utc.to_string(),
        profile,
        budgets,
        measurements,
        verdict,
        budget_failures,
        missing_data,
        caveats,
        provenance,
        cache: ValidationBrokerStressCacheProvenance {
            cache_key: format!("validation-broker-stress:{input_fingerprint}"),
            cache_status: "cold_synthetic_fixture".to_string(),
            ttl_seconds: 86_400,
            input_fingerprint,
        },
        guards: ValidationBrokerStressGuards {
            synthetic_data,
            no_live_rch_mutation: true,
            provider_calls: 0,
            live_mutations: 0,
            release_claim_allowed: false,
        },
        suppressed_claims: default_suppressed_claims(),
        no_claims: default_no_claims(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationAdmissionRequestContext {
    pub request_id: String,
    pub request: ValidationSlotRequest,
    pub requested_at_utc: String,
    pub bead_priority: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ValidationAdmissionPolicyFields {
    pub request_age_seconds: i64,
    pub bead_priority: u8,
    pub age_priority_boosted: bool,
    pub broad_gate: bool,
    pub rch_required: bool,
    pub rch_saturated: bool,
    pub rch_local_fallback: bool,
    pub low_cargo_headroom: bool,
    pub low_scratch_headroom: bool,
    pub doctor_failed: bool,
    pub dirty_worktree: bool,
    pub active_equivalent_slots: usize,
    pub reusable_equivalent_slots: usize,
    pub stale_equivalent_slots: usize,
    pub active_broad_gates: usize,
    pub stale_in_progress_beads: usize,
    pub blocking_source_degraded: bool,
    pub reuse_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationAdmissionSourceStatus {
    pub source_id: String,
    pub state: ValidationSourceState,
    pub degraded_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationRejectedReusableSlot {
    pub slot_id: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationAdmissionDecisionRecord {
    pub schema: String,
    pub decision_id: String,
    pub request_id: String,
    pub generated_at: String,
    pub decision: ValidationAdmissionDecision,
    pub confidence: String,
    pub reasons: Vec<String>,
    pub source_statuses: Vec<ValidationAdmissionSourceStatus>,
    pub required_actions: Vec<String>,
    pub reusable_slot: Option<String>,
    pub coalesced_artifacts: Vec<ValidationSlotArtifact>,
    pub rejected_reusable_slots: Vec<ValidationRejectedReusableSlot>,
    pub suppressed_claims: Vec<String>,
    pub no_claims: Vec<String>,
    pub policy: ValidationAdmissionPolicyFields,
}

#[allow(clippy::too_many_lines)]
pub fn decide_validation_admission(
    context: ValidationAdmissionRequestContext,
    inputs: &ValidationBrokerInputSnapshot,
    slot_store: &ValidationSlotStoreSnapshot,
    policy: &ValidationAdmissionPolicy,
    now_utc: &str,
) -> Result<ValidationAdmissionDecisionRecord> {
    validate_request(&context.request)?;
    policy.validate()?;
    let now = parse_utc(now_utc)?;
    let requested_at = parse_utc(&context.requested_at_utc)?;
    let request_age_seconds = now.signed_duration_since(requested_at).num_seconds().max(0);
    let broad_gate = is_broad_validation_request(&context.request);
    let rch_required = is_rch_required(&context.request);
    let age_priority_boosted =
        context.bead_priority <= 1 || request_age_seconds >= policy.request_age_boost_seconds;
    let matching = matching_slots(&context.request, slot_store, now_utc, policy)?;
    let active_broad_gates = active_broad_gate_count(slot_store, now_utc)?;
    let mut reasons = Vec::new();
    let mut required_actions = Vec::new();
    let mut reusable_slot = None;
    let mut coalesced_artifacts = Vec::new();
    let blocking_source_reasons = blocking_source_reasons(inputs, slot_store);
    let source_statuses = admission_source_statuses(inputs, slot_store);
    let rejected_reusable_slots = rejected_reusable_slots(&context.request, slot_store)?;
    let hard_rch_backpressure = inputs.rch.free_slots == Some(0)
        || inputs
            .rch
            .queued_builds
            .is_some_and(|queued| queued > policy.max_queued_builds)
        || inputs
            .rch
            .active_builds
            .is_some_and(|active| active > policy.max_active_builds);
    let soft_broad_backpressure = broad_gate
        && (inputs.rch.saturated
            || inputs
                .rch
                .free_slots
                .is_some_and(|free| free < policy.min_free_slots_for_broad_gate)
            || active_broad_gates >= policy.max_active_broad_gates);
    let low_headroom = inputs.cargo_headroom.low_headroom || inputs.scratch_headroom.low_headroom;

    let fields = ValidationAdmissionPolicyFields {
        request_age_seconds,
        bead_priority: context.bead_priority,
        age_priority_boosted,
        broad_gate,
        rch_required,
        rch_saturated: inputs.rch.saturated,
        rch_local_fallback: inputs.rch.local_fallback,
        low_cargo_headroom: inputs.cargo_headroom.low_headroom,
        low_scratch_headroom: inputs.scratch_headroom.low_headroom,
        doctor_failed: inputs.doctor.has_failures,
        dirty_worktree: inputs.git.dirty,
        active_equivalent_slots: matching.active.len(),
        reusable_equivalent_slots: matching.reusable.len(),
        stale_equivalent_slots: matching.stale.len(),
        active_broad_gates,
        stale_in_progress_beads: inputs.beads.stale_in_progress_ids.len(),
        blocking_source_degraded: !blocking_source_reasons.is_empty(),
        reuse_required: policy.reuse_required,
    };

    let decision = if rch_required && inputs.rch.local_fallback {
        reasons.push("rch_required_but_local_fallback_detected".to_string());
        required_actions.push("restore_remote_rch_or_run_later_do_not_run_locally".to_string());
        ValidationAdmissionDecision::DenyLocalFallback
    } else if let Some(lease) = matching.reusable.first().copied() {
        reasons.push(format!(
            "equivalent_reusable_slot_available:{}",
            lease.slot_id
        ));
        reusable_slot = Some(lease.slot_id.clone());
        coalesced_artifacts.clone_from(&lease.artifacts);
        ValidationAdmissionDecision::Coalesce
    } else if let Some(lease) = matching.stale.first().copied() {
        reasons.push(format!(
            "equivalent_stale_slot_recoverable:{}",
            lease.slot_id
        ));
        required_actions.push("record_stale_recovery_without_killing_other_processes".to_string());
        ValidationAdmissionDecision::StaleRecover
    } else if !inputs.beads.stale_in_progress_ids.is_empty() {
        reasons.push(format!(
            "stale_in_progress_beads_detected:{}",
            inputs.beads.stale_in_progress_ids.join(",")
        ));
        required_actions.push("recover_stale_beads_before_new_validation".to_string());
        ValidationAdmissionDecision::StaleRecover
    } else if let Some(lease) = matching.active.first().copied() {
        reasons.push(format!(
            "equivalent_active_slot_in_flight:{}",
            lease.slot_id
        ));
        required_actions.push("wait_for_equivalent_validation_result".to_string());
        ValidationAdmissionDecision::Wait
    } else if !blocking_source_reasons.is_empty() {
        reasons.extend(blocking_source_reasons);
        required_actions.push("refresh_degraded_authoritative_sources".to_string());
        ValidationAdmissionDecision::DegradedBlock
    } else if inputs.doctor.has_failures {
        reasons.push("doctor_preflight_has_failures".to_string());
        required_actions.push("resolve_doctor_failures_before_validation".to_string());
        ValidationAdmissionDecision::DegradedBlock
    } else if low_headroom && broad_gate && policy.allow_narrow_scope {
        reasons.push("broad_gate_has_insufficient_target_or_tmp_headroom".to_string());
        required_actions.push("narrow_to_package_test_or_non_compile_gate".to_string());
        ValidationAdmissionDecision::Narrow
    } else if low_headroom {
        reasons.push("target_or_tmp_headroom_below_required_bytes".to_string());
        required_actions.push("restore_scratch_headroom_before_validation".to_string());
        ValidationAdmissionDecision::DegradedBlock
    } else if policy.reuse_required {
        reasons.push("reuse_required_but_no_valid_reusable_slot".to_string());
        required_actions.push("run_required_gate_or_wait_for_matching_artifact".to_string());
        ValidationAdmissionDecision::DegradedBlock
    } else if hard_rch_backpressure && broad_gate && policy.allow_narrow_scope {
        reasons.push("hard_rch_backpressure_on_broad_gate".to_string());
        required_actions.push("narrow_scope_or_wait_for_rch_capacity".to_string());
        ValidationAdmissionDecision::Narrow
    } else if hard_rch_backpressure {
        reasons.push("hard_rch_backpressure".to_string());
        required_actions.push("wait_for_rch_capacity".to_string());
        ValidationAdmissionDecision::Wait
    } else if soft_broad_backpressure && !age_priority_boosted && policy.allow_narrow_scope {
        reasons.push("fresh_low_priority_broad_gate_under_global_backpressure".to_string());
        required_actions.push("narrow_scope_or_wait_for_broad_gate_slot".to_string());
        ValidationAdmissionDecision::Narrow
    } else {
        if soft_broad_backpressure && age_priority_boosted {
            reasons.push("age_or_priority_boost_overrides_soft_backpressure".to_string());
        } else {
            reasons.push("validation_admission_allowed".to_string());
        }
        ValidationAdmissionDecision::Allow
    };

    let confidence = if fields.blocking_source_degraded || inputs.agent_mail.is_degraded() {
        "medium"
    } else {
        "high"
    }
    .to_string();

    Ok(ValidationAdmissionDecisionRecord {
        schema: VALIDATION_BROKER_DECISION_SCHEMA.to_string(),
        decision_id: admission_decision_id(&context, now_utc, &decision)?,
        request_id: context.request_id,
        generated_at: now_utc.to_string(),
        decision,
        confidence,
        reasons,
        source_statuses,
        required_actions,
        reusable_slot,
        coalesced_artifacts,
        rejected_reusable_slots,
        suppressed_claims: default_suppressed_claims(),
        no_claims: default_no_claims(),
        policy: fields,
    })
}

fn source_health(
    provenance: ValidationSourceProvenance,
    degraded_reasons: Vec<String>,
) -> ValidationSourceHealth {
    if degraded_reasons.is_empty() {
        ValidationSourceHealth::available(provenance)
    } else {
        ValidationSourceHealth::degraded(provenance, degraded_reasons)
    }
}

fn collect_source_reasons(
    degraded_reasons: &mut Vec<String>,
    source_health: &ValidationSourceHealth,
) {
    for reason in &source_health.degraded_reasons {
        degraded_reasons.push(format!("{}: {reason}", source_health.provenance.source));
    }
}

#[derive(Debug, Clone)]
struct MatchingValidationSlots<'a> {
    active: Vec<&'a ValidationSlotLease>,
    reusable: Vec<&'a ValidationSlotLease>,
    stale: Vec<&'a ValidationSlotLease>,
}

fn matching_slots<'a>(
    request: &ValidationSlotRequest,
    slot_store: &'a ValidationSlotStoreSnapshot,
    now_utc: &str,
    policy: &ValidationAdmissionPolicy,
) -> Result<MatchingValidationSlots<'a>> {
    let mut slots = MatchingValidationSlots {
        active: Vec::new(),
        reusable: Vec::new(),
        stale: Vec::new(),
    };

    for lease in slot_store.latest_by_slot_id.values() {
        if lease.state.is_terminal() || !lease.matches_request_equivalence(request)? {
            continue;
        }
        let stale_now = lease.is_stale_at(now_utc)?;
        match lease.state {
            ValidationSlotState::Reusable
                if !stale_now
                    && !lease.artifacts.is_empty()
                    && reusable_artifact_is_fresh(
                        lease,
                        now_utc,
                        policy.reusable_artifact_freshness_seconds,
                    )? =>
            {
                slots.reusable.push(lease);
            }
            ValidationSlotState::Reusable | ValidationSlotState::Stale => {
                slots.stale.push(lease);
            }
            ValidationSlotState::Active | ValidationSlotState::Requested if stale_now => {
                slots.stale.push(lease);
            }
            ValidationSlotState::Active | ValidationSlotState::Requested => {
                slots.active.push(lease);
            }
            ValidationSlotState::Failed
            | ValidationSlotState::Released
            | ValidationSlotState::Expired
            | ValidationSlotState::Degraded => {}
        }
    }

    Ok(slots)
}

fn reusable_artifact_is_fresh(
    lease: &ValidationSlotLease,
    now_utc: &str,
    freshness_seconds: i64,
) -> Result<bool> {
    let now = parse_utc(now_utc)?;
    let heartbeat = parse_utc(&lease.heartbeat_at_utc)?;
    let age_seconds = now.signed_duration_since(heartbeat).num_seconds().max(0);
    Ok(age_seconds <= freshness_seconds)
}

fn active_broad_gate_count(
    slot_store: &ValidationSlotStoreSnapshot,
    now_utc: &str,
) -> Result<usize> {
    let mut count = 0;
    for lease in slot_store.latest_by_slot_id.values() {
        if matches!(
            lease.state,
            ValidationSlotState::Active | ValidationSlotState::Requested
        ) && !lease.is_stale_at(now_utc)?
            && is_broad_command(&lease.command, &lease.command_class)
        {
            count += 1;
        }
    }
    Ok(count)
}

fn rejected_reusable_slots(
    request: &ValidationSlotRequest,
    slot_store: &ValidationSlotStoreSnapshot,
) -> Result<Vec<ValidationRejectedReusableSlot>> {
    let command_fingerprint = command_fingerprint(request)?;
    let environment_fingerprint = environment_fingerprint(&request.environment)?;
    let mut rejected = Vec::new();

    for lease in slot_store.latest_by_slot_id.values() {
        if !matches!(lease.state, ValidationSlotState::Reusable) {
            continue;
        }
        let reasons = reusable_slot_mismatch_reasons(
            lease,
            request,
            &command_fingerprint,
            &environment_fingerprint,
        );
        if !reasons.is_empty() {
            rejected.push(ValidationRejectedReusableSlot {
                slot_id: lease.slot_id.as_str().into(),
                reasons,
            });
        }
    }

    Ok(rejected)
}

fn reusable_slot_mismatch_reasons(
    lease: &ValidationSlotLease,
    request: &ValidationSlotRequest,
    request_command_fingerprint: &str,
    request_environment_fingerprint: &str,
) -> Vec<String> {
    let mut reasons = Vec::new();
    push_mismatch(
        &mut reasons,
        "command_fingerprint",
        lease.command_fingerprint.as_str(),
        request_command_fingerprint,
    );
    push_mismatch(
        &mut reasons,
        "environment_fingerprint",
        lease.environment_fingerprint.as_str(),
        request_environment_fingerprint,
    );
    push_mismatch(&mut reasons, "cwd", &lease.cwd, &request.cwd);
    push_mismatch(&mut reasons, "git_head", &lease.git_head, &request.git_head);
    push_mismatch(
        &mut reasons,
        "feature_flags",
        &lease.feature_flags,
        &request.feature_flags,
    );
    push_mismatch(
        &mut reasons,
        "target_dir",
        &lease.target_dir,
        &request.target_dir,
    );
    push_mismatch(&mut reasons, "tmpdir", &lease.tmpdir, &request.tmpdir);
    push_mismatch(&mut reasons, "runner", &lease.runner, &request.runner);
    push_mismatch(
        &mut reasons,
        "rust_toolchain",
        &lease.rust_toolchain,
        &request.rust_toolchain,
    );
    push_mismatch(
        &mut reasons,
        "artifact_schema",
        &lease.artifact_schema,
        &request.artifact_schema,
    );
    push_mismatch(
        &mut reasons,
        "artifact_hash",
        &lease.artifact_hash,
        &request.artifact_hash,
    );
    reasons
}

fn push_mismatch<T: PartialEq + ?Sized>(
    reasons: &mut Vec<String>,
    field: &str,
    lease_value: &T,
    request_value: &T,
) {
    if lease_value != request_value {
        reasons.push(format!("{field}_mismatch"));
    }
}

fn is_broad_validation_request(request: &ValidationSlotRequest) -> bool {
    is_broad_command(&request.command, &request.command_class)
}

fn is_broad_command(command: &[String], command_class: &str) -> bool {
    if command
        .iter()
        .any(|segment| matches!(segment.as_str(), "--all-targets" | "--workspace"))
    {
        return true;
    }

    let command_class = command_class.trim().to_ascii_lowercase();
    let is_compile_gate = matches!(
        command_class.as_str(),
        "cargo_check" | "cargo_clippy" | "cargo_test"
    );
    is_compile_gate && !has_narrow_cargo_scope(command)
}

fn has_narrow_cargo_scope(command: &[String]) -> bool {
    command.iter().any(|segment| {
        matches!(
            segment.as_str(),
            "-p" | "--package" | "--test" | "--bin" | "--example" | "--bench"
        ) || segment.starts_with("--package=")
            || segment.starts_with("--test=")
            || segment.starts_with("--bin=")
            || segment.starts_with("--example=")
            || segment.starts_with("--bench=")
    })
}

fn is_rch_required(request: &ValidationSlotRequest) -> bool {
    contains_any(&request.runner, &["rch_required", "rch required"])
}

fn blocking_source_reasons(
    inputs: &ValidationBrokerInputSnapshot,
    slot_store: &ValidationSlotStoreSnapshot,
) -> Vec<String> {
    let mut reasons = Vec::new();
    collect_source_reasons(&mut reasons, &inputs.rch.health);
    collect_source_reasons(&mut reasons, &inputs.cargo_headroom.health);
    collect_source_reasons(&mut reasons, &inputs.doctor.health);
    collect_source_reasons(&mut reasons, &inputs.git.health);
    collect_source_reasons(&mut reasons, &inputs.beads.health);
    collect_source_reasons(&mut reasons, &inputs.scratch_headroom.health);
    for reason in &slot_store.degraded_reasons {
        reasons.push(format!("validation_slot_store: {reason}"));
    }
    reasons
}

fn admission_source_statuses(
    inputs: &ValidationBrokerInputSnapshot,
    slot_store: &ValidationSlotStoreSnapshot,
) -> Vec<ValidationAdmissionSourceStatus> {
    let mut statuses = Vec::with_capacity(8);
    push_source_status(&mut statuses, &inputs.rch.health);
    push_source_status(&mut statuses, &inputs.cargo_headroom.health);
    push_source_status(&mut statuses, &inputs.doctor.health);
    push_source_status(&mut statuses, &inputs.git.health);
    push_source_status(&mut statuses, &inputs.beads.health);
    push_source_status(&mut statuses, &inputs.scratch_headroom.health);
    push_source_status(&mut statuses, &inputs.agent_mail);
    statuses.push(ValidationAdmissionSourceStatus {
        source_id: "validation_slot_store".to_string(),
        state: if slot_store.is_degraded() {
            ValidationSourceState::Degraded
        } else {
            ValidationSourceState::Available
        },
        degraded_reasons: slot_store.degraded_reasons.clone(),
    });
    statuses
}

fn push_source_status(
    statuses: &mut Vec<ValidationAdmissionSourceStatus>,
    source_health: &ValidationSourceHealth,
) {
    statuses.push(ValidationAdmissionSourceStatus {
        source_id: source_health.provenance.source.clone(),
        state: source_health.state.clone(),
        degraded_reasons: source_health.degraded_reasons.clone(),
    });
}

fn admission_decision_id(
    context: &ValidationAdmissionRequestContext,
    now_utc: &str,
    decision: &ValidationAdmissionDecision,
) -> Result<String> {
    fingerprint_json(&json!({
        "request_id": &context.request_id,
        "slot_id": &context.request.slot_id,
        "bead_id": &context.request.bead_id,
        "generated_at": now_utc,
        "decision": decision_key(decision),
    }))
}

const fn decision_key(decision: &ValidationAdmissionDecision) -> &'static str {
    match decision {
        ValidationAdmissionDecision::Allow => "allow",
        ValidationAdmissionDecision::Wait => "wait",
        ValidationAdmissionDecision::Coalesce => "coalesce",
        ValidationAdmissionDecision::Narrow => "narrow",
        ValidationAdmissionDecision::DenyLocalFallback => "deny_local_fallback",
        ValidationAdmissionDecision::StaleRecover => "stale_recover",
        ValidationAdmissionDecision::DegradedBlock => "degraded_block",
    }
}

fn default_suppressed_claims() -> Vec<String> {
    default_no_claims()
}

fn default_no_claims() -> Vec<String> {
    vec![
        "not_ci_success".to_string(),
        "not_release_performance_evidence".to_string(),
        "not_dropin_certification_evidence".to_string(),
        "not_permission_to_skip_required_gates".to_string(),
        "not_permission_to_modify_other_agents_files".to_string(),
    ]
}

fn count_from_line(raw: &str, marker: &str) -> Option<u64> {
    raw.lines()
        .find(|line| line.contains(marker))
        .and_then(first_u64)
}

fn worker_slots(raw: &str) -> (Option<u64>, Option<u64>) {
    raw.lines()
        .find(|line| line.contains("slots free"))
        .map(numbers_in_line)
        .and_then(|numbers| match numbers.as_slice() {
            [free, total, ..] => Some((Some(*free), Some(*total))),
            _ => None,
        })
        .unwrap_or((None, None))
}

fn numbers_in_line(line: &str) -> Vec<u64> {
    line.split(|ch: char| !ch.is_ascii_digit())
        .filter(|segment| !segment.is_empty())
        .filter_map(|segment| segment.parse::<u64>().ok())
        .collect()
}

fn first_u64(line: &str) -> Option<u64> {
    numbers_in_line(line).into_iter().next()
}

fn contains_any(raw: &str, needles: &[&str]) -> bool {
    let haystack = raw.to_ascii_lowercase();
    needles.iter().any(|needle| haystack.contains(needle))
}

const fn is_git_short_status_code(code: u8) -> bool {
    matches!(
        code,
        b' ' | b'M' | b'T' | b'A' | b'D' | b'R' | b'C' | b'U' | b'?' | b'!'
    )
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn u64_field(value: &Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(|field| field.as_u64().or_else(|| field.as_str()?.parse().ok()))
    })
}

fn is_success_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "ok" | "pass" | "passed" | "success" | "healthy" | "available"
    )
}

fn line_degraded_reason(line_index: usize, label: &str, err: impl Display) -> String {
    format!("line {} {label}: {err}", line_index + 1)
}

fn snapshot(
    leases: Vec<ValidationSlotLease>,
    latest_by_slot_id: BTreeMap<String, ValidationSlotLease>,
    degraded_reasons: Vec<String>,
) -> ValidationSlotStoreSnapshot {
    let status = if degraded_reasons.is_empty() {
        ValidationSlotStoreStatus::Available
    } else {
        ValidationSlotStoreStatus::Degraded
    };
    ValidationSlotStoreSnapshot {
        schema: VALIDATION_BROKER_SLOT_STORE_SCHEMA.to_string(),
        status,
        leases,
        latest_by_slot_id,
        degraded_reasons,
    }
}

fn validation_stress_missing_data(profile: &ValidationBrokerStressProfile) -> Vec<String> {
    let mut missing = Vec::new();
    for (name, present) in [
        ("cpu_count", profile.cpu_count.is_some()),
        ("memory_bytes", profile.memory_bytes.is_some()),
        ("active_agents", profile.active_agents.is_some()),
        (
            "requested_validations_per_minute",
            profile.requested_validations_per_minute.is_some(),
        ),
        ("total_slots", profile.total_slots.is_some()),
        ("active_slots", profile.active_slots.is_some()),
        ("reusable_slots", profile.reusable_slots.is_some()),
        ("stale_slots", profile.stale_slots.is_some()),
        ("slot_store_records", profile.slot_store_records.is_some()),
        ("slot_store_bytes", profile.slot_store_bytes.is_some()),
    ] {
        if !present {
            missing.push(name.to_string());
        }
    }
    missing
}

fn validation_stress_measurements(
    profile: &ValidationBrokerStressProfile,
) -> Result<ValidationBrokerStressMeasurements> {
    let cpu_count = required_stress_u64(profile.cpu_count, "cpu_count")?;
    let memory_bytes = required_stress_u64(profile.memory_bytes, "memory_bytes")?;
    let active_agents = required_stress_u64(profile.active_agents, "active_agents")?;
    let requested_validations = required_stress_u64(
        profile.requested_validations_per_minute,
        "requested_validations_per_minute",
    )?;
    let total_slots = required_stress_u64(profile.total_slots, "total_slots")?;
    let active_slots = required_stress_u64(profile.active_slots, "active_slots")?;
    let reusable_slots = required_stress_u64(profile.reusable_slots, "reusable_slots")?;
    let stale_slots = required_stress_u64(profile.stale_slots, "stale_slots")?;
    let slot_store_records = required_stress_u64(profile.slot_store_records, "slot_store_records")?;
    let slot_store_bytes = required_stress_u64(profile.slot_store_bytes, "slot_store_bytes")?;

    if active_slots > total_slots {
        return Err(Error::validation(
            "active_slots must not exceed total_slots in stress profile",
        ));
    }
    if cpu_count == 0 {
        return Err(Error::validation(
            "cpu_count must be positive in stress profile",
        ));
    }
    if memory_bytes == 0 {
        return Err(Error::validation(
            "memory_bytes must be positive in stress profile",
        ));
    }
    if active_agents == 0 {
        return Err(Error::validation(
            "active_agents must be positive in stress profile",
        ));
    }
    if requested_validations == 0 {
        return Err(Error::validation(
            "requested_validations_per_minute must be positive in stress profile",
        ));
    }
    if total_slots == 0 {
        return Err(Error::validation(
            "total_slots must be positive in stress profile",
        ));
    }
    if slot_store_records
        < active_slots
            .saturating_add(reusable_slots)
            .saturating_add(stale_slots)
    {
        return Err(Error::validation(
            "slot_store_records must cover active, reusable, and stale slots in stress profile",
        ));
    }

    let free_slots = total_slots.saturating_sub(active_slots);
    let plan_latency_ms = 4
        + div_ceil(active_agents, 16)
        + div_ceil(slot_store_records, 2_000)
        + div_ceil(requested_validations, 120);
    let status_latency_ms =
        3 + div_ceil(slot_store_records, 4_000) + div_ceil(active_slots + stale_slots, 512);
    let stale_scan_ms = 2 + div_ceil(slot_store_records, 1_000) + div_ceil(stale_slots, 256);
    let request_throughput_per_minute = free_slots.saturating_mul(60);
    let memory_growth_bytes = slot_store_bytes
        .saturating_add(slot_store_records.saturating_mul(96))
        .saturating_add(active_agents.saturating_mul(1_024));

    Ok(ValidationBrokerStressMeasurements {
        plan_latency_ms: Some(plan_latency_ms),
        status_latency_ms: Some(status_latency_ms),
        stale_scan_ms: Some(stale_scan_ms),
        request_throughput_per_minute: Some(request_throughput_per_minute),
        slot_store_bytes: Some(slot_store_bytes),
        memory_growth_bytes: Some(memory_growth_bytes),
        duplicate_gate_opportunities: Some(reusable_slots),
    })
}

fn required_stress_u64(value: Option<u64>, name: &str) -> Result<u64> {
    value.ok_or_else(|| Error::validation(format!("stress profile missing {name}")))
}

fn validation_stress_budget_failures(
    measurements: &ValidationBrokerStressMeasurements,
    budgets: &ValidationBrokerStressBudgets,
) -> Vec<String> {
    let mut failures = Vec::new();
    push_stress_max_failure(
        &mut failures,
        "plan_latency_ms",
        measurements.plan_latency_ms,
        budgets.max_plan_latency_ms,
    );
    push_stress_max_failure(
        &mut failures,
        "status_latency_ms",
        measurements.status_latency_ms,
        budgets.max_status_latency_ms,
    );
    push_stress_max_failure(
        &mut failures,
        "stale_scan_ms",
        measurements.stale_scan_ms,
        budgets.max_stale_scan_ms,
    );
    push_stress_max_failure(
        &mut failures,
        "slot_store_bytes",
        measurements.slot_store_bytes,
        budgets.max_slot_store_bytes,
    );
    push_stress_max_failure(
        &mut failures,
        "memory_growth_bytes",
        measurements.memory_growth_bytes,
        budgets.max_memory_growth_bytes,
    );
    push_stress_min_failure(
        &mut failures,
        "request_throughput_per_minute",
        measurements.request_throughput_per_minute,
        budgets.min_request_throughput_per_minute,
    );
    failures
}

fn push_stress_max_failure(
    failures: &mut Vec<String>,
    metric: &str,
    actual: Option<u64>,
    maximum: u64,
) {
    match actual {
        Some(value) if value > maximum => failures.push(format!("{metric}_exceeded")),
        None => failures.push(format!("{metric}_missing")),
        Some(_) => {}
    }
}

fn push_stress_min_failure(
    failures: &mut Vec<String>,
    metric: &str,
    actual: Option<u64>,
    minimum: u64,
) {
    match actual {
        Some(value) if value < minimum => failures.push(format!("{metric}_below_minimum")),
        None => failures.push(format!("{metric}_missing")),
        Some(_) => {}
    }
}

const fn blocked_validation_stress_measurements() -> ValidationBrokerStressMeasurements {
    ValidationBrokerStressMeasurements {
        plan_latency_ms: None,
        status_latency_ms: None,
        stale_scan_ms: None,
        request_throughput_per_minute: None,
        slot_store_bytes: None,
        memory_growth_bytes: None,
        duplicate_gate_opportunities: None,
    }
}

fn validation_stress_input_fingerprint(
    profile: &ValidationBrokerStressProfile,
    budgets: &ValidationBrokerStressBudgets,
    provenance: &ValidationSourceProvenance,
) -> Result<String> {
    fingerprint_json(&json!({
        "profile": profile,
        "budgets": budgets,
        "provenance": provenance,
    }))
}

const fn div_ceil(value: u64, divisor: u64) -> u64 {
    value.div_ceil(divisor)
}

fn validate_request(request: &ValidationSlotRequest) -> Result<()> {
    require_non_empty(&request.slot_id, "slot_id")?;
    require_non_empty(&request.owner_agent, "owner_agent")?;
    require_non_empty(&request.bead_id, "bead_id")?;
    if request.command.is_empty() {
        return Err(Error::validation("command must not be empty"));
    }
    for segment in &request.command {
        require_non_empty(segment, "command segment")?;
    }
    require_non_empty(&request.command_class, "command_class")?;
    require_non_empty(&request.cwd, "cwd")?;
    require_non_empty(&request.git_head, "git_head")?;
    require_non_empty(&request.target_dir, "target_dir")?;
    require_non_empty(&request.tmpdir, "tmpdir")?;
    require_non_empty(&request.runner, "runner")?;
    for feature_flag in &request.feature_flags {
        require_non_empty(feature_flag, "feature flag")?;
    }
    Ok(())
}

fn ensure_future_expiry(start_utc: &str, expires_utc: &str) -> Result<()> {
    let start = parse_utc(start_utc)?;
    let expires = parse_utc(expires_utc)?;
    if expires > start {
        Ok(())
    } else {
        Err(Error::validation(format!(
            "expires_at_utc {expires_utc} must be after {start_utc}"
        )))
    }
}

fn parse_utc(raw: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map_err(|err| Error::validation(format!("invalid UTC timestamp {raw:?}: {err}")))?;
    if parsed.offset().local_minus_utc() == 0 {
        Ok(parsed.with_timezone(&Utc))
    } else {
        Err(Error::validation(format!(
            "timestamp {raw:?} must use UTC offset"
        )))
    }
}

fn command_fingerprint(request: &ValidationSlotRequest) -> Result<String> {
    fingerprint_json(&json!({
        "command": request.command,
        "command_class": request.command_class,
        "cwd": request.cwd,
        "feature_flags": request.feature_flags,
        "rust_toolchain": request.rust_toolchain,
    }))
}

fn environment_fingerprint(environment: &BTreeMap<String, String>) -> Result<String> {
    fingerprint_json(&json!(environment))
}

fn fingerprint_json(value: &Value) -> Result<String> {
    let encoded = serde_json::to_vec(value)?;
    let digest = Sha256::digest(encoded);
    Ok(hex_lower(&digest))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn non_empty(value: String, label: &str) -> Result<String> {
    if value.trim().is_empty() {
        Err(Error::validation(format!("{label} must not be empty")))
    } else {
        Ok(value)
    }
}

fn require_non_empty(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(Error::validation(format!("{label} must not be empty")))
    } else {
        Ok(())
    }
}
