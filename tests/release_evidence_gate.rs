//! Release gate: validates that the conformance evidence bundle exists,
//! is structurally valid, and meets minimum thresholds for release.
//!
//! This test suite enforces that releases are evidence-based. It checks:
//! - Required evidence artifacts exist on disk
//! - Evidence artifacts have valid schemas
//! - Pass-rate and failure thresholds meet release criteria
//! - Exception policy is complete and current
//!
//! See also: `tests/release_readiness.rs` for the readiness report generator.
#![allow(clippy::too_many_lines)]

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

struct UniqueJsonValue(Value);

impl<'de> Deserialize<'de> for UniqueJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonValueVisitor)
    }
}

struct UniqueJsonValueVisitor;

impl<'de> Visitor<'de> for UniqueJsonValueVisitor {
    type Value = UniqueJsonValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueJsonValue)
            .ok_or_else(|| E::custom("non-finite number is not valid JSON"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(Value::String(value.to_string())))
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(Value::String(value.to_string())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        UniqueJsonValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueJsonValue>()? {
            values.push(value.0);
        }
        Ok(UniqueJsonValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON object key: {key}"
                )));
            }
            let value = object.next_value::<UniqueJsonValue>()?;
            values.insert(key, value.0);
        }
        Ok(UniqueJsonValue(Value::Object(values)))
    }
}

fn parse_release_json(contents: &[u8]) -> Result<Value, String> {
    let mut deserializer = serde_json::Deserializer::from_slice(contents);
    let value = UniqueJsonValue::deserialize(&mut deserializer)
        .map_err(|error| error.to_string())?
        .0;
    deserializer.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn load_json(relative: &str) -> Option<Value> {
    let path = repo_root().join(relative);
    let contents = std::fs::read(&path).ok()?;
    parse_release_json(&contents).ok()
}

fn require_json(relative: &str) -> Value {
    load_json(relative).unwrap_or_else(|| panic!("required evidence file missing: {relative}"))
}

fn require_text(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| format!("__UNREADABLE_TEXT_FILE__ {relative}: {err}"))
}

const FRANKEN_NODE_CLAIM_CONTRACT_PATH: &str = "docs/franken-node-claim-gating-contract.json";
const FRANKEN_NODE_CLAIM_CONTRACT_SCHEMA: &str = "pi.frankennode.claim_gating_contract.v1";
const FRANKEN_NODE_REQUIRED_TIER_IDS: &[&str] = &[
    "TIER-1-EXTENSION-HOST-PARITY",
    "TIER-2-TARGETED-RUNTIME-PARITY",
    "TIER-3-FULL-NODE-BUN-REPLACEMENT",
];
const FRANKEN_NODE_REQUIRED_ARTIFACTS: &[&str] = &[
    "tests/full_suite_gate/franken_node_claim_verdict.json",
    "tests/full_suite_gate/practical_finish_checkpoint.json",
];
const FRANKEN_NODE_REQUIRED_OVERCLAIM_BLOCKERS: &[&str] = &[
    "missing_required_evidence",
    "missing_or_stale_verdict_artifact",
    "forbidden_claim_phrase_detected",
];
const FRANKEN_NODE_REQUIRED_LOG_FIELDS: &[&str] = &[
    "run_id",
    "tier_id",
    "decision",
    "blocking_reasons",
    "evidence_refs",
    "timestamp_utc",
];
const FRANKEN_NODE_TIER2_REQUIRED_EVIDENCE_TOKENS: &[&str] = &[
    "compatibility matrix with executable conformance harness",
    "package/ecosystem interoperability contract evidence (cjs/esm/npm)",
];
const FRANKEN_NODE_TIER3_REQUIRED_EVIDENCE_TOKENS: &[&str] = &[
    "package/ecosystem interoperability strict-tier evidence and claim-tier linkage",
    "kernel extraction boundary manifest and reintegration mapping evidence",
    "runtime-substrate generalization evidence for bd-3ar8v.7.5",
    "multi-tier execution engine evidence for bd-3ar8v.7.6",
    "compatibility remediation backlog generator evidence for bd-3ar8v.7.16",
    "crate reintegration evidence into pi_agent_rust",
];

fn collect_non_empty_string_array(
    value: &Value,
    pointer: &str,
    label: &str,
    errors: &mut Vec<String>,
) -> Vec<String> {
    let Some(entries) = value.pointer(pointer).and_then(Value::as_array) else {
        errors.push(format!("{label} must be an array at {pointer}"));
        return Vec::new();
    };
    if entries.is_empty() {
        errors.push(format!("{label} must be non-empty at {pointer}"));
        return Vec::new();
    }

    let mut out = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Some(raw) = entry.as_str() else {
            errors.push(format!("{label}[{index}] must be a string at {pointer}"));
            continue;
        };
        let normalized = raw.trim();
        if normalized.is_empty() {
            errors.push(format!("{label}[{index}] must be non-empty at {pointer}"));
            continue;
        }
        out.push(normalized.to_string());
    }
    out
}

fn validate_franken_node_claim_contract(contract: &Value) -> Result<(), String> {
    let mut errors = Vec::new();

    let schema = contract.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != FRANKEN_NODE_CLAIM_CONTRACT_SCHEMA {
        errors.push(format!(
            "schema must be {FRANKEN_NODE_CLAIM_CONTRACT_SCHEMA}, found {schema}"
        ));
    }

    for field in [
        "/mission_statement",
        "/claim_gate_policy/release_claim_gate_mode",
    ] {
        let value = contract
            .pointer(field)
            .and_then(Value::as_str)
            .map_or("", str::trim);
        if value.is_empty() {
            errors.push(format!("missing required non-empty string at {field}"));
        }
    }

    let release_mode = contract
        .pointer("/claim_gate_policy/release_claim_gate_mode")
        .and_then(Value::as_str)
        .unwrap_or("");
    if release_mode != "hard_fail_if_unmet" {
        errors.push(format!(
            "claim_gate_policy.release_claim_gate_mode must be hard_fail_if_unmet, found {release_mode}"
        ));
    }

    let mut observed_tier_ids = HashSet::new();
    let Some(claim_tiers) = contract.get("claim_tiers").and_then(Value::as_array) else {
        errors.push("claim_tiers must be an array".to_string());
        return Err(errors.join("; "));
    };
    if claim_tiers.is_empty() {
        errors.push("claim_tiers must be non-empty".to_string());
    }

    for (index, tier) in claim_tiers.iter().enumerate() {
        let Some(tier_id) = tier.get("tier_id").and_then(Value::as_str).map(str::trim) else {
            errors.push(format!("claim_tiers[{index}].tier_id must be a string"));
            continue;
        };
        if tier_id.is_empty() {
            errors.push(format!("claim_tiers[{index}].tier_id must be non-empty"));
            continue;
        }
        observed_tier_ids.insert(tier_id.to_string());

        let allowed = collect_non_empty_string_array(
            tier,
            "/allowed_claim_language",
            &format!("claim_tiers[{index}].allowed_claim_language"),
            &mut errors,
        );
        let required_evidence = collect_non_empty_string_array(
            tier,
            "/required_evidence",
            &format!("claim_tiers[{index}].required_evidence"),
            &mut errors,
        );
        let forbidden = collect_non_empty_string_array(
            tier,
            "/forbidden_claim_language",
            &format!("claim_tiers[{index}].forbidden_claim_language"),
            &mut errors,
        );

        if required_evidence.is_empty() {
            errors.push(format!(
                "claim_tiers[{index}] must include required_evidence entries"
            ));
        }
        let required_evidence_tokens: &[&str] = match tier_id {
            "TIER-2-TARGETED-RUNTIME-PARITY" => FRANKEN_NODE_TIER2_REQUIRED_EVIDENCE_TOKENS,
            "TIER-3-FULL-NODE-BUN-REPLACEMENT" => FRANKEN_NODE_TIER3_REQUIRED_EVIDENCE_TOKENS,
            _ => &[],
        };
        if !required_evidence_tokens.is_empty() {
            let evidence_set = required_evidence
                .iter()
                .map(|entry| entry.to_ascii_lowercase())
                .collect::<HashSet<_>>();
            for required_token in required_evidence_tokens {
                if !evidence_set.contains(&required_token.to_ascii_lowercase()) {
                    errors.push(format!(
                        "claim_tiers[{index}] ({tier_id}) required_evidence missing token: {required_token}"
                    ));
                }
            }
        }

        if !allowed.is_empty() && !forbidden.is_empty() {
            let allowed_set = allowed
                .iter()
                .map(|entry| entry.to_ascii_lowercase())
                .collect::<HashSet<_>>();
            let overlap = forbidden
                .iter()
                .map(|entry| entry.to_ascii_lowercase())
                .find(|entry| allowed_set.contains(entry));
            if let Some(phrase) = overlap {
                errors.push(format!(
                    "claim_tiers[{index}] has overlap between allowed_claim_language and forbidden_claim_language: {phrase}"
                ));
            }
        }
    }

    for tier_id in FRANKEN_NODE_REQUIRED_TIER_IDS {
        if !observed_tier_ids.contains(*tier_id) {
            errors.push(format!("missing required claim tier: {tier_id}"));
        }
    }

    let forbidden_patterns = collect_non_empty_string_array(
        contract,
        "/forbidden_claim_patterns",
        "forbidden_claim_patterns",
        &mut errors,
    );
    let forbidden_pattern_set = forbidden_patterns
        .iter()
        .map(|pattern| pattern.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    for required_pattern in [
        "strict drop-in replacement for node/bun",
        "production-ready full runtime replacement without certification",
    ] {
        if !forbidden_pattern_set.contains(required_pattern) {
            errors.push(format!(
                "forbidden_claim_patterns missing required pattern: {required_pattern}"
            ));
        }
    }

    let strict_replacement = contract
        .pointer("/claim_gate_policy/strict_replacement_requires")
        .and_then(Value::as_object);
    let Some(strict_replacement) = strict_replacement else {
        errors.push("claim_gate_policy.strict_replacement_requires must be an object".to_string());
        return Err(errors.join("; "));
    };

    let strict_overall_verdict = strict_replacement
        .get("overall_verdict")
        .and_then(Value::as_str)
        .unwrap_or("");
    if strict_overall_verdict != "CERTIFIED" {
        errors.push(format!(
            "claim_gate_policy.strict_replacement_requires.overall_verdict must be CERTIFIED, found {strict_overall_verdict}"
        ));
    }

    let required_artifacts = strict_replacement
        .get("required_artifacts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let required_artifact_set = required_artifacts
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect::<HashSet<_>>();
    for required_artifact in FRANKEN_NODE_REQUIRED_ARTIFACTS {
        if !required_artifact_set.contains(*required_artifact) {
            errors.push(format!(
                "strict_replacement_requires.required_artifacts missing {required_artifact}"
            ));
        }
    }

    let overclaim_blockers = collect_non_empty_string_array(
        contract,
        "/claim_gate_policy/overclaim_blockers",
        "claim_gate_policy.overclaim_blockers",
        &mut errors,
    );
    let overclaim_blocker_set = overclaim_blockers
        .iter()
        .map(|entry| entry.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    for required_blocker in FRANKEN_NODE_REQUIRED_OVERCLAIM_BLOCKERS {
        if !overclaim_blocker_set.contains(&required_blocker.to_ascii_lowercase()) {
            errors.push(format!(
                "claim_gate_policy.overclaim_blockers missing {required_blocker}"
            ));
        }
    }

    let structured_logging_fields = collect_non_empty_string_array(
        contract,
        "/structured_logging_contract/required_fields",
        "structured_logging_contract.required_fields",
        &mut errors,
    );
    let structured_logging_field_set = structured_logging_fields
        .iter()
        .map(|entry| entry.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    for required_field in FRANKEN_NODE_REQUIRED_LOG_FIELDS {
        if !structured_logging_field_set.contains(&required_field.to_ascii_lowercase()) {
            errors.push(format!(
                "structured_logging_contract.required_fields missing {required_field}"
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn find_latest_phase1_matrix_validation(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    for relative in [
        "tests/perf/reports/phase1_matrix_validation.json",
        "tests/perf/runs/results/phase1_matrix_validation.json",
    ] {
        let candidate = root.join(relative);
        if candidate.is_file() {
            candidates.push(candidate);
        }
    }

    let e2e_results_dir = root.join("tests/e2e_results");
    if let Ok(entries) = std::fs::read_dir(e2e_results_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("results/phase1_matrix_validation.json");
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }

    candidates.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    candidates.pop()
}

fn require_phase1_matrix_validation() -> (String, Value) {
    let root = repo_root();
    let path = find_latest_phase1_matrix_validation(&root).unwrap_or_else(|| {
        panic!(
            "release gate BLOCKED: missing phase1_matrix_validation.json evidence artifact; \
             expected at tests/perf/reports or tests/e2e_results/*/results"
        )
    });
    let display_path = path.strip_prefix(&root).map_or_else(
        |_| path.display().to_string(),
        |rel| rel.display().to_string(),
    );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {display_path}: {err}"));
    let json = parse_release_json(text.as_bytes())
        .unwrap_or_else(|err| panic!("{display_path} is not valid JSON: {err}"));
    (display_path, json)
}

fn find_latest_parameter_sweeps(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    for relative in [
        "tests/perf/reports/parameter_sweeps.json",
        "tests/perf/runs/results/parameter_sweeps.json",
    ] {
        let candidate = root.join(relative);
        if candidate.is_file() {
            candidates.push(candidate);
        }
    }

    let e2e_results_dir = root.join("tests/e2e_results");
    if let Ok(entries) = std::fs::read_dir(e2e_results_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("results/parameter_sweeps.json");
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }

    candidates.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    candidates.pop()
}

fn find_latest_opportunity_matrix(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    for relative in [
        "tests/perf/reports/opportunity_matrix.json",
        "tests/perf/runs/results/opportunity_matrix.json",
    ] {
        let candidate = root.join(relative);
        if candidate.is_file() {
            candidates.push(candidate);
        }
    }

    let e2e_results_dir = root.join("tests/e2e_results");
    if let Ok(entries) = std::fs::read_dir(e2e_results_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("results/opportunity_matrix.json");
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }

    candidates.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    candidates.pop()
}

fn require_parameter_sweeps() -> (String, Value) {
    let root = repo_root();
    let path = find_latest_parameter_sweeps(&root).unwrap_or_else(|| {
        panic!(
            "release gate BLOCKED: missing parameter_sweeps.json evidence artifact; \
             expected at tests/perf/reports or tests/e2e_results/*/results"
        )
    });
    let display_path = path.strip_prefix(&root).map_or_else(
        |_| path.display().to_string(),
        |rel| rel.display().to_string(),
    );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {display_path}: {err}"));
    let json = parse_release_json(text.as_bytes())
        .unwrap_or_else(|err| panic!("{display_path} is not valid JSON: {err}"));
    (display_path, json)
}

fn require_opportunity_matrix() -> (String, Value) {
    let root = repo_root();
    let path = find_latest_opportunity_matrix(&root).unwrap_or_else(|| {
        panic!(
            "release gate BLOCKED: missing opportunity_matrix.json evidence artifact; \
             expected at tests/perf/reports or tests/e2e_results/*/results"
        )
    });
    let display_path = path.strip_prefix(&root).map_or_else(
        |_| path.display().to_string(),
        |rel| rel.display().to_string(),
    );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {display_path}: {err}"));
    let json = parse_release_json(text.as_bytes())
        .unwrap_or_else(|err| panic!("{display_path} is not valid JSON: {err}"));
    (display_path, json)
}

// ============================================================================
// Evidence bundle existence checks
// ============================================================================

const REQUIRED_ARTIFACTS: &[(&str, &str)] = &[
    (
        "tests/ext_conformance/reports/conformance_summary.json",
        "Extension conformance summary",
    ),
    (
        "tests/ext_conformance/reports/conformance_baseline.json",
        "Conformance baseline with thresholds",
    ),
    (
        "tests/perf/reports/budget_summary.json",
        "Performance budget summary",
    ),
    (
        "tests/ext_conformance/artifacts/RISK_REVIEW.json",
        "Security and licensing risk review",
    ),
    (
        "tests/ext_conformance/artifacts/PROVENANCE_VERIFICATION.json",
        "Extension provenance verification",
    ),
    (
        "docs/traceability_matrix.json",
        "Requirement-to-test traceability matrix",
    ),
];

#[test]
fn all_required_evidence_artifacts_exist() {
    let root = repo_root();
    let mut missing = Vec::new();

    for (path, label) in REQUIRED_ARTIFACTS {
        if !root.join(path).is_file() {
            missing.push(format!("  - {label}: {path}"));
        }
    }

    assert!(
        missing.is_empty(),
        "release gate BLOCKED: missing evidence artifacts:\n{}",
        missing.join("\n")
    );
}

#[test]
fn all_evidence_artifacts_are_valid_json() {
    for (path, label) in REQUIRED_ARTIFACTS {
        let v = load_json(path);
        assert!(
            v.is_some(),
            "evidence artifact is not valid JSON: {label} ({path})"
        );
    }
}

#[test]
fn agent_release_profile_guidance_matches_cargo_and_readme() {
    let cargo_text = require_text("Cargo.toml");
    let cargo = cargo_text.parse::<toml::Table>();
    assert!(
        cargo.is_ok(),
        "Cargo.toml must parse as TOML: {:?}",
        cargo.err()
    );
    let Ok(cargo) = cargo else {
        return;
    };

    let release = cargo
        .get("profile")
        .and_then(toml::Value::as_table)
        .and_then(|profiles| profiles.get("release"))
        .and_then(toml::Value::as_table);
    assert!(
        release.is_some(),
        "Cargo.toml must define [profile.release]"
    );
    let Some(release) = release else {
        return;
    };

    let opt_level = release
        .get("opt-level")
        .and_then(toml::Value::as_str)
        .unwrap_or("");
    assert_eq!(
        opt_level, "z",
        "shipping release profile must stay size-budgeted"
    );
    assert_eq!(
        release.get("lto").and_then(toml::Value::as_bool),
        Some(true),
        "release profile must keep LTO enabled"
    );
    assert_eq!(
        release
            .get("codegen-units")
            .and_then(toml::Value::as_integer),
        Some(1),
        "release profile must keep single-codegen-unit optimization"
    );
    assert_eq!(
        release.get("panic").and_then(toml::Value::as_str),
        Some("abort"),
        "release profile must keep panic=abort"
    );
    assert_eq!(
        release.get("strip").and_then(toml::Value::as_bool),
        Some(true),
        "release profile must keep symbol stripping enabled"
    );

    let agents = require_text("AGENTS.md");
    let readme = require_text("README.md");
    let release_profile_tokens = [
        "[profile.release]",
        "opt-level = \"z\"",
        "lto = true",
        "codegen-units = 1",
        "panic = \"abort\"",
        "strip = true",
    ];

    for token in release_profile_tokens {
        assert!(
            agents.contains(token),
            "AGENTS.md release profile guidance missing Cargo.toml token: {token}"
        );
        assert!(
            readme.contains(token),
            "README.md release profile guidance missing Cargo.toml token: {token}"
        );
    }

    assert!(
        agents.contains("jemalloc is opt-in via `--features jemalloc`"),
        "AGENTS.md must describe jemalloc as opt-in"
    );
    assert!(
        readme.contains("opt-in jemalloc benchmark variants"),
        "README.md must describe jemalloc benchmark variants as opt-in"
    );
    assert!(
        !agents.contains("jemalloc is enabled by default"),
        "AGENTS.md must not describe jemalloc as enabled by default"
    );
    assert!(
        agents.contains("<22 MiB") && readme.contains("22.0 MiB"),
        "AGENTS.md and README.md must agree on the release binary size budget"
    );
}

#[test]
fn phase1_matrix_validation_artifact_is_present_and_parseable() {
    let (artifact, matrix) = require_phase1_matrix_validation();
    let schema = matrix.get("schema").and_then(Value::as_str).unwrap_or("");
    assert_eq!(
        schema, "pi.perf.phase1_matrix_validation.v1",
        "phase1 matrix schema mismatch in {artifact}"
    );
}

#[test]
fn parameter_sweeps_artifact_is_present_and_parseable() {
    let (_, matrix) = require_phase1_matrix_validation();
    let consumption_contract = require_consumption_contract(&matrix, "phase1_matrix_validation");
    let sweeps_present = find_latest_parameter_sweeps(&repo_root()).is_some();
    if !requires_strict_parameter_sweeps_contract(consumption_contract, sweeps_present) {
        assert_orchestrate_parameter_sweeps_contract_tokens();
        return;
    }

    let (artifact, sweeps) = require_parameter_sweeps();
    let schema = sweeps.get("schema").and_then(Value::as_str).unwrap_or("");
    assert_eq!(
        schema, "pi.perf.parameter_sweeps.v1",
        "parameter sweeps schema mismatch in {artifact}"
    );
}

#[test]
fn opportunity_matrix_artifact_is_present_and_parseable() {
    let (_, matrix) = require_phase1_matrix_validation();
    let consumption_contract = require_consumption_contract(&matrix, "phase1_matrix_validation");
    let opportunity_present = find_latest_opportunity_matrix(&repo_root()).is_some();
    if !requires_strict_opportunity_matrix_contract(consumption_contract, opportunity_present) {
        assert_orchestrate_opportunity_matrix_contract_tokens();
        return;
    }

    let (artifact, opportunity) = require_opportunity_matrix();
    let schema = opportunity
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        schema, "pi.perf.opportunity_matrix.v1",
        "opportunity matrix schema mismatch in {artifact}"
    );
}

// ============================================================================
// Schema validation
// ============================================================================

#[test]
fn conformance_summary_has_required_fields() {
    let sm = require_json("tests/ext_conformance/reports/conformance_summary.json");

    assert!(sm.get("schema").is_some(), "missing schema field");
    let run_id = sm
        .get("run_id")
        .and_then(Value::as_str)
        .map_or("", str::trim);
    assert!(
        !run_id.is_empty(),
        "missing or empty run_id in conformance_summary.json"
    );
    let correlation_id = sm
        .get("correlation_id")
        .and_then(Value::as_str)
        .map_or("", str::trim);
    assert!(
        !correlation_id.is_empty(),
        "missing or empty correlation_id in conformance_summary.json"
    );
    assert!(sm.get("counts").is_some(), "missing counts field");
    assert!(sm.get("pass_rate_pct").is_some(), "missing pass_rate_pct");
    assert!(sm.get("per_tier").is_some(), "missing per_tier");
    assert!(sm.get("evidence").is_some(), "missing evidence");

    let counts = sm.get("counts").unwrap();
    assert!(counts.get("pass").is_some(), "missing counts.pass");
    assert!(counts.get("fail").is_some(), "missing counts.fail");
    assert!(counts.get("total").is_some(), "missing counts.total");
}

#[test]
fn baseline_has_required_fields() {
    let bl = require_json("tests/ext_conformance/reports/conformance_baseline.json");

    assert!(bl.get("schema").is_some(), "missing schema");
    assert!(
        bl.get("extension_conformance").is_some(),
        "missing extension_conformance"
    );
    assert!(
        bl.get("regression_thresholds").is_some(),
        "missing regression_thresholds"
    );
    assert!(
        bl.get("exception_policy").is_some(),
        "missing exception_policy"
    );
}

#[test]
fn traceability_matrix_has_requirements() {
    let tm = require_json("docs/traceability_matrix.json");

    let reqs = tm
        .get("requirements")
        .and_then(Value::as_array)
        .expect("traceability matrix must have requirements array");

    assert!(
        !reqs.is_empty(),
        "traceability matrix must have at least one requirement"
    );

    for req in reqs {
        assert!(req.get("id").is_some(), "requirement missing id field");
        assert!(
            req.get("unit_tests").is_some(),
            "requirement {:?} missing unit_tests",
            req.get("id")
        );
    }
}

fn require_consumption_contract<'a>(matrix: &'a Value, artifact: &str) -> &'a Map<String, Value> {
    matrix
        .pointer("/consumption_contract")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("consumption_contract must be an object in {artifact}"))
}

fn assert_consumption_contract_downstream_beads(
    consumption_contract: &Map<String, Value>,
    artifact: &str,
) {
    let downstream_beads = consumption_contract
        .get("downstream_beads")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!("consumption_contract.downstream_beads must be an array in {artifact}")
        });
    let downstream_bead_set: HashSet<&str> =
        downstream_beads.iter().filter_map(Value::as_str).collect();
    for bead_id in ["bd-3ar8v.6.1", "bd-3ar8v.6.2"] {
        assert!(
            downstream_bead_set.contains(bead_id),
            "consumption_contract.downstream_beads missing {bead_id} in {artifact}"
        );
    }
}

fn requires_strict_weighted_contract(
    consumption_contract: &Map<String, Value>,
    matrix: &Value,
) -> bool {
    let artifact_ready_for_phase5 = consumption_contract
        .get("artifact_ready_for_phase5")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let weighted_present = matrix
        .get("weighted_bottleneck_attribution")
        .and_then(Value::as_object)
        .is_some();
    artifact_ready_for_phase5 || weighted_present
}

fn requires_strict_parameter_sweeps_contract(
    consumption_contract: &Map<String, Value>,
    sweeps_present: bool,
) -> bool {
    let artifact_ready_for_phase5 = consumption_contract
        .get("artifact_ready_for_phase5")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    artifact_ready_for_phase5 || sweeps_present
}

fn requires_strict_opportunity_matrix_contract(
    consumption_contract: &Map<String, Value>,
    opportunity_present: bool,
) -> bool {
    let artifact_ready_for_phase5 = consumption_contract
        .get("artifact_ready_for_phase5")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    artifact_ready_for_phase5 || opportunity_present
}

fn assert_orchestrate_weighted_contract_tokens(artifact: &str) {
    let orchestrate = std::fs::read_to_string(repo_root().join("scripts/perf/orchestrate.sh"))
        .expect("scripts/perf/orchestrate.sh should be readable");
    for token in [
        "\"weighted_bottleneck_attribution\"",
        "\"pi.perf.phase1_weighted_bottleneck_attribution.v1\"",
        "weighted_bottleneck_attribution.global_ranking",
        "weighted_bottleneck_attribution.per_scale",
    ] {
        assert!(
            orchestrate.contains(token),
            "orchestrate contract token missing while weighted attribution artifact is absent in {artifact}: {token}"
        );
    }
}

fn assert_orchestrate_parameter_sweeps_contract_tokens() {
    let orchestrate = std::fs::read_to_string(repo_root().join("scripts/perf/orchestrate.sh"))
        .expect("scripts/perf/orchestrate.sh should be readable");
    for token in [
        "parameter_sweeps.json",
        "\"pi.perf.parameter_sweeps.v1\"",
        "\"parameter_sweeps\": \"pi.perf.parameter_sweeps.v1\"",
        "phase1_matrix_validation.weighted_bottleneck_attribution",
        "weighted_bottleneck_guided_grid",
        "manifest[\"parameter_sweeps\"]",
    ] {
        assert!(
            orchestrate.contains(token),
            "orchestrate contract token missing for parameter_sweeps artifact: {token}"
        );
    }
}

fn assert_orchestrate_opportunity_matrix_contract_tokens() {
    let orchestrate = std::fs::read_to_string(repo_root().join("scripts/perf/orchestrate.sh"))
        .expect("scripts/perf/orchestrate.sh should be readable");
    for token in [
        "\"opportunity_matrix\"",
        "\"pi.perf.opportunity_matrix.v1\"",
        "\"generated_at\"",
        "\"source_identity\"",
        "\"readiness\"",
        "\"decision\"",
        "\"NO_DECISION\"",
        "\"ranked_opportunities\"",
        "\"fail_closed_conditions\"",
        "decision = \"RANKED\" if readiness_ok else \"NO_DECISION\"",
        "weighted_bottleneck_attribution.global_ranking",
        "\"bd-3ar8v.6.1\"",
    ] {
        assert!(
            orchestrate.contains(token),
            "orchestrate contract token missing for opportunity_matrix artifact: {token}"
        );
    }
}

fn require_weighted_attribution<'a>(matrix: &'a Value, artifact: &str) -> &'a Map<String, Value> {
    matrix
        .get("weighted_bottleneck_attribution")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!("phase1 matrix missing weighted_bottleneck_attribution object in {artifact}")
        })
}

