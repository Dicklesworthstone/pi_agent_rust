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
fn release_publication_never_builds_with_the_registry_token() {
    let workflow = require_text(".github/workflows/release.yml");
    assert_eq!(
        workflow
            .matches("PI_CRATES_IO_RELEASE_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}")
            .count(),
        1,
        "the registry secret must be injected into exactly one workflow step"
    );
    let publish_job = workflow
        .split_once("\n  publish_crate:")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("\n  publish_github_release:")
                .map(|(job, _)| job)
        })
        .expect("release workflow must retain an isolated crates publication job");
    assert!(
        publish_job.contains("environment:\n      name: release")
            && publish_job.contains("permissions:\n      contents: read")
            && !publish_job.contains("contents: write"),
        "crates publication must remain review-gated with read-only repository permissions"
    );
    let publish_step = workflow
        .split_once("- name: Publish only when Cargo proves the exact verified crate checksum")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| suffix.split_once("\n      - name:").map(|(step, _)| step))
        .expect("release workflow must retain the checksum-gated publication step");
    assert!(
        publish_step.contains("PI_CRATES_IO_RELEASE_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}"),
        "publication step must receive the registry credential only in its secret-scoped environment"
    );
    let token_capture = publish_step
        .find(concat!(
            "release_crates_io_token=\"$",
            "{PI_CRATES_IO_RELEASE_TOKEN:-}\""
        ))
        .expect("publication step must capture the injected token in a shell-only variable");
    let token_unset = publish_step
        .find("unset PI_CRATES_IO_RELEASE_TOKEN")
        .expect("publication step must remove the exported token before invoking subprocesses");
    let crate_reverification = publish_step
        .find("actual_crate_sha=\"$(sha256sum")
        .expect("publication step must reverify the crate after narrowing token scope");
    let token_handoff = publish_step
        .find("PI_CRATES_IO_RELEASE_TOKEN=\"$release_crates_io_token\"")
        .expect("publication step must hand the token only to the publish process");
    let cargo_publish = publish_step
        .find("cargo publish")
        .expect("publication step must retain the Cargo upload boundary");
    let token_clear = publish_step[token_handoff..]
        .find("unset release_crates_io_token")
        .map(|offset| token_handoff + offset)
        .expect("publication step must clear its shell-only token after Cargo returns");
    let receipt_validation = publish_step
        .find("if [ -f \"$PI_CREDENTIAL_RECEIPT\"")
        .expect("publication step must validate the credential receipt after token clearing");
    assert!(
        publish_step.contains(concat!(
            "run: |\n          set -euo pipefail\n          set +x\n          ",
            "release_crates_io_token=\"$",
            "{PI_CRATES_IO_RELEASE_TOKEN:-}\""
        ),) && publish_step.contains("export -n release_crates_io_token")
            && publish_step
                .matches("PI_CRATES_IO_RELEASE_TOKEN=\"$release_crates_io_token\"")
                .count()
                == 1
            && token_capture < token_unset
            && token_unset < crate_reverification
            && crate_reverification < token_handoff
            && token_handoff < cargo_publish
            && cargo_publish < token_clear
            && token_clear < receipt_validation,
        "registry credential must remain unavailable to pre-publication verification subprocesses"
    );
    assert!(
        publish_step.contains("cargo publish") && publish_step.contains("--no-verify"),
        "secret-scoped Cargo publication must not run Cargo's package build verification"
    );

    let runbook = require_text("docs/releasing.md");
    let manual_lane = runbook
        .split_once("## Manual DSR lane (no GitHub Actions)")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("## Pre-release flow (rc)")
                .map(|(body, _)| body)
        })
        .expect("manual release lane must have stable section boundaries");
    let manual_xtrace_disable = manual_lane
        .find("set -euo pipefail\nset +x\numask 077")
        .expect("manual lane must disable shell tracing before reading the registry token");
    let manual_token_capture = manual_lane
        .find(
            "release_crates_io_token=\"${CARGO_REGISTRY_TOKEN:-${CARGO_REGISTRIES_CRATES_IO_TOKEN:-}}\"",
        )
        .expect("manual lane must capture the registry token in a shell-only variable");
    let manual_token_nonempty = manual_lane
        .find("[[ -n \"$release_crates_io_token\" ]]")
        .expect("manual lane must require a registry token before its first subprocess");
    let manual_token_unset = manual_lane
        .find(
            "builtin unset CARGO_REGISTRY_TOKEN CARGO_REGISTRIES_CRATES_IO_TOKEN \\\n  PI_CRATES_IO_RELEASE_TOKEN",
        )
        .expect("manual lane must remove every exported registry-token spelling");
    let manual_token_length = manual_lane
        .find("(( ${#release_crates_io_token} <= 4096 ))")
        .expect("manual lane must bound the token before its first subprocess");
    let manual_token_line_guard = manual_lane
        .find("case \"$release_crates_io_token\" in *$'\\n'*|*$'\\r'*) exit 1 ;; esac")
        .expect("manual lane must reject line-breaking token bytes before its first subprocess");
    let first_tool_resolution = manual_lane
        .find("release_cargo_entrypoint=\"$(builtin type -P -- cargo)\"")
        .expect("manual lane must resolve its Cargo entrypoint");
    assert!(
        manual_xtrace_disable < manual_token_capture
            && manual_token_capture < manual_token_nonempty
            && manual_token_nonempty < manual_token_length
            && manual_token_length < manual_token_line_guard
            && manual_token_line_guard < manual_token_unset
            && manual_token_unset < first_tool_resolution,
        "manual release must validate and narrow the token before its first subprocess"
    );
    assert!(
        !manual_lane[manual_xtrace_disable..manual_token_unset].contains("$("),
        "manual release bootstrap must not spawn a token-inheriting command substitution"
    );
    assert!(
        manual_lane.contains("builtin export -n release_crates_io_token")
            && !manual_lane.contains("PI_CRATES_IO_RELEASE_TOKEN=\"$release_crates_io_token\"")
            && manual_lane.contains("builtin printf '%s\\n' \"$controller_token\" |")
            && manual_lane.contains("\"$release_bash_path\" --noprofile --norc -c")
            && manual_lane.contains("[[ -z \"${PI_CRATES_IO_RELEASE_TOKEN:-}\" ]]")
            && manual_lane.contains("IFS= read -r scoped_release_token")
            && manual_lane.contains("export PI_CRATES_IO_RELEASE_TOKEN=\"$scoped_release_token\"",)
            && manual_lane.contains("unset scoped_release_token")
            && manual_lane.contains("exec 0</dev/null"),
        "manual release must pass the token through an anonymous pipe into exactly one clean child, never argv"
    );
    assert!(
        manual_lane.contains("release_build_env() {\n  env -i")
            && manual_lane.contains("HOME=\"$RELEASE_BUILD_HOME\"")
            && manual_lane.contains("CARGO_HOME=\"$RELEASE_BUILD_CARGO_HOME\"")
            && manual_lane.contains("GIT_CONFIG_GLOBAL=/dev/null")
            && manual_lane.contains("GIT_CONFIG_NOSYSTEM=1"),
        "manual build, test, and package commands must use an isolated allowlisted environment"
    );
    let publisher_setup = manual_lane
        .split_once("publisher_env() {")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("record_exact_crates_state() {")
                .map(|(body, _)| body)
        })
        .expect("manual release must define an isolated publisher environment");
    assert!(
        manual_lane.contains("publisher_home=\"$MANUAL_RELEASE_STATE_DIR/publisher-home\"")
            && manual_lane.contains(
                "publisher_cargo_home=\"$MANUAL_RELEASE_STATE_DIR/publisher-cargo-home\"",
            )
            && publisher_setup.starts_with("\n     env -i")
            && publisher_setup.contains("HOME=\"$publisher_home\"")
            && publisher_setup.contains("CARGO_HOME=\"$publisher_cargo_home\"")
            && publisher_setup.contains("GIT_CONFIG_GLOBAL=/dev/null")
            && publisher_setup.contains("publisher_env cargo publish --manifest-path")
            && publisher_setup.contains("--dry-run --locked --registry crates-io")
            && !publisher_setup.contains("env -u CARGO_REGISTRY_TOKEN"),
        "publisher dry-run and configuration proofs must not inherit operator home or credentials"
    );
    let workflow_sha256 = pi::package_manager::hex_encode(&Sha256::digest(workflow.as_bytes()));
    assert!(
        runbook.contains(&workflow_sha256),
        "manual release workflow pin must match the exact reviewed workflow bytes"
    );
    let provider_raw = workflow
        .split_once("          source = r'''")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once(
                    "          '''\n          Path(os.environ[\"PROVIDER_PATH\"]).write_text(source, encoding=\"utf-8\")",
                )
                .map(|(source, _)| source)
        })
        .expect("release workflow must contain one extractable credential provider");
    let mut provider_lines = provider_raw.split_inclusive('\n');
    let provider_header = provider_lines
        .next()
        .expect("credential provider must not be empty");
    assert_eq!(
        provider_header, "#!/usr/bin/env python3\n",
        "credential provider must retain its exact header"
    );
    let mut provider_source = provider_header.to_owned();
    for line in provider_lines {
        if line == "\n" {
            provider_source.push('\n');
        } else {
            provider_source.push_str(
                line.strip_prefix("          ")
                    .expect("credential provider must retain auditable YAML indentation"),
            );
        }
    }
    let provider_sha256 =
        pi::package_manager::hex_encode(&Sha256::digest(provider_source.as_bytes()));
    assert!(
        runbook.contains(&provider_sha256),
        "manual release provider pin must match the exact extracted provider bytes"
    );
    let crates_reconciler = runbook
        .split_once("reconcile_exact_crates_publication() {")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("CRATES_ATTEMPT_ID=")
                .map(|(body, _)| body)
        })
        .expect("manual release runbook must define the exact crates.io reconciler");
    let registry_read = crates_reconciler
        .find("record_exact_crates_state \"$before_state\" 1")
        .expect("crates.io reconciler must query authority before publication");
    let scoped_publish = crates_reconciler
        .find("publish_exact_crate_with_scoped_token \"$actual_receipt\"")
        .expect("crates.io reconciler must use the audited credential handoff");
    assert!(
        registry_read < scoped_publish,
        "manual publication must reconcile first, then use the scoped upload boundary"
    );
    assert!(
        crates_reconciler.contains(
            "set +e\n       (\n         set -euo pipefail\n         publish_exact_crate_with_scoped_token \"$actual_receipt\""
        ),
        "the status-capturing parent must not disable fail-fast semantics inside the credential-scoped publish child"
    );

    let scoped_handoff = manual_lane
        .split_once("publish_exact_crate_with_scoped_token() {")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| suffix.split_once("precrate_ruleset=").map(|(body, _)| body))
        .expect("manual release must define one scoped credential handoff");
    let pipe_write = scoped_handoff
        .find("builtin printf '%s\\n' \"$controller_token\" |")
        .expect("controller must write the token only through an anonymous pipe");
    let clean_child = scoped_handoff[pipe_write..]
        .find("publisher_env \\")
        .map(|offset| pipe_write + offset)
        .expect("token reader must start through the isolated publisher environment");
    let token_read = scoped_handoff[clean_child..]
        .find("IFS= read -r scoped_release_token")
        .map(|offset| clean_child + offset)
        .expect("clean publisher child must read the token from its pipe");
    let token_export = scoped_handoff[token_read..]
        .find("export PI_CRATES_IO_RELEASE_TOKEN=\"$scoped_release_token\"")
        .map(|offset| token_read + offset)
        .expect("clean publisher child must export the token only after reading it");
    let stdin_close = scoped_handoff[token_export..]
        .find("exec 0</dev/null")
        .map(|offset| token_export + offset)
        .expect("Cargo stdin must be detached from the credential pipe");
    let cargo_exec = scoped_handoff[stdin_close..]
        .find("exec cargo publish --manifest-path \"$1\" --locked --no-verify")
        .map(|offset| stdin_close + offset)
        .expect("clean publisher child must exec the no-verify Cargo upload");
    assert!(
        pipe_write < clean_child
            && clean_child < token_read
            && token_read < token_export
            && token_export < stdin_close
            && stdin_close < cargo_exec
            && scoped_handoff.contains("set -euo pipefail"),
        "manual token handoff must remain pipefail-protected and ordered read/export/close/exec"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn manual_release_token_handoff_is_not_argv_and_propagates_publish_failure() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    const FAKE_TOKEN: &str = "fake release token +=:_[]/7391";
    let runbook = require_text("docs/releasing.md");
    let manual_lane = runbook
        .split_once("## Manual DSR lane (no GitHub Actions)")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("## Pre-release flow (rc)")
                .map(|(body, _)| body)
        })
        .expect("manual release lane must have stable section boundaries");
    let helper_start = manual_lane
        .find("   publish_exact_crate_with_scoped_token() {")
        .expect("manual release must define the scoped-token helper");
    let helper_tail = &manual_lane[helper_start..];
    let helper_end = helper_tail
        .find("\n   }\n\n   precrate_ruleset=")
        .map(|offset| offset + "\n   }".len())
        .expect("scoped-token helper must have a stable end boundary");
    let helper = &helper_tail[..helper_end];

    let temp = tempfile::tempdir().expect("create isolated token-handoff fixture");
    let fake_cargo = temp.path().join("cargo");
    let success_receipt = temp.path().join("success.receipt");
    let failure_receipt = temp.path().join("failure.receipt");
    std::fs::write(
        &fake_cargo,
        format!(
            r#"#!/bin/bash
set -euo pipefail
expected_token='fake release token +=:_[]/7391'
test "${{PI_CRATES_IO_RELEASE_TOKEN:-}}" = "$expected_token"
test "${{PI_EXPECTED_CRATE_NAME:-}}" = pi_agent_rust
test "${{PI_EXPECTED_CRATE_VERSION:-}}" = 0.2.0
test "${{PI_EXPECTED_CRATE_SHA256:-}}" = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
test "$#" -eq 11
test "$1" = publish
test "$2" = --manifest-path
test "$4" = --locked
test "$5" = --no-verify
test "$6" = --registry
test "$7" = crates-io
test "$8" = --config
test "${{10}}" = --config
cmdline="$(tr '\0' '\n' < "/proc/$$/cmdline")"
case "$cmdline" in *"$expected_token"*) exit 91 ;; esac
stdin_target="$(readlink "/proc/$$/fd/0")"
test "$stdin_target" = /dev/null
printf 'token_exact=yes\nargv_token=no\nstdin=%s\n' "$stdin_target" \
  > "$PI_CREDENTIAL_RECEIPT"
case "$3" in *failure.toml) exit 47 ;; esac
{empty}"#,
            empty = "",
        ),
    )
    .expect("write fake Cargo executable");
    let mut permissions = std::fs::metadata(&fake_cargo)
        .expect("stat fake Cargo executable")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&fake_cargo, permissions).expect("make fake Cargo executable");

    let harness = format!(
        r#"set -euo pipefail
release_crates_io_token="${{CARGO_REGISTRY_TOKEN:-}}"
test -n "$release_crates_io_token"
export -n release_crates_io_token
unset CARGO_REGISTRY_TOKEN CARGO_REGISTRIES_CRATES_IO_TOKEN PI_CRATES_IO_RELEASE_TOKEN
fixture_dir="$1"
success_receipt="$2"
failure_receipt="$3"
PATH="$fixture_dir:/usr/bin:/bin"
export PATH
RELEASE_VERSION=0.2.0
expected_crate_sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
publisher_cwd="$fixture_dir"
manifest_abs="$fixture_dir/success.toml"
registry_credential_config='registry.credential-provider="/fake/provider"'
named_credential_config='registries.crates-io.credential-provider="/fake/provider"'
release_bash_path=/bin/bash
publisher_env() {{
  local argument
  for argument in "$@"; do
    case "$argument" in
      *"$release_crates_io_token"*) return 93 ;;
    esac
  done
  /usr/bin/env -i PATH="$PATH" LANG=C.UTF-8 LC_ALL=C.UTF-8 "$@"
}}
{helper}
publish_exact_crate_with_scoped_token "$success_receipt"
manifest_abs="$fixture_dir/failure.toml"
set +e
publish_exact_crate_with_scoped_token "$failure_receipt"
failure_status=$?
set -e
test "$failure_status" -eq 47
printf 'failure_status=%s\n' "$failure_status"
"#,
    );
    let output = Command::new("/bin/bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            &harness,
            "bash",
            temp.path().to_str().expect("UTF-8 fixture path"),
            success_receipt.to_str().expect("UTF-8 success receipt"),
            failure_receipt.to_str().expect("UTF-8 failure receipt"),
        ])
        .env_clear()
        .env("CARGO_REGISTRY_TOKEN", FAKE_TOKEN)
        .output()
        .expect("execute scoped-token helper simulation");
    assert!(
        output.status.success(),
        "scoped-token helper simulation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&success_receipt).expect("read success receipt"),
        "token_exact=yes\nargv_token=no\nstdin=/dev/null\n"
    );
    assert_eq!(
        std::fs::read_to_string(&failure_receipt).expect("read failure receipt"),
        "token_exact=yes\nargv_token=no\nstdin=/dev/null\n"
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("simulation stdout must be UTF-8"),
        "failure_status=47\n"
    );
}

