//! Native `pi doctor` reports as validation-admission inputs.
//!
//! Consumer health is not swarm readiness. Require the explicit resource and
//! live-admission findings, retain every reported failure, and reject incomplete
//! or contradictory evidence instead of falling back to a legacy checks array.

use super::{
    VALIDATION_BROKER_DOCTOR_INPUT_SCHEMA, ValidationDoctorCheck, ValidationDoctorInput,
    ValidationSourceProvenance, source_health, string_field,
};
use serde_json::Value;

const RESOURCE_SCHEMA: &str = "pi.doctor.swarm_resource_preflight.v1";
const ADMISSION_SCHEMA: &str = "pi.doctor.swarm_admission.v1";

#[derive(Default)]
struct NativeDoctorEvidence {
    checks: Vec<ValidationDoctorCheck>,
    counts: [u64; 4],
    resource_findings: usize,
    admission_findings: usize,
    has_failures: bool,
    degraded_reasons: Vec<String>,
}

pub(super) fn normalize_native_doctor_json(
    provenance: ValidationSourceProvenance,
    value: &Value,
) -> ValidationDoctorInput {
    let mut evidence = NativeDoctorEvidence::default();
    if let Some(findings) = value.get("findings").and_then(Value::as_array) {
        for (index, finding) in findings.iter().enumerate() {
            evidence.observe(index, finding);
        }
    } else {
        evidence
            .degraded_reasons
            .push("doctor_findings_invalid".to_string());
    }
    if evidence.checks.is_empty() {
        evidence
            .degraded_reasons
            .push("doctor_findings_empty".to_string());
    }
    evidence.validate_summary(value);
    for (name, count) in [
        ("resource_preflight", evidence.resource_findings),
        ("admission", evidence.admission_findings),
    ] {
        if count != 1 {
            evidence
                .degraded_reasons
                .push(format!("doctor_swarm_{name}_expected_once:found={count}"));
        }
    }

    ValidationDoctorInput {
        schema: VALIDATION_BROKER_DOCTOR_INPUT_SCHEMA.to_string(),
        health: source_health(provenance, evidence.degraded_reasons),
        checks: evidence.checks,
        has_failures: evidence.has_failures,
    }
}

impl NativeDoctorEvidence {
    fn observe(&mut self, index: usize, finding: &Value) {
        let category = string_field(finding, &["category"]).unwrap_or_default();
        if !matches!(
            category.as_str(),
            "config" | "dirs" | "auth" | "shell" | "sessions" | "swarm" | "extensions"
        ) {
            self.degraded_reasons
                .push(format!("doctor_finding_{}_category_invalid", index + 1));
        }
        let title = string_field(finding, &["title"]).unwrap_or_else(|| {
            self.degraded_reasons
                .push(format!("doctor_finding_{}_title_missing", index + 1));
            format!("unnamed_finding_{}", index + 1)
        });
        let severity = string_field(finding, &["severity"]).unwrap_or_default();
        let counter = match severity.as_str() {
            "pass" => Some(0),
            "info" => Some(1),
            "warn" => Some(2),
            "fail" => {
                self.has_failures = true;
                Some(3)
            }
            _ => None,
        };
        if let Some(counter) = counter {
            self.counts[counter] += 1;
        } else {
            self.degraded_reasons
                .push(format!("doctor_finding_{}_severity_invalid", index + 1));
        }
        self.checks.push(ValidationDoctorCheck {
            name: format!("{category}:{title}"),
            status: severity.clone(),
            message: string_field(finding, &["detail"]),
        });

        let Some(data) = finding.get("data") else {
            return;
        };
        let schema = data.get("schema").and_then(Value::as_str);
        if matches!(schema, Some(RESOURCE_SCHEMA | ADMISSION_SCHEMA)) && category != "swarm" {
            self.degraded_reasons
                .push("doctor_swarm_evidence_category_invalid".to_string());
            return;
        }
        match schema {
            Some(RESOURCE_SCHEMA) => {
                self.resource_findings += 1;
                self.observe_resource(data, &severity);
            }
            Some(ADMISSION_SCHEMA) => {
                self.admission_findings += 1;
                match data.get("action").and_then(Value::as_str) {
                    Some("admit") => {}
                    // Doctor can describe backpressure as a warning. It is still
                    // a stop signal for admission, never permission to build.
                    Some("backpressure" | "deny") => self.has_failures = true,
                    _ => self
                        .degraded_reasons
                        .push("doctor_swarm_admission_action_invalid".to_string()),
                }
            }
            _ => {}
        }
    }