fn assert_weighted_schema_and_status<'a>(
    weighted: &'a Map<String, Value>,
    artifact: &str,
) -> &'a str {
    let weighted_schema = weighted.get("schema").and_then(Value::as_str).unwrap_or("");
    assert_eq!(
        weighted_schema, "pi.perf.phase1_weighted_bottleneck_attribution.v1",
        "weighted attribution schema mismatch in {artifact}"
    );

    let status = weighted.get("status").and_then(Value::as_str).unwrap_or("");
    assert!(
        matches!(status, "computed" | "missing"),
        "weighted attribution status must be computed|missing in {artifact}, got {status:?}"
    );
    status
}

fn assert_weighted_payload_shape(weighted: &Map<String, Value>, status: &str, artifact: &str) {
    let per_scale = weighted
        .get("per_scale")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("weighted attribution per_scale must be an array in {artifact}"));
    let global_ranking = weighted
        .get("global_ranking")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!("weighted attribution global_ranking must be an array in {artifact}")
        });

    if status != "computed" {
        return;
    }

    assert!(
        !per_scale.is_empty(),
        "weighted attribution per_scale must be non-empty when status=computed in {artifact}"
    );
    assert!(
        !global_ranking.is_empty(),
        "weighted attribution global_ranking must be non-empty when status=computed in {artifact}"
    );

    let observed_stages: HashSet<&str> = global_ranking
        .iter()
        .filter_map(|row| row.get("stage").and_then(Value::as_str))
        .collect();
    let expected_stages: HashSet<&str> = ["open_ms", "append_ms", "save_ms", "index_ms"]
        .iter()
        .copied()
        .collect();
    assert_eq!(
        observed_stages, expected_stages,
        "weighted attribution global_ranking stages mismatch in {artifact}"
    );
}

fn assert_phase5_downstream_consumers(matrix: &Value, artifact: &str) {
    let downstream_consumers = matrix
        .pointer("/consumption_contract/downstream_consumers")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!("consumption_contract.downstream_consumers must be an object in {artifact}")
        });

    for (consumer, bead_id, selector) in [
        (
            "opportunity_matrix",
            "bd-3ar8v.6.1",
            "weighted_bottleneck_attribution.global_ranking",
        ),
        (
            "parameter_sweeps",
            "bd-3ar8v.6.2",
            "weighted_bottleneck_attribution.per_scale",
        ),
    ] {
        let entry = downstream_consumers
            .get(consumer)
            .and_then(Value::as_object)
            .unwrap_or_else(|| {
                panic!("consumption_contract.downstream_consumers.{consumer} missing in {artifact}")
            });

        let observed_bead = entry.get("bead_id").and_then(Value::as_str).unwrap_or("");
        assert_eq!(
            observed_bead, bead_id,
            "downstream consumer bead mismatch for {consumer} in {artifact}"
        );

        let observed_selector = entry.get("selector").and_then(Value::as_str).unwrap_or("");
        assert_eq!(
            observed_selector, selector,
            "downstream consumer selector mismatch for {consumer} in {artifact}"
        );

        let source_artifact = entry
            .get("source_artifact")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(
            source_artifact, "phase1_matrix_validation",
            "downstream consumer source_artifact mismatch for {consumer} in {artifact}"
        );
    }
}

fn parse_positive_u64(raw: Option<&Value>) -> Option<u64> {
    match raw {
        Some(Value::Number(value)) => value.as_u64().filter(|parsed| *parsed > 0),
        Some(Value::String(value)) => value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|parsed| *parsed > 0),
        _ => None,
    }
}

#[test]
fn phase1_weighted_attribution_contract_links_phase5_consumers() {
    let (artifact, matrix) = require_phase1_matrix_validation();
    let consumption_contract = require_consumption_contract(&matrix, &artifact);

    assert_consumption_contract_downstream_beads(consumption_contract, &artifact);

    if !requires_strict_weighted_contract(consumption_contract, &matrix) {
        assert_orchestrate_weighted_contract_tokens(&artifact);
        return;
    }

    let weighted = require_weighted_attribution(&matrix, &artifact);
    let status = assert_weighted_schema_and_status(weighted, &artifact);
    assert_weighted_payload_shape(weighted, status, &artifact);
    assert_phase5_downstream_consumers(&matrix, &artifact);
}