#[test]
fn manual_release_reconciliation_binds_durable_identity_and_live_asset_bytes() {
    let runbook = require_text("docs/releasing.md");
    assert!(
        runbook.contains(
            "release_identity_receipt=\"$MANUAL_RELEASE_STATE_DIR/github-release-identity.json\""
        ),
        "manual release must retain a durable GitHub release identity receipt"
    );

    let verifier = runbook
        .split_once("verify_exact_release() {")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("reconcile_exact_github_publication() {")
                .map(|(body, _)| body)
        })
        .expect("manual release runbook must define the exact GitHub verifier");
    assert!(
        verifier.contains("--arg target_commitish \"$recorded_target_commitish\"")
            && verifier.contains(".target_commitish == $target_commitish"),
        "API target_commitish metadata must bind to the durable identity receipt"
    );
    assert!(
        !verifier.contains("--arg target \"$expected_source_commit\"")
            && !verifier.contains("upload_receipts"),
        "verifier must not misuse ignored target_commitish metadata or a function-local upload path"
    );
    assert!(
        verifier.contains("test \"$remote_tag_object\" = \"$local_tag_object\"")
            && verifier.contains("test \"$remote_tag_commit\" = \"$expected_source_commit\"")
            && verifier.contains("cmp \"$local_asset\" \"$downloaded_asset\""),
        "annotated tag identity, peeled commit, and authoritative downloaded bytes must be proved"
    );

    let publication = runbook
        .split_once("reconcile_exact_github_publication() {")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| suffix.split_once("```\n").map(|(body, _)| body))
        .expect("manual release runbook must define public-state reconciliation");
    let state_read = publication
        .find("github-release-before-publication.json")
        .expect("publication reconciler must read current state first");
    let draft_guard = publication
        .find("if test \"$current_draft\" = true; then")
        .expect("publication reconciler must mutate only an exact draft");
    let patch = publication
        .find("gh api --method PATCH")
        .expect("publication reconciler must retain an explicit PATCH boundary");
    assert!(
        state_read < draft_guard && draft_guard < patch,
        "publication retry must inspect authority before deciding whether to PATCH"
    );
    assert!(
        publication.contains("verify_exact_release false \"after-public-${attempt_id}\""),
        "public state must be authoritatively reverified after an attempted or adopted transition"
    );
}

#[test]
fn manual_release_lane_is_actions_independent_and_preserves_ambiguous_crates_state() {
    let runbook = require_text("docs/releasing.md");
    let manual_lane = runbook
        .split_once("## Manual DSR lane (no GitHub Actions)")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("## Pre-release flow (rc)")
                .map(|(body, _)| body)
        })
        .expect("manual release lane must have stable section boundaries");

    for forbidden in [
        "/actions/",
        "gh run",
        "WORKFLOW_BASELINE",
        "verify_workflow_baseline_unchanged",
        "workflow_runs",
        "run_attempt",
    ] {
        assert!(
            !manual_lane.contains(forbidden),
            "manual no-Actions lane must not depend on {forbidden}"
        );
    }
    assert!(
        manual_lane.contains("reconcile_exact_crates_publication() {")
            && manual_lane.contains("crates_reconcile_status=0")
            && manual_lane.contains("crates_reconcile_status=$?")
            && !manual_lane.contains("crates_reconcile_pid=$!")
            && !manual_lane.contains("wait \"$crates_reconcile_pid\"")
            && manual_lane.contains("crates-publication-unresolved.txt")
            && manual_lane.contains("unset release_crates_io_token")
            && !manual_lane
                .contains("if (\n     set -euo pipefail\n     reconcile_exact_crates_publication"),
        "crates publication must use a foreground fail-fast child with durable unresolved state"
    );
    let evidence_commit = manual_lane
        .find("git commit -m \"Record ${RELEASE_TAG} release evidence [skip actions]\"")
        .expect("manual lane must create an explicitly skipped evidence commit");
    let evidence_subject_check = manual_lane[evidence_commit..]
        .find("release-evidence HEAD lacks [skip actions]")
        .map(|offset| evidence_commit + offset)
        .expect("manual lane must verify the resulting evidence-commit subject");
    let branch_push = manual_lane
        .find("origin_push_guarded \\\n     refs/heads/main:refs/heads/main")
        .expect("manual lane must retain its guarded branch push");
    let branch_subject_check = manual_lane[..branch_push]
        .rfind("branch-push HEAD lacks [skip actions]")
        .expect("manual lane must reverify the commit marker immediately before branch push");
    let local_tag = manual_lane
        .find("git tag -a \"$RELEASE_TAG\"")
        .expect("manual lane must create the annotated release tag");
    let tag_subject_check = manual_lane[..local_tag]
        .rfind("tag source lacks [skip actions]")
        .expect("manual lane must verify the tagged commit marker before tag creation");
    assert!(
        evidence_commit < evidence_subject_check
            && evidence_subject_check < branch_subject_check
            && branch_subject_check < branch_push
            && branch_push < tag_subject_check
            && tag_subject_check < local_tag,
        "source, evidence, branch-push, and tag boundaries must all retain [skip actions]"
    );
    let reconciliation_call = manual_lane
        .rfind("reconcile_exact_crates_publication \\")
        .expect("manual lane must invoke the reconciler inside its foreground child");
    let crates_foreground_sequence = concat!(
        "set +e\n",
        "   (\n",
        "     set -euo pipefail\n",
        "     reconcile_exact_crates_publication \\\n",
        "       \"$CRATES_ATTEMPT_ID\" \"$crates_attempt_dir\"\n",
        "   )\n",
        "   crates_reconcile_status=$?\n",
        "   set -e",
    );
    let foreground_child = manual_lane
        .find(crates_foreground_sequence)
        .expect("crates reconciler must have an exact foreground status boundary");
    let captured_status = manual_lane[reconciliation_call..]
        .find("crates_reconcile_status=$?")
        .map(|offset| reconciliation_call + offset)
        .expect("parent must capture the crates child exit status");
    let token_clear = manual_lane[reconciliation_call..]
        .find("unset release_crates_io_token")
        .expect("successful reconciliation must clear the parent-shell token");
    let unresolved_receipt = manual_lane[reconciliation_call..]
        .find("crates-publication-unresolved.txt")
        .expect("failed reconciliation must preserve a durable unresolved receipt");
    assert!(
        foreground_child < reconciliation_call
            && reconciliation_call < captured_status
            && token_clear < unresolved_receipt,
        "foreground child, captured status, success, and unresolved branches must remain explicit and ordered"
    );
    assert!(
        !crates_foreground_sequence.contains(") &") && !crates_foreground_sequence.contains("$!"),
        "irreversible crates reconciliation must not outlive the controller"
    );
    assert!(
        manual_lane.contains("assert_origin_push_disabled() {")
            && manual_lane.contains("git push --atomic \"$release_remote_url\" \"$@\"")
            && !manual_lane.contains("enable_origin_push")
            && !manual_lane.contains("git remote set-url --push origin \"$release_remote_url\"",),
        "the persistent origin push URL must remain guarded across every push attempt"
    );

    let github_publication = manual_lane
        .find("PUBLICATION_ATTEMPT_ID=\"$(uuidgen")
        .expect("manual lane must retain the GitHub publication boundary");
    let successful_crates_gate = manual_lane[..github_publication]
        .rfind("test \"$crates_reconcile_status\" -eq 0")
        .expect("GitHub publication must reassert successful crates reconciliation");
    let successful_receipt_gate = manual_lane[..github_publication]
        .rfind("crates-publication-reconciliation.txt")
        .expect("GitHub publication must require the successful crates receipt");
    assert!(
        reconciliation_call < successful_crates_gate
            && successful_crates_gate < successful_receipt_gate
            && successful_receipt_gate < github_publication,
        "successful crates state and its durable receipt must gate the GitHub public transition"
    );
    assert!(
        manual_lane[successful_receipt_gate..github_publication]
            .contains("grep -Fxc 'registry_state=exact'"),
        "the successful crates receipt gate must require one exact registry-state line"
    );
    assert!(
        manual_lane.contains("release_bash_path=\"$(operator_tool_path bash)\"")
            && manual_lane.contains("release_bwrap_path=\"$(operator_tool_path bwrap)\"")
            && manual_lane.contains("release_git_path=\"$(operator_tool_path git)\"")
            && manual_lane.contains("release_sha256sum_path=\"$(operator_tool_path sha256sum)\"")
            && manual_lane.contains("\"$release_bash_path\" --noprofile --norc -c")
            && manual_lane.matches("\"$release_bwrap_path\" \\").count() == 2
            && manual_lane.contains("' bash \"$release_git_path\" \"$source_commit\"")
            && manual_lane.contains("' bash \"$release_sha256sum_path\" \"$preserved_inputs\"")
            && !manual_lane.contains("/usr/local/bin/git")
            && !manual_lane.contains("command -v bwrap")
            && !manual_lane.contains("\n     bwrap --die-with-parent")
            && !manual_lane.contains("/usr/bin/bash --noprofile --norc -c")
            && !manual_lane.contains("/usr/bin/sha256sum --check"),
        "bubblewrap checkpoints must execute the exact reverified operator-tool paths"
    );
    assert!(
        manual_lane
            .contains("sed find chmod head tail tee tr cat mkdir env uname df nproc sysctl ubs br")
            && manual_lane.contains(
                "rg timeout base64 flock mv od basename sleep cp paste am bv cut dd fd mkfifo"
            )
            && manual_lane
                .contains("pgrep ps rch rm sh tmux touch which install rmdir xz yes ls seq whoami"),
        "operator-tool inventory must cover audited pre-existing E2E and quality-gate subprocesses"
    );
    assert!(
        manual_lane
            .contains("This operator-tool receipt does **not** claim complete transitive process",)
            && manual_lane.contains(
                "descendants selected internally by Cargo,\nrustc, native linker drivers",
            )
            && manual_lane
                .contains("fixture executables generated\ninside isolated test directories",)
            && manual_lane.contains("no byte-identity claim for those excluded descendants",)
            && !manual_lane.contains("record every controller tool\nused by a release gate"),
        "the operator-tool receipt must not overclaim complete compiler or fixture process closure"
    );
    assert!(
        manual_lane.contains("bin-sh usr-bin-node home-bun home-bun-node bin-bash bin-echo",)
            && manual_lane.contains(
                "/bin/sh /usr/bin/node /home/ubuntu/.bun/bin/bun /home/ubuntu/.bun/bin/node",
            )
            && manual_lane.contains("/bin/bash /bin/echo")
            && manual_lane.contains("record_operator_tool() {")
            && manual_lane.contains("printf '%s\\t%s\\t%s\\t%s\\n'")
            && manual_lane.contains("'$1 == tool { print $4 }'")
            && manual_lane
                .matches("builtin type -P -- \"$release_tool\"")
                .count()
                == 3
            && manual_lane
                .matches("builtin type -t -- \"$release_tool\"")
                .count()
                == 2
            && manual_lane.matches("builtin hash -r").count() == 1
            && manual_lane.contains("[[ \"$requested_path\" == /* ]]")
            && manual_lane.contains(
                "[[ \"$expected_requested_path\" == /* && \"$expected_resolved_path\" == /* ]]",
            )
            && manual_lane.contains("[[ -n \"${BASH_VERSION:-}\" && \"$-\" == *p* ]]")
            && manual_lane.contains("builtin unset BASH_ENV ENV CDPATH GLOBIGNORE")
            && manual_lane.contains("[[ ! -v BASH_ENV && ! -v ENV ]]")
            && manual_lane.contains("done < \"/proc/$$/environ\"")
            && manual_lane.contains("release_path_descendant_tool_names=(kill)")
            && manual_lane.contains(
                "record_operator_tool \"path-$release_tool\" \"$release_tool_requested_path\""
            )
            && manual_lane.contains("release_realpath_path=\"$(operator_tool_path realpath)\"")
            && manual_lane.contains(
                "release_controller_bash=\"$(\"$release_realpath_path\" -e -- \"/proc/$$/exe\")\"",
            )
            && manual_lane.contains("test \"$release_controller_bash\" = \"$release_bash_path\"")
            && manual_lane.contains("$(builtin pwd -P)")
            && manual_lane.contains("The `path-kill` row separately binds the external"),
        "operator-tool receipts must bind PATH and hard-coded entrypoints to their resolved targets"
    );

    let clean_launch = manual_lane
        .find("exec /bin/bash\n--noprofile --norc -p")
        .expect("manual lane must launch one clean privileged Bash controller");
    let fail_fast = manual_lane
        .find("```bash\nset -euo pipefail\nset +x\numask 077")
        .expect("controller must become fail-fast before any hygiene assertion");
    let privileged_check = manual_lane
        .find("[[ -n \"${BASH_VERSION:-}\" && \"$-\" == *p* ]]")
        .expect("controller must verify privileged Bash mode");
    let hash_clear = manual_lane
        .find("builtin hash -r")
        .expect("controller must clear any inherited command cache once");
    let hash_disable = manual_lane
        .find("builtin set +h")
        .expect("controller must disable command hashing");
    let alias_disable = manual_lane
        .find("builtin shopt -u expand_aliases")
        .expect("controller must disable aliases");
    let raw_function_guard = manual_lane
        .find("done < \"/proc/$$/environ\"")
        .expect("controller must reject raw exported Bash functions");
    let environment_unset = manual_lane
        .find("builtin unset BASH_ENV ENV CDPATH GLOBIGNORE")
        .expect("controller must clear child-shell startup controls");
    let environment_absent = manual_lane
        .find("[[ ! -v BASH_ENV && ! -v ENV ]]")
        .expect("controller must prove startup controls absent");
    let function_shadow_guard = manual_lane
        .find("if builtin declare -F \"$release_tool\" >/dev/null; then")
        .expect("controller must reject live tool-shadowing functions");
    let token_capture = manual_lane
        .find(
            "release_crates_io_token=\"${CARGO_REGISTRY_TOKEN:-${CARGO_REGISTRIES_CRATES_IO_TOKEN:-}}\"",
        )
        .expect("controller must capture the registry credential");
    let token_unset = manual_lane
        .find("builtin unset CARGO_REGISTRY_TOKEN CARGO_REGISTRIES_CRATES_IO_TOKEN")
        .expect("controller must clear exported credential spellings");
    let first_path_lookup = manual_lane
        .find("release_cargo_entrypoint=\"$(builtin type -P -- cargo)\"")
        .expect("controller must defer PATH resolution until credentials are narrowed");
    let operator_receipt = manual_lane
        .find("release_tool_receipt=\"$MANUAL_RELEASE_STATE_DIR/operator-tools.tsv\"")
        .expect("controller must create its operator-tool receipt");
    let verified_binding = manual_lane
        .find("verify_operator_tools\nrelease_bash_path=\"$(operator_tool_path bash)\"")
        .expect("controller must verify tools before binding its running Bash");
    let controller_resolution = manual_lane
        .find("release_controller_bash=\"$(\"$release_realpath_path\" -e -- \"/proc/$$/exe\")\"")
        .expect("controller must resolve its running executable through a verified tool");
    let controller_binding = manual_lane
        .find("test \"$release_controller_bash\" = \"$release_bash_path\"")
        .expect("controller must equal the receipted Bash executable");
    assert!(
        clean_launch < fail_fast
            && fail_fast < privileged_check
            && privileged_check < hash_clear
            && hash_clear < hash_disable
            && hash_disable < alias_disable
            && alias_disable < raw_function_guard
            && raw_function_guard < environment_unset
            && environment_unset < environment_absent
            && environment_absent < function_shadow_guard
            && function_shadow_guard < token_capture
            && token_capture < token_unset
            && token_unset < first_path_lookup
            && first_path_lookup < operator_receipt
            && operator_receipt < verified_binding
            && verified_binding < controller_resolution
            && controller_resolution < controller_binding,
        "clean-shell, credential, tool-receipt, and running-Bash bindings must remain ordered"
    );
}

#[test]
fn manual_release_controller_preamble_rejects_dispatch_shadowing() {
    let runbook = require_text("docs/releasing.md");
    let manual_lane = runbook
        .split_once("## Manual DSR lane (no GitHub Actions)")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("## Pre-release flow (rc)")
                .map(|(body, _)| body)
        })
        .expect("manual release lane must have stable section boundaries");
    let preamble = manual_lane
        .split_once("```bash\n")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("export RUSTUP_TOOLCHAIN")
                .map(|(body, _)| body)
        })
        .expect("manual controller preamble must precede toolchain export");

    let run = |script: &str, raw_function: bool| {
        let mut command = std::process::Command::new("/bin/bash");
        command
            .args(["--noprofile", "--norc", "-p", "-c", script])
            .env_clear();
        if raw_function {
            // The value is a literal bash exported-function body; the braces
            // are shell syntax, not Rust formatting placeholders.
            #[allow(clippy::literal_string_with_formatting_args)]
            {
                command.env("BASH_FUNC_git%%", "() {  :\n}");
            }
        }
        command.output().expect("execute controller preamble")
    };

    let clean = run(preamble, false);
    assert!(
        clean.status.success(),
        "clean controller preamble failed: {}",
        String::from_utf8_lossy(&clean.stderr)
    );

    let function_shadow = run(&format!("git() {{ :; }}\n{preamble}"), false);
    assert!(
        !function_shadow.status.success(),
        "tool-shadowing function must fail the controller preamble"
    );

    let alias_shadow = run(&format!("alias git='printf shadowed'\n{preamble}"), false);
    assert!(
        !alias_shadow.status.success(),
        "tool-shadowing alias must fail the controller preamble"
    );

    let raw_function = run(preamble, true);
    assert!(
        !raw_function.status.success()
            && String::from_utf8_lossy(&raw_function.stderr)
                .contains("refusing exported shell function environment"),
        "raw exported function must fail before any controller subprocess"
    );
}