    fn observe_resource(&mut self, data: &Value, severity: &str) {
        if severity != "pass" {
            self.degraded_reasons
                .push("doctor_swarm_resource_preflight_not_ready".to_string());
        }
        match data.get("critical_failures").and_then(Value::as_array) {
            Some(failures) => {
                self.has_failures |= !failures.is_empty();
                if failures
                    .iter()
                    .any(|failure| failure.as_str().is_none_or(|text| text.trim().is_empty()))
                {
                    self.degraded_reasons
                        .push("doctor_swarm_resource_critical_failures_invalid".to_string());
                }
            }
            None => self
                .degraded_reasons
                .push("doctor_swarm_resource_critical_failures_invalid".to_string()),
        }
        match data.get("source_errors").and_then(Value::as_array) {
            Some(errors) if errors.is_empty() => {}
            _ => self
                .degraded_reasons
                .push("doctor_swarm_resource_sources_incomplete".to_string()),
        }
        if data.get("plan_error") != Some(&Value::Null) {
            self.degraded_reasons
                .push("doctor_swarm_resource_plan_unavailable".to_string());
        }
        self.observe_resource_plan(data);
        self.observe_headroom(data);
    }

    fn observe_resource_plan(&mut self, data: &Value) {
        // A null error is not a plan. These are the nonzero core budgets the
        // native producer emits. Remote fanout may legitimately be disabled.
        let budgets = data.get("recommended_budgets");
        for name in [
            "agent_concurrency",
            "tool_concurrency",
            "extension_hostcall_lanes",
            "max_queue_depth",
            "max_rss_bytes",
        ] {
            if budgets
                .and_then(|budgets| budgets.get(name))
                .and_then(Value::as_u64)
                .is_none_or(|value| value == 0)
            {
                self.degraded_reasons
                    .push(format!("doctor_swarm_resource_budget_{name}_invalid"));
            }
        }
        if budgets
            .and_then(|budgets| budgets.get("rch_verification_fanout"))
            .and_then(Value::as_u64)
            .is_none()
        {
            self.degraded_reasons
                .push("doctor_swarm_resource_budget_rch_verification_fanout_invalid".to_string());
        }
    }

    fn observe_headroom(&mut self, data: &Value) {
        let headroom = data.get("tmpfs_headroom");
        let root = headroom
            .and_then(|headroom| headroom.get("expected_root"))
            .and_then(Value::as_str)
            .filter(|root| reported_path_within_root(root, root));
        let minimum = headroom
            .and_then(|headroom| headroom.get("warn_available_kb"))
            .and_then(Value::as_u64)
            .filter(|minimum| *minimum > 0);
        if root.is_none() || minimum.is_none() {
            self.degraded_reasons
                .push("doctor_swarm_headroom_policy_invalid".to_string());
        }
        let Some(paths) = headroom
            .and_then(|headroom| headroom.get("paths"))
            .and_then(Value::as_array)
        else {
            self.degraded_reasons
                .push("doctor_swarm_headroom_paths_missing".to_string());
            return;
        };
        let mut counts = [0_usize; 2];
        for path in paths {
            let (index, name) = match path.get("env_name").and_then(Value::as_str) {
                Some("CARGO_TARGET_DIR") => (0, "CARGO_TARGET_DIR"),
                Some("TMPDIR") => (1, "TMPDIR"),
                _ => {
                    self.degraded_reasons
                        .push("doctor_swarm_headroom_env_invalid".to_string());
                    continue;
                }
            };
            counts[index] += 1;
            self.observe_headroom_path(path, name, root, minimum);
        }
        for (name, count) in ["CARGO_TARGET_DIR", "TMPDIR"].into_iter().zip(counts) {
            if count != 1 {
                self.degraded_reasons.push(format!(
                    "doctor_swarm_headroom_{name}_expected_once:found={count}"
                ));
            }
        }
    }