#[test]
fn opportunity_matrix_contract_links_phase1_matrix_and_readiness() {
    let (phase1_artifact, phase1_matrix) = require_phase1_matrix_validation();
    let consumption_contract = require_consumption_contract(&phase1_matrix, &phase1_artifact);
    let opportunity_present = find_latest_opportunity_matrix(&repo_root()).is_some();
    if !requires_strict_opportunity_matrix_contract(consumption_contract, opportunity_present) {
        assert_orchestrate_opportunity_matrix_contract_tokens();
        return;
    }

    let (artifact, opportunity) = require_opportunity_matrix();

    let source_identity = opportunity
        .pointer("/source_identity")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!("opportunity_matrix.source_identity must be an object in {artifact}")
        });
    let source_artifact = source_identity
        .get("source_artifact")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        source_artifact, "phase1_matrix_validation",
        "opportunity_matrix.source_identity.source_artifact mismatch in {artifact}"
    );
    let source_artifact_path = source_identity
        .get("source_artifact_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !source_artifact_path.is_empty(),
        "opportunity_matrix.source_identity.source_artifact_path must be non-empty in {artifact}"
    );
    let normalized_source_path = source_artifact_path.replace('\\', "/");
    assert!(
        normalized_source_path.ends_with("phase1_matrix_validation.json"),
        "opportunity_matrix.source_identity.source_artifact_path must reference phase1_matrix_validation.json in {artifact}"
    );
    let normalized_phase1_artifact = phase1_artifact.replace('\\', "/");
    assert!(
        normalized_source_path.ends_with(&normalized_phase1_artifact)
            || normalized_phase1_artifact.ends_with("phase1_matrix_validation.json"),
        "opportunity_matrix source artifact path must align with discovered phase1 artifact: source={source_artifact_path:?}, phase1={phase1_artifact:?}"
    );

    let opportunity_correlation = opportunity
        .get("correlation_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let phase1_correlation = phase1_matrix
        .get("correlation_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !opportunity_correlation.is_empty() && !phase1_correlation.is_empty(),
        "opportunity_matrix/phase1 correlation_id must be non-empty in {artifact} and {phase1_artifact}"
    );
    assert_eq!(
        opportunity_correlation, phase1_correlation,
        "opportunity_matrix correlation_id must match phase1 matrix correlation_id ({artifact} vs {phase1_artifact})"
    );

    let readiness = opportunity
        .pointer("/readiness")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("opportunity_matrix.readiness must be an object in {artifact}"));
    let readiness_status = readiness
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        matches!(readiness_status, "ready" | "blocked" | "no_decision"),
        "opportunity_matrix.readiness.status must be ready|blocked|no_decision in {artifact}, got {readiness_status:?}"
    );
    let readiness_decision = readiness
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        matches!(readiness_decision, "RANKED" | "NO_DECISION"),
        "opportunity_matrix.readiness.decision must be RANKED|NO_DECISION in {artifact}, got {readiness_decision:?}"
    );
    let readiness_mode = readiness.get("mode").and_then(Value::as_str).unwrap_or("");
    assert_eq!(
        readiness_mode, "fail_closed",
        "opportunity_matrix.readiness.mode must be fail_closed in {artifact}"
    );
    let ready_for_phase5 = readiness.get("ready_for_phase5").and_then(Value::as_bool);
    assert!(
        ready_for_phase5.is_some(),
        "opportunity_matrix.readiness.ready_for_phase5 must be a boolean in {artifact}"
    );
    let ranked_opportunities = opportunity
        .pointer("/ranked_opportunities")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!("opportunity_matrix.ranked_opportunities must be an array in {artifact}")
        });
    let phase1_ready = consumption_contract
        .get("artifact_ready_for_phase5")
        .and_then(Value::as_bool);
    if let Some(phase1_ready) = phase1_ready {
        assert_eq!(
            ready_for_phase5,
            Some(phase1_ready),
            "opportunity_matrix.readiness.ready_for_phase5 must match phase1 consumption_contract.artifact_ready_for_phase5 ({artifact} vs {phase1_artifact})"
        );
    }
    match readiness_status {
        "ready" => {
            assert_eq!(
                ready_for_phase5,
                Some(true),
                "opportunity_matrix.readiness.ready_for_phase5 must be true when status=ready in {artifact}"
            );
            assert_eq!(
                readiness_decision, "RANKED",
                "opportunity_matrix.readiness.decision must be RANKED when status=ready in {artifact}"
            );
            assert!(
                !ranked_opportunities.is_empty(),
                "opportunity_matrix.ranked_opportunities must be non-empty when readiness.status=ready in {artifact}"
            );
            for (index, row) in ranked_opportunities.iter().enumerate() {
                let row_obj = row.as_object().unwrap_or_else(|| {
                    panic!(
                        "opportunity_matrix.ranked_opportunities[{index}] must be an object in {artifact}"
                    )
                });
                let rank = parse_positive_u64(row_obj.get("rank")).unwrap_or_else(|| {
                    panic!(
                        "opportunity_matrix.ranked_opportunities[{index}].rank must be a positive integer in {artifact}"
                    )
                });
                assert_eq!(
                    rank,
                    (index + 1) as u64,
                    "opportunity_matrix.ranked_opportunities[{index}].rank must equal index+1 in {artifact}"
                );
                let stage = row_obj
                    .get("stage")
                    .and_then(Value::as_str)
                    .map_or("", str::trim);
                assert!(
                    !stage.is_empty(),
                    "opportunity_matrix.ranked_opportunities[{index}].stage must be non-empty in {artifact}"
                );

                let weighted_contribution_pct = row_obj
                    .get("weighted_contribution_pct")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN);
                assert!(
                    weighted_contribution_pct.is_finite() && weighted_contribution_pct >= 0.0,
                    "opportunity_matrix.ranked_opportunities[{index}].weighted_contribution_pct must be non-negative numeric in {artifact}"
                );
                let expected_gain_pct = row_obj
                    .get("expected_gain_pct")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN);
                assert!(
                    expected_gain_pct.is_finite() && expected_gain_pct >= 0.0,
                    "opportunity_matrix.ranked_opportunities[{index}].expected_gain_pct must be non-negative numeric in {artifact}"
                );
                let priority_score = row_obj
                    .get("priority_score")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN);
                assert!(
                    priority_score.is_finite() && priority_score > 0.0,
                    "opportunity_matrix.ranked_opportunities[{index}].priority_score must be positive numeric in {artifact}"
                );

                let confidence = row_obj
                    .get("confidence")
                    .and_then(Value::as_object)
                    .unwrap_or_else(|| {
                        panic!(
                            "opportunity_matrix.ranked_opportunities[{index}].confidence must be an object in {artifact}"
                        )
                    });
                let confidence_level = confidence
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                assert!(
                    matches!(confidence_level, "low" | "medium" | "high"),
                    "opportunity_matrix.ranked_opportunities[{index}].confidence.level must be low|medium|high in {artifact}, got {confidence_level:?}"
                );
                let confidence_score = confidence
                    .get("score")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN);
                assert!(
                    confidence_score.is_finite() && (0.0..=1.0).contains(&confidence_score),
                    "opportunity_matrix.ranked_opportunities[{index}].confidence.score must be within [0,1] in {artifact}"
                );
                let confidence_sufficient = confidence
                    .get("sufficient_for_decision")
                    .and_then(Value::as_bool);
                assert!(
                    confidence_sufficient.is_some(),
                    "opportunity_matrix.ranked_opportunities[{index}].confidence.sufficient_for_decision must be a boolean in {artifact}"
                );

                let user_impact = row_obj
                    .get("user_impact")
                    .and_then(Value::as_object)
                    .unwrap_or_else(|| {
                        panic!(
                            "opportunity_matrix.ranked_opportunities[{index}].user_impact must be an object in {artifact}"
                        )
                    });
                for field in ["resume_latency", "extension_responsiveness", "failure_risk"] {
                    let value = user_impact
                        .get(field)
                        .and_then(Value::as_str)
                        .map_or("", str::trim);
                    assert!(
                        !value.is_empty(),
                        "opportunity_matrix.ranked_opportunities[{index}].user_impact.{field} must be non-empty in {artifact}"
                    );
                }
            }
        }
        "blocked" => {
            assert_eq!(
                ready_for_phase5,
                Some(false),
                "opportunity_matrix.readiness.ready_for_phase5 must be false when status=blocked in {artifact}"
            );
            assert_eq!(
                readiness_decision, "NO_DECISION",
                "opportunity_matrix.readiness.decision must be NO_DECISION when status=blocked in {artifact}"
            );
            let blocking_reasons = readiness
                .get("blocking_reasons")
                .and_then(Value::as_array)
                .unwrap_or_else(|| {
                    panic!(
                        "opportunity_matrix.readiness.blocking_reasons must be an array when status=blocked in {artifact}"
                    )
                });
            assert!(
                !blocking_reasons.is_empty(),
                "opportunity_matrix.readiness.blocking_reasons must be non-empty when status=blocked in {artifact}"
            );
            assert!(
                ranked_opportunities.is_empty(),
                "opportunity_matrix.ranked_opportunities must be empty when readiness.status=blocked in {artifact}"
            );
        }
        "no_decision" => {
            assert_eq!(
                ready_for_phase5,
                Some(false),
                "opportunity_matrix.readiness.ready_for_phase5 must be false when status=no_decision in {artifact}"
            );
            assert_eq!(
                readiness_decision, "NO_DECISION",
                "opportunity_matrix.readiness.decision must be NO_DECISION when status=no_decision in {artifact}"
            );
            let no_decision_reasons = readiness
                .get("no_decision_reasons")
                .and_then(Value::as_array)
                .or_else(|| readiness.get("blocking_reasons").and_then(Value::as_array))
                .unwrap_or_else(|| {
                    panic!(
                        "opportunity_matrix.readiness.no_decision_reasons|blocking_reasons must be an array when status=no_decision in {artifact}"
                    )
                });
            assert!(
                !no_decision_reasons.is_empty(),
                "opportunity_matrix.readiness.no_decision_reasons must be non-empty when status=no_decision in {artifact}"
            );
            assert!(
                ranked_opportunities.is_empty(),
                "opportunity_matrix.ranked_opportunities must be empty when readiness.status=no_decision in {artifact}"
            );
        }
        _ => panic!(),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn parameter_sweeps_contract_links_phase1_matrix_and_readiness() {
    let (phase1_artifact, phase1_matrix) = require_phase1_matrix_validation();
    let consumption_contract = require_consumption_contract(&phase1_matrix, &phase1_artifact);
    let sweeps_present = find_latest_parameter_sweeps(&repo_root()).is_some();
    if !requires_strict_parameter_sweeps_contract(consumption_contract, sweeps_present) {
        assert_orchestrate_parameter_sweeps_contract_tokens();
        return;
    }

    let (artifact, sweeps) = require_parameter_sweeps();

    let source_identity = sweeps
        .pointer("/source_identity")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!("parameter_sweeps.source_identity must be an object in {artifact}")
        });

    let source_artifact = source_identity
        .get("source_artifact")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        source_artifact, "phase1_matrix_validation",
        "parameter_sweeps.source_identity.source_artifact mismatch in {artifact}"
    );

    let source_artifact_path = source_identity
        .get("source_artifact_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !source_artifact_path.is_empty(),
        "parameter_sweeps.source_identity.source_artifact_path must be non-empty in {artifact}"
    );
    let normalized_source_path = source_artifact_path.replace('\\', "/");
    assert!(
        normalized_source_path.ends_with("phase1_matrix_validation.json"),
        "parameter_sweeps.source_identity.source_artifact_path must reference phase1_matrix_validation.json in {artifact}"
    );
    let normalized_phase1_artifact = phase1_artifact.replace('\\', "/");
    assert!(
        normalized_source_path.ends_with(&normalized_phase1_artifact)
            || normalized_phase1_artifact.ends_with("phase1_matrix_validation.json"),
        "parameter_sweeps source artifact path must align with discovered phase1 artifact: source={source_artifact_path:?}, phase1={phase1_artifact:?}"
    );

    let weighted_schema = source_identity
        .get("weighted_bottleneck_schema")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        weighted_schema, "pi.perf.phase1_weighted_bottleneck_attribution.v1",
        "parameter_sweeps.source_identity.weighted_bottleneck_schema mismatch in {artifact}"
    );

    let weighted_status = source_identity
        .get("weighted_bottleneck_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        matches!(weighted_status, "computed" | "missing"),
        "parameter_sweeps.source_identity.weighted_bottleneck_status must be computed|missing in {artifact}, got {weighted_status:?}"
    );

    let sweeps_correlation = sweeps
        .get("correlation_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let phase1_correlation = phase1_matrix
        .get("correlation_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !sweeps_correlation.is_empty() && !phase1_correlation.is_empty(),
        "parameter_sweeps/phase1 correlation_id must be non-empty in {artifact} and {phase1_artifact}"
    );
    assert_eq!(
        sweeps_correlation, phase1_correlation,
        "parameter_sweeps correlation_id must match phase1 matrix correlation_id ({artifact} vs {phase1_artifact})"
    );

    let readiness = sweeps
        .pointer("/readiness")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("parameter_sweeps.readiness must be an object in {artifact}"));
    let readiness_status = readiness
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let ready_for_phase5 = readiness.get("ready_for_phase5").and_then(Value::as_bool);
    let blocking_reasons = readiness
        .get("blocking_reasons")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!("parameter_sweeps.readiness.blocking_reasons must be an array in {artifact}")
        });

    assert!(
        matches!(readiness_status, "ready" | "blocked"),
        "parameter_sweeps.readiness.status must be ready|blocked in {artifact}, got {readiness_status:?}"
    );
    match readiness_status {
        "ready" => {
            assert_eq!(
                ready_for_phase5,
                Some(true),
                "parameter_sweeps.readiness.ready_for_phase5 must be true when status=ready in {artifact}"
            );
            assert!(
                blocking_reasons.is_empty(),
                "parameter_sweeps.readiness.blocking_reasons must be empty when status=ready in {artifact}"
            );
        }
        "blocked" => {
            assert_eq!(
                ready_for_phase5,
                Some(false),
                "parameter_sweeps.readiness.ready_for_phase5 must be false when status=blocked in {artifact}"
            );
            assert!(
                !blocking_reasons.is_empty(),
                "parameter_sweeps.readiness.blocking_reasons must be non-empty when status=blocked in {artifact}"
            );
        }
        _ => panic!(),
    }

    let phase1_ready = phase1_matrix
        .pointer("/consumption_contract/artifact_ready_for_phase5")
        .and_then(Value::as_bool);
    if let Some(phase1_ready) = phase1_ready {
        assert_eq!(
            ready_for_phase5,
            Some(phase1_ready),
            "parameter_sweeps.readiness.ready_for_phase5 must match phase1 consumption_contract.artifact_ready_for_phase5 ({artifact} vs {phase1_artifact})"
        );
    }

    let selected_defaults = sweeps
        .pointer("/selected_defaults")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!("parameter_sweeps.selected_defaults must be an object in {artifact}")
        });
    let mut selected_default_values = HashMap::new();
    for key in ["flush_cadence_ms", "queue_max_items", "compaction_quota_mb"] {
        let parsed = parse_positive_u64(selected_defaults.get(key)).unwrap_or_else(|| {
            panic!(
                "parameter_sweeps.selected_defaults.{key} must be a positive integer in {artifact}"
            )
        });
        selected_default_values.insert(key, parsed);
    }

    let dimensions = sweeps
        .pointer("/sweep_plan/dimensions")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!("parameter_sweeps.sweep_plan.dimensions must be an array in {artifact}")
        });
    let mut observed_dimension_names = HashSet::new();
    for (index, dimension) in dimensions.iter().enumerate() {
        let dimension_obj = dimension.as_object().unwrap_or_else(|| {
            panic!(
                "parameter_sweeps.sweep_plan.dimensions[{index}] must be an object in {artifact}"
            )
        });
        let name = dimension_obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        assert!(
            !name.is_empty(),
            "parameter_sweeps.sweep_plan.dimensions[{index}].name must be non-empty in {artifact}"
        );
        observed_dimension_names.insert(name.clone());
        let candidate_values = dimension_obj
            .get("candidate_values")
            .and_then(Value::as_array)
            .unwrap_or_else(|| {
                panic!("parameter_sweeps.sweep_plan.dimensions[{index}].candidate_values must be an array in {artifact}")
            });
        assert!(
            !candidate_values.is_empty(),
            "parameter_sweeps.sweep_plan.dimensions[{index}].candidate_values must be non-empty in {artifact}"
        );
        let parsed_candidates: HashSet<u64> = candidate_values
            .iter()
            .map(|candidate| {
                parse_positive_u64(Some(candidate)).unwrap_or_else(|| {
                    panic!(
                        "parameter_sweeps.sweep_plan.dimensions[{index}].candidate_values entries must be positive integers in {artifact}"
                    )
                })
            })
            .collect();
        if let Some(selected_default) = selected_default_values.get(name.as_str()) {
            assert!(
                parsed_candidates.contains(selected_default),
                "parameter_sweeps.selected_defaults.{name}={selected_default} must appear in sweep_plan.dimensions[{index}].candidate_values in {artifact}"
            );
        }
    }
    for required in ["flush_cadence_ms", "queue_max_items", "compaction_quota_mb"] {
        assert!(
            observed_dimension_names.contains(required),
            "parameter_sweeps.sweep_plan.dimensions missing required knob {required} in {artifact}"
        );
    }
}

// ============================================================================
// Threshold enforcement
// ============================================================================

/// Compute pass/(pass+fail), ignoring N/A extensions that lack evidence.
/// Matches the `effective_pass_rate_pct` logic in `conformance_regression_gate.rs`.
fn effective_pass_rate_pct(sm: &Value) -> f64 {
    let pass = sm
        .pointer("/counts/pass")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let fail = sm
        .pointer("/counts/fail")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = sm
        .pointer("/counts/total")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tested = pass + fail;
    let reported = sm
        .get("pass_rate_pct")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    if tested > 0 && tested < total {
        #[allow(clippy::cast_precision_loss)]
        {
            (pass as f64 / tested as f64) * 100.0
        }
    } else {
        reported
    }
}

#[test]
fn conformance_pass_rate_meets_release_threshold() {
    let sm = require_json("tests/ext_conformance/reports/conformance_summary.json");
    let bl = require_json("tests/ext_conformance/reports/conformance_baseline.json");

    let current_rate = effective_pass_rate_pct(&sm);
    let min_rate = bl
        .pointer("/regression_thresholds/overall_pass_rate_min_pct")
        .and_then(Value::as_f64)
        .unwrap_or(80.0);

    assert!(
        current_rate >= min_rate,
        "release gate BLOCKED: conformance pass rate {current_rate:.1}% \
         (effective: pass/(pass+fail), ignoring N/A) < minimum {min_rate:.1}%"
    );
}

#[test]
fn failure_count_within_release_threshold() {
    let sm = require_json("tests/ext_conformance/reports/conformance_summary.json");

    let fail = sm
        .pointer("/counts/fail")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let max_fail: u64 = 36;

    assert!(
        fail <= max_fail,
        "release gate BLOCKED: {fail} failures exceed maximum {max_fail}"
    );
}

const PERF_BUDGET_SUMMARY_SCHEMA: &str = "pi.perf.budget_summary.v2";
const PERF_CANONICAL_BUDGET_INVENTORY_SHA256: &str =
    "96e3147ef23e1c634d56265581975a2b619ac9a701f4839ef6f3f4b3987226ad";
const PERF_TOP_LEVEL_FIELDS: &[&str] = &[
    "schema",
    "generated_at",
    "source_commit",
    "run_id",
    "correlation_id",
    "strict_mode",
    "total_budgets",
    "ci_enforced",
    "ci_with_data",
    "ci_fail",
    "ci_no_data",
    "pass",
    "fail",
    "no_data",
    "data_contract_failures_count",
    "failing_data_contracts",
    "budgets",
    "budget_results",
    "claim_readiness",
];
const PERF_BUDGET_FIELDS: &[&str] = &[
    "name",
    "category",
    "metric",
    "unit",
    "threshold",
    "comparison",
    "methodology",
    "ci_enforced",
];
const PERF_RESULT_REQUIRED_FIELDS: &[&str] = &[
    "budget_name",
    "category",
    "threshold",
    "comparison",
    "unit",
    "actual",
    "status",
    "source",
    "ci_enforced",
];
const PERF_FAILURE_REQUIRED_FIELDS: &[&str] = &["contract_id", "detail", "remediation"];
const PERF_CLAIM_READINESS_FIELDS: &[&str] = &[
    "status",
    "performance_claims_authorized",
    "blocking_reason_codes",
];

#[derive(Debug)]
struct PerformanceClaimValidation {
    claim_ready: bool,
}

#[derive(Debug)]
struct PerformanceBudgetDefinition {
    category: String,
    unit: String,
    threshold: f64,
    comparison: String,
    ci_enforced: bool,
}

fn perf_exact_object<'a>(
    value: &'a Value,
    required: &[&str],
    optional: &[&str],
    label: &str,
) -> Result<&'a Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    let missing: Vec<_> = required
        .iter()
        .filter(|field| !object.contains_key(**field))
        .copied()
        .collect();
    let unexpected: Vec<_> = object
        .keys()
        .filter(|field| !required.contains(&field.as_str()) && !optional.contains(&field.as_str()))
        .cloned()
        .collect();
    if missing.is_empty() && unexpected.is_empty() {
        Ok(object)
    } else {
        Err(format!(
            "{label} fields are not exact (missing={missing:?}, unexpected={unexpected:?})"
        ))
    }
}

fn perf_nonempty_string<'a>(value: &'a Value, label: &str) -> Result<&'a str, String> {
    let raw = value
        .as_str()
        .ok_or_else(|| format!("{label} must be a string"))?;
    if raw.is_empty() || raw.trim() != raw {
        Err(format!(
            "{label} must be non-empty and free of surrounding whitespace"
        ))
    } else {
        Ok(raw)
    }
}

fn perf_uint(value: &Value, label: &str) -> Result<u64, String> {
    value
        .as_u64()
        .filter(|number| *number <= i64::MAX.unsigned_abs())
        .ok_or_else(|| format!("{label} must be a non-negative signed 64-bit integer"))
}

fn perf_finite_number(value: &Value, label: &str, positive: bool) -> Result<f64, String> {
    let number = value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| format!("{label} must be a finite number"))?;
    if positive && number <= 0.0 {
        Err(format!("{label} must be a positive finite number"))
    } else {
        Ok(number)
    }
}

fn perf_nullable_lineage<'a>(value: &'a Value, label: &str) -> Result<Option<&'a str>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let raw = perf_nonempty_string(value, label)?;
    let mut chars = raw.chars();
    let valid_start = chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric());
    let valid_rest =
        chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '/' | '-'));
    if valid_start && valid_rest && raw.len() <= 256 {
        Ok(Some(raw))
    } else {
        Err(format!("{label} must be a canonical lineage identifier"))
    }
}

fn perf_source_commit(value: &Value) -> Result<Option<&str>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let raw = perf_nonempty_string(value, "source_commit")?;
    if matches!(raw.len(), 40 | 64)
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(Some(raw))
    } else {
        Err("source_commit must be null or a canonical full lowercase Git object ID".to_string())
    }
}

fn perf_generated_at(value: &Value) -> Result<DateTime<Utc>, String> {
    let raw = perf_nonempty_string(value, "generated_at")?;
    let bytes = raw.as_bytes();
    let millisecond_utc_shape = bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if !millisecond_utc_shape {
        return Err(
            "generated_at must use canonical millisecond-precision UTC RFC3339".to_string(),
        );
    }
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map_err(|err| format!("generated_at is not valid RFC3339: {err}"))?;
    if parsed.offset().local_minus_utc() != 0 {
        return Err("generated_at must use UTC".to_string());
    }
    let utc = parsed.with_timezone(&Utc);
    if utc.to_rfc3339_opts(SecondsFormat::Millis, true) != raw {
        return Err(
            "generated_at must use canonical millisecond-precision UTC RFC3339".to_string(),
        );
    }
    Ok(utc)
}

fn perf_usize_as_u64(value: usize, label: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("derived {label} exceeds u64"))
}

fn perf_budget_inventory_sha256(budgets: &[Value]) -> Result<String, String> {
    let mut canonical = String::from("[");
    for (index, budget) in budgets.iter().enumerate() {
        let label = format!("budgets[{index}]");
        let object = budget
            .as_object()
            .ok_or_else(|| format!("{label} must be an object"))?;
        if index != 0 {
            canonical.push(',');
        }
        let name = serde_json::to_string(perf_nonempty_string(
            &object["name"],
            &format!("{label}.name"),
        )?)
        .map_err(|err| format!("failed to serialize {label}.name: {err}"))?;
        let category = serde_json::to_string(perf_nonempty_string(
            &object["category"],
            &format!("{label}.category"),
        )?)
        .map_err(|err| format!("failed to serialize {label}.category: {err}"))?;
        let metric = serde_json::to_string(perf_nonempty_string(
            &object["metric"],
            &format!("{label}.metric"),
        )?)
        .map_err(|err| format!("failed to serialize {label}.metric: {err}"))?;
        let unit = serde_json::to_string(perf_nonempty_string(
            &object["unit"],
            &format!("{label}.unit"),
        )?)
        .map_err(|err| format!("failed to serialize {label}.unit: {err}"))?;
        let threshold =
            perf_finite_number(&object["threshold"], &format!("{label}.threshold"), true)?;
        let rounded_threshold = (threshold * 1_000_000.0).round() / 1_000_000.0;
        if threshold.total_cmp(&rounded_threshold).is_ne() {
            return Err(format!(
                "{label}.threshold exceeds canonical six-decimal precision"
            ));
        }
        let comparison = serde_json::to_string(perf_nonempty_string(
            &object["comparison"],
            &format!("{label}.comparison"),
        )?)
        .map_err(|err| format!("failed to serialize {label}.comparison: {err}"))?;
        let ci_enforced = object["ci_enforced"]
            .as_bool()
            .ok_or_else(|| format!("{label}.ci_enforced must be a boolean"))?;
        let methodology = serde_json::to_string(perf_nonempty_string(
            &object["methodology"],
            &format!("{label}.methodology"),
        )?)
        .map_err(|err| format!("failed to serialize {label}.methodology: {err}"))?;
        write!(
            canonical,
            "{{\"name\":{name},\"category\":{category},\"metric\":{metric},\"unit\":{unit},\"threshold\":{threshold:.6},\"comparison\":{comparison},\"ci_enforced\":{ci_enforced},\"methodology\":{methodology}}}"
        )
        .map_err(|err| format!("failed to serialize canonical budget inventory: {err}"))?;
    }
    canonical.push(']');
    Ok(format!("{:x}", Sha256::digest(canonical.as_bytes())))
}