#[test]
fn manual_release_retries_use_fresh_attempts_and_exact_success_receipts() {
    let runbook = require_text("docs/releasing.md");
    let manual_lane = runbook
        .split_once("## Manual DSR lane (no GitHub Actions)")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("## Pre-release flow (rc)")
                .map(|(body, _)| body)
        })
        .expect("manual release lane must have stable section boundaries");

    for rooted_path in [
        "MANUAL_RELEASE_STATE_DIR=\"$MANUAL_RELEASE_ROOT/state\"",
        "release_checkout=\"$MANUAL_RELEASE_ROOT/checkout\"",
        "PRESERVED_DSR_STATE_DIR=\"$MANUAL_RELEASE_ROOT/dsr-state-$DSR_BUILD_RUN_ID\"",
        "RAW_RELEASE_DIR=\"$MANUAL_RELEASE_ROOT/raw-assets-$DSR_BUILD_RUN_ID\"",
        "release_cargo_parent=\"$MANUAL_RELEASE_STATE_DIR/controller-cargo\"",
        "publisher_home=\"$MANUAL_RELEASE_STATE_DIR/publisher-home\"",
        "publisher_cargo_home=\"$MANUAL_RELEASE_STATE_DIR/publisher-cargo-home\"",
        "publisher_target_dir=\"$MANUAL_RELEASE_STATE_DIR/publisher-target\"",
        "publisher_tmp_dir=\"$MANUAL_RELEASE_STATE_DIR/publisher-tmp\"",
        "smoke_attempt_dir=\"$MANUAL_RELEASE_STATE_DIR/target-smoke-$SMOKE_ATTEMPT_ID\"",
        "post_boundary_attempt_dir=\"$MANUAL_RELEASE_STATE_DIR/post-boundary-$POST_BOUNDARY_ATTEMPT_ID\"",
        "crates_attempt_dir=\"$MANUAL_RELEASE_STATE_DIR/crates-$CRATES_ATTEMPT_ID\"",
        "publication_attempt_dir=\"$MANUAL_RELEASE_STATE_DIR/publication-$PUBLICATION_ATTEMPT_ID\"",
        "installer_root=\"$MANUAL_RELEASE_STATE_DIR/post-public-installer-linux-amd64\"",
    ] {
        assert!(
            manual_lane.contains(rooted_path),
            "isolated manual release root is missing {rooted_path}"
        );
    }
    assert!(
        !manual_lane.contains("/data/tmp/pi-v0.2.0-dsr-state-")
            && !manual_lane.contains("/data/tmp/pi-v0.2.0-raw-assets-"),
        "mutable DSR state and raw outputs must not escape the isolated release root"
    );

    let smoke_stage = manual_lane
        .split_once("run_target_runtime_smoke_attempt() (")
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .split_once("reconcile_post_boundary_attempt() (")
                .map(|(body, _)| body)
        })
        .expect("target-runtime smoke stage must be isolated from the remote boundary");
    for required in [
        "set -euo pipefail",
        "$MANUAL_RELEASE_STATE_DIR/target-smoke-$SMOKE_ATTEMPT_ID",
        concat!("$", "{attempt_id}-$", "{label}"),
        "${attempt_id}-windows-amd64",
        "$attempt_dir/smoke-linux-amd64.txt",
        "$attempt_dir/smoke-linux-arm64-qemu-emulated.txt",
        "$attempt_dir/smoke-darwin-amd64.txt",
        "$attempt_dir/smoke-darwin-arm64.txt",
        "$attempt_dir/smoke-windows-amd64.txt",
        "test \"${#SMOKE_RECEIPTS[@]}\" = 5",
        "smoke_attempt_limit=3",
        "run_target_runtime_smoke_attempt \\",
        "smoke_attempt_pid=$!",
        "wait \"$smoke_attempt_pid\"",
        "target-runtime-smokes-unresolved.txt",
        "target-runtime-smokes-success.txt",
        "canonical_smoke_proof=\"$MANUAL_RELEASE_STATE_DIR/target-runtime-smokes.sha256\"",
    ] {
        assert!(
            smoke_stage.contains(required),
            "target smoke retry contract is missing {required}"
        );
    }
    assert!(
        !smoke_stage.contains("if run_target_runtime_smoke_attempt")
            && !smoke_stage.contains("&& run_target_runtime_smoke_attempt")
            && !smoke_stage.contains("|| run_target_runtime_smoke_attempt"),
        "the fail-fast smoke child must not execute in a Bash conditional context"
    );
    for destructive in ["rm ", "unlink ", "Remove-Item", "git clean"] {
        assert!(
            !smoke_stage.contains(destructive),
            "target-smoke recovery must retain partial state, but found {destructive}"
        );
    }
    let canonical_proof = smoke_stage
        .find("canonical_smoke_proof=\"$MANUAL_RELEASE_STATE_DIR/target-runtime-smokes.sha256\"")
        .expect("successful smoke proof must be promoted canonically");
    let success_receipt = smoke_stage
        .find(
            "smoke_success_receipt=\"$MANUAL_RELEASE_STATE_DIR/target-runtime-smoke-success.txt\"",
        )
        .expect("successful smoke attempt must retain an exact root receipt");
    assert!(
        canonical_proof < success_receipt
            && smoke_stage[canonical_proof..success_receipt]
                .contains("sha256sum --check --strict \"$canonical_smoke_proof\""),
        "the exact five-receipt proof must validate before success is recorded"
    );

    for (label, sequence, status, unresolved, success) in [
        (
            "post-boundary tag/draft",
            concat!(
                "set +e\n",
                "   (\n",
                "     set -euo pipefail\n",
                "     reconcile_post_boundary_attempt \\\n",
                "       \"$POST_BOUNDARY_ATTEMPT_ID\" \"$post_boundary_attempt_dir\"\n",
                "   )\n",
                "   post_boundary_reconcile_status=$?\n",
                "   set -e",
            ),
            "post_boundary_reconcile_status=$?",
            "post-boundary-unresolved.txt",
            "post-boundary-reconciliation.txt",
        ),
        (
            "final GitHub publication",
            concat!(
                "set +e\n",
                "   (\n",
                "     set -euo pipefail\n",
                "     reconcile_final_publication_attempt \\\n",
                "       \"$PUBLICATION_ATTEMPT_ID\" \"$publication_attempt_dir\" \\\n",
                "       \"$successful_crates_receipt\"\n",
                "   )\n",
                "   publication_reconcile_status=$?\n",
                "   set -e",
            ),
            "publication_reconcile_status=$?",
            "publication-attempt-unresolved.txt",
            "publication-attempt-success.txt",
        ),
    ] {
        let sequence_position = manual_lane
            .find(sequence)
            .unwrap_or_else(|| panic!("{label} must use the exact foreground child boundary"));
        let status_position = sequence_position
            + sequence
                .find(status)
                .unwrap_or_else(|| panic!("{label} sequence must capture child status"));
        assert!(
            sequence_position < status_position
                && manual_lane[status_position..].contains(unresolved)
                && manual_lane[status_position..].contains(success)
                && !sequence.contains(") &")
                && !sequence.contains("$!"),
            "{label} must retain separate exact and unresolved attempt receipts"
        );
    }

    assert!(
        manual_lane.contains("test \"$post_boundary_reconcile_status\" -eq 0")
            && manual_lane.contains("test \"$publication_reconcile_status\" -eq 0"),
        "later irreversible steps must require exact successful retry receipts"
    );
    for retained_installer_control in [
        "TMPDIR=\"$installer_root/tmp\"",
        "PI_INSTALLER_RETAIN_TEMP=1",
        "PI_INSTALLER_LOCK_DIR=\"$installer_lock\"",
        "test -d \"$installer_lock\" && test ! -L \"$installer_lock\"",
        "test -f \"$installer_lock/pid\" && test ! -L \"$installer_lock/pid\"",
        "Retaining installer temporary directory:",
        "Retaining installer lock directory: $installer_lock",
    ] {
        assert!(
            manual_lane.contains(retained_installer_control),
            "post-public installer proof must retain {retained_installer_control}"
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
        agents.contains("<48 MiB") && readme.contains("48.0 MiB"),
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
    "4e24380af0ca4fe8fd94850d63e607868d15d704a42d434bdb1c762e7e327663";
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

fn perf_measurement_control_failure_ids(budget_name: &str) -> Option<&'static [&'static str]> {
    match budget_name {
        "binary_size_release" => Some(&[
            "missing_binary_size_measurement_control",
            "invalid_binary_size_measurement_control",
        ]),
        "idle_memory_rss" => Some(&[
            "missing_idle_rss_measurement_control",
            "invalid_idle_rss_measurement_control",
        ]),
        "ext_cold_load_simple_p95" | "ext_cold_load_complex_p95" => Some(&[
            "missing_cold_load_measurement_control",
            "invalid_cold_load_measurement_control",
            "noisy_cold_load_measurement_control",
        ]),
        _ => None,
    }
}

fn perf_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn perf_proof_field<'a>(fields: &'a [&str], index: usize, key: &str) -> Result<&'a str, String> {
    fields
        .get(index)
        .and_then(|field| field.strip_prefix(key))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("measurement negative-control proof requires {key}<value>"))
}

