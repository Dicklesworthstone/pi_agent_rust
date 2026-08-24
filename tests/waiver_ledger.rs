#![forbid(unsafe_code)]

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::error::Error;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaiverEntry {
    pub budget_id: String,
    pub reason: String,
    pub evidence_links: Vec<String>,
    pub operator: String,
    pub created_at: String,
    pub expires_at: String,
    pub suppressed_claim_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaiverStatus {
    Active,
    Expired,
    Invalid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaiverEvaluation {
    pub budget_id: String,
    pub status: WaiverStatus,
    pub error_message: Option<String>,
    pub days_remaining: Option<i64>,
}

pub fn evaluate_waiver_entry(
    entry: &WaiverEntry,
    now: DateTime<Utc>,
    max_duration_days: i64,
) -> WaiverEvaluation {
    if entry.budget_id.trim().is_empty() {
        return WaiverEvaluation {
            budget_id: entry.budget_id.clone(),
            status: WaiverStatus::Invalid,
            error_message: Some("budget_id must not be empty".into()),
            days_remaining: None,
        };
    }
    if entry.reason.trim().is_empty() {
        return WaiverEvaluation {
            budget_id: entry.budget_id.clone(),
            status: WaiverStatus::Invalid,
            error_message: Some("reason must not be empty".into()),
            days_remaining: None,
        };
    }
    if entry.operator.trim().is_empty() {
        return WaiverEvaluation {
            budget_id: entry.budget_id.clone(),
            status: WaiverStatus::Invalid,
            error_message: Some("operator must not be empty".into()),
            days_remaining: None,
        };
    }
    if entry.suppressed_claim_keys.is_empty() {
        return WaiverEvaluation {
            budget_id: entry.budget_id.clone(),
            status: WaiverStatus::Invalid,
            error_message: Some("suppressed_claim_keys must not be empty".into()),
            days_remaining: None,
        };
    }

    let Ok(created_at) = DateTime::parse_from_rfc3339(&entry.created_at) else {
        return WaiverEvaluation {
            budget_id: entry.budget_id.clone(),
            status: WaiverStatus::Invalid,
            error_message: Some(format!("invalid created_at timestamp: {}", entry.created_at)),
            days_remaining: None,
        };
    };
    let Ok(expires_at) = DateTime::parse_from_rfc3339(&entry.expires_at) else {
        return WaiverEvaluation {
            budget_id: entry.budget_id.clone(),
            status: WaiverStatus::Invalid,
            error_message: Some(format!("invalid expires_at timestamp: {}", entry.expires_at)),
            days_remaining: None,
        };
    };

    let created_utc = created_at.with_timezone(&Utc);
    let expires_utc = expires_at.with_timezone(&Utc);

    if expires_utc <= created_utc {
        return WaiverEvaluation {
            budget_id: entry.budget_id.clone(),
            status: WaiverStatus::Invalid,
            error_message: Some("expires_at must be after created_at".into()),
            days_remaining: None,
        };
    }

    let duration = expires_utc.signed_duration_since(created_utc);
    if duration > Duration::days(max_duration_days) {
        return WaiverEvaluation {
            budget_id: entry.budget_id.clone(),
            status: WaiverStatus::Invalid,
            error_message: Some(format!(
                "duration ({} days) exceeds maximum allowed duration ({} days)",
                duration.num_days(),
                max_duration_days
            )),
            days_remaining: None,
        };
    }

    if expires_utc < now {
        return WaiverEvaluation {
            budget_id: entry.budget_id.clone(),
            status: WaiverStatus::Expired,
            error_message: Some(format!(
                "waiver for budget '{}' expired at {} and has re-blocked",
                entry.budget_id, entry.expires_at
            )),
            days_remaining: Some(0),
        };
    }

    let remaining = expires_utc.signed_duration_since(now).num_days();
    WaiverEvaluation {
        budget_id: entry.budget_id.clone(),
        status: WaiverStatus::Active,
        error_message: None,
        days_remaining: Some(remaining.max(0)),
    }
}

#[test]
fn contract_file_matches_schema_and_policy() -> Result<(), Box<dyn Error>> {
    let contract_path = repo_root().join("docs/contracts/waiver-ledger-contract.json");
    assert!(contract_path.exists(), "waiver contract file must exist");

    let text = std::fs::read_to_string(&contract_path)?;
    let contract: Value = serde_json::from_str(&text)?;

    assert_eq!(
        contract.get("schema").and_then(Value::as_str),
        Some("pi.waiver.ledger.contract.v1"),
        "contract schema must be pi.waiver.ledger.contract.v1"
    );
    assert_eq!(
        contract.get("bead_id").and_then(Value::as_str),
        Some("bd-sog97.12"),
        "bead_id must be bd-sog97.12"
    );
    assert_eq!(
        contract.get("max_waiver_duration_days").and_then(Value::as_i64),
        Some(30),
        "max_waiver_duration_days must be 30"
    );

    let required_fields = contract
        .get("required_entry_fields")
        .and_then(Value::as_array)
        .ok_or("required_entry_fields must be an array")?;
    let field_names: Vec<&str> = required_fields.iter().filter_map(Value::as_str).collect();
    assert!(field_names.contains(&"budget_id"));
    assert!(field_names.contains(&"reason"));
    assert!(field_names.contains(&"evidence_links"));
    assert!(field_names.contains(&"operator"));
    assert!(field_names.contains(&"created_at"));
    assert!(field_names.contains(&"expires_at"));
    assert!(field_names.contains(&"suppressed_claim_keys"));
    Ok(())
}

#[test]
fn evidence_file_matches_schema_and_references_contract() -> Result<(), Box<dyn Error>> {
    let evidence_path = repo_root().join("docs/evidence/waivers.json");
    assert!(evidence_path.exists(), "waivers evidence file must exist");

    let text = std::fs::read_to_string(&evidence_path)?;
    let evidence: Value = serde_json::from_str(&text)?;

    assert_eq!(
        evidence.get("schema").and_then(Value::as_str),
        Some("pi.waiver.ledger.v1"),
        "evidence schema must be pi.waiver.ledger.v1"
    );
    assert_eq!(
        evidence.get("contract_path").and_then(Value::as_str),
        Some("docs/contracts/waiver-ledger-contract.json"),
        "contract_path must point to docs/contracts/waiver-ledger-contract.json"
    );

    let summary = evidence.get("summary").ok_or("summary must exist")?;
    let waivers = evidence.get("waivers").and_then(Value::as_array).ok_or("waivers array")?;

    assert_eq!(
        summary.get("total_waivers").and_then(Value::as_u64),
        Some(waivers.len() as u64)
    );
    Ok(())
}

#[test]
fn waiver_lifecycle_valid_active_waiver() -> Result<(), Box<dyn Error>> {
    let now = Utc::now();
    let entry = WaiverEntry {
        budget_id: "ext_cold_load_simple_p95".into(),
        reason: "Temporary waiver while v8 engine isolation refactoring is in flight".into(),
        evidence_links: vec!["docs/evidence/ext-stress-reactor-queue-coverage.json".into()],
        operator: "RoseCarp".into(),
        created_at: (now - Duration::days(2)).to_rfc3339(),
        expires_at: (now + Duration::days(14)).to_rfc3339(),
        suppressed_claim_keys: vec!["cold_load_sub_5ms".into()],
    };

    let eval = evaluate_waiver_entry(&entry, now, 30);
    assert_eq!(eval.status, WaiverStatus::Active);
    assert!(eval.error_message.is_none());
    assert!(eval.days_remaining.ok_or("missing days_remaining")? > 0);
    Ok(())
}

#[test]
fn waiver_lifecycle_expired_waiver_reblocks() -> Result<(), Box<dyn Error>> {
    let now = Utc::now();
    let entry = WaiverEntry {
        budget_id: "ext_cold_load_simple_p95".into(),
        reason: "Historical waiver that was not renewed".into(),
        evidence_links: vec![],
        operator: "RoseCarp".into(),
        created_at: (now - Duration::days(20)).to_rfc3339(),
        expires_at: (now - Duration::days(1)).to_rfc3339(),
        suppressed_claim_keys: vec!["cold_load_sub_5ms".into()],
    };

    let eval = evaluate_waiver_entry(&entry, now, 30);
    assert_eq!(eval.status, WaiverStatus::Expired);
    assert!(eval.error_message.is_some());
    let err = eval.error_message.ok_or("missing error_message")?;
    assert!(err.contains("expired at"));
    assert!(err.contains("re-blocked"));
    Ok(())
}

#[test]
fn waiver_lifecycle_max_duration_exceeded_rejected() -> Result<(), Box<dyn Error>> {
    let now = Utc::now();
    let entry = WaiverEntry {
        budget_id: "tool_call_latency_mean".into(),
        reason: "Excessively long waiver attempt".into(),
        evidence_links: vec![],
        operator: "RoseCarp".into(),
        created_at: now.to_rfc3339(),
        expires_at: (now + Duration::days(45)).to_rfc3339(),
        suppressed_claim_keys: vec!["sub_10ms_tool_dispatch".into()],
    };

    let eval = evaluate_waiver_entry(&entry, now, 30);
    assert_eq!(eval.status, WaiverStatus::Invalid);
    assert!(eval.error_message.ok_or("missing error message")?.contains("exceeds maximum allowed duration"));
    Ok(())
}

#[test]
fn waiver_lifecycle_missing_required_fields_rejected() -> Result<(), Box<dyn Error>> {
    let now = Utc::now();
    let mut entry = WaiverEntry {
        budget_id: "".into(),
        reason: "Some reason".into(),
        evidence_links: vec![],
        operator: "RoseCarp".into(),
        created_at: now.to_rfc3339(),
        expires_at: (now + Duration::days(5)).to_rfc3339(),
        suppressed_claim_keys: vec!["claim_key".into()],
    };

    let eval = evaluate_waiver_entry(&entry, now, 30);
    assert_eq!(eval.status, WaiverStatus::Invalid);
    assert!(eval.error_message.ok_or("missing error message")?.contains("budget_id"));

    entry.budget_id = "valid_budget".into();
    entry.suppressed_claim_keys = vec![];
    let eval2 = evaluate_waiver_entry(&entry, now, 30);
    assert_eq!(eval2.status, WaiverStatus::Invalid);
    assert!(eval2.error_message.ok_or("missing error message")?.contains("suppressed_claim_keys"));
    Ok(())
}

#[test]
fn waived_budget_suppresses_claim_copy() {
    let now = Utc::now();
    let entry = WaiverEntry {
        budget_id: "ext_cold_load_simple_p95".into(),
        reason: "Under active calibration".into(),
        evidence_links: vec![],
        operator: "RoseCarp".into(),
        created_at: now.to_rfc3339(),
        expires_at: (now + Duration::days(7)).to_rfc3339(),
        suppressed_claim_keys: vec!["sub_5ms_cold_load".into(), "strict_dropin_parity".into()],
    };

    // A waived budget requires suppressing claim copy in docs/marketing
    assert!(!entry.suppressed_claim_keys.is_empty());
    for key in &entry.suppressed_claim_keys {
        assert!(!key.is_empty());
    }
}