fn validate_performance_budget_summary(
    summary: &Value,
    now: DateTime<Utc>,
    maximum_age: Duration,
    source_binding_valid: bool,
) -> Result<PerformanceClaimValidation, String> {
    let top = perf_exact_object(summary, PERF_TOP_LEVEL_FIELDS, &[], "performance summary")?;
    if top.get("schema").and_then(Value::as_str) != Some(PERF_BUDGET_SUMMARY_SCHEMA) {
        return Err(format!(
            "schema must be {PERF_BUDGET_SUMMARY_SCHEMA}, found {:?}",
            top.get("schema")
        ));
    }

    let generated_at = perf_generated_at(&top["generated_at"])?;
    if generated_at > now + Duration::minutes(5) {
        return Err(
            "performance summary timestamp is more than five minutes in the future".to_string(),
        );
    }
    let source_commit = perf_source_commit(&top["source_commit"])?;
    if source_commit.is_some() && !source_binding_valid {
        return Err("asserted performance source_commit is not bound to release HEAD".to_string());
    }
    let run_id = perf_nullable_lineage(&top["run_id"], "run_id")?;
    let correlation_id = perf_nullable_lineage(&top["correlation_id"], "correlation_id")?;
    if run_id != correlation_id {
        return Err("run_id and correlation_id must both be null or match".to_string());
    }
    let strict_mode = top["strict_mode"]
        .as_bool()
        .ok_or_else(|| "strict_mode must be a boolean".to_string())?;

    let count_names = [
        "total_budgets",
        "ci_enforced",
        "ci_with_data",
        "ci_fail",
        "ci_no_data",
        "pass",
        "fail",
        "no_data",
        "data_contract_failures_count",
    ];
    let mut counts = HashMap::new();
    for name in count_names {
        counts.insert(name, perf_uint(&top[name], name)?);
    }

    let budgets = top["budgets"]
        .as_array()
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| "budgets must be a non-empty array".to_string())?;
    let results = top["budget_results"]
        .as_array()
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| "budget_results must be a non-empty array".to_string())?;
    let failures = top["failing_data_contracts"]
        .as_array()
        .ok_or_else(|| "failing_data_contracts must be an array".to_string())?;

    let mut definitions = HashMap::new();
    for (index, budget) in budgets.iter().enumerate() {
        let label = format!("budgets[{index}]");
        let object = perf_exact_object(budget, PERF_BUDGET_FIELDS, &[], &label)?;
        let name = perf_nonempty_string(&object["name"], &format!("{label}.name"))?;
        for field in ["category", "metric", "unit", "methodology"] {
            perf_nonempty_string(&object[field], &format!("{label}.{field}"))?;
        }
        let definition = PerformanceBudgetDefinition {
            category: perf_nonempty_string(&object["category"], &format!("{label}.category"))?
                .to_string(),
            unit: perf_nonempty_string(&object["unit"], &format!("{label}.unit"))?.to_string(),
            threshold: perf_finite_number(
                &object["threshold"],
                &format!("{label}.threshold"),
                true,
            )?,
            comparison: match perf_nonempty_string(
                &object["comparison"],
                &format!("{label}.comparison"),
            )? {
                comparison @ ("maximum" | "minimum") => comparison.to_string(),
                comparison => {
                    return Err(format!(
                        "{label}.comparison has unsupported value {comparison:?}"
                    ));
                }
            },
            ci_enforced: object["ci_enforced"]
                .as_bool()
                .ok_or_else(|| format!("{label}.ci_enforced must be a boolean"))?,
        };
        if definitions.insert(name.to_string(), definition).is_some() {
            return Err(format!("duplicate budget name: {name}"));
        }
    }

    let inventory_sha256 = perf_budget_inventory_sha256(budgets)?;
    if inventory_sha256 != PERF_CANONICAL_BUDGET_INVENTORY_SHA256 {
        return Err(format!(
            "budget inventory does not match the canonical producer contract (observed_sha256={inventory_sha256}, expected_sha256={PERF_CANONICAL_BUDGET_INVENTORY_SHA256})"
        ));
    }

    let mut result_names = HashSet::new();
    let mut result_order = Vec::with_capacity(results.len());
    let mut pass_count = 0usize;
    let mut fail_count = 0usize;
    let mut no_data_count = 0usize;
    let mut ci_with_data = 0usize;
    let mut ci_fail = 0usize;
    let mut ci_no_data = 0usize;
    for (index, result) in results.iter().enumerate() {
        let label = format!("budget_results[{index}]");
        let object = perf_exact_object(
            result,
            PERF_RESULT_REQUIRED_FIELDS,
            &["failure_reason"],
            &label,
        )?;
        let name = perf_nonempty_string(&object["budget_name"], &format!("{label}.budget_name"))?;
        if !result_names.insert(name.to_string()) {
            return Err(format!("duplicate budget result: {name}"));
        }
        result_order.push(name.to_string());
        let definition = definitions
            .get(name)
            .ok_or_else(|| format!("budget result has no matching definition: {name}"))?;
        let category = perf_nonempty_string(&object["category"], &format!("{label}.category"))?;
        let unit = perf_nonempty_string(&object["unit"], &format!("{label}.unit"))?;
        let comparison =
            perf_nonempty_string(&object["comparison"], &format!("{label}.comparison"))?;
        let threshold =
            perf_finite_number(&object["threshold"], &format!("{label}.threshold"), true)?;
        let ci_enforced = object["ci_enforced"]
            .as_bool()
            .ok_or_else(|| format!("{label}.ci_enforced must be a boolean"))?;
        if category != definition.category
            || unit != definition.unit
            || comparison != definition.comparison
            || threshold.total_cmp(&definition.threshold).is_ne()
            || ci_enforced != definition.ci_enforced
        {
            return Err(format!(
                "budget result {name} does not match its category/unit/threshold/CI definition"
            ));
        }
        perf_nonempty_string(&object["source"], &format!("{label}.source"))?;

        let status = object["status"]
            .as_str()
            .ok_or_else(|| format!("{label}.status must be a string"))?;
        if !matches!(status, "PASS" | "FAIL" | "NO_DATA") {
            return Err(format!(
                "budget result {name} has unsupported status: {status}"
            ));
        }
        let failure_reason = object.get("failure_reason");
        if let Some(reason) = failure_reason {
            perf_nonempty_string(reason, &format!("{label}.failure_reason"))?;
        }

        if object["actual"].is_null() {
            if strict_mode && definition.ci_enforced {
                if status != "FAIL"
                    || failure_reason.and_then(Value::as_str) != Some("missing_measurement_data")
                {
                    return Err(format!(
                        "strict CI budget {name} without data must be FAIL with failure_reason=missing_measurement_data"
                    ));
                }
            } else if status != "NO_DATA" || failure_reason.is_some() {
                return Err(format!(
                    "budget {name} without data must be NO_DATA without a failure reason"
                ));
            }
        } else {
            let actual = perf_finite_number(&object["actual"], &format!("{label}.actual"), false)?;
            if actual < 0.0 {
                return Err(format!("{label}.actual must be non-negative"));
            }
            let passes = if definition.comparison == "minimum" {
                actual >= threshold
            } else {
                actual <= threshold
            };
            let expected_status = if passes { "PASS" } else { "FAIL" };
            if status != expected_status || failure_reason.is_some() {
                return Err(format!(
                    "budget result {name} is inconsistent with actual={actual}, threshold={threshold}, and expected status={expected_status}"
                ));
            }
        }

        match status {
            "PASS" => pass_count += 1,
            "FAIL" => fail_count += 1,
            "NO_DATA" => no_data_count += 1,
            _ => unreachable!("status enum validated above"),
        }
        if definition.ci_enforced {
            ci_with_data += usize::from(!object["actual"].is_null());
            ci_fail += usize::from(status == "FAIL");
            ci_no_data += usize::from(status == "NO_DATA");
        }
    }

    let definition_names: HashSet<_> = definitions.keys().cloned().collect();
    let definition_order: Vec<_> = budgets
        .iter()
        .map(|budget| {
            perf_nonempty_string(&budget["name"], "canonical budget name").map(str::to_string)
        })
        .collect::<Result<_, _>>()?;
    if result_names != definition_names || result_order != definition_order {
        let missing: Vec<_> = definition_names
            .difference(&result_names)
            .cloned()
            .collect();
        return Err(format!(
            "budget_results must match canonical budget declaration order and membership (missing={missing:?})"
        ));
    }

    let mut failure_fingerprints = HashSet::new();
    for (index, failure) in failures.iter().enumerate() {
        let label = format!("failing_data_contracts[{index}]");
        let object = perf_exact_object(
            failure,
            PERF_FAILURE_REQUIRED_FIELDS,
            &["budget_name"],
            &label,
        )?;
        let contract_id =
            perf_nonempty_string(&object["contract_id"], &format!("{label}.contract_id"))?;
        let detail = perf_nonempty_string(&object["detail"], &format!("{label}.detail"))?;
        let remediation =
            perf_nonempty_string(&object["remediation"], &format!("{label}.remediation"))?;
        let budget_name = match object.get("budget_name") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let name = perf_nonempty_string(value, &format!("{label}.budget_name"))?;
                if !definitions.contains_key(name) {
                    return Err(format!(
                        "data-contract failure references unknown budget: {name}"
                    ));
                }
                Some(name)
            }
        };
        if !failure_fingerprints.insert((contract_id, detail, remediation, budget_name)) {
            return Err(format!("duplicate data-contract failure at index {index}"));
        }
    }

    let derived_counts = [
        ("total_budgets", budgets.len()),
        (
            "ci_enforced",
            definitions
                .values()
                .filter(|definition| definition.ci_enforced)
                .count(),
        ),
        ("ci_with_data", ci_with_data),
        ("ci_fail", ci_fail),
        ("ci_no_data", ci_no_data),
        ("pass", pass_count),
        ("fail", fail_count),
        ("no_data", no_data_count),
        ("data_contract_failures_count", failures.len()),
    ];
    for (name, expected) in derived_counts {
        let expected = perf_usize_as_u64(expected, name)?;
        if counts[name] != expected {
            return Err(format!(
                "{name}={} is inconsistent with derived value {expected}",
                counts[name]
            ));
        }
    }
    if counts["pass"] + counts["fail"] + counts["no_data"] != counts["total_budgets"] {
        return Err("pass + fail + no_data must equal total_budgets".to_string());
    }

    let claim = perf_exact_object(
        &top["claim_readiness"],
        PERF_CLAIM_READINESS_FIELDS,
        &[],
        "claim_readiness",
    )?;
    let reasons = claim["blocking_reason_codes"]
        .as_array()
        .ok_or_else(|| "claim_readiness.blocking_reason_codes must be an array".to_string())?;
    let reported_reasons: Vec<_> = reasons
        .iter()
        .enumerate()
        .map(|(index, reason)| {
            perf_nonempty_string(
                reason,
                &format!("claim_readiness.blocking_reason_codes[{index}]"),
            )
        })
        .collect::<Result<_, _>>()?;
    if !reported_reasons.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(
            "claim_readiness.blocking_reason_codes must be sorted and duplicate-free".to_string(),
        );
    }

    let mut expected_reasons = Vec::new();
    if counts["no_data"] != 0 {
        expected_reasons.push("budget_data_missing");
    }
    if counts["fail"] != 0 {
        expected_reasons.push("budget_failed");
    }
    if counts["ci_with_data"] != counts["ci_enforced"] || counts["ci_no_data"] != 0 {
        expected_reasons.push("ci_budget_data_missing");
    }
    if counts["ci_fail"] != 0 {
        expected_reasons.push("ci_budget_failed");
    }
    if correlation_id.is_none() {
        expected_reasons.push("correlation_id_missing");
    }
    if counts["data_contract_failures_count"] != 0 {
        expected_reasons.push("data_contract_failure");
    }
    if run_id.is_none() {
        expected_reasons.push("run_id_missing");
    }
    if source_commit.is_none() {
        expected_reasons.push("source_commit_unbound");
    }
    if !strict_mode {
        expected_reasons.push("strict_mode_disabled");
    }
    if reported_reasons != expected_reasons {
        return Err(format!(
            "claim_readiness blockers disagree with derived blockers (reported={reported_reasons:?}, expected={expected_reasons:?})"
        ));
    }

    let claim_ready = expected_reasons.is_empty();
    let expected_status = if claim_ready {
        "claim_ready"
    } else {
        "blocked"
    };
    if claim["status"].as_str() != Some(expected_status) {
        return Err(format!(
            "claim_readiness.status must be {expected_status:?}"
        ));
    }
    if claim["performance_claims_authorized"].as_bool() != Some(claim_ready) {
        return Err(format!(
            "claim_readiness.performance_claims_authorized must be {claim_ready}"
        ));
    }

    if claim_ready && now.signed_duration_since(generated_at) > maximum_age {
        return Err("performance summary is too stale to authorize claims".to_string());
    }

    Ok(PerformanceClaimValidation { claim_ready })
}

const PERFORMANCE_BUDGET_SUMMARY_PATH: &str = "tests/perf/reports/budget_summary.json";

#[derive(Debug)]
struct PerformanceGitContext {
    worktree: PathBuf,
    git_dir: PathBuf,
}

fn scrub_git_environment(command: &mut std::process::Command) {
    for (variable, _) in std::env::vars_os() {
        if variable.to_string_lossy().starts_with("GIT_") {
            command.env_remove(variable);
        }
    }
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
}

fn sanitized_perf_git_command(context: &PerformanceGitContext) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(&context.worktree)
        .arg("--git-dir")
        .arg(&context.git_dir)
        .arg("--work-tree")
        .arg(&context.worktree)
        .args([
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.bare=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.ignoreStat=false",
            "-c",
        ])
        .arg(format!("core.worktree={}", context.worktree.display()));
    scrub_git_environment(&mut command);
    command
}

fn perf_git_output_at(context: &PerformanceGitContext, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = sanitized_perf_git_command(context)
        .args(args)
        .output()
        .map_err(|err| format!("failed to execute git {}: {err}", args.join(" ")))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn perf_git_stdout_at(context: &PerformanceGitContext, args: &[&str]) -> Result<String, String> {
    String::from_utf8(perf_git_output_at(context, args)?)
        .map(|stdout| stdout.trim().to_string())
        .map_err(|err| format!("git {} output was not UTF-8: {err}", args.join(" ")))
}

fn performance_git_context(root: &Path) -> Result<PerformanceGitContext, String> {
    let worktree = std::fs::canonicalize(root)
        .map_err(|err| format!("performance repository root is unavailable: {err}"))?;
    let marker = worktree.join(".git");
    let marker_metadata = std::fs::symlink_metadata(&marker)
        .map_err(|err| format!("performance repository .git marker is unavailable: {err}"))?;
    if marker_metadata.file_type().is_symlink() {
        return Err("performance repository .git marker must not be a symlink".to_string());
    }
    let git_dir = if marker_metadata.is_dir() {
        std::fs::canonicalize(&marker)
            .map_err(|err| format!("performance repository git directory is invalid: {err}"))?
    } else if marker_metadata.is_file() {
        let marker_text = std::fs::read_to_string(&marker)
            .map_err(|err| format!("performance repository gitfile is unreadable: {err}"))?;
        let marker_line = marker_text.trim_end_matches(['\r', '\n']);
        let target = marker_line
            .strip_prefix("gitdir: ")
            .filter(|target| {
                !target.is_empty() && !target.contains('\0') && target.lines().count() == 1
            })
            .ok_or_else(|| "performance repository gitfile is malformed".to_string())?;
        let target = Path::new(target);
        let candidate = if target.is_absolute() {
            target.to_path_buf()
        } else {
            worktree.join(target)
        };
        let target_metadata = std::fs::symlink_metadata(&candidate).map_err(|err| {
            format!("performance repository gitfile target is unavailable: {err}")
        })?;
        if target_metadata.file_type().is_symlink() || !target_metadata.is_dir() {
            return Err(
                "performance repository gitfile target must be a non-symlink directory".to_string(),
            );
        }
        std::fs::canonicalize(candidate)
            .map_err(|err| format!("performance repository gitfile target is invalid: {err}"))?
    } else {
        return Err(
            "performance repository .git marker must be a directory or gitfile".to_string(),
        );
    };

    let context = PerformanceGitContext { worktree, git_dir };
    let top_level = perf_git_stdout_at(&context, &["rev-parse", "--show-toplevel"])?;
    let canonical_top_level = std::fs::canonicalize(&top_level)
        .map_err(|err| format!("performance repository top level is invalid: {err}"))?;
    if canonical_top_level != context.worktree {
        return Err("performance repository worktree identity mismatch".to_string());
    }
    let reported_git_dir = perf_git_stdout_at(&context, &["rev-parse", "--absolute-git-dir"])?;
    let canonical_reported_git_dir = std::fs::canonicalize(&reported_git_dir).map_err(|err| {
        format!("performance repository reported git directory is invalid: {err}")
    })?;
    if canonical_reported_git_dir != context.git_dir {
        return Err("performance repository git directory identity mismatch".to_string());
    }
    if perf_git_stdout_at(&context, &["rev-parse", "--is-inside-work-tree"])? != "true" {
        return Err("performance repository is not a worktree".to_string());
    }
    Ok(context)
}

fn validate_performance_checkout_clean(context: &PerformanceGitContext) -> Result<(), String> {
    let status = perf_git_output_at(
        context,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--no-renames",
        ],
    )?;
    if !status.is_empty() {
        let entries: Vec<_> = status
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .take(3)
            .map(|entry| String::from_utf8_lossy(entry).into_owned())
            .collect();
        return Err(format!(
            "performance summary repository is not clean: {entries:?}"
        ));
    }

    let index = perf_git_output_at(context, &["ls-files", "-v", "-z"])?;
    let flagged: Vec<_> = index
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty() && !entry.starts_with(b"H "))
        .take(3)
        .map(|entry| String::from_utf8_lossy(entry.get(2..).unwrap_or_default()).into_owned())
        .collect();
    if !flagged.is_empty() {
        return Err(format!(
            "performance summary repository uses non-default assume-unchanged/skip-worktree index flags: {flagged:?}"
        ));
    }
    Ok(())
}

