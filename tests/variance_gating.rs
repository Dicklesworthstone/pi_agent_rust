#![forbid(unsafe_code)]

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const VARIANCE_CONTRACT_SCHEMA: &str = "pi.perf.variance_gating.contract.v1";
pub const VARIANCE_ARTIFACT_SCHEMA: &str = "pi.perf.variance_gate_report.v1";
pub const HOST_TOPOLOGY_SCHEMA: &str = "pi.perf.host_topology_fingerprint.v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostTopologyFingerprint {
    pub schema: String,
    pub os: String,
    pub arch: String,
    pub cpu_brand: String,
    pub cpu_cores: usize,
    pub mem_total_mb: u64,
    pub governor: String,
    pub turbo_boost: String,
    pub aslr: String,
    pub thp: String,
    pub noise_score: u8,
    pub git_commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatedBudgetResult {
    pub budget_name: String,
    pub category: String,
    pub threshold: f64,
    pub comparison: String,
    pub unit: String,
    pub environment: HostTopologyFingerprint,
    pub gate_status: String, // "ACCEPTED" | "REJECTED_NO_DATA"
    pub rejection_reason: Option<String>,
    pub empirical_value: Option<f64>,
    pub budget_verdict: String, // "PASS" | "FAIL" | "NO_DATA"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VarianceGateArtifact {
    pub schema: String,
    pub generated_at: String,
    pub contract_path: String,
    pub max_admissible_noise_score: u8,
    pub total_evaluated: usize,
    pub accepted_count: usize,
    pub rejected_noise_count: usize,
    pub results: Vec<GatedBudgetResult>,
    pub overall_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub schema: String,
    pub status: String,
    pub evaluated_budgets: usize,
    pub accepted_budgets: usize,
    pub rejected_noise_budgets: usize,
    pub errors: Vec<String>,
}

pub fn compute_noise_score(governor: &str, turbo: &str, thp: &str, aslr: &str) -> u8 {
    let mut score: u8 = 0;
    if governor != "performance" && governor != "unavailable" {
        score += 3;
    }
    if turbo == "enabled" {
        score += 2;
    }
    if thp != "never" && thp != "unavailable" {
        score += 1;
    }
    if aslr != "disabled" && aslr != "unavailable" {
        score += 1;
    }
    score
}

pub fn create_host_topology_fingerprint(
    governor: &str,
    turbo: &str,
    thp: &str,
    aslr: &str,
    git_commit: &str,
) -> HostTopologyFingerprint {
    let noise_score = compute_noise_score(governor, turbo, thp, aslr);
    HostTopologyFingerprint {
        schema: HOST_TOPOLOGY_SCHEMA.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpu_brand: "Standard Benchmark Topology".to_string(),
        cpu_cores: 16,
        mem_total_mb: 64000,
        governor: governor.to_string(),
        turbo_boost: turbo.to_string(),
        aslr: aslr.to_string(),
        thp: thp.to_string(),
        noise_score,
        git_commit: git_commit.to_string(),
    }
}

pub fn evaluate_gated_series(
    budget_name: &str,
    category: &str,
    threshold: f64,
    comparison: &str,
    unit: &str,
    samples: &[f64],
    env: &HostTopologyFingerprint,
    max_noise_score: u8,
) -> GatedBudgetResult {
    if env.noise_score > max_noise_score {
        let reason = format!(
            "host environment noise score {} exceeds maximum allowed threshold {} (governor={}, turbo={})",
            env.noise_score, max_noise_score, env.governor, env.turbo_boost
        );
        GatedBudgetResult {
            budget_name: budget_name.to_string(),
            category: category.to_string(),
            threshold,
            comparison: comparison.to_string(),
            unit: unit.to_string(),
            environment: env.clone(),
            gate_status: "REJECTED_NO_DATA".to_string(),
            rejection_reason: Some(reason),
            empirical_value: None, // strictly NOT averaged into compliance
            budget_verdict: "NO_DATA".to_string(),
        }
    } else {
        let p95 = if samples.is_empty() {
            0.0
        } else {
            let mut sorted = samples.to_vec();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let idx = (((sorted.len() as f64) * 0.95).ceil() as usize).clamp(1, sorted.len());
            sorted.get(idx.saturating_sub(1)).copied().unwrap_or(0.0)
        };

        let budget_verdict = if comparison == "minimum" {
            if p95 >= threshold {
                "PASS".to_string()
            } else {
                "FAIL".to_string()
            }
        } else {
            if p95 <= threshold {
                "PASS".to_string()
            } else {
                "FAIL".to_string()
            }
        };

        GatedBudgetResult {
            budget_name: budget_name.to_string(),
            category: category.to_string(),
            threshold,
            comparison: comparison.to_string(),
            unit: unit.to_string(),
            environment: env.clone(),
            gate_status: "ACCEPTED".to_string(),
            rejection_reason: None,
            empirical_value: Some((p95 * 100.0).round() / 100.0),
            budget_verdict,
        }
    }
}