    fn observe_headroom_path(
        &mut self,
        path: &Value,
        name: &str,
        root: Option<&str>,
        minimum: Option<u64>,
    ) {
        for field in ["exists", "under_expected_root", "ready"] {
            match path.get(field).and_then(Value::as_bool) {
                Some(true) => {}
                Some(false) => {
                    self.has_failures = true;
                    self.degraded_reasons
                        .push(format!("doctor_swarm_headroom_{name}_{field}_false"));
                }
                None => self
                    .degraded_reasons
                    .push(format!("doctor_swarm_headroom_{name}_{field}_invalid")),
            }
        }
        match (path.get("path").and_then(Value::as_str), root) {
            (Some(path), Some(root)) if reported_path_within_root(path, root) => {}
            _ => self
                .degraded_reasons
                .push(format!("doctor_swarm_headroom_{name}_path_invalid")),
        }
        match (path.get("available_kb").and_then(Value::as_u64), minimum) {
            (Some(available), Some(minimum)) if available >= minimum => {}
            (Some(_), Some(_)) => {
                self.has_failures = true;
                self.degraded_reasons
                    .push(format!("doctor_swarm_headroom_{name}_below_minimum"));
            }
            _ => self
                .degraded_reasons
                .push(format!("doctor_swarm_headroom_{name}_measurement_invalid")),
        }
        match path.get("problem") {
            Some(Value::Null) => {}
            Some(Value::String(problem)) if !problem.trim().is_empty() => {
                self.has_failures = true;
                self.degraded_reasons
                    .push(format!("doctor_swarm_headroom_{name}_problem_reported"));
            }
            _ => self
                .degraded_reasons
                .push(format!("doctor_swarm_headroom_{name}_problem_invalid")),
        }
    }

    fn validate_summary(&mut self, report: &Value) {
        for (severity, count) in ["pass", "info", "warn", "fail"]
            .into_iter()
            .zip(self.counts)
        {
            if report
                .get("summary")
                .and_then(|summary| summary.get(severity))
                .and_then(Value::as_u64)
                != Some(count)
            {
                self.degraded_reasons
                    .push(format!("doctor_summary_{severity}_mismatch"));
            }
        }
        let expected = if self.counts[3] > 0 {
            "fail"
        } else if self.counts[2] > 0 {
            "warn"
        } else {
            "pass"
        };
        let overall = report.get("overall").and_then(Value::as_str);
        self.has_failures |= overall == Some("fail");
        if overall != Some(expected) {
            self.degraded_reasons
                .push("doctor_overall_mismatch".to_string());
        }
    }
}