fn validate_perf_measurement_control_source(
    budget_name: &str,
    source: &str,
    actual: f64,
) -> Result<(), String> {
    let (artifact_source, proof) = source.split_once("#control=").ok_or_else(|| {
        format!(
            "budget result {budget_name} with data lacks its required measurement negative-control proof"
        )
    })?;
    if artifact_source.is_empty() || artifact_source.contains("#control=") {
        return Err(format!(
            "budget result {budget_name} has a malformed measurement artifact source"
        ));
    }
    let fields: Vec<_> = proof.split(';').collect();
    match budget_name {
        "binary_size_release" => {
            if fields.len() != 7
                || fields[0] != "release_binary_v1"
                || fields[4] != "profile=release"
                || fields[5] != "opt_level=z"
                || fields[6] != "strip=true"
            {
                return Err("binary_size_release has a malformed measurement proof".to_string());
            }
            let control_sha256 = perf_proof_field(&fields, 1, "control_sha256=")?;
            let binary_sha256 = perf_proof_field(&fields, 2, "binary_sha256=")?;
            let size_bytes = perf_proof_field(&fields, 3, "size_bytes=")?
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| "binary_size_release size_bytes must be positive".to_string())?;
            if !perf_lower_sha256(control_sha256) || !perf_lower_sha256(binary_sha256) {
                return Err(
                    "binary_size_release proof hashes must be lowercase SHA-256".to_string()
                );
            }
            let measured_mb = size_bytes as f64 / 1024.0 / 1024.0;
            if (actual - measured_mb).abs() > 1e-12 {
                return Err(
                    "binary_size_release actual does not match negative-control size_bytes"
                        .to_string(),
                );
            }
        }
        "idle_memory_rss" => {
            if fields.len() != 7 || fields[0] != "idle_rss_v1" || fields[3] != "process=pi" {
                return Err("idle_memory_rss has a malformed measurement proof".to_string());
            }
            let control_sha256 = perf_proof_field(&fields, 1, "control_sha256=")?;
            perf_proof_field(&fields, 2, "pid=")?
                .parse::<u32>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| "idle_memory_rss pid must be positive".to_string())?;
            let allocator = perf_proof_field(&fields, 4, "allocator=")?;
            let binary_sha256 = perf_proof_field(&fields, 5, "binary_sha256=")?;
            let rss_bytes = perf_proof_field(&fields, 6, "rss_bytes=")?
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| "idle_memory_rss rss_bytes must be positive".to_string())?;
            if !matches!(allocator, "system" | "jemalloc")
                || !perf_lower_sha256(control_sha256)
                || !perf_lower_sha256(binary_sha256)
            {
                return Err("idle_memory_rss proof identity is invalid".to_string());
            }
            let measured_mb = rss_bytes as f64 / 1024.0 / 1024.0;
            if (actual - measured_mb).abs() > 1e-12 {
                return Err(
                    "idle_memory_rss actual does not match negative-control rss_bytes".to_string(),
                );
            }
        }
        "ext_cold_load_simple_p95" | "ext_cold_load_complex_p95" => {
            if fields.len() != 8 || fields[0] != "bench_env_v1" {
                return Err(format!("{budget_name} has a malformed measurement proof"));
            }
            for (index, key) in [
                (1, "control_sha256="),
                (2, "artifact_sha256="),
                (3, "bench_env_sha256="),
            ] {
                if !perf_lower_sha256(perf_proof_field(&fields, index, key)?) {
                    return Err(format!(
                        "{budget_name} proof hashes must be lowercase SHA-256"
                    ));
                }
            }
            let noise_score = perf_proof_field(&fields, 4, "noise_score=")?
                .parse::<u8>()
                .map_err(|_| format!("{budget_name} noise_score must be an integer"))?;
            for (index, key) in [(5, "governor="), (6, "aslr="), (7, "thp=")] {
                perf_proof_field(&fields, index, key)?;
            }
            if noise_score != 0 {
                return Err(format!(
                    "budget result {budget_name} admits a noisy bench-env fingerprint"
                ));
            }
        }
        _ => {}
    }
    Ok(())
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
    Ok(pi::package_manager::hex_encode(&Sha256::digest(
        canonical.as_bytes(),
    )))
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
    let mut missing_measurement_controls = HashSet::new();
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
        let source = perf_nonempty_string(&object["source"], &format!("{label}.source"))?;

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
            if perf_measurement_control_failure_ids(name).is_some() {
                missing_measurement_controls.insert(name.to_string());
                if status != "NO_DATA" || failure_reason.is_some() {
                    return Err(format!(
                        "budget {name} without a verified measurement control must be NO_DATA without a failure reason"
                    ));
                }
            } else if strict_mode && definition.ci_enforced {
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
            if perf_measurement_control_failure_ids(name).is_some() {
                validate_perf_measurement_control_source(name, source, actual)?;
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
    let mut failure_ids_by_budget: HashMap<String, HashSet<String>> = HashMap::new();
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
                failure_ids_by_budget
                    .entry(name.to_string())
                    .or_default()
                    .insert(contract_id.to_string());
                Some(name)
            }
        };
        if !failure_fingerprints.insert((contract_id, detail, remediation, budget_name)) {
            return Err(format!("duplicate data-contract failure at index {index}"));
        }
    }

    for budget_name in missing_measurement_controls {
        let allowed_ids = perf_measurement_control_failure_ids(&budget_name)
            .expect("measurement-control budget was classified above");
        let has_named_failure = failure_ids_by_budget
            .get(&budget_name)
            .is_some_and(|observed| {
                allowed_ids
                    .iter()
                    .any(|allowed| observed.contains(*allowed))
            });
        if !has_named_failure {
            return Err(format!(
                "budget result {budget_name} without data lacks a named measurement-control failure"
            ));
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

    if now.signed_duration_since(generated_at) > maximum_age {
        return Err("performance summary is too stale for release admission".to_string());
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
) -> Result<Vec<u8>, String> {
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
    Ok(head_bytes)
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
) -> Result<Vec<u8>, String>
where
    F: FnOnce() -> Result<(), String>,
{
    let context = performance_git_context(root)?;
    let head = perf_git_stdout_at(&context, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    validate_performance_checkout_clean(&context)?;
    let initial_artifact_bytes =
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
    validate_performance_checkout_clean(&context)?;
    let final_artifact_bytes =
        validate_performance_artifact_at_head(&context, artifact_path, &head)?;
    if final_artifact_bytes != initial_artifact_bytes {
        return Err(
            "performance summary bytes changed during source binding validation".to_string(),
        );
    }
    validate_performance_checkout_clean(&context)?;
    let final_head = perf_git_stdout_at(&context, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if final_head != head {
        return Err(
            "performance repository HEAD changed during source binding validation".to_string(),
        );
    }
    Ok(final_artifact_bytes)
}

fn validate_performance_source_binding_at(
    root: &Path,
    artifact_path: &str,
    source_commit: &str,
) -> Result<Vec<u8>, String> {
    validate_performance_source_binding_at_with_finalizer(
        root,
        artifact_path,
        source_commit,
        || Ok(()),
    )
}

fn validate_performance_head_binding_at(
    root: &Path,
    artifact_path: &str,
) -> Result<Vec<u8>, String> {
    let context = performance_git_context(root)?;
    let head = perf_git_stdout_at(&context, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    validate_performance_checkout_clean(&context)?;
    let initial_artifact_bytes =
        validate_performance_artifact_at_head(&context, artifact_path, &head)?;
    validate_performance_checkout_clean(&context)?;
    let final_artifact_bytes =
        validate_performance_artifact_at_head(&context, artifact_path, &head)?;
    if final_artifact_bytes != initial_artifact_bytes {
        return Err("performance summary bytes changed during HEAD binding validation".to_string());
    }
    validate_performance_checkout_clean(&context)?;
    let final_head = perf_git_stdout_at(&context, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if final_head != head {
        return Err(
            "performance repository HEAD changed during HEAD binding validation".to_string(),
        );
    }
    Ok(final_artifact_bytes)
}

fn load_source_bound_performance_summary_at_with_probe<F>(
    root: &Path,
    artifact_path: &str,
    after_unbound_probe: F,
) -> Result<(Value, bool), String>
where
    F: FnOnce() -> Result<(), String>,
{
    let context = performance_git_context(root)?;
    let full_path = contained_regular_artifact_path(&context, artifact_path)?;
    let probed_bytes = std::fs::read(&full_path)
        .map_err(|err| format!("performance summary probe is unreadable: {err}"))?;
    let probed_summary = parse_release_json(&probed_bytes)
        .map_err(|err| format!("performance summary probe is invalid JSON: {err}"))?;
    let probed_source_commit = probed_summary
        .get("source_commit")
        .and_then(Value::as_str)
        .map(str::to_string);

    after_unbound_probe()?;

    let (bound_bytes, source_binding_valid) =
        if let Some(source_commit) = probed_source_commit.as_deref() {
            (
                validate_performance_source_binding_at(root, artifact_path, source_commit)?,
                true,
            )
        } else {
            (
                validate_performance_head_binding_at(root, artifact_path)?,
                false,
            )
        };
    if bound_bytes != probed_bytes {
        return Err(
            "performance summary changed between its initial parse and source binding".to_string(),
        );
    }
    let bound_summary = parse_release_json(&bound_bytes)
        .map_err(|err| format!("source-bound performance summary is invalid JSON: {err}"))?;
    if probed_source_commit.as_deref().is_some()
        && bound_summary.get("source_commit").and_then(Value::as_str)
            != probed_source_commit.as_deref()
    {
        return Err(
            "source-bound performance summary disagrees with its probed source_commit".to_string(),
        );
    }
    Ok((bound_summary, source_binding_valid))
}

fn load_source_bound_performance_summary() -> Result<(Value, bool), String> {
    load_source_bound_performance_summary_at_with_probe(
        &repo_root(),
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        || Ok(()),
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
    let measurement_control_failures = [
        (
            "binary_size_release",
            "missing_binary_size_measurement_control",
        ),
        ("idle_memory_rss", "missing_idle_rss_measurement_control"),
        (
            "ext_cold_load_simple_p95",
            "missing_cold_load_measurement_control",
        ),
        (
            "ext_cold_load_complex_p95",
            "missing_cold_load_measurement_control",
        ),
    ]
    .into_iter()
    .map(|(budget_name, contract_id)| {
        json!({
            "contract_id": contract_id,
            "budget_name": budget_name,
            "detail": "measurement control missing",
            "remediation": "regenerate the measurement control"
        })
    })
    .collect::<Vec<_>>();
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
        "data_contract_failures_count": measurement_control_failures.len(),
        "failing_data_contracts": measurement_control_failures,
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
        let proof_hash = "b".repeat(64);
        match result["budget_name"].as_str() {
            Some("binary_size_release") => {
                result["source"] = json!(format!(
                    "fixture://release/pi#control=release_binary_v1;control_sha256={proof_hash};binary_sha256={proof_hash};size_bytes=50331648;profile=release;opt_level=z;strip=true"
                ));
            }
            Some("idle_memory_rss") => {
                result["source"] = json!(format!(
                    "fixture://idle-rss#control=idle_rss_v1;control_sha256={proof_hash};pid=4242;process=pi;allocator=system;binary_sha256={proof_hash};rss_bytes=52428800"
                ));
            }
            Some("ext_cold_load_simple_p95" | "ext_cold_load_complex_p95") => {
                result["source"] = json!(format!(
                    "fixture://criterion#control=bench_env_v1;control_sha256={proof_hash};artifact_sha256={proof_hash};bench_env_sha256={proof_hash};noise_score=0;governor=performance;aslr=enabled;thp=never"
                ));
            }
            _ => {}
        }
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

fn install_hostile_head_replacement(root: &Path) -> String {
    let head = fixture_git_output(root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .trim()
        .to_string();
    let tree = fixture_git_output(root, &["rev-parse", "--verify", concat!("HEAD^", "{tree}")])
        .trim()
        .to_string();
    let replacement = fixture_git_output(
        root,
        &[
            "-c",
            "user.name=Pi release-gate fixture",
            "-c",
            "user.email=pi-release-gate@example.invalid",
            "commit-tree",
            &tree,
            "-m",
            "hostile replacement commit without release ancestry",
        ],
    )
    .trim()
    .to_string();
    fixture_git_output(
        root,
        &[
            "update-ref",
            &format!("refs/pi-hostile-replacements/{head}"),
            &replacement,
        ],
    );
    "refs/pi-hostile-replacements/".to_string()
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
    // On macOS the default TMPDIR lives behind trusted OS symlinks
    // (/var -> /private/var, /tmp -> /private/tmp). The embedded validators
    // canonicalize the repository root strictly, so hand them a fixture root
    // whose supplied path already equals its canonical form.
    let base = std::fs::canonicalize(&base).expect("canonicalize release-gate fixture base");
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

fn e2e_source_snapshot_from_clean_checkout(root: &Path, source_commit: &str) -> String {
    fn update_framed(digest: &mut Sha256, value: &[u8]) {
        digest.update(value.len().to_string().as_bytes());
        digest.update(b":");
        digest.update(value);
    }

    let context = performance_git_context(root).expect("resolve E2E fixture Git context");
    let tree_bytes = perf_git_output_at(
        &context,
        &["ls-tree", "-r", "-z", "--full-tree", source_commit],
    )
    .expect("capture E2E fixture source tree");
    let index_bytes = perf_git_output_at(&context, &["ls-files", "--stage", "-z"])
        .expect("capture E2E fixture index");
    let flag_bytes = perf_git_output_at(&context, &["ls-files", "-v", "-z"])
        .expect("capture E2E fixture index flags");

    let mut digest = Sha256::new();
    update_framed(&mut digest, b"pi.e2e.source_snapshot.v1");
    update_framed(&mut digest, source_commit.as_bytes());
    update_framed(&mut digest, &tree_bytes);
    update_framed(&mut digest, &index_bytes);
    update_framed(&mut digest, &flag_bytes);

    for record in tree_bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .expect("E2E fixture tree record tab");
        let metadata = &record[..tab];
        let path = &record[tab + 1..];
        let fields = metadata.split(|byte| *byte == b' ').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "E2E fixture tree metadata");
        let mode = fields[0];
        let object_id = std::str::from_utf8(fields[2]).expect("UTF-8 E2E fixture object ID");
        let blob = perf_git_output_at(&context, &["cat-file", "blob", object_id])
            .expect("read E2E fixture source blob");
        update_framed(&mut digest, path);
        update_framed(&mut digest, mode);
        update_framed(&mut digest, &blob);
    }
    format!(
        "sha256:{}",
        pi::package_manager::hex_encode(&digest.finalize())
    )
}

fn retained_e2e_evidence_fixture() -> (PathBuf, PathBuf) {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("pi-release-evidence-gate-fixtures");
    std::fs::create_dir_all(&base).expect("create retained release-gate fixture base");
    // On macOS the default TMPDIR lives behind trusted OS symlinks
    // (/var -> /private/var, /tmp -> /private/tmp). The embedded validators
    // canonicalize the repository root strictly, so hand them a fixture root
    // whose supplied path already equals its canonical form.
    let base = std::fs::canonicalize(&base).expect("canonicalize release-gate fixture base");
    let root = base.join(format!("e2e-fixture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("src")).expect("create E2E fixture source directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"release-gate-e2e-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\ninclude = [\"/Cargo.toml\"]\n",
    )
    .expect("write E2E fixture Cargo.toml");
    std::fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n")
        .expect("write E2E fixture source");
    std::fs::write(
        root.join(".gitignore"),
        "tests/e2e_results/**/output.log\ntests/e2e_results/**/test-log.jsonl\ntests/e2e_results/**/artifact-index.jsonl\n",
    )
    .expect("write E2E fixture diagnostic ignore rules");
    std::fs::create_dir_all(root.join("tests")).expect("create E2E fixture tests directory");
    std::fs::write(
        root.join("tests/suite_classification.toml"),
        "[suite.unit]\nfiles = [\"release_evidence_gate\"]\n\n[suite.vcr]\nfiles = [\"vcr_probe\"]\n\n[suite.e2e]\nfiles = [\"e2e_extension_registration\"]\n",
    )
    .expect("write E2E fixture suite classification");
    for test_name in [
        "release_evidence_gate",
        "vcr_probe",
        "e2e_extension_registration",
    ] {
        std::fs::write(
            root.join("tests").join(format!("{test_name}.rs")),
            "#[test]\nfn fixture() {}\n",
        )
        .expect("write E2E fixture integration test");
    }
    fixture_git_output(&root, &["init", "--quiet", "--initial-branch=main"]);
    let source_commit = commit_performance_binding_fixture(&root, "initial E2E source snapshot");
    let source_snapshot = e2e_source_snapshot_from_clean_checkout(&root, &source_commit);

    let generated = Utc::now();
    let generated_at = generated.to_rfc3339_opts(SecondsFormat::Secs, true);
    let run_timestamp = generated.format("%Y%m%dT%H%M%SZ").to_string();
    let evidence_dir = root.join(format!("tests/e2e_results/{run_timestamp}"));
    let suite_name = "e2e_extension_registration";
    let suite_dir = evidence_dir.join(suite_name);
    std::fs::create_dir_all(&suite_dir).expect("create E2E fixture evidence directory");
    let correlation_id = "release-gate-e2e-fixture";
    let artifact_dir = evidence_dir.to_str().expect("UTF-8 E2E artifact directory");
    let mut suite_result = json!({
        "schema": "pi.e2e.result.v1",
        "result_kind": "suite",
        "correlation_id": correlation_id,
        "suite": suite_name,
        "exit_code": 0,
        "duration_ms": 1,
        "passed": 1,
        "failed": 0,
        "ignored": 0,
        "total": 1,
        "log_file": suite_dir.join("output.log"),
        "test_log_jsonl": suite_dir.join("test-log.jsonl"),
        "artifact_index_jsonl": suite_dir.join("artifact-index.jsonl"),
        "timestamp": run_timestamp
    });
    let unit_names = ["release_evidence_gate", "vcr_probe"];
    let mut unit_results = unit_names
        .iter()
        .map(|target| {
            let target_dir = evidence_dir.join("unit").join(target);
            json!({
                "schema": "pi.e2e.result.v1",
                "result_kind": "unit",
                "correlation_id": correlation_id,
                "target": target,
                "exit_code": 0,
                "duration_ms": 1,
                "passed": 1,
                "failed": 0,
                "ignored": 0,
                "total": 1,
                "log_file": target_dir.join("output.log"),
                "test_log_jsonl": target_dir.join("test-log.jsonl"),
                "artifact_index_jsonl": target_dir.join("artifact-index.jsonl"),
                "timestamp": run_timestamp
            })
        })
        .collect::<Vec<_>>();
    let write_diagnostics = |directory: &Path| {
        std::fs::create_dir_all(directory).expect("create E2E diagnostic directory");
        std::fs::write(
            directory.join("output.log"),
            "test result: ok. 1 passed; 0 failed; 0 ignored\n",
        )
        .expect("write E2E output log");
        std::fs::write(
            directory.join("test-log.jsonl"),
            "{\"schema\":\"pi.test.log.v1\",\"category\":\"harness\",\"message\":\"fixture\"}\n",
        )
        .expect("write E2E structured test log");
        std::fs::write(directory.join("artifact-index.jsonl"), b"")
            .expect("write E2E artifact index");
    };
    write_diagnostics(&suite_dir);
    let diagnostic_artifacts = |directory: &Path| {
        let binding = |name: &str| {
            let path = directory.join(name);
            let bytes = std::fs::read(&path).expect("read E2E fixture diagnostic");
            json!({
                "path": path,
                "sha256": format!("sha256:{}", pi::package_manager::hex_encode(&Sha256::digest(&bytes))),
                "size_bytes": bytes.len()
            })
        };
        json!({
            "schema": "pi.e2e.diagnostic_artifacts.v1",
            "output_log": binding("output.log"),
            "test_log_jsonl": binding("test-log.jsonl"),
            "artifact_index_jsonl": binding("artifact-index.jsonl")
        })
    };
    suite_result["diagnostic_artifacts"] = diagnostic_artifacts(&suite_dir);
    let lib_dir = evidence_dir.join("lib");
    std::fs::create_dir_all(&lib_dir).expect("create E2E lib result directory");
    std::fs::write(
        lib_dir.join("output.log"),
        "test result: ok. 1 passed; 0 failed; 0 ignored\n",
    )
    .expect("write E2E lib output log");
    let lib_output = std::fs::read(lib_dir.join("output.log")).expect("read E2E lib output log");
    let lib_result = json!({
        "schema": "pi.e2e.result.v1",
        "result_kind": "lib",
        "correlation_id": correlation_id,
        "target": "lib",
        "exit_code": 0,
        "duration_ms": 1,
        "passed": 1,
        "failed": 0,
        "ignored": 0,
        "total": 1,
        "log_file": lib_dir.join("output.log"),
        "diagnostic_artifacts": {
            "schema": "pi.e2e.diagnostic_artifacts.v1",
            "output_log": {
                "path": lib_dir.join("output.log"),
                "sha256": format!("sha256:{}", pi::package_manager::hex_encode(&Sha256::digest(&lib_output))),
                "size_bytes": lib_output.len()
            },
            "test_log_jsonl": null,
            "artifact_index_jsonl": null
        },
        "timestamp": run_timestamp
    });
    let runner_outcome_path = evidence_dir.join("runner_outcome.json");
    let runner_outcome = json!({
        "schema": "pi.e2e.runner_outcome.v1",
        "generated_at": generated_at,
        "timestamp": run_timestamp,
        "profile": "ci",
        "artifact_dir": artifact_dir,
        "correlation_id": correlation_id,
        "source_commit": source_commit,
        "source_snapshot": source_snapshot,
        "status": "pass",
        "exit_code": 0,
        "source_snapshot_verified": true,
        "failed_phases": []
    });

    let environment_path = evidence_dir.join("environment.json");
    let summary_path = evidence_dir.join("summary.json");
    let mut checks = Vec::new();
    let mut add_check = |id: &str, path: &Path| {
        checks.push(json!({
            "id": id,
            "path": path,
            "diagnostics": "fixture proof",
            "ok": true
        }));
    };
    for id in [
        "run.source_commit_format",
        "run.source_snapshot_format",
        "contract.source_commit_matches_run",
        "contract.source_snapshot_matches_run",
    ] {
        add_check(id, &evidence_dir.join("evidence_contract.json"));
    }
    for id in [
        "environment",
        "environment.json_parse",
        "environment.keys",
        "environment.schema",
        "environment.correlation_id_nonempty",
        "environment.generated_at_matches_run",
        "environment.source_commit_format",
        "environment.source_snapshot_format",
        "environment.source_commit_matches_run",
        "environment.source_snapshot_matches_run",
        "environment.git_sha_matches_source_commit",
    ] {
        add_check(id, &environment_path);
    }
    for id in [
        "summary",
        "summary.json_parse",
        "summary.keys",
        "summary.schema",
        "summary.correlation_id_nonempty",
        "summary.generated_at_matches_run",
        "summary.source_commit_format",
        "summary.source_snapshot_format",
        "summary.source_commit_matches_run",
        "summary.source_snapshot_matches_run",
        "run.correlation_id_matches_environment",
        "run.generated_at_matches_environment",
        "run.source_commit_matches_environment",
        "run.source_snapshot_matches_environment",
        "summary.failed_suites_matches_suite_results",
        "summary.lib_matches_result",
        "summary.runner_outcome_matches_file",
    ] {
        add_check(id, &summary_path);
    }
    for id in [
        "runner_outcome",
        "runner_outcome.json_parse",
        "runner_outcome.keys",
        "runner_outcome.keys_exact",
        "runner_outcome.schema",
        "runner_outcome.generated_at_matches_run",
        "runner_outcome.timestamp_matches_run",
        "runner_outcome.profile_matches_run",
        "runner_outcome.artifact_dir_matches_run",
        "runner_outcome.correlation_id_matches_run",
        "runner_outcome.source_commit_matches_run",
        "runner_outcome.source_snapshot_matches_run",
        "runner_outcome.status_pass",
        "runner_outcome.exit_code_zero",
        "runner_outcome.source_snapshot_verified",
        "runner_outcome.failed_phases_empty",
    ] {
        add_check(id, &runner_outcome_path);
    }
    let lib_result_path = lib_dir.join("result.json");
    for suffix in [
        "",
        ".json_parse",
        ".keys",
        ".schema",
        ".kind",
        ".name",
        ".correlation_id_matches_summary",
        ".exit_code_zero",
        ".duration_ms_nonnegative",
        ".counts_nonnegative",
        ".counts_consistent",
        ".tests_executed",
        ".timestamp_matches_run",
        ".diagnostic_artifacts.object",
        ".diagnostic_artifacts.keys_exact",
        ".diagnostic_artifacts.schema",
        ".log_file_nonempty",
        ".log_file_path_matches",
    ] {
        add_check(&format!("lib:lib:result{suffix}"), &lib_result_path);
    }
    for suffix in [
        ".object",
        ".keys",
        ".path_matches",
        ".sha256_format",
        ".size_format",
    ] {
        add_check(
            &format!("lib:lib:result.diagnostic_artifacts.output_log{suffix}"),
            &lib_result_path,
        );
    }
    for suffix in [
        ".regular_non_executable",
        ".stable_read",
        ".size_matches",
        ".sha256_matches",
    ] {
        add_check(
            &format!("lib:lib:result.diagnostic_artifacts.output_log{suffix}"),
            &lib_dir.join("output.log"),
        );
    }
    for id in [
        "lib:lib:result.log_file_exists",
        "lib:lib:result.log_file_budget",
        "lib:lib:result.log_file_redaction",
    ] {
        add_check(id, &lib_dir.join("output.log"));
    }
    for field in ["test_log_jsonl", "artifact_index_jsonl"] {
        add_check(
            &format!("lib:lib:result.diagnostic_artifacts.{field}_null"),
            &lib_result_path,
        );
    }
    for (target, result) in unit_names.iter().zip(&mut unit_results) {
        let result_path = evidence_dir.join("unit").join(target).join("result.json");
        std::fs::create_dir_all(result_path.parent().expect("unit result parent"))
            .expect("create E2E unit result directory");
        write_diagnostics(result_path.parent().expect("unit result parent"));
        result["diagnostic_artifacts"] =
            diagnostic_artifacts(result_path.parent().expect("unit result parent"));
        std::fs::write(
            &result_path,
            serde_json::to_vec_pretty(result).expect("serialize E2E unit result"),
        )
        .expect("write E2E unit result");
        for suffix in [
            "",
            ".json_parse",
            ".keys",
            ".schema",
            ".kind",
            ".name",
            ".correlation_id_matches_summary",
        ] {
            add_check(&format!("unit:{target}:result{suffix}"), &result_path);
        }
        let target_dir = result_path.parent().expect("unit result parent");
        for field in ["log_file", "test_log_jsonl", "artifact_index_jsonl"] {
            add_check(
                &format!("unit:{target}:result.{field}_nonempty"),
                &result_path,
            );
            add_check(
                &format!("unit:{target}:result.{field}_path_matches"),
                &result_path,
            );
        }
        for suffix in ["object", "schema"] {
            add_check(
                &format!("unit:{target}:result.diagnostic_artifacts.{suffix}"),
                &result_path,
            );
        }
        for (field, artifact) in [
            ("output_log", "output.log"),
            ("test_log_jsonl", "test-log.jsonl"),
            ("artifact_index_jsonl", "artifact-index.jsonl"),
        ] {
            let prefix = format!("unit:{target}:result.diagnostic_artifacts.{field}");
            for suffix in [
                "object",
                "keys",
                "path_matches",
                "sha256_format",
                "size_format",
            ] {
                add_check(&format!("{prefix}.{suffix}"), &result_path);
            }
            for suffix in [
                "regular_non_executable",
                "stable_read",
                "size_matches",
                "sha256_matches",
            ] {
                add_check(&format!("{prefix}.{suffix}"), &target_dir.join(artifact));
            }
        }
        for id in [
            format!("unit:{target}:result.log_file_exists"),
            format!("unit:{target}:result.log_file_budget"),
            format!("unit:{target}:result.log_file_redaction"),
        ] {
            add_check(&id, &target_dir.join("output.log"));
        }
        for id in [
            format!("unit:{target}.test_log_jsonl.file_budget"),
            format!("unit:{target}.test_log_jsonl.redaction_scan"),
            format!("unit:{target}.test_log_jsonl.minimum_signal_harness_category"),
        ] {
            add_check(&id, &target_dir.join("test-log.jsonl"));
        }
        for id in [
            format!("unit:{target}.artifact_index_jsonl.file_budget"),
            format!("unit:{target}.artifact_index_jsonl.redaction_scan"),
        ] {
            add_check(&id, &target_dir.join("artifact-index.jsonl"));
        }
    }
    for suffix in [
        "",
        ".json_parse",
        ".keys",
        ".schema",
        ".kind",
        ".name",
        ".correlation_id_matches_summary",
    ] {
        add_check(
            &format!("suite:{suite_name}:result{suffix}"),
            &suite_dir.join("result.json"),
        );
    }
    for field in ["log_file", "test_log_jsonl", "artifact_index_jsonl"] {
        add_check(
            &format!("suite:{suite_name}:result.{field}_nonempty"),
            &suite_dir.join("result.json"),
        );
        add_check(
            &format!("suite:{suite_name}:result.{field}_path_matches"),
            &suite_dir.join("result.json"),
        );
    }
    for suffix in ["object", "schema"] {
        add_check(
            &format!("suite:{suite_name}:result.diagnostic_artifacts.{suffix}"),
            &suite_dir.join("result.json"),
        );
    }
    for (field, artifact) in [
        ("output_log", "output.log"),
        ("test_log_jsonl", "test-log.jsonl"),
        ("artifact_index_jsonl", "artifact-index.jsonl"),
    ] {
        let prefix = format!("suite:{suite_name}:result.diagnostic_artifacts.{field}");
        for suffix in [
            "object",
            "keys",
            "path_matches",
            "sha256_format",
            "size_format",
        ] {
            add_check(
                &format!("{prefix}.{suffix}"),
                &suite_dir.join("result.json"),
            );
        }
        for suffix in [
            "regular_non_executable",
            "stable_read",
            "size_matches",
            "sha256_matches",
        ] {
            add_check(&format!("{prefix}.{suffix}"), &suite_dir.join(artifact));
        }
    }
    for id in [
        format!("suite:{suite_name}:result.log_file_exists"),
        format!("suite:{suite_name}:result.log_file_budget"),
        format!("suite:{suite_name}:result.log_file_redaction"),
    ] {
        add_check(&id, &suite_dir.join("output.log"));
    }
    for id in [
        format!("suite:{suite_name}.test_log_jsonl.file_budget"),
        format!("suite:{suite_name}.test_log_jsonl.redaction_scan"),
        format!("suite:{suite_name}.test_log_jsonl.minimum_signal_harness_category"),
    ] {
        add_check(&id, &suite_dir.join("test-log.jsonl"));
    }
    for id in [
        format!("suite:{suite_name}.artifact_index_jsonl.file_budget"),
        format!("suite:{suite_name}.artifact_index_jsonl.redaction_scan"),
    ] {
        add_check(&id, &suite_dir.join("artifact-index.jsonl"));
    }

    let documents = vec![
        (
            evidence_dir.join("evidence_contract.json"),
            json!({
                "schema": "pi.evidence.contract.v1",
                "generated_at": generated_at,
                "profile": "ci",
                "strict_conformance": false,
                "status": "pass",
                "errors": [],
                "checks": checks,
                "correlation_id": correlation_id,
                "artifact_dir": artifact_dir,
                "source_commit": source_commit,
                "source_snapshot": source_snapshot,
                "runner_outcome": {
                    "schema": "pi.e2e.runner_outcome.v1",
                    "path": runner_outcome_path,
                    "status": "pass",
                    "exit_code": 0
                }
            }),
        ),
        (
            evidence_dir.join("environment.json"),
            json!({
                "schema": "pi.e2e.environment.v1",
                "generated_at": generated_at,
                "profile": "ci",
                "rerun_from": null,
                "correlation_id": correlation_id,
                "artifact_dir": artifact_dir,
                "shard": {"kind": "none", "name": "unsharded", "index": null, "total": null},
                "unit_targets": unit_names,
                "e2e_suites": [suite_name],
                "git_sha": source_commit,
                "source_commit": source_commit,
                "source_snapshot": source_snapshot
            }),
        ),
        (
            evidence_dir.join("summary.json"),
            json!({
                "schema": "pi.e2e.summary.v1",
                "generated_at": generated_at,
                "profile": "ci",
                "rerun_from": null,
                "correlation_id": correlation_id,
                "artifact_dir": artifact_dir,
                "shard": {"kind": "none", "name": "unsharded", "index": null, "total": null},
                "source_commit": source_commit,
                "source_snapshot": source_snapshot,
                "total_units": unit_results.len(),
                "passed_units": unit_results.len(),
                "failed_units": 0,
                "failed_unit_names": [],
                "lib": lib_result,
                "runner_outcome": runner_outcome,
                "unit_targets": unit_results,
                "total_suites": 1,
                "passed_suites": 1,
                "failed_suites": 0,
                "failed_names": [],
                "suites": [suite_result]
            }),
        ),
        (lib_dir.join("result.json"), lib_result),
        (runner_outcome_path, runner_outcome),
        (suite_dir.join("result.json"), suite_result),
    ];
    for (path, document) in documents {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&document).expect("serialize E2E fixture document"),
        )
        .expect("write E2E fixture document");
    }
    fixture_git_output(&root, &["add", "-f", "--", "tests/e2e_results"]);
    commit_performance_binding_fixture(&root, "record E2E evidence follow-up");
    (root, evidence_dir)
}

fn retained_conformance_evidence_fixture() -> (PathBuf, PathBuf, String) {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("pi-release-evidence-gate-fixtures");
    std::fs::create_dir_all(&base).expect("create retained release-gate fixture base");
    // On macOS the default TMPDIR lives behind trusted OS symlinks
    // (/var -> /private/var, /tmp -> /private/tmp). The embedded validators
    // canonicalize the repository root strictly, so hand them a fixture root
    // whose supplied path already equals its canonical form.
    let base = std::fs::canonicalize(&base).expect("canonicalize release-gate fixture base");
    let root = base.join(format!(
        "conformance-binding-fixture-{}",
        uuid::Uuid::new_v4()
    ));
    let summary_path = root.join("tests/ext_conformance/reports/conformance_summary.json");
    std::fs::create_dir_all(root.join("src")).expect("create conformance fixture source directory");
    std::fs::create_dir_all(summary_path.parent().expect("conformance summary parent"))
        .expect("create conformance fixture reports directory");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"release-gate-conformance-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\ninclude = [\"/Cargo.toml\", \"/src/**\"]\n",
    )
    .expect("write conformance fixture Cargo.toml");
    std::fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n")
        .expect("write conformance fixture source");
    std::fs::write(root.join(".gitignore"), "tests/ext_conformance/reports/*\n")
        .expect("write conformance fixture evidence ignore rule");
    let manifest_path = root.join("tests/ext_conformance/VALIDATED_MANIFEST.json");
    std::fs::create_dir_all(manifest_path.parent().expect("conformance manifest parent"))
        .expect("create conformance fixture manifest directory");
    let capabilities = json!({
        "registers_tools": true,
        "registers_commands": false,
        "registers_flags": false,
        "registers_providers": false,
        "subscribes_events": [],
        "uses_exec": false,
        "uses_http": false,
        "uses_ui": false,
        "uses_session": false,
        "is_multi_file": false,
        "has_npm_deps": false
    });
    let registrations = json!({
        "tools": ["fixture_tool"],
        "commands": [],
        "flags": [],
        "event_handlers": []
    });
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.ext.validated-manifest.v1",
            "generated_at": "2026-01-01T00:00:00Z",
            "extensions": [
                {
                    "id": "fixture-a",
                    "entry_path": "fixture-a/index.ts",
                    "source_tier": "fixture-tier",
                    "conformance_tier": 1,
                    "capabilities": capabilities,
                    "registrations": registrations
                },
                {
                    "id": "fixture-b",
                    "entry_path": "fixture-b/index.ts",
                    "source_tier": "fixture-tier",
                    "conformance_tier": 1,
                    "capabilities": capabilities,
                    "registrations": registrations
                }
            ]
        }))
        .expect("serialize conformance fixture manifest"),
    )
    .expect("write conformance fixture manifest");
    for extension_id in ["fixture-a", "fixture-b"] {
        let artifact = root.join(format!(
            "tests/ext_conformance/artifacts/{extension_id}/index.ts"
        ));
        std::fs::create_dir_all(artifact.parent().expect("fixture artifact parent"))
            .expect("create fixture artifact directory");
        std::fs::write(&artifact, "export default {};\n").expect("write fixture artifact");
        let golden = root.join(format!(
            "tests/ext_conformance/fixtures/{extension_id}.json"
        ));
        std::fs::create_dir_all(golden.parent().expect("golden fixture parent"))
            .expect("create golden fixture directory");
        std::fs::write(&golden, "{}\n").expect("write golden fixture");
    }
    fixture_git_output(&root, &["init", "--quiet", "--initial-branch=main"]);
    let source_commit = commit_performance_binding_fixture(&root, "initial conformance source");
    let context = performance_git_context(&root).expect("resolve conformance fixture Git context");
    let source_tree = perf_git_output_at(
        &context,
        &["ls-tree", "-r", "-z", "--full-tree", &source_commit],
    )
    .expect("capture conformance fixture source tree");
    let source_tree_sha256 = pi::package_manager::hex_encode(&Sha256::digest(source_tree));
    let generated = Utc::now();
    let generated_seconds = generated.to_rfc3339_opts(SecondsFormat::Secs, true);
    let generated_millis = generated.to_rfc3339_opts(SecondsFormat::Millis, true);
    let reports = root.join("tests/ext_conformance/reports");

    std::fs::write(
        reports.join("load_time_benchmark.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.ext.load_time_benchmark.v1",
            "generated_at": generated_seconds,
            "counts": {"total": 2, "ts_success": 2, "rust_success": 2, "paired": 2},
            "results": [
                {
                    "extension": "fixture-a/index.ts",
                    "ts": {"success": true, "error": null, "load_time_ms": 1},
                    "rust": {"success": true, "error": null, "load_time_ms": 2},
                    "ratio": 2.0
                },
                {
                    "extension": "fixture-b/index.ts",
                    "ts": {"success": true, "error": null, "load_time_ms": 1},
                    "rust": {"success": true, "error": null, "load_time_ms": 2},
                    "ratio": 2.0
                }
            ]
        }))
        .expect("serialize load-time fixture"),
    )
    .expect("write load-time fixture");
    std::fs::write(
        reports.join("scenario_conformance.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.ext.scenario_conformance.v1",
            "generated_at": generated_seconds,
            "counts": {"total": 2, "pass": 2, "fail": 0, "error": 0, "skip": 0},
            "pass_rate_pct": 100.0,
            "results": [
                {
                    "extension_id": "fixture-a",
                    "scenario_id": "scenario-a",
                    "status": "pass",
                    "duration_ms": 1,
                    "summary": "fixture A passed"
                },
                {
                    "extension_id": "fixture-b",
                    "scenario_id": "scenario-b",
                    "status": "pass",
                    "duration_ms": 1,
                    "summary": "fixture B passed"
                }
            ]
        }))
        .expect("serialize scenario fixture"),
    )
    .expect("write scenario fixture");
    std::fs::write(
        reports.join("smoke_triage.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.ext.smoke_triage.v1",
            "generated_at": generated_seconds,
            "counts": {"total": 2, "pass": 2, "fail": 0, "error": 0, "skip": 0},
            "pass_rate_pct": 100.0,
            "extensions": [
                {
                    "extension_id": "fixture-a",
                    "pass": 1, "fail": 0, "error": 0, "skip": 0,
                    "failures": [], "failure_categories": {}
                },
                {
                    "extension_id": "fixture-b",
                    "pass": 1, "fail": 0, "error": 0, "skip": 0,
                    "failures": [], "failure_categories": {}
                }
            ]
        }))
        .expect("serialize smoke fixture"),
    )
    .expect("write smoke fixture");

    let parity_dir = reports.join("parity");
    let negative_dir = reports.join("negative");
    std::fs::create_dir_all(&parity_dir).expect("create parity fixture directory");
    std::fs::create_dir_all(&negative_dir).expect("create negative fixture directory");
    let parity_events = ["fixture-a", "fixture-b"]
        .iter()
        .enumerate()
        .map(|(index, extension_id)| {
            serde_json::to_string(&json!({
                "schema": "pi.ext.parity.v1",
                "ts": generated_millis,
                "run_id": "fixture-parity-run",
                "extension_id": extension_id,
                "scenario_id": format!("parity-{index}"),
                "kind": "tool",
                "summary": "fixture parity match",
                "source_tier": "fixture-tier",
                "runtime_tier": "legacy-js",
                "status": "match",
                "ts_ms": 1,
                "rust_ms": 1,
                "diffs": [],
                "error": null,
                "skip_reason": null
            }))
            .expect("serialize parity fixture event")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(parity_dir.join("parity_events.jsonl"), parity_events)
        .expect("write parity fixture events");
    let negative_event = serde_json::to_string(&json!({
        "schema": "pi.ext.negative_conformance.v1",
        "ts": generated_millis,
        "test_name": "empty_cap_strict",
        "capability": "",
        "mode": "strict",
        "reason": "empty_capability",
        "expected_decision": "deny",
        "actual_decision": "Deny",
        "status": "pass",
        "duration_ms": 1
    }))
    .expect("serialize negative fixture event")
        + "\n";
    std::fs::write(negative_dir.join("negative_events.jsonl"), negative_event)
        .expect("write negative fixture events");
    std::fs::write(
        negative_dir.join("triage.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.ext.negative_triage.v1",
            "generated_at": generated_seconds,
            "counts": {"total": 1, "pass": 1, "fail": 0},
            "pass_rate_pct": 100.0
        }))
        .expect("serialize negative triage fixture"),
    )
    .expect("write negative triage fixture");

    for extension_id in ["fixture-a", "fixture-b"] {
        let smoke_log = reports.join(format!("extensions/{extension_id}.jsonl"));
        let parity_log = reports.join(format!("parity/extensions/{extension_id}.jsonl"));
        std::fs::create_dir_all(smoke_log.parent().expect("smoke log parent"))
            .expect("create smoke log directory");
        std::fs::create_dir_all(parity_log.parent().expect("parity log parent"))
            .expect("create parity log directory");
        std::fs::write(&smoke_log, "{\"status\":\"pass\"}\n").expect("write smoke evidence log");
        std::fs::write(
            &parity_log,
            serde_json::to_string(&json!({
                "scenario_id": if extension_id == "fixture-a" {
                    "parity-0"
                } else {
                    "parity-1"
                },
                "extension_id": extension_id,
                "kind": "tool",
                "summary": "fixture parity match",
                "status": "match",
                "source_tier": "fixture-tier",
                "runtime_tier": "legacy-js",
                "ts_ms": 1,
                "rust_ms": 1
            }))
            .expect("serialize parity evidence log")
                + "\n",
        )
        .expect("write parity evidence log");
    }

    let events_path = root.join("tests/ext_conformance/reports/conformance_events.jsonl");
    let events = ["fixture-a", "fixture-b"]
        .iter()
        .map(|extension_id| {
            serde_json::to_string(&json!({
                "schema": "pi.ext.conformance_report.v2",
                "ts": generated_millis.clone(),
                "extension_id": extension_id,
                "version": null,
                "source_tier": "fixture-tier",
                "conformance_tier": 1,
                "artifact_path": format!(
                    "tests/ext_conformance/artifacts/{extension_id}/index.ts"
                ),
                "evidence": {
                    "fixture": format!("tests/ext_conformance/fixtures/{extension_id}.json"),
                    "smoke_log": format!(
                        "tests/ext_conformance/reports/extensions/{extension_id}.jsonl"
                    ),
                    "parity_log": format!(
                        "tests/ext_conformance/reports/parity/extensions/{extension_id}.jsonl"
                    )
                },
                "capabilities": capabilities.clone(),
                "registrations": registrations.clone(),
                "rust_load_ms": 2,
                "ts_load_ms": 1,
                "load_ratio": 2.0,
                "scenario_pass": 1,
                "scenario_fail": 0,
                "scenario_skip": 0,
                "smoke_pass": 1,
                "smoke_fail": 0,
                "parity_match": 1,
                "parity_mismatch": 0,
                "failures": [],
                "overall_status": "PASS"
            }))
            .expect("serialize conformance fixture event")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&events_path, events).expect("write conformance fixture events");
    std::fs::write(
        &summary_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.ext.conformance_summary.v2",
            "generated_at": generated_seconds,
            "run_id": "fixture-run",
            "correlation_id": "fixture-correlation",
            "git_commit": source_commit,
            "source_tree_sha256": source_tree_sha256,
            "counts": {
                "total": 2,
                "pass": 2,
                "fail": 0,
                "na": 0,
                "tested": 2
            },
            "pass_rate_pct": 100.0,
            "coverage_rate_pct": 100.0,
            "negative": {"pass": 1, "fail": 0},
            "per_tier": {
                "fixture-tier": {"total": 2, "pass": 2, "fail": 0, "na": 0}
            },
            "evidence": {
                "golden_fixtures": 2,
                "smoke_logs": 2,
                "parity_logs": 2,
                "load_time_benchmarks": 2
            }
        }))
        .expect("serialize conformance binding summary"),
    )
    .expect("write conformance binding summary");
    fixture_git_output(&root, &["add", "-f", "--", "tests/ext_conformance/reports"]);
    commit_performance_binding_fixture(&root, "record conformance summary evidence");
    (root, summary_path, source_commit)
}