enum VarianceIssue<'a> {
    InvalidSchema(&'a str, &'a str),
    NotRejected(&'a str, u8, u8),
    HasEmpiricalValue(&'a str, u8),
    CleanRejected(&'a str, u8, u8),
}

impl std::fmt::Display for VarianceIssue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSchema(b, s) => write!(f, "invalid topology schema for {b}: {s}"),
            Self::NotRejected(b, score, max) => write!(
                f,
                "budget {b} with noise_score {score} > {max} was not REJECTED_NO_DATA"
            ),
            Self::HasEmpiricalValue(b, score) => write!(
                f,
                "budget {b} with noise_score {score} has empirical_value, violating no-averaging invariant"
            ),
            Self::CleanRejected(b, score, max) => write!(
                f,
                "clean budget {b} with noise_score {score} <= {max} was rejected"
            ),
        }
    }
}

pub fn verify_variance_artifact(
    artifact: &VarianceGateArtifact,
    contract_path: &Path,
) -> VerificationReport {
    let mut errors = Vec::new();

    if artifact.schema != VARIANCE_ARTIFACT_SCHEMA {
        errors.push(format!(
            "invalid schema: expected {}, got {}",
            VARIANCE_ARTIFACT_SCHEMA, artifact.schema
        ));
    }

    if !contract_path.exists() {
        errors.push(format!(
            "contract does not exist on disk: {}",
            contract_path.display()
        ));
    }

    let mut accepted = 0;
    let mut rejected = 0;
    let mut issues = Vec::new();

    for r in &artifact.results {
        if r.environment.schema != HOST_TOPOLOGY_SCHEMA {
            issues.push(VarianceIssue::InvalidSchema(
                &r.budget_name,
                &r.environment.schema,
            ));
        }

        if r.environment.noise_score > artifact.max_admissible_noise_score {
            if r.gate_status != "REJECTED_NO_DATA" {
                issues.push(VarianceIssue::NotRejected(
                    &r.budget_name,
                    r.environment.noise_score,
                    artifact.max_admissible_noise_score,
                ));
            }
            if r.empirical_value.is_some() {
                issues.push(VarianceIssue::HasEmpiricalValue(
                    &r.budget_name,
                    r.environment.noise_score,
                ));
            }
            rejected += 1;
        } else {
            if r.gate_status != "ACCEPTED" {
                issues.push(VarianceIssue::CleanRejected(
                    &r.budget_name,
                    r.environment.noise_score,
                    artifact.max_admissible_noise_score,
                ));
            }
            accepted += 1;
        }
    }

    errors.extend(issues.into_iter().map(|i| i.to_string()));

    if accepted != artifact.accepted_count {
        errors.push(format!(
            "accepted count mismatch: declared {}, actual {}",
            artifact.accepted_count, accepted
        ));
    }

    if rejected != artifact.rejected_noise_count {
        errors.push(format!(
            "rejected noise count mismatch: declared {}, actual {}",
            artifact.rejected_noise_count, rejected
        ));
    }

    let status = if errors.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    VerificationReport {
        schema: "pi.perf.variance_gate.verification_report.v1".to_string(),
        status,
        evaluated_budgets: artifact.results.len(),
        accepted_budgets: accepted,
        rejected_noise_budgets: rejected,
        errors,
    }
}

#[test]
fn contract_file_matches_schema_and_policy() -> Result<()> {
    let contract_path = Path::new("docs/contracts/variance-gating-contract.json");
    assert!(contract_path.exists(), "contract file must exist");

    let text = fs::read_to_string(contract_path)?;
    let val: serde_json::Value = serde_json::from_str(&text)?;

    assert_eq!(
        val.get("schema").and_then(|v| v.as_str()),
        Some(VARIANCE_CONTRACT_SCHEMA)
    );
    assert_eq!(
        val.get("bead_id").and_then(|v| v.as_str()),
        Some("bd-sog97.21")
    );
    assert_eq!(
        val.get("gating_rules")
            .and_then(|gr| gr.get("max_admissible_noise_score_strict"))
            .and_then(|m| m.as_u64()),
        Some(0)
    );

    Ok(())
}

#[test]
fn noise_score_computation_all_factors() {
    // 0 noise: performance governor, no turbo, THP never, ASLR disabled
    assert_eq!(
        compute_noise_score("performance", "disabled", "never", "disabled"),
        0
    );

    // Governor penalty (+3)
    assert_eq!(
        compute_noise_score("powersave", "disabled", "never", "disabled"),
        3
    );

    // Turbo penalty (+2)
    assert_eq!(
        compute_noise_score("performance", "enabled", "never", "disabled"),
        2
    );

    // THP penalty (+1)
    assert_eq!(
        compute_noise_score("performance", "disabled", "always", "disabled"),
        1
    );

    // ASLR penalty (+1)
    assert_eq!(
        compute_noise_score("performance", "disabled", "never", "full"),
        1
    );

    // Max penalty (+7)
    assert_eq!(
        compute_noise_score("powersave", "enabled", "always", "full"),
        7
    );
}