fn contained_regular_artifact_path(
    context: &PerformanceGitContext,
    artifact_path: &str,
) -> Result<PathBuf, String> {
    let relative = Path::new(artifact_path);
    if artifact_path.is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "performance summary path must be a normalized repository-relative path: {artifact_path:?}"
        ));
    }

    let mut candidate = context.worktree.clone();
    for component in relative.components() {
        let std::path::Component::Normal(segment) = component else {
            unreachable!("non-normal components rejected above");
        };
        candidate.push(segment);
        let metadata = std::fs::symlink_metadata(&candidate).map_err(|err| {
            format!(
                "performance summary path component {} is unavailable: {err}",
                candidate.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err("performance summary path must not contain symlink components".to_string());
        }
    }

    let canonical_artifact = std::fs::canonicalize(&candidate)
        .map_err(|err| format!("performance summary path could not be resolved: {err}"))?;
    if !canonical_artifact.starts_with(&context.worktree) {
        return Err("performance summary path escapes the repository root".to_string());
    }
    let metadata = std::fs::metadata(&canonical_artifact)
        .map_err(|err| format!("performance summary metadata is unavailable: {err}"))?;
    if !metadata.is_file() {
        return Err("performance summary must be a regular file".to_string());
    }
    Ok(canonical_artifact)
}

fn validate_performance_artifact_at_head(
    context: &PerformanceGitContext,
    artifact_path: &str,
    head: &str,
) -> Result<(), String> {
    let full_path = contained_regular_artifact_path(context, artifact_path)?;
    let tree_entry = perf_git_output_at(
        context,
        &["ls-tree", "--full-tree", "-z", head, "--", artifact_path],
    )?;
    let entries: Vec<_> = tree_entry
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect();
    let [entry] = entries.as_slice() else {
        return Err("performance summary is not tracked exactly once at HEAD".to_string());
    };
    let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
        return Err("performance summary HEAD tree entry is malformed".to_string());
    };
    let (metadata, tracked_path_with_tab) = entry.split_at(tab);
    let tracked_path = &tracked_path_with_tab[1..];
    let metadata_fields: Vec<_> = metadata
        .split(|byte| *byte == b' ')
        .filter(|field| !field.is_empty())
        .collect();
    if metadata_fields.len() != 3
        || !matches!(metadata_fields[0], b"100644" | b"100755")
        || metadata_fields[1] != b"blob"
        || tracked_path != artifact_path.as_bytes()
    {
        return Err(
            "performance summary HEAD entry must be the exact tracked regular-file blob"
                .to_string(),
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let live_mode = std::fs::symlink_metadata(&full_path)
            .map_err(|err| format!("performance summary current mode is unavailable: {err}"))?
            .permissions()
            .mode();
        let live_git_mode = if live_mode & 0o111 == 0 {
            b"100644".as_slice()
        } else {
            b"100755".as_slice()
        };
        if live_git_mode != metadata_fields[0] {
            return Err("performance summary current mode does not exactly match HEAD".to_string());
        }
    }

    let blob_oid = std::str::from_utf8(metadata_fields[2])
        .map_err(|err| format!("performance summary blob ID is not UTF-8: {err}"))?;
    let head_bytes = perf_git_output_at(context, &["cat-file", "blob", blob_oid])?;
    let live_bytes = std::fs::read(&full_path)
        .map_err(|err| format!("performance summary current bytes are unreadable: {err}"))?;
    if live_bytes != head_bytes {
        return Err("performance summary current bytes do not exactly match HEAD".to_string());
    }
    Ok(())
}

fn performance_followup_path_allowed(path: &str, packaged: bool) -> bool {
    path.starts_with("tests/perf/reports/")
        || path.starts_with("tests/e2e_results/")
        || path.starts_with("tests/ext_conformance/reports/")
        || path.starts_with("tests/certification/")
        || (path.starts_with("docs/evidence/") && !packaged)
}

fn performance_path_is_packaged(
    context: &PerformanceGitContext,
    source_commit: &str,
    path: &str,
) -> Result<bool, String> {
    let cargo_expression = format!("{source_commit}:Cargo.toml");
    let cargo_toml = String::from_utf8(perf_git_output_at(context, &["show", &cargo_expression])?)
        .map_err(|err| format!("source Cargo.toml is not UTF-8: {err}"))?;
    let document: toml::Value = toml::from_str(&cargo_toml).map_err(|err| {
        format!("unable to parse source Cargo.toml package include policy: {err}")
    })?;
    let patterns = document
        .get("package")
        .and_then(|package| package.get("include"))
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "source Cargo.toml package.include must be an array".to_string())?;
    for value in patterns {
        let raw = value
            .as_str()
            .filter(|pattern| !pattern.is_empty())
            .ok_or_else(|| {
                "source Cargo.toml package.include entries must be non-empty strings".to_string()
            })?;
        let normalized = raw.strip_prefix('/').unwrap_or(raw);
        let pattern = glob::Pattern::new(normalized)
            .map_err(|err| format!("invalid package.include pattern {raw:?}: {err}"))?;
        if pattern.matches(path)
            || normalized.strip_suffix("/**").is_some_and(|prefix| {
                path.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_performance_source_binding_at_with_finalizer<F>(
    root: &Path,
    artifact_path: &str,
    source_commit: &str,
    before_final_check: F,
) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
{
    let context = performance_git_context(root)?;
    let head = perf_git_stdout_at(&context, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    validate_performance_checkout_clean(&context)?;
    validate_performance_artifact_at_head(&context, artifact_path, &head)?;

    let source_expression = format!("{source_commit}^{{commit}}");
    let resolved = perf_git_stdout_at(&context, &["rev-parse", "--verify", &source_expression])?;
    if resolved != source_commit {
        return Err("source_commit does not resolve to the exact recorded commit".to_string());
    }

    let ancestor = sanitized_perf_git_command(&context)
        .args(["merge-base", "--is-ancestor", source_commit, &head])
        .output()
        .map_err(|err| format!("failed to verify performance source ancestry: {err}"))?;
    if !ancestor.status.success() {
        return Err(if ancestor.status.code() == Some(1) {
            "performance source commit is not an ancestor of release HEAD".to_string()
        } else {
            format!(
                "unable to verify performance source ancestry: {}",
                String::from_utf8_lossy(&ancestor.stderr).trim()
            )
        });
    }
    if source_commit != head {
        let changed = perf_git_output_at(
            &context,
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                source_commit,
                &head,
            ],
        )?;
        let paths: Vec<_> = changed
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                std::str::from_utf8(path).map_err(|err| format!("changed path is not UTF-8: {err}"))
            })
            .collect::<Result<_, _>>()?;
        if paths.is_empty() {
            return Err(
                "source_commit differs from HEAD but the source-to-release diff is empty"
                    .to_string(),
            );
        }
        for path in paths {
            let packaged = path.starts_with("docs/evidence/")
                && performance_path_is_packaged(&context, source_commit, path)?;
            if !performance_followup_path_allowed(path, packaged) {
                return Err(format!(
                    "non-evidence or packaged path changed after source_commit: {path}"
                ));
            }
        }
    }

    before_final_check()?;
    let final_head = perf_git_stdout_at(&context, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if final_head != head {
        return Err(
            "performance repository HEAD changed during source binding validation".to_string(),
        );
    }
    validate_performance_checkout_clean(&context)?;
    validate_performance_artifact_at_head(&context, artifact_path, &head)?;
    let final_head = perf_git_stdout_at(&context, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if final_head != head {
        return Err(
            "performance repository HEAD changed during source binding validation".to_string(),
        );
    }
    validate_performance_checkout_clean(&context)?;
    let final_head = perf_git_stdout_at(&context, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if final_head != head {
        return Err(
            "performance repository HEAD changed during source binding validation".to_string(),
        );
    }
    Ok(())
}

fn validate_performance_source_binding_at(
    root: &Path,
    artifact_path: &str,
    source_commit: &str,
) -> Result<(), String> {
    validate_performance_source_binding_at_with_finalizer(
        root,
        artifact_path,
        source_commit,
        || Ok(()),
    )
}

fn validate_performance_source_binding(source_commit: &str) -> Result<(), String> {
    validate_performance_source_binding_at(
        &repo_root(),
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        source_commit,
    )
}

fn performance_fixture_timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn exact_libtest_output_proves_one(
    listing: &str,
    execution: &str,
    test_name: &str,
) -> Result<(), String> {
    let listed: Vec<_> = listing
        .lines()
        .map(str::trim)
        .filter(|line| line.ends_with(": test"))
        .collect();
    let has_listed_benchmarks = listing
        .lines()
        .map(str::trim)
        .any(|line| line.ends_with(": benchmark") || line.ends_with(": bench"));
    let list_summaries: Vec<_> = listing
        .lines()
        .map(str::trim)
        .filter(|line| {
            let mut parts = line.split_whitespace();
            matches!(
                (
                    parts.next(),
                    parts.next(),
                    parts.next(),
                    parts.next(),
                    parts.next(),
                ),
                (
                    Some(_),
                    Some("test," | "tests,"),
                    Some(_),
                    Some("benchmark" | "benchmarks"),
                    None
                )
            )
        })
        .collect();
    let expected_listing = format!("{test_name}: test");
    if listed != [expected_listing.as_str()]
        || has_listed_benchmarks
        || !(list_summaries.is_empty() || list_summaries == ["1 test, 0 benchmarks"])
    {
        return Err("exact filter did not list exactly one test".to_string());
    }

    let running: Vec<_> = execution
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("running ") && line.ends_with(" test"))
        .collect();
    let results: Vec<_> = execution
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("test result:"))
        .collect();
    if running != ["running 1 test"]
        || results.len() != 1
        || !results[0].starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")
        || !results[0].contains(" filtered out; finished in ")
    {
        return Err("exact filter did not execute one non-ignored passing test".to_string());
    }
    Ok(())
}

fn canonical_performance_budgets_fixture() -> Vec<Value> {
    require_json("tests/perf/reports/budget_summary.json")
        .get("budgets")
        .and_then(Value::as_array)
        .cloned()
        .expect("checked-in performance summary must provide canonical budgets")
}

fn blocked_performance_summary_fixture(now: DateTime<Utc>) -> Value {
    let budgets = canonical_performance_budgets_fixture();
    let total_budgets = budgets.len();
    let ci_enforced = budgets
        .iter()
        .filter(|budget| budget["ci_enforced"].as_bool() == Some(true))
        .count();
    let budget_results: Vec<_> = budgets
        .iter()
        .map(|budget| {
            json!({
                "budget_name": budget["name"],
                "category": budget["category"],
                "threshold": budget["threshold"],
                "comparison": budget["comparison"],
                "unit": budget["unit"],
                "actual": null,
                "status": "NO_DATA",
                "source": "fixture has no measurement",
                "ci_enforced": budget["ci_enforced"]
            })
        })
        .collect();
    let first_budget_name = budgets
        .first()
        .and_then(|budget| budget["name"].as_str())
        .expect("canonical budget inventory must be non-empty")
        .to_string();
    json!({
        "schema": PERF_BUDGET_SUMMARY_SCHEMA,
        "generated_at": performance_fixture_timestamp(now),
        "source_commit": null,
        "run_id": null,
        "correlation_id": null,
        "strict_mode": false,
        "total_budgets": total_budgets,
        "ci_enforced": ci_enforced,
        "ci_with_data": 0,
        "ci_fail": 0,
        "ci_no_data": ci_enforced,
        "pass": 0,
        "fail": 0,
        "no_data": total_budgets,
        "data_contract_failures_count": 1,
        "failing_data_contracts": [{
            "contract_id": "missing_or_stale_budget_artifact",
            "budget_name": first_budget_name,
            "detail": "measurement missing",
            "remediation": "regenerate the measurement"
        }],
        "budgets": budgets,
        "budget_results": budget_results,
        "claim_readiness": {
            "status": "blocked",
            "performance_claims_authorized": false,
            "blocking_reason_codes": [
                "budget_data_missing",
                "ci_budget_data_missing",
                "correlation_id_missing",
                "data_contract_failure",
                "run_id_missing",
                "source_commit_unbound",
                "strict_mode_disabled"
            ]
        }
    })
}

fn claim_ready_performance_summary_fixture(now: DateTime<Utc>) -> Value {
    let mut summary = blocked_performance_summary_fixture(now);
    let total_budgets = summary["budgets"]
        .as_array()
        .expect("fixture budgets")
        .len();
    let ci_enforced = summary["budgets"]
        .as_array()
        .expect("fixture budgets")
        .iter()
        .filter(|budget| budget["ci_enforced"].as_bool() == Some(true))
        .count();
    summary["source_commit"] = Value::String("a".repeat(40));
    summary["run_id"] = json!("perf-run-1");
    summary["correlation_id"] = json!("perf-run-1");
    summary["strict_mode"] = json!(true);
    summary["ci_with_data"] = json!(ci_enforced);
    summary["ci_no_data"] = json!(0);
    summary["pass"] = json!(total_budgets);
    summary["no_data"] = json!(0);
    summary["data_contract_failures_count"] = json!(0);
    summary["failing_data_contracts"] = json!([]);
    for result in summary["budget_results"]
        .as_array_mut()
        .expect("fixture budget results")
    {
        result["actual"] = result["threshold"].clone();
        result["status"] = json!("PASS");
    }
    summary["claim_readiness"] = json!({
        "status": "claim_ready",
        "performance_claims_authorized": true,
        "blocking_reason_codes": []
    });
    summary
}

fn fixture_git_output(root: &Path, args: &[&str]) -> String {
    let mut command = if std::fs::symlink_metadata(root.join(".git")).is_ok() {
        let context = performance_git_context(root).expect("resolve fixture Git context");
        sanitized_perf_git_command(&context)
    } else {
        let mut command = std::process::Command::new("git");
        command.arg("-C").arg(root);
        scrub_git_environment(&mut command);
        command
    };
    let output = command
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("fixture git {} failed to run: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "fixture git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|err| panic!("fixture git {} output was not UTF-8: {err}", args.join(" ")))
}

fn fixture_git_context_and_head(root: &Path) -> (PerformanceGitContext, String) {
    let context = performance_git_context(root).expect("resolve fixture Git context");
    let head = perf_git_stdout_at(&context, &["rev-parse", "--verify", "HEAD^{commit}"])
        .expect("resolve fixture HEAD");
    (context, head)
}

fn commit_performance_binding_fixture(root: &Path, message: &str) -> String {
    fixture_git_output(root, &["add", "--all"]);
    fixture_git_output(
        root,
        &[
            "-c",
            "user.name=Pi release-gate fixture",
            "-c",
            "user.email=pi-release-gate@example.invalid",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
    fixture_git_output(root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .trim()
        .to_string()
}

fn retained_performance_binding_fixture(packaged_evidence: bool) -> (PathBuf, String) {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("pi-release-evidence-gate-fixtures");
    std::fs::create_dir_all(&base).expect("create retained release-gate fixture base");
    let root = base.join(format!("fixture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("src")).expect("create fixture source directory");
    std::fs::create_dir_all(root.join("tests/perf/reports"))
        .expect("create fixture performance report directory");
    let include = if packaged_evidence {
        r#"include = ["/Cargo.toml", "/docs/evidence/shipped.json"]"#
    } else {
        r#"include = ["/Cargo.toml"]"#
    };
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"release-gate-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n{include}\n"
        ),
    )
    .expect("write fixture Cargo.toml");
    std::fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n").expect("write fixture source");
    std::fs::write(
        root.join(PERFORMANCE_BUDGET_SUMMARY_PATH),
        b"{\"fixture\":true}\n",
    )
    .expect("write fixture performance summary");
    if packaged_evidence {
        std::fs::create_dir_all(root.join("docs/evidence"))
            .expect("create packaged evidence directory");
        std::fs::write(
            root.join("docs/evidence/shipped.json"),
            b"{\"version\":1}\n",
        )
        .expect("write packaged evidence fixture");
    }
    fixture_git_output(&root, &["init", "--quiet", "--initial-branch=main"]);
    let source_commit = commit_performance_binding_fixture(&root, "initial source snapshot");
    (root, source_commit)
}

fn retained_e2e_evidence_fixture() -> (PathBuf, PathBuf) {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("pi-release-evidence-gate-fixtures");
    std::fs::create_dir_all(&base).expect("create retained release-gate fixture base");
    let root = base.join(format!("e2e-fixture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("src")).expect("create E2E fixture source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"release-gate-e2e-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\ninclude = [\"/Cargo.toml\"]\n",
    )
    .expect("write E2E fixture Cargo.toml");
    std::fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n")
        .expect("write E2E fixture source");
    fixture_git_output(&root, &["init", "--quiet", "--initial-branch=main"]);
    let source_commit = commit_performance_binding_fixture(&root, "initial E2E source snapshot");

    let evidence_dir = root.join("tests/e2e_results/20260805T010203Z");
    let suite_dir = evidence_dir.join("smoke");
    std::fs::create_dir_all(&suite_dir).expect("create E2E fixture evidence directory");
    let correlation_id = "release-gate-e2e-fixture";
    let artifact_dir = "tests/e2e_results/20260805T010203Z";
    let suite_result = json!({
        "suite": "smoke",
        "exit_code": 0,
        "passed": 1,
        "failed": 0,
        "ignored": 0,
        "total": 1
    });
    let documents = [
        (
            evidence_dir.join("evidence_contract.json"),
            json!({
                "schema": "pi.evidence.contract.v1",
                "profile": "ci",
                "strict_conformance": false,
                "status": "pass",
                "errors": [],
                "checks": [{
                    "id": "fixture",
                    "path": "tests/e2e_results/20260805T010203Z/summary.json",
                    "diagnostics": "",
                    "ok": true
                }],
                "correlation_id": correlation_id,
                "artifact_dir": artifact_dir
            }),
        ),
        (
            evidence_dir.join("environment.json"),
            json!({
                "schema": "pi.e2e.environment.v1",
                "profile": "ci",
                "rerun_from": null,
                "correlation_id": correlation_id,
                "artifact_dir": artifact_dir,
                "unit_targets": ["release_evidence_gate"],
                "e2e_suites": ["smoke"],
                "git_sha": source_commit
            }),
        ),
        (
            evidence_dir.join("summary.json"),
            json!({
                "schema": "pi.e2e.summary.v1",
                "profile": "ci",
                "rerun_from": null,
                "correlation_id": correlation_id,
                "artifact_dir": artifact_dir,
                "total_units": 1,
                "passed_units": 1,
                "failed_units": 0,
                "unit_targets": [{
                    "target": "release_evidence_gate",
                    "exit_code": 0,
                    "passed": 1,
                    "failed": 0,
                    "ignored": 0,
                    "total": 1
                }],
                "total_suites": 1,
                "passed_suites": 1,
                "failed_suites": 0,
                "failed_names": [],
                "suites": [suite_result]
            }),
        ),
        (suite_dir.join("result.json"), suite_result),
    ];
    for (path, document) in documents {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&document).expect("serialize E2E fixture document"),
        )
        .expect("write E2E fixture document");
    }
    commit_performance_binding_fixture(&root, "record E2E evidence follow-up");
    (root, evidence_dir)
}

fn retained_dropin_evidence_fixture() -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("pi-release-evidence-gate-fixtures");
    std::fs::create_dir_all(&base).expect("create retained release-gate fixture base");
    let root = base.join(format!("dropin-binding-fixture-{}", uuid::Uuid::new_v4()));
    let contract_path = root.join("docs/contracts/dropin-certification-contract.json");
    let verdict_path = root.join("docs/evidence/dropin-certification-verdict.json");
    std::fs::create_dir_all(root.join("src")).expect("create drop-in fixture source directory");
    std::fs::create_dir_all(contract_path.parent().expect("drop-in contract parent"))
        .expect("create drop-in contract directory");
    std::fs::create_dir_all(verdict_path.parent().expect("drop-in verdict parent"))
        .expect("create drop-in verdict directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"release-gate-dropin-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\ninclude = [\"/Cargo.toml\"]\n",
    )
    .expect("write drop-in fixture Cargo.toml");
    std::fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n")
        .expect("write drop-in fixture source");
    std::fs::write(
        &contract_path,
        serde_json::to_vec_pretty(&json!({
            "release_process_enforcement": {
                "verdict_artifact_contract": {
                    "required_fields": [
                        "git_commit",
                        "generated_at_utc",
                        "overall_verdict",
                        "hard_gate_results",
                        "blocking_reasons",
                        "evidence_index"
                    ],
                    "schema": "pi.dropin.certification_verdict.v1",
                    "path": "docs/evidence/dropin-certification-verdict.json"
                }
            }
        }))
        .expect("serialize drop-in binding contract"),
    )
    .expect("write drop-in binding contract");
    fixture_git_output(&root, &["init", "--quiet", "--initial-branch=main"]);
    let source_commit = commit_performance_binding_fixture(&root, "initial drop-in source");
    std::fs::write(
        &verdict_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.dropin.certification_verdict.v1",
            "git_commit": source_commit,
            "generated_at_utc": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            "overall_verdict": "NOT_CERTIFIED",
            "hard_gate_results": [],
            "blocking_reasons": ["fixture"],
            "evidence_index": []
        }))
        .expect("serialize drop-in binding verdict"),
    )
    .expect("write drop-in binding verdict");
    commit_performance_binding_fixture(&root, "record drop-in verdict evidence");
    (root, contract_path, verdict_path)
}

fn release_gate_embedded_python(marker: &str) -> String {
    let script = require_text("scripts/release_gate.sh");
    let section = script
        .get(
            script
                .find(marker)
                .unwrap_or_else(|| panic!("release gate marker not found: {marker}"))..,
        )
        .expect("marker index must be a character boundary");
    let heredoc_marker = "<<'PY'\n";
    let program_start = section.find(heredoc_marker).map_or_else(
        || panic!("Python heredoc not found after release gate marker: {marker}"),
        |index| index + heredoc_marker.len(),
    );
    let program_end = section[program_start..].find("\nPY\n").map_or_else(
        || panic!("Python heredoc terminator not found after marker: {marker}"),
        |index| program_start + index,
    );
    section[program_start..program_end].to_string()
}

fn release_gate_python_command(marker: &str, args: &[&str]) -> (std::process::Command, String) {
    let mut command = std::process::Command::new("python3");
    command
        .arg("-")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    (command, release_gate_embedded_python(marker))
}

fn run_release_gate_python(
    mut command: std::process::Command,
    program: &str,
) -> std::process::Output {
    let mut child = command.spawn().expect("spawn embedded release-gate Python");
    let mut stdin = child.stdin.take().expect("embedded Python stdin");
    std::io::Write::write_all(&mut stdin, program.as_bytes())
        .expect("write embedded release-gate Python");
    drop(stdin);
    child
        .wait_with_output()
        .expect("wait for embedded release-gate Python")
}

fn git_executable_on_path() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH must be set for release-gate tests");
    std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git executable must be discoverable on PATH")
}

#[test]
fn release_evidence_json_rejects_duplicate_keys_recursively() {
    for (label, document, duplicate_key) in [
        (
            "top-level",
            br#"{"schema":"first","schema":"forged"}"#.as_slice(),
            "schema",
        ),
        (
            "nested object",
            br#"{"claim":{"status":"pass","status":"forged"}}"#.as_slice(),
            "status",
        ),
        (
            "object inside array",
            br#"{"rows":[{"id":"first","id":"forged"}]}"#.as_slice(),
            "id",
        ),
    ] {
        let error = parse_release_json(document)
            .expect_err("duplicate release-evidence object key must fail closed");
        assert!(
            error.contains(&format!("duplicate JSON object key: {duplicate_key}")),
            "{label}: {error}"
        );
    }
}

#[test]
fn release_gate_embedded_python_rejects_nested_duplicate_json_keys() {
    let (root, _) = retained_performance_binding_fixture(false);
    std::fs::write(
        root.join(PERFORMANCE_BUDGET_SUMMARY_PATH),
        br#"{"claim_readiness":{"status":"blocked","status":"forged"}}
"#,
    )
    .expect("write duplicate-key performance summary fixture");
    commit_performance_binding_fixture(&root, "record duplicate-key evidence fixture");

    let root_arg = root.to_str().expect("UTF-8 fixture root");
    let summary = root.join(PERFORMANCE_BUDGET_SUMMARY_PATH);
    let summary_arg = summary.to_str().expect("UTF-8 fixture summary path");
    let (command, program) = release_gate_python_command(
        "if [[ -f \"$PERFORMANCE_SUMMARY\" ]]; then",
        &[root_arg, summary_arg, "0", "168"],
    );
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "the performance validator reports contract failure through stdout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("duplicate JSON object key: status"),
        "{stdout}"
    );

    let release_gate = require_text("scripts/release_gate.sh");
    for (line_number, line) in release_gate.lines().enumerate() {
        if line.contains("json.loads(") {
            let inline_hook = line.contains("object_pairs_hook=reject_duplicate_keys");
            let multiline_hook = release_gate
                .lines()
                .skip(line_number + 1)
                .take(3)
                .any(|candidate| candidate.contains("object_pairs_hook=reject_duplicate_keys"));
            assert!(
                inline_hook || multiline_hook,
                "release-gate JSON ingestion at line {} lacks duplicate-key rejection",
                line_number + 1
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn release_gate_embedded_e2e_validator_binds_the_exact_parsed_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let (root, evidence_dir) = retained_e2e_evidence_fixture();
    let root_arg = root.to_str().expect("UTF-8 E2E fixture root");
    let evidence_arg = evidence_dir
        .to_str()
        .expect("UTF-8 E2E fixture evidence path");
    let marker = "if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then";
    let (command, program) = release_gate_python_command(marker, &[root_arg, evidence_arg]);
    let positive = run_release_gate_python(command, &program);
    assert!(
        positive.status.success(),
        "{}",
        String::from_utf8_lossy(&positive.stderr)
    );
    assert!(
        String::from_utf8_lossy(&positive.stdout).starts_with("pass|"),
        "{}",
        String::from_utf8_lossy(&positive.stdout)
    );

    let summary_path = evidence_dir.join("summary.json");
    let original_bytes = std::fs::read(&summary_path).expect("read original E2E summary");
    let original_path = root
        .parent()
        .expect("fixture parent")
        .join(format!("e2e-original-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&original_path, &original_bytes).expect("retain original E2E summary bytes");
    let mut substituted_bytes = original_bytes;
    substituted_bytes.extend_from_slice(b" \n");
    std::fs::write(&summary_path, substituted_bytes).expect("substitute parse-time E2E bytes");

    let wrapper_dir = root
        .parent()
        .expect("fixture parent")
        .join(format!("e2e-git-wrapper-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&wrapper_dir).expect("create E2E Git wrapper directory");
    let wrapper = wrapper_dir.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nif [ ! -f \"$PI_E2E_RESTORE_MARKER\" ]; then\n  cp \"$PI_E2E_ORIGINAL\" \"$PI_E2E_TARGET\" || exit 97\n  : > \"$PI_E2E_RESTORE_MARKER\" || exit 98\nfi\nexec \"$PI_E2E_REAL_GIT\" \"$@\"\n",
    )
    .expect("write E2E Git wrapper");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("E2E Git wrapper metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("make E2E Git wrapper executable");
    let marker_path = wrapper_dir.join("restored");
    let real_git = git_executable_on_path();
    let original_path_env = std::env::var_os("PATH").expect("PATH must be available");
    let mut wrapped_path_entries = vec![wrapper_dir];
    wrapped_path_entries.extend(std::env::split_paths(&original_path_env));
    let wrapped_path = std::env::join_paths(wrapped_path_entries).expect("construct wrapped PATH");
    let (mut command, program) = release_gate_python_command(marker, &[root_arg, evidence_arg]);
    command
        .env("PATH", wrapped_path)
        .env("PI_E2E_REAL_GIT", &real_git)
        .env("PI_E2E_ORIGINAL", &original_path)
        .env("PI_E2E_TARGET", &summary_path)
        .env("PI_E2E_RESTORE_MARKER", marker_path);
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("bytes parsed by the validator differ from release HEAD"),
        "{stdout}"
    );
}

#[cfg(unix)]
#[test]
fn release_gate_embedded_e2e_validator_rejects_live_executable_mode() {
    use std::os::unix::fs::PermissionsExt;

    let (root, evidence_dir) = retained_e2e_evidence_fixture();
    let summary = evidence_dir.join("summary.json");
    let mut permissions = std::fs::metadata(&summary)
        .expect("E2E summary metadata")
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(&summary, permissions).expect("make live E2E summary executable");
    let root_arg = root.to_str().expect("UTF-8 E2E fixture root");
    let evidence_arg = evidence_dir
        .to_str()
        .expect("UTF-8 E2E fixture evidence path");
    let (command, program) = release_gate_python_command(
        "if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then",
        &[root_arg, evidence_arg],
    );
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(stdout.contains("must not be executable"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn release_gate_embedded_dropin_validator_binds_parsed_bytes_and_modes() {
    use std::os::unix::fs::PermissionsExt;

    let marker = "DROPIN_VERDICT=\"$PROJECT_ROOT/docs/evidence/dropin-certification-verdict.json\"";
    for (relative, label) in [
        (
            "docs/contracts/dropin-certification-contract.json",
            "drop-in contract",
        ),
        (
            "docs/evidence/dropin-certification-verdict.json",
            "drop-in verdict",
        ),
    ] {
        let (root, contract_path, verdict_path) = retained_dropin_evidence_fixture();
        let root_arg = root.to_str().expect("UTF-8 drop-in binding fixture root");
        let contract_arg = contract_path.to_str().expect("UTF-8 drop-in contract path");
        let verdict_arg = verdict_path.to_str().expect("UTF-8 drop-in verdict path");
        let args = [root_arg, contract_arg, verdict_arg, "0", "168"];

        let (command, program) = release_gate_python_command(marker, &args);
        let positive = run_release_gate_python(command, &program);
        assert!(
            positive.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&positive.stderr)
        );
        assert!(
            String::from_utf8_lossy(&positive.stdout).starts_with("warn|"),
            "{label}: {}",
            String::from_utf8_lossy(&positive.stdout)
        );

        let target = root.join(relative);
        let original_bytes = std::fs::read(&target).expect("read original drop-in input");
        let original_path = root
            .parent()
            .expect("drop-in fixture parent")
            .join(format!("dropin-original-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&original_path, &original_bytes).expect("retain original drop-in bytes");
        let mut substituted_bytes = original_bytes;
        substituted_bytes.extend_from_slice(b" \n");
        std::fs::write(&target, substituted_bytes).expect("substitute parse-time drop-in bytes");

        let wrapper_dir = root
            .parent()
            .expect("drop-in fixture parent")
            .join(format!("dropin-git-wrapper-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&wrapper_dir).expect("create drop-in Git wrapper directory");
        let wrapper = wrapper_dir.join("git");
        std::fs::write(
            &wrapper,
            "#!/bin/sh\nif [ ! -f \"$PI_DROPIN_RESTORE_MARKER\" ]; then\n  cp \"$PI_DROPIN_ORIGINAL\" \"$PI_DROPIN_TARGET\" || exit 97\n  : > \"$PI_DROPIN_RESTORE_MARKER\" || exit 98\nfi\nexec \"$PI_DROPIN_REAL_GIT\" \"$@\"\n",
        )
        .expect("write drop-in Git wrapper");
        let mut wrapper_permissions = std::fs::metadata(&wrapper)
            .expect("drop-in Git wrapper metadata")
            .permissions();
        wrapper_permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, wrapper_permissions)
            .expect("make drop-in Git wrapper executable");
        let restore_marker = wrapper_dir.join("restored");
        let real_git = git_executable_on_path();
        let current_path = std::env::var_os("PATH").expect("PATH for drop-in binding test");
        let mut wrapped_path = vec![wrapper_dir.clone()];
        wrapped_path.extend(std::env::split_paths(&current_path));
        let wrapped_path = std::env::join_paths(wrapped_path).expect("construct wrapped PATH");
        let (mut command, program) = release_gate_python_command(marker, &args);
        command
            .env("PATH", wrapped_path)
            .env("PI_DROPIN_REAL_GIT", real_git)
            .env("PI_DROPIN_ORIGINAL", &original_path)
            .env("PI_DROPIN_TARGET", &target)
            .env("PI_DROPIN_RESTORE_MARKER", restore_marker);
        let output = run_release_gate_python(command, &program);
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with("fail|"), "{label}: {stdout}");
        assert!(
            stdout.contains(&format!(
                "{label} bytes parsed by the validator differ from release HEAD"
            )),
            "{label}: {stdout}"
        );

        let mut permissions = std::fs::metadata(&target)
            .expect("drop-in decision-input metadata")
            .permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        std::fs::set_permissions(&target, permissions)
            .expect("make drop-in decision input executable");
        let (command, program) = release_gate_python_command(marker, &args);
        let output = run_release_gate_python(command, &program);
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with("fail|"), "{label}: {stdout}");
        assert!(
            stdout.contains(&format!("{label} must not be executable")),
            "{label}: {stdout}"
        );
    }
}

#[test]
fn release_gate_embedded_dropin_validator_rejects_future_and_stale_evidence() {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("pi-release-evidence-gate-fixtures");
    let root = base.join(format!("dropin-fixture-{}", uuid::Uuid::new_v4()));
    let contract_path = root.join("docs/contracts/dropin-certification-contract.json");
    let verdict_path = root.join("docs/evidence/dropin-certification-verdict.json");
    std::fs::create_dir_all(contract_path.parent().expect("contract parent"))
        .expect("create drop-in contract directory");
    std::fs::create_dir_all(verdict_path.parent().expect("verdict parent"))
        .expect("create drop-in verdict directory");
    std::fs::write(
        &contract_path,
        serde_json::to_vec_pretty(&json!({
            "release_process_enforcement": {
                "verdict_artifact_contract": {
                    "required_fields": [
                        "git_commit",
                        "generated_at_utc",
                        "overall_verdict",
                        "hard_gate_results",
                        "blocking_reasons",
                        "evidence_index"
                    ],
                    "schema": "pi.dropin.certification_verdict.v1",
                    "path": "docs/evidence/dropin-certification-verdict.json"
                }
            }
        }))
        .expect("serialize drop-in contract fixture"),
    )
    .expect("write drop-in contract fixture");
    let mut verdict = json!({
        "schema": "pi.dropin.certification_verdict.v1",
        "git_commit": "a".repeat(40),
        "generated_at_utc": (Utc::now() + Duration::minutes(6)).to_rfc3339_opts(SecondsFormat::Secs, true),
        "overall_verdict": "NOT_CERTIFIED",
        "hard_gate_results": [],
        "blocking_reasons": ["fixture"],
        "evidence_index": []
    });
    std::fs::write(
        &verdict_path,
        serde_json::to_vec_pretty(&verdict).expect("serialize future drop-in verdict fixture"),
    )
    .expect("write future drop-in verdict fixture");
    let root_arg = root.to_str().expect("UTF-8 drop-in fixture root");
    let contract_arg = contract_path.to_str().expect("UTF-8 contract fixture path");
    let verdict_arg = verdict_path.to_str().expect("UTF-8 verdict fixture path");
    let (command, program) = release_gate_python_command(
        "DROPIN_VERDICT=\"$PROJECT_ROOT/docs/evidence/dropin-certification-verdict.json\"",
        &[root_arg, contract_arg, verdict_arg, "0", "168"],
    );
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("more than five minutes in the future"),
        "{stdout}"
    );

    verdict["generated_at_utc"] = Value::String(
        (Utc::now() - Duration::hours(169)).to_rfc3339_opts(SecondsFormat::Secs, true),
    );
    std::fs::write(
        &verdict_path,
        serde_json::to_vec_pretty(&verdict).expect("serialize stale drop-in verdict fixture"),
    )
    .expect("write stale drop-in verdict fixture");
    let (command, program) = release_gate_python_command(
        "DROPIN_VERDICT=\"$PROJECT_ROOT/docs/evidence/dropin-certification-verdict.json\"",
        &[root_arg, contract_arg, verdict_arg, "0", "168"],
    );
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("older than the configured 168h evidence limit"),
        "{stdout}"
    );
}

#[test]
fn performance_source_binding_accepts_clean_head_and_non_product_followup() {
    let (root, source_commit) = retained_performance_binding_fixture(false);
    validate_performance_source_binding_at(&root, PERFORMANCE_BUDGET_SUMMARY_PATH, &source_commit)
        .expect("clean source HEAD must bind");

    std::fs::write(
        root.join("tests/perf/reports/followup.json"),
        b"{\"evidence\":true}\n",
    )
    .expect("write evidence-only follow-up");
    commit_performance_binding_fixture(&root, "add non-product evidence follow-up");
    validate_performance_source_binding_at(&root, PERFORMANCE_BUDGET_SUMMARY_PATH, &source_commit)
        .expect("non-product evidence follow-up must remain admissible");
}

#[test]
fn performance_source_binding_rejects_dirty_staged_and_untracked_changes() {
    let (dirty_root, dirty_source) = retained_performance_binding_fixture(false);
    std::fs::write(dirty_root.join("src/lib.rs"), "pub fn dirty() {}\n")
        .expect("write dirty source");
    let dirty_error = validate_performance_source_binding_at(
        &dirty_root,
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        &dirty_source,
    )
    .expect_err("dirty source must invalidate binding");
    assert!(
        dirty_error.contains("repository is not clean"),
        "{dirty_error}"
    );

    let (staged_root, staged_source) = retained_performance_binding_fixture(false);
    std::fs::write(staged_root.join("src/lib.rs"), "pub fn staged() {}\n")
        .expect("write staged source");
    fixture_git_output(&staged_root, &["add", "--", "src/lib.rs"]);
    let staged_error = validate_performance_source_binding_at(
        &staged_root,
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        &staged_source,
    )
    .expect_err("staged source must invalidate binding");
    assert!(
        staged_error.contains("repository is not clean"),
        "{staged_error}"
    );

    let (untracked_root, untracked_source) = retained_performance_binding_fixture(false);
    std::fs::write(
        untracked_root.join("untracked-release-input"),
        b"not measured\n",
    )
    .expect("write untracked source");
    let untracked_error = validate_performance_source_binding_at(
        &untracked_root,
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        &untracked_source,
    )
    .expect_err("untracked source must invalidate binding");
    assert!(
        untracked_error.contains("repository is not clean"),
        "{untracked_error}"
    );
}

#[test]
fn release_gate_hardening_rejects_ignored_untracked_performance_summary() {
    let (root, _) = retained_performance_binding_fixture(false);
    std::fs::write(
        root.join(".gitignore"),
        format!("/{PERFORMANCE_BUDGET_SUMMARY_PATH}\n"),
    )
    .expect("write fixture ignore policy");
    fixture_git_output(&root, &["add", "--", ".gitignore"]);
    fixture_git_output(
        &root,
        &[
            "update-index",
            "--force-remove",
            "--",
            PERFORMANCE_BUDGET_SUMMARY_PATH,
        ],
    );
    fixture_git_output(
        &root,
        &[
            "-c",
            "user.name=Pi release-gate fixture",
            "-c",
            "user.email=pi-release-gate@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "ignore untracked performance summary",
        ],
    );
    assert!(
        root.join(PERFORMANCE_BUDGET_SUMMARY_PATH).is_file(),
        "the adversarial ignored artifact must remain readable in the worktree"
    );
    assert!(
        fixture_git_output(
            &root,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=none",
                "--no-renames",
            ],
        )
        .is_empty(),
        "the ignored artifact must evade ordinary clean-status checks"
    );

    let root_arg = root.to_str().expect("UTF-8 fixture root");
    let summary = root.join(PERFORMANCE_BUDGET_SUMMARY_PATH);
    let summary_arg = summary.to_str().expect("UTF-8 fixture summary path");
    let (command, program) = release_gate_python_command(
        "if [[ -f \"$PERFORMANCE_SUMMARY\" ]]; then",
        &[root_arg, summary_arg, "0", "168"],
    );
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "the performance validator reports contract failure through stdout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("not tracked exactly once at release HEAD"),
        "{stdout}"
    );
}

#[test]
fn performance_source_binding_rejects_non_default_index_flags() {
    let (root, source_commit) = retained_performance_binding_fixture(false);
    fixture_git_output(&root, &["update-index", "--skip-worktree", "src/lib.rs"]);
    let error = validate_performance_source_binding_at(
        &root,
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        &source_commit,
    )
    .expect_err("skip-worktree must invalidate binding");
    assert!(error.contains("non-default"), "{error}");
}

#[test]
fn performance_source_binding_rejects_artifact_byte_substitution() {
    let (root, _) = retained_performance_binding_fixture(false);
    std::fs::write(
        root.join(PERFORMANCE_BUDGET_SUMMARY_PATH),
        b"{\"fixture\":false}\n",
    )
    .expect("substitute live performance summary bytes");
    let (context, head) = fixture_git_context_and_head(&root);
    let error =
        validate_performance_artifact_at_head(&context, PERFORMANCE_BUDGET_SUMMARY_PATH, &head)
            .expect_err("live artifact substitution must fail HEAD-byte binding");
    assert!(error.contains("do not exactly match HEAD"), "{error}");
}

#[cfg(unix)]
#[test]
fn release_gate_hardening_rejects_artifact_mode_substitution() {
    use std::os::unix::fs::PermissionsExt;

    let (root, _) = retained_performance_binding_fixture(false);
    let summary = root.join(PERFORMANCE_BUDGET_SUMMARY_PATH);
    let mut permissions = std::fs::metadata(&summary)
        .expect("fixture performance summary metadata")
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(&summary, permissions)
        .expect("make live performance summary executable");
    let (context, head) = fixture_git_context_and_head(&root);
    let error =
        validate_performance_artifact_at_head(&context, PERFORMANCE_BUDGET_SUMMARY_PATH, &head)
            .expect_err("live artifact mode substitution must fail HEAD binding");
    assert!(
        error.contains("mode does not exactly match HEAD"),
        "{error}"
    );
}

#[test]
fn release_gate_hardening_snapshot_rejects_raw_bytes_hidden_by_clean_filter() {
    let (root, _) = retained_performance_binding_fixture(false);
    fixture_git_output(
        &root,
        &[
            "config",
            "filter.release-normalize.clean",
            "sed s/dirty/fixture/",
        ],
    );
    fixture_git_output(&root, &["config", "filter.release-normalize.smudge", "cat"]);
    fixture_git_output(
        &root,
        &["config", "filter.release-normalize.required", "true"],
    );
    std::fs::write(
        root.join(".gitattributes"),
        "src/lib.rs filter=release-normalize\n",
    )
    .expect("write clean-filter fixture attributes");
    commit_performance_binding_fixture(&root, "add adversarial clean filter");
    std::fs::write(root.join("src/lib.rs"), "pub fn dirty() {}\n")
        .expect("write raw bytes normalized by clean filter");
    let filtered_diff = fixture_git_output(&root, &["diff", "--quiet", "--", "src/lib.rs"]);
    assert!(
        filtered_diff.is_empty(),
        "Git's clean filter must hide the raw-byte substitution from Git's content diff"
    );

    let root_arg = root.to_str().expect("UTF-8 fixture root");
    let (command, program) =
        release_gate_python_command("capture_repository_snapshot() {", &[root_arg]);
    let output = run_release_gate_python(command, &program);
    assert!(!output.status.success(), "raw-byte substitution must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("raw worktree bytes differ from release HEAD at 'src/lib.rs'"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn release_gate_hardening_snapshot_rejects_executable_mode_substitution() {
    use std::os::unix::fs::PermissionsExt;

    let (root, _) = retained_performance_binding_fixture(false);
    let source = root.join("src/lib.rs");
    let mut permissions = std::fs::metadata(&source)
        .expect("fixture source metadata")
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(&source, permissions).expect("make fixture source executable");

    let root_arg = root.to_str().expect("UTF-8 fixture root");
    let (command, program) =
        release_gate_python_command("capture_repository_snapshot() {", &[root_arg]);
    let output = run_release_gate_python(command, &program);
    assert!(!output.status.success(), "mode substitution must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("raw worktree mode differs from release HEAD at 'src/lib.rs'"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn release_gate_hardening_snapshot_rehash_rejects_mutation_after_initial_hash() {
    use std::os::unix::fs::PermissionsExt;

    let (root, _) = retained_performance_binding_fixture(false);
    let wrapper_dir = root
        .parent()
        .expect("fixture parent")
        .join(format!("snapshot-git-wrapper-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&wrapper_dir).expect("create snapshot Git wrapper directory");
    let wrapper = wrapper_dir.join("git");
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
set -eu
case " $* " in
  *" rev-parse --verify HEAD^{commit} "*)
    count=0
    if [ -f "$PI_RELEASE_GATE_TEST_COUNTER" ]; then
      count=$(sed -n '1p' "$PI_RELEASE_GATE_TEST_COUNTER")
    fi
    count=$((count + 1))
    printf '%s\n' "$count" > "$PI_RELEASE_GATE_TEST_COUNTER"
    if [ "$count" -eq 2 ]; then
      printf 'pub fn mutated_after_initial_hash() {}\n' > "$PI_RELEASE_GATE_TEST_MUTATION_TARGET"
    fi
    ;;
esac
exec "$PI_RELEASE_GATE_TEST_REAL_GIT" "$@"
"#,
    )
    .expect("write snapshot Git wrapper");
    let mut wrapper_permissions = std::fs::metadata(&wrapper)
        .expect("snapshot Git wrapper metadata")
        .permissions();
    wrapper_permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, wrapper_permissions)
        .expect("make snapshot Git wrapper executable");

    let counter = wrapper_dir.join("head-counter");
    let mutation_target = root.join("src/lib.rs");
    let real_git = git_executable_on_path();
    let current_path = std::env::var_os("PATH").expect("PATH for snapshot test");
    let mut wrapped_path = vec![wrapper_dir];
    wrapped_path.extend(std::env::split_paths(&current_path));
    let wrapped_path = std::env::join_paths(wrapped_path).expect("construct wrapped PATH");

    let root_arg = root.to_str().expect("UTF-8 fixture root");
    let (mut command, program) =
        release_gate_python_command("capture_repository_snapshot() {", &[root_arg]);
    command
        .env("PATH", wrapped_path)
        .env("PI_RELEASE_GATE_TEST_COUNTER", &counter)
        .env("PI_RELEASE_GATE_TEST_MUTATION_TARGET", &mutation_target)
        .env("PI_RELEASE_GATE_TEST_REAL_GIT", &real_git);
    let output = run_release_gate_python(command, &program);
    assert!(
        !output.status.success(),
        "mutation after the first raw hash must fail"
    );
    assert_eq!(
        std::fs::read_to_string(&counter)
            .expect("read Git wrapper counter")
            .trim(),
        "2",
        "the fixture must mutate only after the initial raw-worktree hash"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("raw worktree bytes differ from release HEAD at 'src/lib.rs'")
            || stderr.contains("raw tracked worktree bytes or modes changed"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn performance_source_binding_rejects_symlinked_artifact_components() {
    use std::os::unix::fs::symlink;

    let (root, _) = retained_performance_binding_fixture(false);
    symlink("reports", root.join("tests/perf/linked-reports"))
        .expect("create retained artifact-directory symlink");
    commit_performance_binding_fixture(&root, "add symlinked artifact alias");
    let (context, head) = fixture_git_context_and_head(&root);
    let error = validate_performance_artifact_at_head(
        &context,
        "tests/perf/linked-reports/budget_summary.json",
        &head,
    )
    .expect_err("artifact path with a symlink component must fail closed");
    assert!(error.contains("symlink components"), "{error}");
}

#[test]
fn performance_source_binding_rejects_packaged_evidence_followup() {
    let (root, source_commit) = retained_performance_binding_fixture(true);
    std::fs::write(
        root.join("docs/evidence/shipped.json"),
        b"{\"version\":2}\n",
    )
    .expect("change packaged evidence");
    commit_performance_binding_fixture(&root, "change packaged evidence after measurement");
    let error = validate_performance_source_binding_at(
        &root,
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        &source_commit,
    )
    .expect_err("packaged evidence follow-up must invalidate source binding");
    assert!(error.contains("packaged path changed"), "{error}");
}

#[test]
fn performance_source_binding_scrubs_hostile_git_environment() {
    const CHILD_FLAG: &str = "PI_RELEASE_GATE_HOSTILE_GIT_CHILD";
    const ROOT_ENV: &str = "PI_RELEASE_GATE_HOSTILE_GIT_ROOT";
    const SOURCE_ENV: &str = "PI_RELEASE_GATE_HOSTILE_GIT_SOURCE";

    if std::env::var_os(CHILD_FLAG).is_some() {
        let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("child fixture root"));
        let source_commit = std::env::var(SOURCE_ENV).expect("child fixture source commit");
        let error = validate_performance_source_binding_at(
            &root,
            PERFORMANCE_BUDGET_SUMMARY_PATH,
            &source_commit,
        )
        .expect_err("sanitized Git must inspect the dirty default worktree and index");
        assert!(error.contains("repository is not clean"), "{error}");
        return;
    }

    let (root, source_commit) = retained_performance_binding_fixture(false);
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn staged_but_hidden_by_alternate_index() {}\n",
    )
    .expect("write source staged only in the default index");
    fixture_git_output(&root, &["add", "--", "src/lib.rs"]);
    std::fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n")
        .expect("restore HEAD bytes in the worktree while retaining the staged default-index edit");
    let alternate_index = root.join(".git/pi-clean-alternate-index");
    let context = performance_git_context(&root).expect("resolve hostile fixture Git context");
    let output = sanitized_perf_git_command(&context)
        .env("GIT_INDEX_FILE", &alternate_index)
        .args(["read-tree", "HEAD"])
        .output()
        .expect("create clean alternate index");
    assert!(
        output.status.success(),
        "failed to create alternate index: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .arg("performance_source_binding_scrubs_hostile_git_environment")
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_FLAG, "1")
        .env(ROOT_ENV, &root)
        .env(SOURCE_ENV, &source_commit)
        .env("GIT_INDEX_FILE", &alternate_index)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "true")
        .env("GIT_NAMESPACE", "hostile-release-gate-namespace")
        .output()
        .expect("run hostile-Git child test");
    assert!(
        output.status.success(),
        "hostile-Git child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn performance_source_binding_ignores_repo_local_worktree_redirect() {
    let (root, source_commit) = retained_performance_binding_fixture(false);
    let decoy = root
        .parent()
        .expect("fixture parent")
        .join(format!("decoy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(decoy.join("src")).expect("create decoy source directory");
    std::fs::create_dir_all(decoy.join("tests/perf/reports"))
        .expect("create decoy performance report directory");
    for path in ["Cargo.toml", "src/lib.rs", PERFORMANCE_BUDGET_SUMMARY_PATH] {
        std::fs::copy(root.join(path), decoy.join(path))
            .unwrap_or_else(|err| panic!("copy {path} into decoy worktree: {err}"));
    }
    fixture_git_output(
        &root,
        &[
            "config",
            "--local",
            "core.worktree",
            decoy.to_str().expect("UTF-8 decoy path"),
        ],
    );
    std::fs::write(root.join("src/lib.rs"), "pub fn dirty_real_worktree() {}\n")
        .expect("dirty the canonical worktree");

    let mut raw_git = std::process::Command::new("git");
    raw_git.arg("-C").arg(&root);
    scrub_git_environment(&mut raw_git);
    let raw_status = raw_git
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--no-renames",
        ])
        .output()
        .expect("run Git with the hostile repository-local worktree setting");
    assert!(
        raw_status.status.success(),
        "raw redirected Git status failed"
    );
    assert!(
        raw_status.stdout.is_empty(),
        "the decoy must demonstrate that core.worktree hides the real dirty file"
    );

    let error = validate_performance_source_binding_at(
        &root,
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        &source_commit,
    )
    .expect_err("the canonical dirty worktree must not be hidden by repository-local config");
    assert!(error.contains("repository is not clean"), "{error}");
}

#[test]
fn performance_source_binding_rejects_head_advance_during_validation() {
    let (root, source_commit) = retained_performance_binding_fixture(false);
    let error = validate_performance_source_binding_at_with_finalizer(
        &root,
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        &source_commit,
        || {
            std::fs::write(
                root.join("tests/perf/reports/late-followup.json"),
                b"{\"late\":true}\n",
            )
            .map_err(|err| format!("write late evidence follow-up: {err}"))?;
            commit_performance_binding_fixture(&root, "advance HEAD during validation");
            Ok(())
        },
    )
    .expect_err("a clean evidence-only HEAD advance must invalidate the captured snapshot");
    assert!(error.contains("HEAD changed during"), "{error}");
}

#[test]
fn performance_budgets_report_has_exact_v2_contract() {
    let summary = require_json("tests/perf/reports/budget_summary.json");
    let source_binding_valid = summary
        .get("source_commit")
        .and_then(Value::as_str)
        .is_some_and(|source_commit| {
            validate_performance_source_binding(source_commit)
                .unwrap_or_else(|err| panic!("invalid asserted performance source binding: {err}"));
            true
        });
    let validated = validate_performance_budget_summary(
        &summary,
        Utc::now(),
        Duration::hours(168),
        source_binding_valid,
    )
    .unwrap_or_else(|err| panic!("invalid performance budget summary: {err}"));

    if env!("CARGO_PKG_VERSION") == "0.2.0" {
        assert!(
            !validated.claim_ready,
            "v0.2.0 must remain explicitly performance-claims-NOT-authorized"
        );
    }
}

#[test]
fn performance_contract_accepts_coherent_blocked_no_data() {
    let now = Utc::now();
    let validated = validate_performance_budget_summary(
        &blocked_performance_summary_fixture(now),
        now,
        Duration::hours(168),
        false,
    )
    .expect("coherent blocked evidence must remain admissible for a no-claims release");
    assert!(!validated.claim_ready);

    let mut forged_source = blocked_performance_summary_fixture(now);
    forged_source["source_commit"] = Value::String("a".repeat(40));
    forged_source["claim_readiness"]["blocking_reason_codes"] = json!([
        "budget_data_missing",
        "ci_budget_data_missing",
        "correlation_id_missing",
        "data_contract_failure",
        "run_id_missing",
        "strict_mode_disabled"
    ]);
    assert!(
        validate_performance_budget_summary(&forged_source, now, Duration::hours(168), false,)
            .is_err(),
        "a blocked artifact may omit source binding, but must not assert a fabricated binding"
    );

    let future = blocked_performance_summary_fixture(now + Duration::minutes(6));
    assert!(
        validate_performance_budget_summary(&future, now, Duration::hours(168), false).is_err(),
        "an impossible future timestamp is malformed even when claims remain blocked"
    );

    for (run_id, correlation_id) in [
        (json!("partial-run"), Value::Null),
        (Value::Null, json!("partial-correlation")),
    ] {
        let mut partial_lineage = blocked_performance_summary_fixture(now);
        partial_lineage["run_id"] = run_id;
        partial_lineage["correlation_id"] = correlation_id;
        assert!(
            validate_performance_budget_summary(
                &partial_lineage,
                now,
                Duration::hours(168),
                false,
            )
            .is_err(),
            "one-sided run/correlation lineage must be malformed, not merely blocked"
        );
    }
}

#[test]
fn performance_contract_rejects_count_or_status_inconsistency() {
    let now = Utc::now();
    let mut bad_count = blocked_performance_summary_fixture(now);
    bad_count["ci_no_data"] = json!(0);
    assert!(
        validate_performance_budget_summary(&bad_count, now, Duration::hours(168), false).is_err()
    );

    let mut bad_status = blocked_performance_summary_fixture(now);
    bad_status["budget_results"][0]["status"] = json!("PASS");
    assert!(
        validate_performance_budget_summary(&bad_status, now, Duration::hours(168), false).is_err()
    );

    let mut negative_actual = claim_ready_performance_summary_fixture(now);
    negative_actual["budget_results"][0]["actual"] = json!(-1.0);
    assert!(
        validate_performance_budget_summary(&negative_actual, now, Duration::hours(168), true)
            .is_err(),
        "negative measurements must never satisfy maximum-style budgets"
    );

    let non_ci_index = claim_ready_performance_summary_fixture(now)["budgets"]
        .as_array()
        .expect("fixture budgets")
        .iter()
        .position(|budget| budget["ci_enforced"].as_bool() == Some(false))
        .expect("canonical inventory must include a non-CI budget");

    let mut non_ci_no_data = claim_ready_performance_summary_fixture(now);
    non_ci_no_data["budget_results"][non_ci_index]["actual"] = Value::Null;
    non_ci_no_data["budget_results"][non_ci_index]["status"] = json!("NO_DATA");
    non_ci_no_data["pass"] =
        json!(non_ci_no_data["pass"].as_u64().expect("fixture pass count") - 1);
    non_ci_no_data["no_data"] = json!(1);
    let error =
        validate_performance_budget_summary(&non_ci_no_data, now, Duration::hours(168), true)
            .expect_err("global authorization must reject missing non-CI budget data");
    assert!(error.contains("budget_data_missing"), "{error}");

    let mut non_ci_failure = claim_ready_performance_summary_fixture(now);
    let threshold = non_ci_failure["budget_results"][non_ci_index]["threshold"]
        .as_f64()
        .expect("fixture threshold");
    let comparison = non_ci_failure["budget_results"][non_ci_index]["comparison"]
        .as_str()
        .expect("fixture comparison");
    let failing_actual = if comparison == "minimum" {
        threshold / 2.0
    } else {
        threshold + 1.0
    };
    non_ci_failure["budget_results"][non_ci_index]["actual"] = json!(failing_actual);
    non_ci_failure["budget_results"][non_ci_index]["status"] = json!("FAIL");
    non_ci_failure["pass"] =
        json!(non_ci_failure["pass"].as_u64().expect("fixture pass count") - 1);
    non_ci_failure["fail"] = json!(1);
    let error =
        validate_performance_budget_summary(&non_ci_failure, now, Duration::hours(168), true)
            .expect_err("global authorization must reject a failed non-CI budget");
    assert!(error.contains("budget_failed"), "{error}");
}

#[test]
fn performance_contract_rejects_forged_claim_readiness() {
    let now = Utc::now();
    let mut forged = blocked_performance_summary_fixture(now);
    forged["claim_readiness"]["performance_claims_authorized"] = json!(true);
    assert!(
        validate_performance_budget_summary(&forged, now, Duration::hours(168), false).is_err()
    );

    let mut mismatched_lineage = claim_ready_performance_summary_fixture(now);
    mismatched_lineage["correlation_id"] = json!("different-run");
    assert!(
        validate_performance_budget_summary(&mismatched_lineage, now, Duration::hours(168), true)
            .is_err()
    );
}

#[test]
fn performance_contract_rejects_forged_inventory_and_comparison_semantics() {
    let now = Utc::now();

    let mut minimal = claim_ready_performance_summary_fixture(now);
    minimal["budgets"]
        .as_array_mut()
        .expect("fixture budgets")
        .truncate(1);
    minimal["budget_results"]
        .as_array_mut()
        .expect("fixture budget results")
        .truncate(1);
    minimal["total_budgets"] = json!(1);
    minimal["ci_enforced"] = json!(1);
    minimal["ci_with_data"] = json!(1);
    minimal["pass"] = json!(1);
    let error = validate_performance_budget_summary(&minimal, now, Duration::hours(168), true)
        .expect_err("a self-consistent minimal inventory must not authorize claims");
    assert!(error.contains("canonical producer contract"), "{error}");

    let mut forged_comparison = claim_ready_performance_summary_fixture(now);
    forged_comparison["budgets"][0]["comparison"] = json!("minimum");
    forged_comparison["budget_results"][0]["comparison"] = json!("minimum");
    let error =
        validate_performance_budget_summary(&forged_comparison, now, Duration::hours(168), true)
            .expect_err("self-consistent forged comparison semantics must not authorize claims");
    assert!(error.contains("canonical producer contract"), "{error}");

    let mut threshold_drift = claim_ready_performance_summary_fixture(now);
    let threshold = threshold_drift["budgets"][0]["threshold"]
        .as_f64()
        .expect("fixture threshold");
    threshold_drift["budgets"][0]["threshold"] = json!(threshold + 0.000_000_1);
    threshold_drift["budget_results"][0]["threshold"] = json!(threshold + 0.000_000_1);
    threshold_drift["budget_results"][0]["actual"] = json!(threshold);
    let error =
        validate_performance_budget_summary(&threshold_drift, now, Duration::hours(168), true)
            .expect_err("sub-canonical threshold precision drift must not authorize claims");
    assert!(error.contains("six-decimal precision"), "{error}");
}

#[test]
fn performance_contract_rejects_reordered_duplicated_or_missing_results() {
    let now = Utc::now();

    let mut reordered = claim_ready_performance_summary_fixture(now);
    reordered["budget_results"]
        .as_array_mut()
        .expect("fixture budget results")
        .swap(0, 1);
    assert!(
        validate_performance_budget_summary(&reordered, now, Duration::hours(168), true).is_err(),
        "reordered results must not preserve canonical membership binding"
    );

    let mut duplicated = claim_ready_performance_summary_fixture(now);
    let first = duplicated["budget_results"][0].clone();
    let results = duplicated["budget_results"]
        .as_array_mut()
        .expect("fixture budget results");
    *results.last_mut().expect("fixture result") = first;
    assert!(
        validate_performance_budget_summary(&duplicated, now, Duration::hours(168), true).is_err(),
        "duplicated results must not preserve canonical membership binding"
    );

    let mut missing = claim_ready_performance_summary_fixture(now);
    missing["budget_results"]
        .as_array_mut()
        .expect("fixture budget results")
        .pop();
    assert!(
        validate_performance_budget_summary(&missing, now, Duration::hours(168), true).is_err(),
        "missing results must not preserve canonical membership binding"
    );
}

#[test]
fn performance_claim_ready_requires_source_binding_and_fresh_timestamp() {
    let now = Utc::now();
    let ready = claim_ready_performance_summary_fixture(now);
    assert!(validate_performance_budget_summary(&ready, now, Duration::hours(168), true).is_ok());
    assert!(validate_performance_budget_summary(&ready, now, Duration::hours(168), false).is_err());

    let stale_time = now - Duration::hours(169);
    let stale = claim_ready_performance_summary_fixture(stale_time);
    assert!(validate_performance_budget_summary(&stale, now, Duration::hours(168), true).is_err());

    let future_time = now + Duration::minutes(6);
    let future = claim_ready_performance_summary_fixture(future_time);
    assert!(validate_performance_budget_summary(&future, now, Duration::hours(168), true).is_err());

    let mut noncanonical_fraction = claim_ready_performance_summary_fixture(now);
    noncanonical_fraction["generated_at"] = json!("2026-08-05T12:34:56.1Z");
    assert!(
        validate_performance_budget_summary(
            &noncanonical_fraction,
            now,
            Duration::hours(168),
            true,
        )
        .is_err(),
        "the Rust contract must reject fractional precision accepted by neither the v2 producer nor shell consumer"
    );
}

#[test]
fn canonical_perf_test_proof_rejects_zero_match_and_ignored_runs() {
    let name = "checked_in_budget_summary_matches_fresh_canonical_evaluation_exactly";
    let one_listing = format!("{name}: test\n\n1 test, 0 benchmarks\n");
    let one_listing_without_summary = format!("{name}: test\n");
    let one_execution = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 42 filtered out; finished in 0.01s\n";
    assert!(exact_libtest_output_proves_one(&one_listing, one_execution, name).is_ok());
    assert!(
        exact_libtest_output_proves_one(&one_listing_without_summary, one_execution, name).is_ok(),
        "current terse libtest output legitimately omits an aggregate list summary"
    );

    let benchmark_listing = format!("{name}: test\nforged: benchmark\n");
    assert!(
        exact_libtest_output_proves_one(&benchmark_listing, one_execution, name).is_err(),
        "an exact-test listing must not contain a benchmark"
    );

    let zero_listing = "0 tests, 0 benchmarks\n";
    let zero_execution = "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 43 filtered out; finished in 0.00s\n";
    assert!(
        exact_libtest_output_proves_one(zero_listing, zero_execution, name).is_err(),
        "a zero-match Cargo test filter must not authorize performance claims"
    );

    let ignored_execution = "running 1 test\ntest result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 42 filtered out; finished in 0.00s\n";
    assert!(
        exact_libtest_output_proves_one(&one_listing, ignored_execution, name).is_err(),
        "an ignored canonical test must not authorize performance claims"
    );
}

#[test]
fn release_gate_exposes_performance_claim_policy_in_report() {
    let script = require_text("scripts/release_gate.sh");
    for required in [
        "RELEASE_GATE_REQUIRE_PERFORMANCE_CLAIM_READY",
        "pi.perf.budget_summary.v2",
        "performance_claim_readiness",
        "performance_claim_canonical_contract",
        "run_id and correlation_id must both be null or match",
        "budget_data_missing",
        "budget_failed",
        "CANONICAL_BUDGET_INVENTORY_SHA256",
        "validate_exact_libtest_output",
        "--list --format terse",
        "0 ignored",
        "checked_in_budget_summary_matches_fresh_canonical_evaluation_exactly",
        "\"require_performance_claim_ready\"",
        "release must make no quantitative or global performance claims",
        "performance summary is not tracked exactly once at release HEAD",
        "performance summary path must not contain symlink components",
        "performance summary raw worktree bytes do not exactly match release HEAD",
        "capture_raw_worktree_digest",
        "final_worktree_digest != initial_worktree_digest",
        "raw worktree mode differs from release HEAD",
    ] {
        assert!(
            script.contains(required),
            "release gate is missing performance-claim policy token: {required}"
        );
    }
}

#[test]
fn performance_source_descendants_are_evidence_only_and_not_packaged() {
    for path in [
        "tests/perf/reports/budget_summary.json",
        "tests/e2e_results/20260805T010203Z/summary.json",
        "tests/ext_conformance/reports/conformance_summary.json",
        "tests/certification/verdict.json",
        "docs/evidence/dropin-certification-verdict.json",
    ] {
        assert!(
            performance_followup_path_allowed(path, false),
            "expected evidence-only follow-up path to be allowed: {path}"
        );
    }
    assert!(!performance_followup_path_allowed("src/agent.rs", false));
    assert!(!performance_followup_path_allowed(
        "scripts/release_gate.sh",
        false
    ));
    assert!(!performance_followup_path_allowed(
        "docs/evidence/tool-output-context-cache.jsonl",
        true
    ));
}

// ============================================================================
// Exception policy completeness
// ============================================================================

#[test]
fn exception_policy_covers_all_current_failures() {
    let bl = require_json("tests/ext_conformance/reports/conformance_baseline.json");

    let entries = bl
        .pointer("/exception_policy/entries")
        .and_then(Value::as_array);
    let total_classified = bl
        .pointer("/remediation_buckets/summary/total_classified")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let Some(entries) = entries else {
        // If no exception policy, there should be no failures.
        assert_eq!(
            total_classified, 0,
            "failures exist ({total_classified}) but no exception policy defined"
        );
        return;
    };

    // Every exception entry must have all required fields.
    let approved = entries
        .iter()
        .filter(|e| {
            e.get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s == "approved" || s == "temporary")
        })
        .count();

    assert!(
        approved > 0 || total_classified == 0,
        "failures exist ({total_classified}) but no approved exceptions"
    );
}

#[test]
fn exception_entries_have_review_dates() {
    let bl = require_json("tests/ext_conformance/reports/conformance_baseline.json");

    let entries = bl
        .pointer("/exception_policy/entries")
        .and_then(Value::as_array);

    let Some(entries) = entries else {
        return;
    };

    for entry in entries {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or("?");
        let review_by = entry.get("review_by").and_then(Value::as_str);

        assert!(
            review_by.is_some(),
            "exception entry {id} missing review_by date"
        );
    }
}

// ============================================================================
// Evidence completeness score
// ============================================================================

#[test]
fn evidence_completeness_score_above_minimum() {
    let root = repo_root();
    let mut present = 0u32;

    for (path, _) in REQUIRED_ARTIFACTS {
        if root.join(path).is_file() {
            present += 1;
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let score = (f64::from(present) / REQUIRED_ARTIFACTS.len() as f64) * 100.0;

    assert!(
        score >= 80.0,
        "evidence completeness {score:.0}% < 80% minimum (present={present}/{})",
        REQUIRED_ARTIFACTS.len()
    );
}

#[test]
fn conformance_evidence_has_linked_test_targets() {
    let sm = require_json("tests/ext_conformance/reports/conformance_summary.json");

    let evidence = sm.get("evidence").and_then(Value::as_object);
    let Some(evidence) = evidence else {
        // Evidence section is optional in summary v1.
        return;
    };

    // At least one evidence category should have non-zero count.
    let total_evidence: u64 = evidence.values().filter_map(Value::as_u64).sum();

    assert!(
        total_evidence > 0,
        "conformance summary has evidence section but all counts are zero"
    );
}

#[test]
fn franken_node_claim_contract_is_present_and_valid() {
    let contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    validate_franken_node_claim_contract(&contract).unwrap_or_else(|err| {
        panic!("franken_node claim contract should validate fail-closed: {err}")
    });
}

#[test]
fn franken_node_claim_contract_fails_closed_on_missing_required_tier() {
    let mut contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    let Some(tiers) = contract
        .get_mut("claim_tiers")
        .and_then(Value::as_array_mut)
    else {
        panic!("fixture claim_tiers must be an array");
    };
    tiers.retain(|tier| {
        tier.get("tier_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            != "TIER-3-FULL-NODE-BUN-REPLACEMENT"
    });

    let err = validate_franken_node_claim_contract(&contract)
        .expect_err("missing required tier must fail closed");
    assert!(
        err.contains("missing required claim tier: TIER-3-FULL-NODE-BUN-REPLACEMENT"),
        "error should name the missing required tier, got: {err}"
    );
}

#[test]
fn franken_node_claim_contract_fails_closed_on_empty_required_evidence_list() {
    let mut contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    contract["claim_tiers"][0]["required_evidence"] = serde_json::json!([]);

    let err = validate_franken_node_claim_contract(&contract)
        .expect_err("empty required_evidence list must fail closed");
    assert!(
        err.contains("must include required_evidence entries")
            || err.contains("required_evidence must be non-empty"),
        "error should explain required_evidence contract failure, got: {err}"
    );
}

#[test]
fn franken_node_claim_contract_fails_closed_on_missing_package_interop_evidence_token() {
    let mut contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    let tiers = contract
        .get_mut("claim_tiers")
        .and_then(Value::as_array_mut)
        .expect("claim_tiers must be an array");
    let targeted_runtime_tier = tiers
        .iter_mut()
        .find(|tier| {
            tier.get("tier_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_some_and(|tier_id| tier_id == "TIER-2-TARGETED-RUNTIME-PARITY")
        })
        .expect("TIER-2-TARGETED-RUNTIME-PARITY must exist");
    let evidence = targeted_runtime_tier
        .get_mut("required_evidence")
        .and_then(Value::as_array_mut)
        .expect("TIER-2 required_evidence must be an array");
    evidence.retain(|entry| {
        !entry.as_str().map_or("", str::trim).eq_ignore_ascii_case(
            "package/ecosystem interoperability contract evidence (CJS/ESM/npm)",
        )
    });

    let err = validate_franken_node_claim_contract(&contract)
        .expect_err("missing package interop evidence token must fail closed");
    assert!(
        err.contains("required_evidence missing token")
            && err.contains("package/ecosystem interoperability contract evidence"),
        "error should identify missing package interop token, got: {err}"
    );
}

#[test]
fn franken_node_claim_contract_fails_closed_on_missing_kernel_mapping_evidence_token() {
    let mut contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    let tiers = contract
        .get_mut("claim_tiers")
        .and_then(Value::as_array_mut)
        .expect("claim_tiers must be an array");
    let target_tier = tiers
        .iter_mut()
        .find(|tier| {
            tier.get("tier_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_some_and(|tier_id| tier_id == "TIER-3-FULL-NODE-BUN-REPLACEMENT")
        })
        .expect("TIER-3-FULL-NODE-BUN-REPLACEMENT must exist");
    let evidence = target_tier
        .get_mut("required_evidence")
        .and_then(Value::as_array_mut)
        .expect("TIER-3 required_evidence must be an array");
    evidence.retain(|entry| {
        !entry.as_str().map_or("", str::trim).eq_ignore_ascii_case(
            "kernel extraction boundary manifest and reintegration mapping evidence",
        )
    });

    let err = validate_franken_node_claim_contract(&contract)
        .expect_err("missing kernel mapping evidence token must fail closed");
    assert!(
        err.contains("required_evidence missing token")
            && err
                .contains("kernel extraction boundary manifest and reintegration mapping evidence"),
        "error should identify missing kernel mapping token, got: {err}"
    );
}

#[test]
fn franken_node_claim_contract_fails_closed_on_missing_runtime_substrate_evidence_token() {
    let mut contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    let tiers = contract
        .get_mut("claim_tiers")
        .and_then(Value::as_array_mut)
        .expect("claim_tiers must be an array");
    let target_tier = tiers
        .iter_mut()
        .find(|tier| {
            tier.get("tier_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_some_and(|tier_id| tier_id == "TIER-3-FULL-NODE-BUN-REPLACEMENT")
        })
        .expect("TIER-3-FULL-NODE-BUN-REPLACEMENT must exist");
    let evidence = target_tier
        .get_mut("required_evidence")
        .and_then(Value::as_array_mut)
        .expect("TIER-3 required_evidence must be an array");
    evidence.retain(|entry| {
        !entry
            .as_str()
            .map_or("", str::trim)
            .eq_ignore_ascii_case("runtime-substrate generalization evidence for bd-3ar8v.7.5")
    });

    let err = validate_franken_node_claim_contract(&contract)
        .expect_err("missing runtime substrate evidence token must fail closed");
    assert!(
        err.contains("required_evidence missing token")
            && err.contains("runtime-substrate generalization evidence for bd-3ar8v.7.5"),
        "error should identify missing runtime substrate evidence token, got: {err}"
    );
}

#[test]
fn franken_node_claim_contract_fails_closed_on_missing_multi_tier_execution_evidence_token() {
    let mut contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    let tiers = contract
        .get_mut("claim_tiers")
        .and_then(Value::as_array_mut)
        .expect("claim_tiers must be an array");
    let tier3_entry = tiers
        .iter_mut()
        .find(|tier| {
            tier.get("tier_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_some_and(|tier_id| tier_id == "TIER-3-FULL-NODE-BUN-REPLACEMENT")
        })
        .expect("TIER-3-FULL-NODE-BUN-REPLACEMENT must exist");
    let evidence = tier3_entry
        .get_mut("required_evidence")
        .and_then(Value::as_array_mut)
        .expect("TIER-3 required_evidence must be an array");
    evidence.retain(|entry| {
        !entry
            .as_str()
            .map_or("", str::trim)
            .eq_ignore_ascii_case("multi-tier execution engine evidence for bd-3ar8v.7.6")
    });

    let err = validate_franken_node_claim_contract(&contract)
        .expect_err("missing multi-tier execution evidence token must fail closed");
    assert!(
        err.contains("required_evidence missing token")
            && err.contains("multi-tier execution engine evidence for bd-3ar8v.7.6"),
        "error should identify missing multi-tier execution evidence token, got: {err}"
    );
}

#[test]
fn franken_node_claim_contract_fails_closed_on_missing_remediation_backlog_evidence_token() {
    let mut contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    let tiers = contract
        .get_mut("claim_tiers")
        .and_then(Value::as_array_mut)
        .expect("claim_tiers must be an array");
    let tier3_entry = tiers
        .iter_mut()
        .find(|tier| {
            tier.get("tier_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_some_and(|tier_id| tier_id == "TIER-3-FULL-NODE-BUN-REPLACEMENT")
        })
        .expect("TIER-3-FULL-NODE-BUN-REPLACEMENT must exist");
    let evidence = tier3_entry
        .get_mut("required_evidence")
        .and_then(Value::as_array_mut)
        .expect("TIER-3 required_evidence must be an array");
    evidence.retain(|entry| {
        !entry.as_str().map_or("", str::trim).eq_ignore_ascii_case(
            "compatibility remediation backlog generator evidence for bd-3ar8v.7.16",
        )
    });

    let err = validate_franken_node_claim_contract(&contract)
        .expect_err("missing remediation backlog evidence token must fail closed");
    assert!(
        err.contains("required_evidence missing token")
            && err
                .contains("compatibility remediation backlog generator evidence for bd-3ar8v.7.16"),
        "error should identify missing remediation backlog evidence token, got: {err}"
    );
}

#[test]
fn franken_node_claim_contract_fails_closed_on_missing_required_overclaim_blocker() {
    let mut contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    let Some(blockers) = contract
        .pointer_mut("/claim_gate_policy/overclaim_blockers")
        .and_then(Value::as_array_mut)
    else {
        panic!("fixture overclaim_blockers must be an array");
    };
    blockers
        .retain(|entry| entry.as_str().map_or("", str::trim) != "forbidden_claim_phrase_detected");

    let err = validate_franken_node_claim_contract(&contract)
        .expect_err("missing required overclaim blocker must fail closed");
    assert!(
        err.contains(
            "claim_gate_policy.overclaim_blockers missing forbidden_claim_phrase_detected"
        ),
        "error should identify missing overclaim blocker token, got: {err}"
    );
}

#[test]
fn franken_node_claim_contract_fails_closed_on_allowed_forbidden_phrase_overlap() {
    let mut contract = require_json(FRANKEN_NODE_CLAIM_CONTRACT_PATH);
    contract["claim_tiers"][0]["forbidden_claim_language"] =
        serde_json::json!(["Extension-hosting parity scope only"]);

    let err = validate_franken_node_claim_contract(&contract)
        .expect_err("allowed/forbidden phrase overlap must fail closed");
    assert!(
        err.contains("overlap between allowed_claim_language and forbidden_claim_language"),
        "error should explain overlap violation, got: {err}"
    );
}