fn run_retained_conformance_validator(root: &Path, summary_path: &Path) -> std::process::Output {
    let marker = "if [[ -f \"$CONFORMANCE_SUMMARY\" ]]; then";
    let args = [
        root.to_str().expect("UTF-8 conformance fixture root"),
        summary_path
            .to_str()
            .expect("UTF-8 conformance fixture summary"),
        "90",
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    run_release_gate_python(command, &program)
}

fn rewrite_first_conformance_event(root: &Path, mutate: impl FnOnce(&mut Value)) {
    let path = root.join("tests/ext_conformance/reports/conformance_events.jsonl");
    let contents = std::fs::read_to_string(&path).expect("read conformance events fixture");
    let mut lines = contents.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut first: Value = serde_json::from_str(&lines[0]).expect("parse first conformance event");
    mutate(&mut first);
    lines[0] = serde_json::to_string(&first).expect("serialize mutated conformance event");
    std::fs::write(path, lines.join("\n") + "\n").expect("write mutated conformance events");
}

#[test]
fn release_gate_conformance_rejects_pass_with_failing_counter() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    rewrite_first_conformance_event(&root, |event| {
        event["scenario_pass"] = json!(0);
        event["scenario_fail"] = json!(1);
        event["failures"] = json!(["retained failure"]);
        event["overall_status"] = json!("PASS");
    });
    commit_performance_binding_fixture(&root, "record contradictory PASS event");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(!output.status.success(), "contradictory PASS event passed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("overall_status contradicts its retained counters"),
        "{stderr}"
    );
}