// Doctor's resource report uses POSIX scratch paths, possibly on a different
// host. Compare components without local filesystem access or host-OS path
// interpretation. This checks report consistency, not symlinks or provenance.
fn reported_path_within_root(path: &str, root: &str) -> bool {
    if !path.starts_with('/') || !root.starts_with('/') {
        return false;
    }
    let has_invalid_component = |value: &str| {
        value
            .split('/')
            .any(|component| component == ".." || component.contains('\0'))
    };
    if has_invalid_component(path) || has_invalid_component(root) {
        return false;
    }
    let path = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>();
    let root = root
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>();
    !root.is_empty() && path.starts_with(&root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{
        CheckCategory, DoctorReport, DoctorSummary, Finding, Fixability, Severity,
    };
    use crate::validation_broker::{
        VALIDATION_BROKER_SLOT_STORE_SCHEMA, ValidationAdmissionDecision,
        ValidationAdmissionDecisionRecord, ValidationAdmissionPolicy,
        ValidationAdmissionRequestContext, ValidationBrokerInputParts,
        ValidationBrokerInputSnapshot, ValidationSlotArtifact, ValidationSlotLease,
        ValidationSlotRequest, ValidationSlotStore, ValidationSlotStoreSnapshot,
        ValidationSlotStoreStatus, ValidationSourceState, decide_validation_admission,
        normalize_available_source, normalize_beads_json, normalize_doctor_json,
        normalize_git_status_text, normalize_headroom_json, normalize_rch_queue_text,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::io::Write as _;

    const START: &str = "2026-10-14T00:00:00Z";
    const NOW: &str = "2026-10-14T00:05:00Z";
    const EXPIRES: &str = "2026-10-14T00:30:00Z";

    fn provenance() -> ValidationSourceProvenance {
        ValidationSourceProvenance::new(
            "doctor",
            vec![
                "pi".into(),
                "doctor".into(),
                "--only".into(),
                "swarm".into(),
                "--format".into(),
                "json".into(),
            ],
            "/workspace",
            START,
            None,
        )
        .expect("provenance")
    }

    fn finding(category: CheckCategory, severity: Severity, data: Option<Value>) -> Finding {
        Finding {
            category,
            severity,
            title: format!("{category} finding"),
            detail: None,
            remediation: None,
            data,
            fixability: Fixability::NotFixable,
        }
    }

    fn native_report(findings: Vec<Finding>) -> Value {
        let mut summary = DoctorSummary::default();
        let mut overall = Severity::Pass;
        for finding in &findings {
            match finding.severity {
                Severity::Pass => summary.pass += 1,
                Severity::Info => summary.info += 1,
                Severity::Warn => {
                    summary.warn += 1;
                    overall = overall.max(Severity::Warn);
                }
                Severity::Fail => {
                    summary.fail += 1;
                    overall = Severity::Fail;
                }
            }
        }
        let report = DoctorReport {
            findings,
            summary,
            overall,
        };
        serde_json::from_str(&report.to_json().expect("native serialization")).expect("native JSON")
    }

    fn ready_findings() -> Vec<Finding> {
        vec![
            finding(
                CheckCategory::Swarm,
                Severity::Pass,
                Some(json!({
                    "schema": RESOURCE_SCHEMA,
                    "critical_failures": [],
                    "source_errors": [],
                    "plan_error": null,
                    "recommended_budgets": {
                        "agent_concurrency": 2,
                        "tool_concurrency": 4,
                        "extension_hostcall_lanes": 2,
                        "rch_verification_fanout": 0,
                        "max_queue_depth": 8,
                        "max_rss_bytes": 1_073_741_824
                    },
                    "tmpfs_headroom": {
                        "expected_root": "/data/tmp/pi_agent_rust_cargo",
                        "warn_available_kb": 10_485_760,
                        "paths": [
                            {
                                "env_name": "CARGO_TARGET_DIR",
                                "path": "/data/tmp/pi_agent_rust_cargo/test/target",
                                "exists": true,
                                "under_expected_root": true,
                                "available_kb": 10_485_760,
                                "ready": true,
                                "problem": null
                            },
                            {
                                "env_name": "TMPDIR",
                                "path": "/data/tmp/pi_agent_rust_cargo/test/tmp",
                                "exists": true,
                                "under_expected_root": true,
                                "available_kb": 10_485_760,
                                "ready": true,
                                "problem": null
                            }
                        ]
                    }
                })),
            ),
            finding(
                CheckCategory::Swarm,
                Severity::Pass,
                Some(json!({"schema": ADMISSION_SCHEMA, "action": "admit"})),
            ),
        ]
    }

    #[test]
    fn accepts_native_swarm_report_and_retains_nonblocking_findings() {
        let mut findings = ready_findings();
        findings.push(finding(CheckCategory::Auth, Severity::Info, None));
        findings.push(finding(CheckCategory::Shell, Severity::Warn, None));
        let input =
            normalize_doctor_json(provenance(), &native_report(findings)).expect("normalize");
        assert_eq!(input.health.state, ValidationSourceState::Available);
        assert!(!input.has_failures);
        assert_eq!(input.checks.len(), 4);
        assert_eq!(input.checks[2].status, "info");
        assert_eq!(input.checks[3].status, "warn");
    }

    #[test]
    fn consumer_health_cannot_stand_in_for_swarm_readiness() {
        let report = native_report(vec![
            finding(CheckCategory::Config, Severity::Pass, None),
            finding(CheckCategory::Auth, Severity::Info, None),
        ]);
        let input = normalize_doctor_json(provenance(), &report).expect("normalize");
        assert_eq!(input.health.state, ValidationSourceState::Degraded);
        assert!(!input.has_failures, "do not invent consumer failures");
        assert!(
            input
                .health
                .degraded_reasons
                .iter()
                .any(|reason| { reason.contains("resource_preflight_expected_once") })
        );
        assert!(
            input
                .health
                .degraded_reasons
                .iter()
                .any(|reason| { reason.contains("admission_expected_once") })
        );
        assert_eq!(input.checks.len(), 2);
    }

    #[test]
    fn preserves_ordinary_failures_in_mixed_native_report() {
        for category in [CheckCategory::Config, CheckCategory::Auth] {
            let mut findings = ready_findings();
            findings.push(finding(category, Severity::Fail, None));
            let input =
                normalize_doctor_json(provenance(), &native_report(findings)).expect("normalize");
            assert_eq!(input.health.state, ValidationSourceState::Available);
            assert!(input.has_failures);
            assert_eq!(input.checks[2].status, "fail");
        }
    }

    #[test]
    fn preserves_structured_admission_stops_even_when_severity_is_warn() {
        for action in ["deny", "backpressure"] {
            let mut findings = ready_findings();
            findings[1].severity = Severity::Warn;
            findings[1].data = Some(json!({"schema": ADMISSION_SCHEMA, "action": action}));
            let input =
                normalize_doctor_json(provenance(), &native_report(findings)).expect("normalize");
            assert!(input.has_failures);
            assert_eq!(input.checks[1].status, "warn");
        }
    }

    #[test]
    fn storage_failures_and_incomplete_resource_sources_never_admit() {
        let cases = [
            json!({"schema": RESOURCE_SCHEMA, "critical_failures": ["TMPDIR:low_headroom"], "source_errors": [], "plan_error": null}),
            json!({"schema": RESOURCE_SCHEMA, "critical_failures": [], "source_errors": ["memory unavailable"], "plan_error": null}),
            json!({"schema": RESOURCE_SCHEMA, "critical_failures": [], "source_errors": [], "plan_error": "no plan"}),
            json!({"schema": RESOURCE_SCHEMA, "critical_failures": null, "source_errors": [], "plan_error": null}),
        ];
        for data in cases {
            let mut findings = ready_findings();
            findings[0].data = Some(data);
            let input =
                normalize_doctor_json(provenance(), &native_report(findings)).expect("normalize");
            assert!(input.has_failures || input.health.is_degraded());
        }
    }

    #[test]
    fn missing_duplicate_or_misclassified_swarm_evidence_is_degraded() {
        let ready = ready_findings();
        let mut duplicate = ready.clone();
        duplicate.push(ready[0].clone());
        let mut wrong_category = ready.clone();
        wrong_category[0].category = CheckCategory::Config;
        for findings in [
            vec![ready[0].clone()],
            vec![ready[1].clone()],
            duplicate,
            wrong_category,
        ] {
            let input =
                normalize_doctor_json(provenance(), &native_report(findings)).expect("normalize");
            assert!(input.health.is_degraded());
        }
    }

    #[test]
    fn native_shape_and_summary_corruption_cannot_fall_back_to_legacy_success() {
        let ready = native_report(ready_findings());
        let mut bad_count = ready.clone();
        bad_count["summary"]["pass"] = json!(100);
        let mut bad_overall = ready.clone();
        bad_overall["overall"] = json!("warn");
        let mut bad_severity = ready.clone();
        bad_severity["findings"][0]["severity"] = json!("healthy");
        let mut missing_action = ready;
        missing_action["findings"][1]["data"]["action"] = Value::Null;
        for mut report in [
            bad_count,
            bad_overall,
            bad_severity,
            missing_action,
            json!({"findings": null}),
            json!({"findings": []}),
        ] {
            report["checks"] = json!([{"name": "legacy success", "status": "ok"}]);
            let input = normalize_doctor_json(provenance(), &report).expect("normalize");
            assert!(input.health.is_degraded());
            assert!(
                input
                    .checks
                    .iter()
                    .all(|check| check.name != "legacy success")
            );
        }
    }

    #[test]
    fn legacy_check_formats_keep_existing_strict_semantics() {
        for report in [
            json!({"checks": [{"name": "scratch", "status": "ok"}]}),
            json!({"preflight": {"checks": [{"id": "scratch", "result": "passed"}]}}),
        ] {
            let input = normalize_doctor_json(provenance(), &report).expect("normalize");
            assert!(!input.health.is_degraded());
            assert!(!input.has_failures);
        }
        let input = normalize_doctor_json(
            provenance(),
            &json!({"checks": [{"name": "scratch", "status": "warn"}]}),
        )
        .expect("normalize");
        assert!(input.has_failures);
    }

    fn source(name: &str) -> ValidationSourceProvenance {
        ValidationSourceProvenance::new(name, vec![name.to_string()], "/workspace", START, None)
            .expect("source provenance")
    }

    fn inputs() -> ValidationBrokerInputSnapshot {
        let headroom = json!({"available_bytes": 50_000, "required_bytes": 10_000});
        ValidationBrokerInputSnapshot::from_parts(ValidationBrokerInputParts {
            captured_at_utc: START.into(),
            rch: normalize_rch_queue_text(
                source("rch"),
                "1 Active Build(s)\n0 Queued Build(s)\n4 / 8 slots free\n",
            )
            .expect("RCH input"),
            cargo_headroom: normalize_headroom_json(source("cargo_headroom"), &headroom)
                .expect("cargo headroom"),
            doctor: normalize_doctor_json(provenance(), &native_report(ready_findings()))
                .expect("doctor input"),
            git: normalize_git_status_text(source("git"), "head", "## main\n").expect("git input"),
            beads: normalize_beads_json(source("beads"), &json!([]), NOW, 3600)
                .expect("Beads input"),
            scratch_headroom: normalize_headroom_json(source("scratch_headroom"), &headroom)
                .expect("scratch headroom"),
            agent_mail: normalize_available_source(source("agent_mail")).expect("mail input"),
        })
        .expect("input snapshot")
    }

    fn request() -> ValidationSlotRequest {
        ValidationSlotRequest {
            slot_id: "cached-validation".into(),
            owner_agent: "TestAgent".into(),
            bead_id: "bd-doctor".into(),
            command: vec!["cargo".into(), "check".into(), "--all-targets".into()],
            command_class: "cargo_check".into(),
            cwd: "/workspace".into(),
            git_head: "head".into(),
            feature_flags: vec!["default".into()],
            target_dir: "/data/tmp/pi_agent_rust_cargo/test/target".into(),
            tmpdir: "/data/tmp/pi_agent_rust_cargo/test/tmp".into(),
            runner: "rch_required".into(),
            rust_toolchain: Some("nightly".into()),
            rch_job_id: None,
            environment: BTreeMap::new(),
            expected_artifacts: vec![],
            artifact_schema: Some("test_result.v1".into()),
            artifact_hash: Some("test-result-hash".into()),
        }
    }

    fn reusable_store() -> ValidationSlotStoreSnapshot {
        let mut lease =
            ValidationSlotLease::acquire(request(), START, EXPIRES).expect("acquire test lease");
        lease
            .mark_reusable(
                "TestAgent",
                NOW,
                vec![ValidationSlotArtifact {
                    path: "result.json".into(),
                    sha256: Some("test-result-hash".into()),
                    schema: Some("test_result.v1".into()),
                }],
            )
            .expect("reusable test lease");
        ValidationSlotStoreSnapshot {
            schema: VALIDATION_BROKER_SLOT_STORE_SCHEMA.into(),
            status: ValidationSlotStoreStatus::Available,
            latest_by_slot_id: BTreeMap::from([(lease.slot_id.clone(), lease.clone())]),
            leases: vec![lease],
            degraded_reasons: vec![],
        }
    }

    fn decide(
        inputs: &ValidationBrokerInputSnapshot,
        store: &ValidationSlotStoreSnapshot,
    ) -> ValidationAdmissionDecisionRecord {
        decide_validation_admission(
            ValidationAdmissionRequestContext {
                request_id: "new-request".into(),
                request: request(),
                requested_at_utc: START.into(),
                bead_priority: 1,
            },
            inputs,
            store,
            &ValidationAdmissionPolicy::default(),
            NOW,
        )
        .expect("admission decision")
    }

    fn assert_blocked_without_reuse(decision: &ValidationAdmissionDecisionRecord) {
        assert_eq!(
            decision.decision,
            ValidationAdmissionDecision::DegradedBlock
        );
        assert!(decision.reusable_slot.is_none());
        assert!(decision.coalesced_artifacts.is_empty());
    }

    #[test]
    fn cached_result_cannot_override_consumer_only_or_failed_doctor() {
        let store = reusable_store();
        let consumer = native_report(vec![finding(CheckCategory::Config, Severity::Pass, None)]);
        let failed = json!({"checks": [{"name": "scratch", "status": "fail"}]});
        for report in [consumer, failed] {
            let mut inputs = inputs();
            inputs.doctor = normalize_doctor_json(provenance(), &report).expect("normalize");
            assert_blocked_without_reuse(&decide(&inputs, &store));
        }
    }

    #[test]
    fn unavailable_authority_blocks_even_without_a_reason_string() {
        for unavailable in [false, true] {
            let mut inputs = inputs();
            inputs.rch.health.state = if unavailable {
                ValidationSourceState::Unavailable
            } else {
                ValidationSourceState::Degraded
            };
            inputs.rch.health.degraded_reasons.clear();
            let decision = decide(&inputs, &reusable_store());
            assert_blocked_without_reuse(&decision);
            assert!(decision.policy.blocking_source_degraded);
            assert!(
                decision
                    .reasons
                    .iter()
                    .any(|reason| reason.starts_with("rch:"))
            );
        }
    }

    #[test]
    fn damaged_slot_store_cannot_reuse_an_earlier_valid_record() {
        let root = tempfile::tempdir().expect("temporary slot store");
        let path = root.path().join("slots.jsonl");
        let store = ValidationSlotStore::new(&path);
        store
            .append_lease("reusable", NOW, &reusable_store().leases[0])
            .expect("valid record");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append corrupt record")
            .write_all(b"{ corrupt later state\n")
            .expect("corrupt row");
        let snapshot = store.load_snapshot();
        assert!(snapshot.is_degraded());
        assert_eq!(snapshot.latest_by_slot_id.len(), 1);
        assert_blocked_without_reuse(&decide(&inputs(), &snapshot));
    }

    #[test]
    fn degraded_slot_state_without_explanations_still_blocks_reuse() {
        let mut store = reusable_store();
        store.status = ValidationSlotStoreStatus::Degraded;
        assert!(store.degraded_reasons.is_empty());
        assert_blocked_without_reuse(&decide(&inputs(), &store));
    }

    #[test]
    fn valid_cached_results_still_coalesce_under_capacity_pressure() {
        let mut inputs = inputs();
        inputs.rch.free_slots = Some(0);
        inputs.rch.saturated = true;
        let decision = decide(&inputs, &reusable_store());
        assert_eq!(decision.decision, ValidationAdmissionDecision::Coalesce);
        assert_eq!(decision.coalesced_artifacts.len(), 1);
        assert_eq!(decision.reusable_slot.as_deref(), Some("cached-validation"));
    }

    #[test]
    fn agent_mail_outage_remains_advisory_for_valid_reuse() {
        let mut inputs = inputs();
        inputs.agent_mail.state = ValidationSourceState::Unavailable;
        let decision = decide(&inputs, &reusable_store());
        assert_eq!(decision.decision, ValidationAdmissionDecision::Coalesce);
        assert_eq!(decision.confidence, "medium");
        assert!(
            decision
                .no_claims
                .iter()
                .any(|claim| { claim == "not_permission_to_skip_required_gates" })
        );
    }

    fn normalize_resource(data: Value) -> ValidationDoctorInput {
        let mut findings = ready_findings();
        findings[0].data = Some(data);
        normalize_doctor_json(provenance(), &native_report(findings)).expect("native input")
    }

    fn ready_resource() -> Value {
        ready_findings()[0].data.clone().expect("resource data")
    }

    fn assert_resource_blocks_new_work_and_reuse(data: Value) {
        let doctor = normalize_resource(data);
        assert!(doctor.health.is_degraded() || doctor.has_failures);
        let mut inputs = inputs();
        inputs.doctor = doctor;
        let mut empty = reusable_store();
        empty.leases.clear();
        empty.latest_by_slot_id.clear();
        assert_blocked_without_reuse(&decide(&inputs, &empty));
        assert_blocked_without_reuse(&decide(&inputs, &reusable_store()));
    }

    #[test]
    fn empty_error_lists_are_not_resource_evidence() {
        // This was previously accepted as healthy despite containing no plan,
        // target, temp directory or storage measurements at all.
        assert_resource_blocks_new_work_and_reuse(json!({
            "schema": RESOURCE_SCHEMA,
            "critical_failures": [],
            "source_errors": [],
            "plan_error": null
        }));
    }

    #[test]
    fn headroom_measurement_overrides_a_misleading_ready_flag() {
        for index in 0..2 {
            let mut data = ready_resource();
            data["tmpfs_headroom"]["paths"][index]["available_kb"] = json!(10_485_759);
            assert!(normalize_resource(data.clone()).has_failures);
            assert_resource_blocks_new_work_and_reuse(data);
        }
    }

    #[test]
    fn complete_headroom_at_the_threshold_and_disabled_remote_lane_are_valid() {
        for available in [10_485_760_u64, 10_485_761, u64::MAX] {
            let mut data = ready_resource();
            for index in 0..2 {
                data["tmpfs_headroom"]["paths"][index]["available_kb"] = json!(available);
            }
            let input = normalize_resource(data);
            assert!(!input.health.is_degraded());
            assert!(!input.has_failures);
        }
    }

    #[test]
    fn incomplete_or_duplicate_storage_paths_are_not_ready() {
        let mut duplicate = ready_resource();
        duplicate["tmpfs_headroom"]["paths"][1]["env_name"] = json!("CARGO_TARGET_DIR");
        assert_resource_blocks_new_work_and_reuse(duplicate);
        for replacement in [Value::Null, json!([]), json!([null])] {
            let mut data = ready_resource();
            data["tmpfs_headroom"]["paths"] = replacement;
            assert_resource_blocks_new_work_and_reuse(data);
        }
        for index in 0..2 {
            let mut data = ready_resource();
            data["tmpfs_headroom"]["paths"]
                .as_array_mut()
                .expect("paths")
                .remove(index);
            assert_resource_blocks_new_work_and_reuse(data);
        }
    }

    #[test]
    fn explicit_storage_hazards_survive_empty_critical_failure_lists() {
        for field in ["exists", "under_expected_root", "ready", "problem"] {
            for index in 0..2 {
                let mut data = ready_resource();
                data["tmpfs_headroom"]["paths"][index][field] = if field == "problem" {
                    json!("low_headroom")
                } else {
                    json!(false)
                };
                assert!(normalize_resource(data.clone()).has_failures);
                assert_resource_blocks_new_work_and_reuse(data);
            }
        }
    }

    #[test]
    fn storage_evidence_requires_typed_measurements_flags_and_policy() {
        for field in [
            "available_kb",
            "exists",
            "under_expected_root",
            "ready",
            "problem",
            "path",
        ] {
            for replacement in [json!([]), json!({}), json!("invalid")] {
                let mut data = ready_resource();
                data["tmpfs_headroom"]["paths"][0][field] = replacement;
                assert_resource_blocks_new_work_and_reuse(data);
            }
            let mut data = ready_resource();
            data["tmpfs_headroom"]["paths"][0]
                .as_object_mut()
                .expect("path")
                .remove(field);
            assert_resource_blocks_new_work_and_reuse(data);
        }
        for replacement in [
            Value::Null,
            json!(0),
            json!(-1),
            json!("10485760"),
            json!(1.5),
        ] {
            let mut data = ready_resource();
            data["tmpfs_headroom"]["warn_available_kb"] = replacement;
            assert_resource_blocks_new_work_and_reuse(data);
        }
    }

    #[test]
    fn storage_paths_cannot_escape_or_merely_share_a_string_prefix() {
        let root = "/data/tmp/pi_agent_rust_cargo";
        assert!(reported_path_within_root(root, root));
        assert!(reported_path_within_root(&format!("{root}/a/./tmp"), root));
        assert!(reported_path_within_root(&format!("{root}//a/tmp"), root));
        for path in [
            "/tmp",
            "relative/tmp",
            "/data/tmp/pi_agent_rust_cargo_other/a/tmp",
            "/data/tmp/pi_agent_rust_cargo/../other/tmp",
            "/data/tmp/pi_agent_rust_cargo/a\0/tmp",
        ] {
            assert!(!reported_path_within_root(path, root));
            let mut data = ready_resource();
            data["tmpfs_headroom"]["paths"][0]["path"] = json!(path);
            assert_resource_blocks_new_work_and_reuse(data);
        }
    }

    #[test]
    fn missing_or_invalid_planned_budgets_block_admission() {
        for replacement in [Value::Null, json!({}), json!([])] {
            let mut data = ready_resource();
            data["recommended_budgets"] = replacement;
            assert_resource_blocks_new_work_and_reuse(data);
        }
        for field in [
            "agent_concurrency",
            "tool_concurrency",
            "extension_hostcall_lanes",
            "max_queue_depth",
            "max_rss_bytes",
            "rch_verification_fanout",
        ] {
            for replacement in [Value::Null, json!(-1), json!("2"), json!(1.5)] {
                let mut data = ready_resource();
                data["recommended_budgets"][field] = replacement;
                assert_resource_blocks_new_work_and_reuse(data);
            }
            if field != "rch_verification_fanout" {
                let mut data = ready_resource();
                data["recommended_budgets"][field] = json!(0);
                assert_resource_blocks_new_work_and_reuse(data);
            }
        }
    }
}