#[test]
fn clean_run_accepted_into_evidence() {
    let clean_env = create_host_topology_fingerprint(
        "performance",
        "disabled",
        "never",
        "disabled",
        "380af591",
    );
    assert_eq!(clean_env.noise_score, 0);

    let samples = vec![4.5, 4.8, 5.0, 5.2, 5.5];
    let result = evaluate_gated_series(
        "startup_version_p95",
        "startup",
        100.0,
        "maximum",
        "ms",
        &samples,
        &clean_env,
        0,
    );

    assert_eq!(result.gate_status, "ACCEPTED");
    assert_eq!(result.budget_verdict, "PASS");
    assert!(result.rejection_reason.is_none());
    assert!(result.empirical_value.is_some());
}

#[test]
fn noisy_run_rejected_with_named_reason_and_no_data() {
    let noisy_env =
        create_host_topology_fingerprint("powersave", "enabled", "always", "full", "380af591");
    assert_eq!(noisy_env.noise_score, 7);

    let samples = vec![4.5, 4.8, 5.0, 5.2, 5.5];
    let result = evaluate_gated_series(
        "startup_version_p95",
        "startup",
        100.0,
        "maximum",
        "ms",
        &samples,
        &noisy_env,
        0, // max allowed 0
    );

    assert_eq!(result.gate_status, "REJECTED_NO_DATA");
    assert_eq!(result.budget_verdict, "NO_DATA");
    assert!(
        result.empirical_value.is_none(),
        "noisy runs must never be averaged into compliance"
    );
    assert!(
        result
            .rejection_reason
            .unwrap()
            .contains("noise score 7 exceeds maximum allowed threshold 0")
    );
}

#[test]
fn mock_artifact_passes_verification() -> Result<()> {
    let contract_path = Path::new("docs/contracts/variance-gating-contract.json");
    let clean_env = create_host_topology_fingerprint(
        "performance",
        "disabled",
        "never",
        "disabled",
        "380af591",
    );

    let artifact = VarianceGateArtifact {
        schema: VARIANCE_ARTIFACT_SCHEMA.to_string(),
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        max_admissible_noise_score: 0,
        total_evaluated: 1,
        accepted_count: 1,
        rejected_noise_count: 0,
        results: vec![GatedBudgetResult {
            budget_name: "startup_version_p95".to_string(),
            category: "startup".to_string(),
            threshold: 100.0,
            comparison: "maximum".to_string(),
            unit: "ms".to_string(),
            environment: clean_env,
            gate_status: "ACCEPTED".to_string(),
            rejection_reason: None,
            empirical_value: Some(5.5),
            budget_verdict: "PASS".to_string(),
        }],
        overall_status: "PASS".to_string(),
    };

    let report = verify_variance_artifact(&artifact, contract_path);
    assert_eq!(report.status, "pass");
    assert_eq!(report.accepted_budgets, 1);
    assert_eq!(report.rejected_noise_budgets, 0);
    assert!(report.errors.is_empty());

    Ok(())
}

#[test]
fn tamper_detection_in_variance_artifact() -> Result<()> {
    let contract_path = Path::new("docs/contracts/variance-gating-contract.json");
    let noisy_env =
        create_host_topology_fingerprint("powersave", "enabled", "never", "disabled", "380af591");

    let artifact = VarianceGateArtifact {
        schema: VARIANCE_ARTIFACT_SCHEMA.to_string(),
        generated_at: "2026-08-24T00:00:00Z".to_string(),
        contract_path: contract_path.to_string_lossy().to_string(),
        max_admissible_noise_score: 0,
        total_evaluated: 1,
        accepted_count: 1, // tamper: should be 0
        rejected_noise_count: 0,
        results: vec![GatedBudgetResult {
            budget_name: "startup_version_p95".to_string(),
            category: "startup".to_string(),
            threshold: 100.0,
            comparison: "maximum".to_string(),
            unit: "ms".to_string(),
            environment: noisy_env,
            gate_status: "ACCEPTED".to_string(), // tamper: noisy env accepted
            rejection_reason: None,
            empirical_value: Some(5.5), // tamper: averaged in
            budget_verdict: "PASS".to_string(),
        }],
        overall_status: "PASS".to_string(),
    };

    let report = verify_variance_artifact(&artifact, contract_path);
    assert_eq!(report.status, "fail");
    assert!(!report.errors.is_empty());

    Ok(())
}