#[test]
fn release_gate_conformance_rejects_all_pass_with_zero_proof() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    rewrite_first_conformance_event(&root, |event| {
        for field in [
            "scenario_pass",
            "scenario_fail",
            "scenario_skip",
            "smoke_pass",
            "smoke_fail",
            "parity_match",
            "parity_mismatch",
        ] {
            event[field] = json!(0);
        }
        event["rust_load_ms"] = Value::Null;
        event["ts_load_ms"] = Value::Null;
        event["load_ratio"] = Value::Null;
        event["failures"] = json!([]);
        event["overall_status"] = json!("PASS");
    });
    commit_performance_binding_fixture(&root, "record proof-free PASS event");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(!output.status.success(), "proof-free PASS event passed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("overall_status contradicts its retained counters"),
        "{stderr}"
    );
}

#[test]
fn release_gate_conformance_rejects_negative_failure() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let negative_event_path =
        root.join("tests/ext_conformance/reports/negative/negative_events.jsonl");
    let mut event: Value = serde_json::from_str(
        std::fs::read_to_string(&negative_event_path)
            .expect("read negative event")
            .trim_end(),
    )
    .expect("parse negative event");
    event["actual_decision"] = json!("Allow");
    event["status"] = json!("fail");
    std::fs::write(
        &negative_event_path,
        serde_json::to_string(&event).expect("serialize failing negative event") + "\n",
    )
    .expect("write failing negative event");
    let triage_path = root.join("tests/ext_conformance/reports/negative/triage.json");
    let mut triage: Value =
        serde_json::from_slice(&std::fs::read(&triage_path).expect("read negative triage fixture"))
            .expect("parse negative triage fixture");
    triage["counts"] = json!({"total": 1, "pass": 0, "fail": 1});
    triage["pass_rate_pct"] = json!(0.0);
    std::fs::write(
        &triage_path,
        serde_json::to_vec_pretty(&triage).expect("serialize failing negative triage"),
    )
    .expect("write failing negative triage");
    let mut summary: Value = serde_json::from_slice(
        &std::fs::read(&summary_path).expect("read conformance summary fixture"),
    )
    .expect("parse conformance summary fixture");
    summary["negative"] = json!({"pass": 0, "fail": 1});
    std::fs::write(
        &summary_path,
        serde_json::to_vec_pretty(&summary).expect("serialize negative-failure summary"),
    )
    .expect("write negative-failure summary");
    commit_performance_binding_fixture(&root, "record negative conformance failure");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(
        !output.status.success(),
        "negative failure passed release admission"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("negative.fail must be zero"), "{stderr}");
}

#[test]
fn release_gate_conformance_rejects_self_reported_negative_pass() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let negative_event_path =
        root.join("tests/ext_conformance/reports/negative/negative_events.jsonl");
    let mut event: Value = serde_json::from_str(
        std::fs::read_to_string(&negative_event_path)
            .expect("read negative event")
            .trim_end(),
    )
    .expect("parse negative event");
    event["actual_decision"] = json!("Allow");
    std::fs::write(
        &negative_event_path,
        serde_json::to_string(&event).expect("serialize contradictory negative event") + "\n",
    )
    .expect("write contradictory negative event");
    commit_performance_binding_fixture(&root, "record self-reported negative pass");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(
        !output.status.success(),
        "self-reported negative pass contradicted by its decisions was admitted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("status contradicts its decisions"),
        "{stderr}"
    );
}

#[test]
fn release_gate_conformance_rejects_parity_error_omitted_from_main_events() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let parity_log = root.join("tests/ext_conformance/reports/parity/extensions/fixture-a.jsonl");
    let mut contents = std::fs::read_to_string(&parity_log).expect("read parity evidence log");
    contents.push_str(
        &serde_json::to_string(&json!({
            "scenario_id": "parity-error-omitted",
            "extension_id": "fixture-a",
            "kind": "tool",
            "summary": "fixture parity oracle error",
            "status": "ts_error",
            "source_tier": "fixture-tier",
            "runtime_tier": "legacy-js",
            "error": "fixture TypeScript oracle failed",
            "ts_ms": 1,
            "rust_ms": 1
        }))
        .expect("serialize omitted parity error"),
    );
    contents.push('\n');
    std::fs::write(&parity_log, contents).expect("write omitted parity error");
    commit_performance_binding_fixture(&root, "record omitted parity error outcome");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(
        !output.status.success(),
        "a parity error absent from parity_events.jsonl was admitted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is missing from parity_events.jsonl"),
        "{stderr}"
    );
}

#[test]
fn release_gate_conformance_rejects_parity_log_event_disagreement() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let parity_log = root.join("tests/ext_conformance/reports/parity/extensions/fixture-a.jsonl");
    let mut record: Value = serde_json::from_str(
        std::fs::read_to_string(&parity_log)
            .expect("read parity evidence log")
            .trim_end(),
    )
    .expect("parse parity evidence log");
    record["status"] = json!("mismatch");
    record["diffs"] = json!(["fixture mismatch"]);
    std::fs::write(
        &parity_log,
        serde_json::to_string(&record).expect("serialize contradictory parity log") + "\n",
    )
    .expect("write contradictory parity log");
    commit_performance_binding_fixture(&root, "record contradictory parity log");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(
        !output.status.success(),
        "a parity log/main-event outcome disagreement was admitted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("status disagrees with the per-extension log"),
        "{stderr}"
    );
}

#[test]
fn release_gate_conformance_rejects_noncanonical_parity_event_fields() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let events_path = root.join("tests/ext_conformance/reports/parity/parity_events.jsonl");
    let contents = std::fs::read_to_string(&events_path).expect("read parity event stream");
    let mut lines = contents.lines();
    let mut first: Value = serde_json::from_str(lines.next().expect("first parity event"))
        .expect("parse first parity event");
    first["unbound_extra_field"] = json!(true);
    let mut rewritten =
        vec![serde_json::to_string(&first).expect("serialize noncanonical parity event")];
    rewritten.extend(lines.map(str::to_string));
    std::fs::write(&events_path, rewritten.join("\n") + "\n")
        .expect("write noncanonical parity event stream");
    commit_performance_binding_fixture(&root, "record noncanonical parity event");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(
        !output.status.success(),
        "a parity event with extra unbound fields was admitted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid canonical schema"), "{stderr}");
}

#[test]
fn release_gate_conformance_rejects_parity_diagnostics_for_wrong_outcome() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let events_path = root.join("tests/ext_conformance/reports/parity/parity_events.jsonl");
    let contents = std::fs::read_to_string(&events_path).expect("read parity event stream");
    let mut lines = contents.lines();
    let mut first: Value = serde_json::from_str(lines.next().expect("first parity event"))
        .expect("parse first parity event");
    first["error"] = json!("contradictory error on a matching result");
    let mut rewritten =
        vec![serde_json::to_string(&first).expect("serialize contradictory parity event")];
    rewritten.extend(lines.map(str::to_string));
    std::fs::write(&events_path, rewritten.join("\n") + "\n")
        .expect("write contradictory parity event stream");
    commit_performance_binding_fixture(&root, "record contradictory parity diagnostics");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(
        !output.status.success(),
        "a matching parity event carrying error diagnostics was admitted"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("diagnostics for the wrong outcome"),
        "{stderr}"
    );
}

#[test]
fn release_gate_conformance_rejects_negative_summary_source_mismatch() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let mut summary: Value = serde_json::from_slice(
        &std::fs::read(&summary_path).expect("read conformance summary fixture"),
    )
    .expect("parse conformance summary fixture");
    summary["negative"] = json!({"pass": 2, "fail": 0});
    std::fs::write(
        &summary_path,
        serde_json::to_vec_pretty(&summary).expect("serialize mismatched negative summary"),
    )
    .expect("write mismatched negative summary");
    commit_performance_binding_fixture(&root, "record mismatched negative summary");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(
        !output.status.success(),
        "mismatched negative summary passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("negative.pass does not match retained negative conformance evidence"),
        "{stderr}"
    );
}

#[test]
fn release_gate_conformance_rejects_untracked_raw_decision_source() {
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    fixture_git_output(
        &root,
        &[
            "update-index",
            "--force-remove",
            "tests/ext_conformance/reports/load_time_benchmark.json",
        ],
    );
    commit_performance_binding_fixture(&root, "untrack raw conformance decision source");

    let output = run_retained_conformance_validator(&root, &summary_path);
    assert!(
        !output.status.success(),
        "untracked raw decision source passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("load-time benchmark must be one tracked blob in release HEAD"),
        "{stderr}"
    );
}

fn retained_dropin_evidence_fixture() -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("pi-release-evidence-gate-fixtures");
    std::fs::create_dir_all(&base).expect("create retained release-gate fixture base");
    // On macOS the default TMPDIR lives behind trusted OS symlinks
    // (/var -> /private/var, /tmp -> /private/tmp). The embedded validators
    // canonicalize the repository root strictly, so hand them a fixture root
    // whose supplied path already equals its canonical form.
    let base = std::fs::canonicalize(&base).expect("canonicalize release-gate fixture base");
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

fn retained_certified_dropin_lane_fixture(
    actual_lane_verdict: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("pi-release-evidence-gate-fixtures");
    std::fs::create_dir_all(&base).expect("create retained release-gate fixture base");
    // On macOS the default TMPDIR lives behind trusted OS symlinks
    // (/var -> /private/var, /tmp -> /private/tmp). The embedded validators
    // canonicalize the repository root strictly, so hand them a fixture root
    // whose supplied path already equals its canonical form.
    let base = std::fs::canonicalize(&base).expect("canonicalize release-gate fixture base");
    let root = base.join(format!(
        "certified-dropin-lane-fixture-{}",
        uuid::Uuid::new_v4()
    ));
    let contract_path = root.join("docs/contracts/dropin-certification-contract.json");
    let verdict_path = root.join("docs/evidence/dropin-certification-verdict.json");
    let lane_path = root.join("tests/full_suite_gate/certification_verdict.json");
    std::fs::create_dir_all(root.join("src"))
        .expect("create certified drop-in fixture source directory");
    for path in [&contract_path, &verdict_path, &lane_path] {
        std::fs::create_dir_all(path.parent().expect("certified drop-in fixture parent"))
            .expect("create certified drop-in fixture directory");
    }
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"release-gate-certified-dropin-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\ninclude = [\"/Cargo.toml\", \"/src/**\"]\n",
    )
    .expect("write certified drop-in fixture Cargo.toml");
    std::fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n")
        .expect("write certified drop-in fixture source");

    let contract = require_json("docs/contracts/dropin-certification-contract.json");
    std::fs::write(
        &contract_path,
        serde_json::to_vec_pretty(&contract).expect("serialize certified drop-in contract"),
    )
    .expect("write certified drop-in contract");
    let hard_gates = contract
        .get("hard_gates")
        .and_then(Value::as_array)
        .expect("canonical drop-in contract hard gates");
    let mut evidence_paths = Vec::<String>::new();
    for gate in hard_gates {
        for artifact in gate
            .get("required_artifacts")
            .and_then(Value::as_array)
            .expect("canonical gate required artifacts")
        {
            let path = artifact.as_str().expect("canonical required artifact path");
            if !evidence_paths.iter().any(|existing| existing == path) {
                evidence_paths.push(path.to_string());
            }
            let fixture_path = root.join(path);
            if let Some(parent) = fixture_path.parent() {
                std::fs::create_dir_all(parent).expect("create certified drop-in evidence parent");
            }
            let contents =
                if fixture_path.extension().and_then(|value| value.to_str()) == Some("json") {
                    b"{}\n".as_slice()
                } else {
                    b"certified drop-in fixture evidence\n".as_slice()
                };
            std::fs::write(&fixture_path, contents)
                .expect("write certified drop-in evidence fixture");
        }
    }
    let lane_generated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let canonical_lane = require_json("tests/full_suite_gate/certification_verdict.json");
    let mut lane_gates = canonical_lane["gates"]
        .as_array()
        .expect("canonical certification lane gates")
        .clone();
    for gate in &mut lane_gates {
        gate["status"] = json!("pass");
    }
    let (summary, promotion_rules) = if actual_lane_verdict == "pass" {
        (
            json!({
                "total_gates": 20,
                "passed": 20,
                "failed": 0,
                "warned": 0,
                "skipped": 0,
                "waived": 0,
                "blocking_pass": 14,
                "blocking_total": 14,
                "all_blocking_pass": true
            }),
            json!({
                "can_promote": true,
                "blocker_gates": [],
                "waiver_gates": [],
                "conditions": ["All blocking gates pass (including waivers)"]
            }),
        )
    } else {
        lane_gates[0]["status"] = json!("fail");
        (
            json!({
                "total_gates": 20,
                "passed": 19,
                "failed": 1,
                "warned": 0,
                "skipped": 0,
                "waived": 0,
                "blocking_pass": 13,
                "blocking_total": 14,
                "all_blocking_pass": false
            }),
            json!({
                "can_promote": false,
                "blocker_gates": ["non_mock_unit"],
                "waiver_gates": [],
                "conditions": ["Blocking gates still failing: non_mock_unit"]
            }),
        )
    };
    std::fs::write(
        &lane_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.ci.certification_lane.v1",
            "lane": "full",
            "generated_at": lane_generated_at,
            "verdict": actual_lane_verdict,
            "policy": "Full certification: all blocking gates must pass for release. Waived gates are tracked but do not block. Expired waivers fail the waiver_lifecycle gate.",
            "gates": lane_gates,
            "waiver_audit": {
                "schema": "pi.ci.waiver_audit.v1",
                "generated_at": lane_generated_at,
                "total_waivers": 0,
                "active": 0,
                "expired": 0,
                "expiring_soon": 0,
                "invalid": 0,
                "waivers": [],
                "raw_waivers": []
            },
            "waivers_applied": [],
            "summary": summary,
            "promotion_rules": promotion_rules,
            "rerun_guidance": {
                "preflight_command": "cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact",
                "full_command": "cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact",
                "single_gate_template": "See reproduce_command field on each gate"
            }
        }))
        .expect("serialize actual certification lane fixture"),
    )
    .expect("write actual certification lane fixture");

    fixture_git_output(&root, &["init", "--quiet", "--initial-branch=main"]);
    let source_commit = commit_performance_binding_fixture(&root, "initial certified source");
    let hard_gate_results = hard_gates
        .iter()
        .map(|gate| {
            json!({
                "gate_id": gate["gate_id"],
                "status": "pass",
                "blocking": gate["blocking"],
                "detail": null,
                "bead": gate["owner_issue_primary"],
                "artifact_paths": gate["required_artifacts"]
            })
        })
        .collect::<Vec<_>>();
    let evidence_index = evidence_paths
        .iter()
        .map(|path| json!({"path": path, "exists": true}))
        .collect::<Vec<_>>();
    std::fs::write(
        &verdict_path,
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.dropin.certification_verdict.v1",
            "git_commit": source_commit,
            "generated_at_utc": lane_generated_at,
            "overall_verdict": "CERTIFIED",
            "hard_gate_results": hard_gate_results,
            "blocking_reasons": [],
            "evidence_index": evidence_index,
            "source": {
                "certification_lane_artifact": "tests/full_suite_gate/certification_verdict.json",
                "lane_schema": "pi.ci.certification_lane.v1",
                "lane_verdict": "pass"
            }
        }))
        .expect("serialize certified drop-in verdict fixture"),
    )
    .expect("write certified drop-in verdict fixture");
    commit_performance_binding_fixture(&root, "record certified drop-in verdict evidence");
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
fn release_gate_embedded_e2e_validator_rejects_failed_lib_result() {
    let marker = "if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then";
    let (root, evidence_dir) = retained_e2e_evidence_fixture();
    let lib_path = evidence_dir.join("lib/result.json");
    let mut lib: Value =
        serde_json::from_slice(&std::fs::read(&lib_path).expect("read lib result"))
            .expect("parse lib result");
    lib["exit_code"] = json!(1);
    lib["passed"] = json!(0);
    lib["failed"] = json!(1);
    std::fs::write(
        &lib_path,
        serde_json::to_vec_pretty(&lib).expect("serialize failed lib result"),
    )
    .expect("write failed lib result");
    let summary_path = evidence_dir.join("summary.json");
    let mut summary: Value =
        serde_json::from_slice(&std::fs::read(&summary_path).expect("read E2E summary"))
            .expect("parse E2E summary");
    summary["lib"] = lib;
    std::fs::write(
        &summary_path,
        serde_json::to_vec_pretty(&summary).expect("serialize failed-lib summary"),
    )
    .expect("write failed-lib summary");
    commit_performance_binding_fixture(&root, "record failed inline-lib result");

    let args = [
        root.to_str().expect("UTF-8 failed-lib root"),
        evidence_dir.to_str().expect("UTF-8 failed-lib evidence"),
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("inline-lib tests did not exit successfully"),
        "{stdout}"
    );
}

#[test]
fn release_gate_embedded_e2e_validator_rejects_failed_runner_outcome() {
    let marker = "if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then";
    let (root, evidence_dir) = retained_e2e_evidence_fixture();
    let outcome_path = evidence_dir.join("runner_outcome.json");
    let mut outcome: Value =
        serde_json::from_slice(&std::fs::read(&outcome_path).expect("read runner outcome"))
            .expect("parse runner outcome");
    outcome["status"] = json!("fail");
    outcome["exit_code"] = json!(1);
    outcome["source_snapshot_verified"] = json!(false);
    outcome["failed_phases"] = json!(["evidence_contract"]);
    std::fs::write(
        &outcome_path,
        serde_json::to_vec_pretty(&outcome).expect("serialize failed runner outcome"),
    )
    .expect("write failed runner outcome");
    let summary_path = evidence_dir.join("summary.json");
    let mut summary: Value =
        serde_json::from_slice(&std::fs::read(&summary_path).expect("read E2E summary"))
            .expect("parse E2E summary");
    summary["runner_outcome"] = outcome;
    std::fs::write(
        &summary_path,
        serde_json::to_vec_pretty(&summary).expect("serialize failed-outcome summary"),
    )
    .expect("write failed-outcome summary");
    let contract_path = evidence_dir.join("evidence_contract.json");
    let mut contract: Value =
        serde_json::from_slice(&std::fs::read(&contract_path).expect("read evidence contract"))
            .expect("parse evidence contract");
    contract["runner_outcome"]["status"] = json!("fail");
    contract["runner_outcome"]["exit_code"] = json!(1);
    std::fs::write(
        &contract_path,
        serde_json::to_vec_pretty(&contract).expect("serialize failed-outcome contract"),
    )
    .expect("write failed-outcome contract");
    commit_performance_binding_fixture(&root, "record failed runner outcome");

    let args = [
        root.to_str().expect("UTF-8 failed-outcome root"),
        evidence_dir
            .to_str()
            .expect("UTF-8 failed-outcome evidence"),
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("runner outcome exit_code must be zero"),
        "{stdout}"
    );
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
    let (command, program) = release_gate_python_command(marker, &[root_arg, evidence_arg, "168"]);
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
        "#!/bin/sh\nif [ ! -f \"$PI_E2E_RESTORE_MARKER\" ]; then\n  case \" $* \" in\n    *\" rev-parse --verify HEAD^{commit} \"*)\n      cp \"$PI_E2E_ORIGINAL\" \"$PI_E2E_TARGET\" || exit 97\n      : > \"$PI_E2E_RESTORE_MARKER\" || exit 98\n      ;;\n  esac\nfi\nexec \"$PI_E2E_REAL_GIT\" \"$@\"\n",
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
    let (mut command, program) =
        release_gate_python_command(marker, &[root_arg, evidence_arg, "168"]);
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

#[test]
fn release_gate_embedded_e2e_validator_rejects_untracked_diagnostic_substitution() {
    let (root, evidence_dir) = retained_e2e_evidence_fixture();
    let diagnostic = evidence_dir
        .join("unit")
        .join("release_evidence_gate")
        .join("output.log");
    fixture_git_output(
        &root,
        &[
            "update-index",
            "--force-remove",
            "--",
            diagnostic
                .strip_prefix(&root)
                .expect("repository-relative diagnostic")
                .to_str()
                .expect("UTF-8 diagnostic path"),
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
            "remove retained E2E diagnostic from HEAD",
        ],
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
        "the ignored diagnostic substitution must evade ordinary Git cleanliness"
    );

    let args = [
        root.to_str().expect("UTF-8 diagnostic fixture root"),
        evidence_dir
            .to_str()
            .expect("UTF-8 diagnostic fixture evidence"),
        "168",
    ];
    let (command, program) =
        release_gate_python_command("if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then", &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(stdout.contains("not tracked by release HEAD"), "{stdout}");
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
        &[root_arg, evidence_arg, "168"],
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

#[test]
fn release_gate_embedded_e2e_validator_rejects_missing_future_and_stale_freshness() {
    let marker = "if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then";

    let (missing_root, missing_evidence) = retained_e2e_evidence_fixture();
    let contract_path = missing_evidence.join("evidence_contract.json");
    let mut contract: Value = serde_json::from_slice(
        &std::fs::read(&contract_path).expect("read missing-timestamp E2E contract"),
    )
    .expect("parse missing-timestamp E2E contract");
    contract
        .as_object_mut()
        .expect("E2E contract object")
        .remove("generated_at");
    std::fs::write(
        &contract_path,
        serde_json::to_vec_pretty(&contract).expect("serialize missing-timestamp E2E contract"),
    )
    .expect("write missing-timestamp E2E contract");
    commit_performance_binding_fixture(&missing_root, "remove E2E generated_at fixture");
    let args = [
        missing_root.to_str().expect("UTF-8 missing-timestamp root"),
        missing_evidence
            .to_str()
            .expect("UTF-8 missing-timestamp evidence"),
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("must each contain generated_at"),
        "{stdout}"
    );

    let (future_root, future_evidence) = retained_e2e_evidence_fixture();
    let future = (Utc::now() + Duration::minutes(6)).to_rfc3339_opts(SecondsFormat::Secs, true);
    for name in ["evidence_contract.json", "environment.json", "summary.json"] {
        let path = future_evidence.join(name);
        let mut payload: Value = serde_json::from_slice(
            &std::fs::read(&path).expect("read future-timestamp E2E document"),
        )
        .expect("parse future-timestamp E2E document");
        payload["generated_at"] = Value::String(future.clone());
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&payload).expect("serialize future-timestamp E2E document"),
        )
        .expect("write future-timestamp E2E document");
    }
    commit_performance_binding_fixture(&future_root, "future-date E2E evidence fixture");
    let args = [
        future_root.to_str().expect("UTF-8 future-timestamp root"),
        future_evidence
            .to_str()
            .expect("UTF-8 future-timestamp evidence"),
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("more than five minutes in the future"),
        "{stdout}"
    );

    let (stale_root, stale_evidence) = retained_e2e_evidence_fixture();
    let args = [
        stale_root.to_str().expect("UTF-8 stale-limit root"),
        stale_evidence.to_str().expect("UTF-8 stale-limit evidence"),
        "0",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(stdout.contains("E2E evidence is stale"), "{stdout}");
}

#[test]
fn release_gate_embedded_e2e_validator_rejects_partial_ci_scope() {
    let marker = "if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then";
    let (root, evidence_dir) = retained_e2e_evidence_fixture();
    let environment_path = evidence_dir.join("environment.json");
    let summary_path = evidence_dir.join("summary.json");
    let mut environment: Value = serde_json::from_slice(
        &std::fs::read(&environment_path).expect("read partial-scope E2E environment"),
    )
    .expect("parse partial-scope E2E environment");
    environment["unit_targets"] = json!(["release_evidence_gate"]);
    std::fs::write(
        &environment_path,
        serde_json::to_vec_pretty(&environment).expect("serialize partial-scope environment"),
    )
    .expect("write partial-scope environment");

    let mut summary: Value = serde_json::from_slice(
        &std::fs::read(&summary_path).expect("read partial-scope E2E summary"),
    )
    .expect("parse partial-scope E2E summary");
    summary["unit_targets"] = Value::Array(vec![summary["unit_targets"][0].clone()]);
    summary["total_units"] = json!(1);
    summary["passed_units"] = json!(1);
    std::fs::write(
        &summary_path,
        serde_json::to_vec_pretty(&summary).expect("serialize partial-scope summary"),
    )
    .expect("write partial-scope summary");
    commit_performance_binding_fixture(&root, "record partial CI evidence fixture");

    let args = [
        root.to_str().expect("UTF-8 partial-scope root"),
        evidence_dir.to_str().expect("UTF-8 partial-scope evidence"),
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("integration scope does not match source classification"),
        "{stdout}"
    );
}

#[test]
fn release_gate_embedded_e2e_validator_recomputes_the_source_snapshot() {
    let marker = "if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then";
    let (root, evidence_dir) = retained_e2e_evidence_fixture();
    let forged_snapshot = format!("sha256:{}", "0".repeat(64));
    for name in ["evidence_contract.json", "environment.json", "summary.json"] {
        let path = evidence_dir.join(name);
        let mut payload: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read source-bound E2E document"))
                .expect("parse source-bound E2E document");
        payload["source_snapshot"] = Value::String(forged_snapshot.clone());
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&payload).expect("serialize forged E2E source snapshot"),
        )
        .expect("write forged E2E source snapshot");
    }
    commit_performance_binding_fixture(&root, "forge E2E source snapshot fixture");

    let args = [
        root.to_str().expect("UTF-8 forged-source root"),
        evidence_dir.to_str().expect("UTF-8 forged-source evidence"),
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("does not match the independently recomputed source commit"),
        "{stdout}"
    );
}

#[test]
fn release_gate_embedded_conformance_validator_detects_product_to_evidence_rename() {
    let marker = "if [[ -f \"$CONFORMANCE_SUMMARY\" ]]; then";
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let evidence_copy = root.join("tests/ext_conformance/reports/renamed_product.rs");
    std::fs::copy(root.join("src/lib.rs"), &evidence_copy)
        .expect("copy product bytes into the conformance evidence namespace");
    fixture_git_output(
        &root,
        &[
            "add",
            "--force",
            "--",
            "tests/ext_conformance/reports/renamed_product.rs",
        ],
    );
    fixture_git_output(&root, &["update-index", "--force-remove", "src/lib.rs"]);
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
            "represent product deletion plus evidence addition",
        ],
    );

    let args = [
        root.to_str().expect("UTF-8 rename fixture root"),
        summary_path.to_str().expect("UTF-8 rename fixture summary"),
        "90",
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    assert!(
        !output.status.success(),
        "rename attack unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("non-evidence path changed after conformance source commit: src/lib.rs"),
        "{stderr}"
    );
}

#[test]
fn release_gate_embedded_conformance_validator_rejects_partial_scenario_inventory() {
    let marker = "if [[ -f \"$CONFORMANCE_SUMMARY\" ]]; then";
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let events_path = root.join("tests/ext_conformance/reports/conformance_events.jsonl");
    let events = std::fs::read_to_string(&events_path).expect("read complete conformance events");
    let first_event = events.lines().next().expect("first conformance event");
    std::fs::write(&events_path, format!("{first_event}\n"))
        .expect("write partial conformance event inventory");
    let mut summary: Value = serde_json::from_slice(
        &std::fs::read(&summary_path).expect("read partial conformance summary"),
    )
    .expect("parse partial conformance summary");
    summary["counts"] = json!({"total": 1, "pass": 1, "fail": 0, "na": 0, "tested": 1});
    summary["per_tier"] = json!({"fixture-tier": {"total": 1, "pass": 1, "fail": 0, "na": 0}});
    std::fs::write(
        &summary_path,
        serde_json::to_vec_pretty(&summary).expect("serialize partial conformance summary"),
    )
    .expect("write partial conformance summary");
    commit_performance_binding_fixture(&root, "record partial conformance evidence fixture");

    let args = [
        root.to_str().expect("UTF-8 partial conformance root"),
        summary_path
            .to_str()
            .expect("UTF-8 partial conformance summary"),
        "90",
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    assert!(
        !output.status.success(),
        "partial conformance inventory passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("conformance event inventory does not exactly cover the source manifest"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn release_gate_embedded_conformance_validator_rechecks_final_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let marker = "if [[ -f \"$CONFORMANCE_SUMMARY\" ]]; then";
    let (root, summary_path, _) = retained_conformance_evidence_fixture();
    let wrapper_dir = root
        .parent()
        .expect("conformance fixture parent")
        .join(format!(
            "conformance-final-wrapper-{}",
            uuid::Uuid::new_v4()
        ));
    std::fs::create_dir_all(&wrapper_dir).expect("create conformance final wrapper directory");
    let wrapper = wrapper_dir.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\ncase \" $* \" in\n  *\" rev-parse --verify HEAD^{commit} \"*)\n    count=0\n    if [ -f \"$PI_CONFORMANCE_COUNT\" ]; then IFS= read -r count < \"$PI_CONFORMANCE_COUNT\"; fi\n    count=$((count + 1))\n    printf '%s\\n' \"$count\" > \"$PI_CONFORMANCE_COUNT\" || exit 97\n    if [ \"$count\" -eq 2 ]; then printf ' ' >> \"$PI_CONFORMANCE_TARGET\" || exit 98; fi\n    ;;\nesac\nexec \"$PI_CONFORMANCE_REAL_GIT\" \"$@\"\n",
    )
    .expect("write conformance final Git wrapper");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("conformance final wrapper metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)
        .expect("make conformance final Git wrapper executable");
    let counter = wrapper_dir.join("head-count");
    let real_git = git_executable_on_path();
    let current_path = std::env::var_os("PATH").expect("PATH for conformance final test");
    let mut wrapped_path = vec![wrapper_dir];
    wrapped_path.extend(std::env::split_paths(&current_path));
    let wrapped_path = std::env::join_paths(wrapped_path).expect("construct wrapped PATH");
    let args = [
        root.to_str().expect("UTF-8 final-recheck fixture root"),
        summary_path
            .to_str()
            .expect("UTF-8 final-recheck fixture summary"),
        "90",
        "168",
    ];
    let (mut command, program) = release_gate_python_command(marker, &args);
    command
        .env("PATH", wrapped_path)
        .env("PI_CONFORMANCE_REAL_GIT", real_git)
        .env("PI_CONFORMANCE_COUNT", counter)
        .env("PI_CONFORMANCE_TARGET", &summary_path);
    let output = run_release_gate_python(command, &program);
    assert!(
        !output.status.success(),
        "late conformance-summary mutation unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("conformance summary bytes changed during validation"),
        "{stderr}"
    );
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
            "#!/bin/sh\ncase \" $* \" in\n  *\" rev-parse --verify HEAD^{commit} \"*)\n    if [ ! -f \"$PI_DROPIN_RESTORE_MARKER\" ]; then\n      cp \"$PI_DROPIN_ORIGINAL\" \"$PI_DROPIN_TARGET\" || exit 97\n      : > \"$PI_DROPIN_RESTORE_MARKER\" || exit 98\n    fi\n    ;;\nesac\nexec \"$PI_DROPIN_REAL_GIT\" \"$@\"\n",
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
fn release_gate_embedded_dropin_validator_rejects_self_reported_lane_pass() {
    let marker = "DROPIN_VERDICT=\"$PROJECT_ROOT/docs/evidence/dropin-certification-verdict.json\"";

    let (root, contract_path, verdict_path) = retained_certified_dropin_lane_fixture("pass");
    let args = [
        root.to_str().expect("UTF-8 passing drop-in fixture root"),
        contract_path
            .to_str()
            .expect("UTF-8 passing drop-in contract path"),
        verdict_path
            .to_str()
            .expect("UTF-8 passing drop-in verdict path"),
        "0",
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "passing drop-in lane child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("pass|"),
        "the fully bound passing lane fixture must reach the positive decision: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let (root, contract_path, verdict_path) = retained_certified_dropin_lane_fixture("fail");
    let args = [
        root.to_str().expect("UTF-8 certified drop-in fixture root"),
        contract_path
            .to_str()
            .expect("UTF-8 certified drop-in contract path"),
        verdict_path
            .to_str()
            .expect("UTF-8 certified drop-in verdict path"),
        "0",
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "the drop-in validator reports a lane mismatch through stdout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("does not match canonical passing gate non_mock_unit"),
        "a verdict's self-reported lane pass must not override the actual lane artifact: {stdout}"
    );
}

#[test]
fn release_gate_embedded_dropin_validator_rejects_skeletal_stale_and_contradictory_lanes() {
    let marker = "DROPIN_VERDICT=\"$PROJECT_ROOT/docs/evidence/dropin-certification-verdict.json\"";

    let (skeletal_root, contract_path, verdict_path) =
        retained_certified_dropin_lane_fixture("pass");
    std::fs::write(
        skeletal_root.join("tests/full_suite_gate/certification_verdict.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": "pi.ci.certification_lane.v1",
            "verdict": "pass"
        }))
        .expect("serialize skeletal lane"),
    )
    .expect("write skeletal lane");
    let skeletal_source =
        commit_performance_binding_fixture(&skeletal_root, "record skeletal lane fixture");
    let mut skeletal_verdict: Value =
        serde_json::from_slice(&std::fs::read(&verdict_path).expect("read skeletal-lane verdict"))
            .expect("parse skeletal-lane verdict");
    skeletal_verdict["git_commit"] = Value::String(skeletal_source);
    std::fs::write(
        &verdict_path,
        serde_json::to_vec_pretty(&skeletal_verdict).expect("serialize skeletal-lane verdict"),
    )
    .expect("write skeletal-lane verdict");
    commit_performance_binding_fixture(&skeletal_root, "bind skeletal lane verdict");
    let args = [
        skeletal_root.to_str().expect("UTF-8 skeletal lane root"),
        contract_path.to_str().expect("UTF-8 skeletal contract"),
        verdict_path.to_str().expect("UTF-8 skeletal verdict"),
        "0",
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(stdout.contains("top-level fields"), "{stdout}");

    let (stale_root, contract_path, verdict_path) = retained_certified_dropin_lane_fixture("pass");
    let lane_path = stale_root.join("tests/full_suite_gate/certification_verdict.json");
    let mut stale_lane: Value =
        serde_json::from_slice(&std::fs::read(&lane_path).expect("read stale certification lane"))
            .expect("parse stale certification lane");
    let stale_time =
        (Utc::now() - Duration::hours(169)).to_rfc3339_opts(SecondsFormat::Millis, true);
    stale_lane["generated_at"] = Value::String(stale_time.clone());
    stale_lane["waiver_audit"]["generated_at"] = Value::String(stale_time);
    std::fs::write(
        &lane_path,
        serde_json::to_vec_pretty(&stale_lane).expect("serialize stale certification lane"),
    )
    .expect("write stale certification lane");
    let stale_source = commit_performance_binding_fixture(&stale_root, "record stale lane fixture");
    let mut stale_verdict: Value =
        serde_json::from_slice(&std::fs::read(&verdict_path).expect("read stale-lane verdict"))
            .expect("parse stale-lane verdict");
    stale_verdict["git_commit"] = Value::String(stale_source);
    std::fs::write(
        &verdict_path,
        serde_json::to_vec_pretty(&stale_verdict).expect("serialize stale-lane verdict"),
    )
    .expect("write stale-lane verdict");
    commit_performance_binding_fixture(&stale_root, "bind stale lane verdict");
    let args = [
        stale_root.to_str().expect("UTF-8 stale lane root"),
        contract_path.to_str().expect("UTF-8 stale contract"),
        verdict_path.to_str().expect("UTF-8 stale verdict"),
        "0",
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("certification lane evidence is stale"),
        "{stdout}"
    );

    let (contradictory_root, contract_path, verdict_path) =
        retained_certified_dropin_lane_fixture("pass");
    let lane_path = contradictory_root.join("tests/full_suite_gate/certification_verdict.json");
    let mut contradictory: Value = serde_json::from_slice(
        &std::fs::read(&lane_path).expect("read contradictory certification lane"),
    )
    .expect("parse contradictory certification lane");
    contradictory["summary"]["passed"] = json!(19);
    std::fs::write(
        &lane_path,
        serde_json::to_vec_pretty(&contradictory)
            .expect("serialize contradictory certification lane"),
    )
    .expect("write contradictory certification lane");
    let contradictory_source = commit_performance_binding_fixture(
        &contradictory_root,
        "record contradictory lane fixture",
    );
    let mut contradictory_verdict: Value = serde_json::from_slice(
        &std::fs::read(&verdict_path).expect("read contradictory-lane verdict"),
    )
    .expect("parse contradictory-lane verdict");
    contradictory_verdict["git_commit"] = Value::String(contradictory_source);
    std::fs::write(
        &verdict_path,
        serde_json::to_vec_pretty(&contradictory_verdict)
            .expect("serialize contradictory-lane verdict"),
    )
    .expect("write contradictory-lane verdict");
    commit_performance_binding_fixture(&contradictory_root, "bind contradictory lane verdict");
    let args = [
        contradictory_root
            .to_str()
            .expect("UTF-8 contradictory lane root"),
        contract_path
            .to_str()
            .expect("UTF-8 contradictory contract"),
        verdict_path.to_str().expect("UTF-8 contradictory verdict"),
        "0",
        "168",
    ];
    let (command, program) = release_gate_python_command(marker, &args);
    let output = run_release_gate_python(command, &program);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("fail|"), "{stdout}");
    assert!(
        stdout.contains("summary does not describe 20 canonical passing gates"),
        "{stdout}"
    );
}

#[test]
fn release_gate_embedded_git_wrappers_ignore_hostile_replacement_objects() {
    let e2e_marker = "if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then";
    let (e2e_root, evidence_dir) = retained_e2e_evidence_fixture();
    let replacement_base = install_hostile_head_replacement(&e2e_root);
    let environment = parse_release_json(
        &std::fs::read(evidence_dir.join("environment.json"))
            .expect("read E2E replacement fixture environment"),
    )
    .expect("parse E2E replacement fixture environment");
    let source_commit = environment
        .get("git_sha")
        .and_then(Value::as_str)
        .expect("E2E fixture source commit");

    let mut hostile_git = std::process::Command::new("git");
    hostile_git.arg("-C").arg(&e2e_root);
    scrub_git_environment(&mut hostile_git);
    let hostile_ancestry = hostile_git
        .env_remove("GIT_NO_REPLACE_OBJECTS")
        .env("GIT_REPLACE_REF_BASE", &replacement_base)
        .args(["merge-base", "--is-ancestor", source_commit, "HEAD"])
        .output()
        .expect("run raw Git under hostile replacement refs");
    assert_eq!(
        hostile_ancestry.status.code(),
        Some(1),
        "fixture replacement must actually erase the source ancestry"
    );

    let e2e_root_arg = e2e_root.to_str().expect("UTF-8 E2E replacement root");
    let evidence_arg = evidence_dir
        .to_str()
        .expect("UTF-8 E2E replacement evidence path");
    let (mut command, program) =
        release_gate_python_command(e2e_marker, &[e2e_root_arg, evidence_arg, "168"]);
    command
        .env("GIT_REPLACE_REF_BASE", &replacement_base)
        .env("GIT_NO_REPLACE_OBJECTS", "0");
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "E2E hostile replacement child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("pass|"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let conformance_marker = "if [[ -f \"$CONFORMANCE_SUMMARY\" ]]; then";
    let (conformance_root, summary_path, source_commit) = retained_conformance_evidence_fixture();
    let replacement_base = install_hostile_head_replacement(&conformance_root);
    let mut hostile_git = std::process::Command::new("git");
    hostile_git.arg("-C").arg(&conformance_root);
    scrub_git_environment(&mut hostile_git);
    let hostile_ancestry = hostile_git
        .env_remove("GIT_NO_REPLACE_OBJECTS")
        .env("GIT_REPLACE_REF_BASE", &replacement_base)
        .args(["merge-base", "--is-ancestor", &source_commit, "HEAD"])
        .output()
        .expect("run raw conformance Git under hostile replacement refs");
    assert_eq!(
        hostile_ancestry.status.code(),
        Some(1),
        "fixture replacement must erase the conformance source ancestry"
    );

    let args = [
        conformance_root
            .to_str()
            .expect("UTF-8 conformance replacement root"),
        summary_path
            .to_str()
            .expect("UTF-8 conformance replacement summary"),
        "90",
        "168",
    ];
    let (mut command, program) = release_gate_python_command(conformance_marker, &args);
    command
        .env("GIT_REPLACE_REF_BASE", replacement_base)
        .env("GIT_NO_REPLACE_OBJECTS", "0");
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "conformance hostile replacement child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("2\t2\t0\t0\t2\t100\t1\t{source_commit}\tfixture-run\tfixture-correlation\n"),
        "conformance validator must preserve the real source ancestry under a hostile ambient Git context"
    );

    let dropin_marker =
        "DROPIN_VERDICT=\"$PROJECT_ROOT/docs/evidence/dropin-certification-verdict.json\"";
    let (dropin_root, contract_path, verdict_path) = retained_dropin_evidence_fixture();
    let replacement_base = install_hostile_head_replacement(&dropin_root);
    let args = [
        dropin_root
            .to_str()
            .expect("UTF-8 drop-in replacement root"),
        contract_path
            .to_str()
            .expect("UTF-8 drop-in replacement contract"),
        verdict_path
            .to_str()
            .expect("UTF-8 drop-in replacement verdict"),
        "0",
        "168",
    ];
    let (mut command, program) = release_gate_python_command(dropin_marker, &args);
    command
        .env("GIT_REPLACE_REF_BASE", replacement_base)
        .env("GIT_NO_REPLACE_OBJECTS", "0");
    let output = run_release_gate_python(command, &program);
    assert!(
        output.status.success(),
        "drop-in hostile replacement child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("warn|"), "{stdout}");
    assert!(
        stdout.contains("release-source drop-in verdict is not certified"),
        "hostile Git context must not divert the drop-in validator into a provenance warning: {stdout}"
    );
    assert!(
        !stdout.contains("not an ancestor"),
        "drop-in replacement-object attack changed the validator decision: {stdout}"
    );
}

#[test]
fn release_gate_embedded_git_wrappers_are_pinned_and_sanitized() {
    for (label, marker, requires_binding_call) in [
        (
            "repository snapshot",
            "capture_repository_snapshot() {",
            false,
        ),
        ("E2E", "if [[ -f \"$EVIDENCE_CONTRACT\" ]]; then", true),
        (
            "conformance",
            "if [[ -f \"$CONFORMANCE_SUMMARY\" ]]; then",
            true,
        ),
        (
            "performance",
            "if [[ -f \"$PERFORMANCE_SUMMARY\" ]]; then",
            true,
        ),
        (
            "drop-in",
            "DROPIN_VERDICT=\"$PROJECT_ROOT/docs/evidence/dropin-certification-verdict.json\"",
            true,
        ),
    ] {
        let program = release_gate_embedded_python(marker);
        assert!(
            program.contains("not key.startswith(\"GIT_\")"),
            "{label} Git wrapper must discard every ambient GIT_* variable"
        );
        assert!(
            program.contains("env[\"GIT_NO_REPLACE_OBJECTS\"] = \"1\""),
            "{label} Git wrapper must disable replacement objects"
        );
        for assignment in [
            "env[\"GIT_CONFIG_GLOBAL\"] = os.devnull",
            "env[\"GIT_CONFIG_NOSYSTEM\"] = \"1\"",
            "env[\"GIT_LITERAL_PATHSPECS\"] = \"1\"",
            "env[\"GIT_NO_REPLACE_OBJECTS\"] = \"1\"",
            "env[\"GIT_OPTIONAL_LOCKS\"] = \"0\"",
            "env[\"GIT_TERMINAL_PROMPT\"] = \"0\"",
        ] {
            assert!(
                program.contains(assignment),
                "{label} Git wrapper is missing exact environment control {assignment}"
            );
        }
        assert!(
            program.contains("\"--git-dir\"") && program.contains("\"--work-tree\""),
            "{label} Git wrapper must pin the resolved repository context"
        );
        for setting in ["core.bare=false", "core.fsmonitor=false", "core.worktree="] {
            assert!(
                program.contains(setting),
                "{label} Git wrapper is missing pinned setting {setting}"
            );
        }
        assert!(
            program.contains("--show-toplevel") && program.contains("--absolute-git-dir"),
            "{label} Git wrapper must probe both sides of its repository binding"
        );
        if requires_binding_call {
            assert!(
                program.matches("verify_repository_binding()").count() >= 2,
                "{label} must invoke verify_repository_binding(), not merely define it"
            );
        }
    }

    let conformance = release_gate_embedded_python("if [[ -f \"$CONFORMANCE_SUMMARY\" ]]; then");
    assert!(
        conformance.contains("git_result(\"merge-base\", \"--is-ancestor\", source_commit, head)"),
        "conformance ancestry must use the hardened Git wrapper"
    );
}

#[test]
fn release_gate_embedded_dropin_validator_rejects_future_and_stale_evidence() {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("pi-release-evidence-gate-fixtures");
    std::fs::create_dir_all(&base).expect("create retained release-gate fixture base");
    // See retained_performance_binding_fixture: keep the supplied fixture root
    // equal to its canonical form so macOS TMPDIR symlinks do not trip the
    // embedded validators' strict path binding.
    let base = std::fs::canonicalize(&base).expect("canonicalize release-gate fixture base");
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
    fixture_git_output(&root, &["init", "--quiet", "--initial-branch=main"]);
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
    let (summary, source_binding_valid) = load_source_bound_performance_summary()
        .unwrap_or_else(|err| panic!("invalid asserted performance source binding: {err}"));
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
fn performance_summary_parse_cannot_be_substituted_before_source_binding() {
    let (root, source_commit) = retained_performance_binding_fixture(false);
    let summary_path = root.join(PERFORMANCE_BUDGET_SUMMARY_PATH);
    let committed_summary = serde_json::to_vec(&json!({
        "fixture": true,
        "source_commit": source_commit,
    }))
    .expect("serialize committed performance summary");
    std::fs::write(&summary_path, &committed_summary)
        .expect("write committed source-bound performance summary");
    commit_performance_binding_fixture(&root, "add source-bound performance summary");

    let substituted_summary = serde_json::to_vec(&json!({
        "fixture": false,
        "source_commit": source_commit,
    }))
    .expect("serialize substituted performance summary");
    std::fs::write(&summary_path, substituted_summary)
        .expect("substitute bytes before the unbound parse");

    let error = load_source_bound_performance_summary_at_with_probe(
        &root,
        PERFORMANCE_BUDGET_SUMMARY_PATH,
        || {
            std::fs::write(&summary_path, &committed_summary)
                .map_err(|err| format!("restore committed performance summary: {err}"))
        },
    )
    .expect_err("restoring different bytes before binding must not validate the parsed payload");
    assert!(
        error.contains("changed between its initial parse and source binding"),
        "{error}"
    );
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

    let stale = blocked_performance_summary_fixture(now - Duration::hours(169));
    assert!(
        validate_performance_budget_summary(&stale, now, Duration::hours(168), false).is_err(),
        "blocked/NO_DATA evidence must still satisfy the configured freshness limit"
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
fn performance_contract_rejects_forged_tampered_and_unproven_measurements() {
    let now = Utc::now();

    let mut forged = claim_ready_performance_summary_fixture(now);
    let binary_index = forged["budget_results"]
        .as_array()
        .expect("fixture budget results")
        .iter()
        .position(|result| result["budget_name"] == "binary_size_release")
        .expect("binary-size result");
    forged["budget_results"][binary_index]["source"] = json!("fixture://release/pi");
    let error = validate_performance_budget_summary(&forged, now, Duration::hours(168), true)
        .expect_err("an unproven binary measurement must be rejected");
    assert!(error.contains("negative-control proof"), "{error}");

    let mut tampered = claim_ready_performance_summary_fixture(now);
    let source = tampered["budget_results"][binary_index]["source"]
        .as_str()
        .expect("binary proof source")
        .replace("size_bytes=50331648", "size_bytes=1");
    tampered["budget_results"][binary_index]["source"] = json!(source);
    let error = validate_performance_budget_summary(&tampered, now, Duration::hours(168), true)
        .expect_err("a size claim detached from its measured bytes must be rejected");
    assert!(error.contains("size_bytes"), "{error}");

    let mut noisy = claim_ready_performance_summary_fixture(now);
    let cold_index = noisy["budget_results"]
        .as_array()
        .expect("fixture budget results")
        .iter()
        .position(|result| result["budget_name"] == "ext_cold_load_simple_p95")
        .expect("cold-load result");
    let source = noisy["budget_results"][cold_index]["source"]
        .as_str()
        .expect("cold-load proof source")
        .replace("noise_score=0", "noise_score=1");
    noisy["budget_results"][cold_index]["source"] = json!(source);
    let error = validate_performance_budget_summary(&noisy, now, Duration::hours(168), true)
        .expect_err("a noisy benchmark environment must be rejected");
    assert!(error.contains("noisy bench-env"), "{error}");

    let mut unnamed_missing = blocked_performance_summary_fixture(now);
    let failures = unnamed_missing["failing_data_contracts"]
        .as_array_mut()
        .expect("fixture failures");
    failures.retain(|failure| failure["budget_name"] != "binary_size_release");
    unnamed_missing["data_contract_failures_count"] = json!(failures.len());
    let error =
        validate_performance_budget_summary(&unnamed_missing, now, Duration::hours(168), false)
            .expect_err("NO_DATA must identify the failed binary measurement control");
    assert!(
        error.contains("named measurement-control failure"),
        "{error}"
    );
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
