//! Live provider E2E harness (real APIs, short prompt, rich JSONL logging).
//!
//! This test is intentionally gated behind `CI_E2E_TESTS=1` to avoid unexpected
//! network usage during normal `cargo test`.

mod common;

use common::{
    LIVE_E2E_EXECUTION_MODE, LIVE_E2E_MAX_ATTEMPTS, LIVE_E2E_REPLAY_BOUNDARY,
    LIVE_E2E_RETRY_BACKOFF_MS, LIVE_E2E_TRACE_ORIGIN, LIVE_SHORT_PROMPT, LiveE2eRegistry,
    LiveProviderTarget, TestHarness, check_cost_budget, ci_e2e_tests_enabled,
    default_cost_thresholds, find_unredacted_keys, run_live_provider_target, validate_jsonl,
    write_live_provider_runs_jsonl,
};
use pi::model::Usage;
use pi::provider::ModelCost;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const LIVE_TARGETS: [LiveProviderTarget; 7] = [
    LiveProviderTarget::new(
        "anthropic",
        "ANTHROPIC_TEST_MODEL",
        &[
            "claude-haiku-4-5",
            "claude-3-5-haiku-20241022",
            "claude-sonnet-4-5",
        ],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new(
        "openai",
        "OPENAI_TEST_MODEL",
        &["gpt-4o-mini", "gpt-4o", "gpt-5.1-codex"],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new(
        "google",
        "GOOGLE_TEST_MODEL",
        &["gemini-2.5-flash", "gemini-1.5-flash", "gemini-2.5-pro"],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new(
        "openrouter",
        "OPENROUTER_TEST_MODEL",
        &[],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new(
        "orcarouter",
        "ORCAROUTER_TEST_MODEL",
        &[],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new("xai", "XAI_TEST_MODEL", &[], LIVE_SHORT_PROMPT),
    LiveProviderTarget::new("deepseek", "DEEPSEEK_TEST_MODEL", &[], LIVE_SHORT_PROMPT),
];

const LIVE_PROVIDER_RESULT_SCHEMA: &str = "pi.test.live.result.v1";
const LIVE_PROVIDER_COST_SCHEMA: &str = "pi.test.live.cost.v1";
const REDACTED_VALUE: &str = "[REDACTED]";
const LIVE_PROVIDER_FILTER_ENV: &str = "PI_LIVE_E2E_PROVIDER";
const LIVE_EXPORT_DIR_ENV: &str = "PI_E2E_EXPORT_DIR";

const SENSITIVE_KEY_FRAGMENTS: [&str; 10] = [
    "api_key",
    "api-key",
    "authorization",
    "bearer",
    "cookie",
    "credential",
    "password",
    "private_key",
    "secret",
    "token",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveProviderResultRecord {
    schema: String,
    #[serde(rename = "type")]
    record_type: String,
    provider: String,
    model: Option<String>,
    api: Option<String>,
    status: String,
    skip_reason: Option<String>,
    error: Option<String>,
    elapsed_ms: u64,
    response_status: Option<u16>,
    request_url: Option<String>,
    request_headers: Vec<(String, String)>,
    request_body_bytes: Option<usize>,
    event_count: usize,
    text_chars: usize,
    thinking_chars: usize,
    tool_calls: usize,
    stop_reason: Option<String>,
    usage: Usage,
    attempts: u32,
    retry_backoff_ms: Vec<u64>,
    credential_source: Option<String>,
    execution_mode: String,
    replay_boundary: String,
    trace_origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveProviderCostRecord {
    schema: String,
    #[serde(rename = "type")]
    record_type: String,
    provider: String,
    model: Option<String>,
    status: String,
    usage_fallback: String,
    cost_source: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    total_tokens: u64,
    provider_reported_cost_usd: Option<f64>,
    estimated_cost_usd: Option<f64>,
    total_cost_usd: f64,
    budget_outcome: String,
    warn_threshold_usd: Option<f64>,
    fail_threshold_usd: Option<f64>,
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "totaltokens"
            | "total_tokens"
            | "inputtokens"
            | "input_tokens"
            | "outputtokens"
            | "output_tokens"
            | "cache_read_tokens"
            | "cache_write_tokens"
    ) {
        return false;
    }
    SENSITIVE_KEY_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

fn scan_for_unredacted_header_pairs(value: &Value, path: &str, leaks: &mut Vec<String>) {
    match value {
        Value::Object(obj) => {
            for (key, child) in obj {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if is_sensitive_key(key)
                    && child
                        .as_str()
                        .is_some_and(|raw| !raw.is_empty() && raw != REDACTED_VALUE)
                {
                    leaks.push(format!("{child_path} contains unredacted sensitive value"));
                }
                scan_for_unredacted_header_pairs(child, &child_path, leaks);
            }
        }
        Value::Array(items) => {
            if items.len() == 2
                && let (Some(key), Some(raw)) = (items[0].as_str(), items[1].as_str())
                && is_sensitive_key(key)
                && !raw.is_empty()
                && raw != REDACTED_VALUE
            {
                leaks.push(format!(
                    "{path}[\"{key}\"] contains unredacted sensitive value"
                ));
            }
            for (index, child) in items.iter().enumerate() {
                scan_for_unredacted_header_pairs(child, &format!("{path}[{index}]"), leaks);
            }
        }
        _ => {}
    }
}

fn assert_json_values_redacted(values: &[Value], label: &str) {
    fn is_allowed_metric_field(path: &str) -> bool {
        matches!(
            path.rsplit('.').next(),
            Some(
                "totalTokens"
                    | "total_tokens"
                    | "inputTokens"
                    | "input_tokens"
                    | "outputTokens"
                    | "output_tokens"
                    | "cacheReadTokens"
                    | "cache_read_tokens"
                    | "cacheWriteTokens"
                    | "cache_write_tokens"
            )
        )
    }

    let mut leaks = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let path = format!("{label}[{}]", index + 1);
        for leak in find_unredacted_keys(value) {
            if is_allowed_metric_field(&leak) {
                continue;
            }
            leaks.push(format!("{path}: {leak}"));
        }
        scan_for_unredacted_header_pairs(value, &path, &mut leaks);
    }
    assert!(
        leaks.is_empty(),
        "{label} contains unredacted sensitive material:\n{}",
        leaks.join("\n"),
    );
}

fn write_jsonl_records<T: Serialize>(path: &Path, records: &[T]) -> std::io::Result<()> {
    let mut output = String::new();
    for record in records {
        output.push_str(
            &serde_json::to_string(record)
                .unwrap_or_else(|_| "{\"serialization\":\"error\"}".to_string()),
        );
        output.push('\n');
    }
    std::fs::write(path, output)
}

fn load_jsonl_values(path: &Path, label: &str) -> Vec<Value> {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read {label} {}: {err}", path.display()));
    content
        .lines()
        .enumerate()
        .filter_map(|(index, raw)| {
            let line = raw.trim();
            if line.is_empty() {
                return None;
            }
            Some(serde_json::from_str::<Value>(line).unwrap_or_else(|err| {
                panic!(
                    "parse {label} line {} at {}: {err}",
                    index + 1,
                    path.display()
                )
            }))
        })
        .collect()
}

fn load_jsonl_records<T>(path: &Path, label: &str) -> Vec<T>
where
    T: for<'de> Deserialize<'de>,
{
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read {label} {}: {err}", path.display()));
    content
        .lines()
        .enumerate()
        .filter_map(|(index, raw)| {
            let line = raw.trim();
            if line.is_empty() {
                return None;
            }
            Some(serde_json::from_str::<T>(line).unwrap_or_else(|err| {
                panic!(
                    "decode {label} line {} at {}: {err}",
                    index + 1,
                    path.display()
                )
            }))
        })
        .collect()
}

fn to_json_values<T: Serialize>(records: &[T], label: &str) -> Vec<Value> {
    records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            serde_json::to_value(record)
                .unwrap_or_else(|err| panic!("serialize {label} record {}: {err}", index + 1))
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)] // token counts in these tests are small and bounded
fn estimate_from_model_rates(usage: &Usage, model_cost: &ModelCost) -> f64 {
    let input_cost = (model_cost.input / 1_000_000.0) * usage.input as f64;
    let output_cost = (model_cost.output / 1_000_000.0) * usage.output as f64;
    let cache_read_cost = (model_cost.cache_read / 1_000_000.0) * usage.cache_read as f64;
    let cache_write_cost = (model_cost.cache_write / 1_000_000.0) * usage.cache_write as f64;
    input_cost + output_cost + cache_read_cost + cache_write_cost
}

fn normalize_jsonl_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("log.jsonl");
    file_name.strip_suffix(".jsonl").map_or_else(
        || path.with_file_name(format!("{file_name}.normalized.jsonl")),
        |prefix| path.with_file_name(format!("{prefix}.normalized.jsonl")),
    )
}

fn build_model_cost_lookup(registry: &LiveE2eRegistry) -> BTreeMap<(String, String), ModelCost> {
    registry
        .registry
        .models()
        .iter()
        .map(|entry| {
            (
                (entry.model.provider.clone(), entry.model.id.clone()),
                entry.model.cost.clone(),
            )
        })
        .collect()
}

fn validate_live_result_records(records: &[LiveProviderResultRecord], expected: usize) {
    assert_eq!(
        records.len(),
        expected,
        "live provider result record count mismatch",
    );
    for (index, record) in records.iter().enumerate() {
        assert_eq!(
            record.schema,
            LIVE_PROVIDER_RESULT_SCHEMA,
            "unexpected result schema at line {}",
            index + 1
        );
        assert_eq!(
            record.record_type,
            "provider_summary",
            "unexpected result type at line {}",
            index + 1
        );
        assert!(
            !record.provider.trim().is_empty(),
            "result provider missing at line {}",
            index + 1
        );
        assert!(
            matches!(record.status.as_str(), "passed" | "failed" | "skipped"),
            "invalid result status at line {}: {}",
            index + 1,
            record.status
        );
        assert_eq!(
            record.execution_mode,
            LIVE_E2E_EXECUTION_MODE,
            "unexpected execution mode at line {}",
            index + 1
        );
        assert_eq!(
            record.replay_boundary,
            LIVE_E2E_REPLAY_BOUNDARY,
            "unexpected replay boundary at line {}",
            index + 1
        );
        assert_eq!(
            record.trace_origin,
            LIVE_E2E_TRACE_ORIGIN,
            "unexpected trace origin at line {}",
            index + 1
        );

        let attempts = usize::try_from(record.attempts).unwrap_or(usize::MAX);
        let expected_backoffs = attempts.saturating_sub(1);
        assert!(
            record.retry_backoff_ms.len() <= expected_backoffs,
            "retry backoff count exceeds attempts at line {}",
            index + 1
        );
        if attempts > 1 {
            assert!(
                !record.retry_backoff_ms.is_empty(),
                "missing retry backoff schedule at line {}",
                index + 1
            );
        }
        for (retry_index, backoff_ms) in record.retry_backoff_ms.iter().enumerate() {
            let expected_ms =
                LIVE_E2E_RETRY_BACKOFF_MS[retry_index.min(LIVE_E2E_RETRY_BACKOFF_MS.len() - 1)];
            assert_eq!(
                *backoff_ms,
                expected_ms,
                "unexpected retry backoff at line {} retry {}",
                index + 1,
                retry_index + 1
            );
        }

        if record.status == "skipped" {
            assert_eq!(
                record.attempts,
                0,
                "skipped runs should report zero attempts at line {}",
                index + 1
            );
        } else {
            assert!(
                (1..=LIVE_E2E_MAX_ATTEMPTS).contains(&attempts),
                "attempt count out of range at line {}: {}",
                index + 1,
                record.attempts
            );
            assert!(
                record
                    .credential_source
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty()),
                "non-skipped runs must report credential_source at line {}",
                index + 1
            );
        }
    }
}

fn validate_live_cost_records(records: &[LiveProviderCostRecord], expected: usize) {
    assert_eq!(
        records.len(),
        expected,
        "live provider cost record count mismatch",
    );
    for (index, record) in records.iter().enumerate() {
        assert_eq!(
            record.schema,
            LIVE_PROVIDER_COST_SCHEMA,
            "unexpected cost schema at line {}",
            index + 1
        );
        assert_eq!(
            record.record_type,
            "cost_summary",
            "unexpected cost type at line {}",
            index + 1
        );
        assert!(
            !record.provider.trim().is_empty(),
            "cost provider missing at line {}",
            index + 1
        );
        assert!(
            matches!(record.budget_outcome.as_str(), "ok" | "warn" | "fail"),
            "invalid cost budget outcome at line {}: {}",
            index + 1,
            record.budget_outcome
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)] // intentional: this is the end-to-end harness orchestration test
fn e2e_live_provider_harness_smoke() {
    let harness = TestHarness::new("e2e_live_provider_harness_smoke");

    if !ci_e2e_tests_enabled() {
        harness.log().warn(
            "live_e2e",
            "Skipping live provider E2E harness (set CI_E2E_TESTS=1 to enable)",
        );
        return;
    }

    let registry = LiveE2eRegistry::load(harness.log())
        .unwrap_or_else(|err| panic!("failed to load live E2E registry: {err}"));

    asupersync::test_utils::run_test(|| {
        let harness_ref = &harness;
        let registry = registry.clone();
        async move {
            let provider_filter = std::env::var(LIVE_PROVIDER_FILTER_ENV)
                .ok()
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty());
            let filtered_targets: Vec<_> = LIVE_TARGETS
                .iter()
                .copied()
                .filter(|target| {
                    provider_filter
                        .as_deref()
                        .is_none_or(|filter| target.provider == filter)
                })
                .collect();

            if let Some(filter) = provider_filter.as_deref() {
                assert!(
                    !filtered_targets.is_empty(),
                    "invalid {LIVE_PROVIDER_FILTER_ENV}='{filter}'; expected one of: {}",
                    LIVE_TARGETS
                        .iter()
                        .map(|target| target.provider)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            let vcr_dir = harness_ref.temp_path("live_provider_vcr");
            std::fs::create_dir_all(&vcr_dir)
                .unwrap_or_else(|err| panic!("create live provider vcr dir: {err}"));

            let mut runs = Vec::with_capacity(filtered_targets.len());
            for target in &filtered_targets {
                let run = run_live_provider_target(harness_ref, &registry, target, &vcr_dir).await;
                runs.push(run);
            }

            let raw_results_path =
                write_live_provider_runs_jsonl(harness_ref, "live_provider_results.jsonl", &runs)
                    .unwrap_or_else(|err| panic!("write live provider results jsonl: {err}"));
            let raw_result_values =
                load_jsonl_values(&raw_results_path, "raw live provider results");

            let run_records: Vec<LiveProviderResultRecord> = runs
                .iter()
                .map(|run| LiveProviderResultRecord {
                    schema: LIVE_PROVIDER_RESULT_SCHEMA.to_string(),
                    record_type: "provider_summary".to_string(),
                    provider: run.provider.clone(),
                    model: run.model.clone(),
                    api: run.api.clone(),
                    status: run.status.clone(),
                    skip_reason: run.skip_reason.clone(),
                    error: run.error.clone(),
                    elapsed_ms: run.elapsed_ms,
                    response_status: run.response_status,
                    request_url: run.request_url.clone(),
                    request_headers: run.request_headers.clone(),
                    request_body_bytes: run.request_body_bytes,
                    event_count: run.event_count,
                    text_chars: run.text_chars,
                    thinking_chars: run.thinking_chars,
                    tool_calls: run.tool_calls,
                    stop_reason: run.stop_reason.clone(),
                    usage: run.usage.clone(),
                    attempts: run.attempts,
                    retry_backoff_ms: run.retry_backoff_ms.clone(),
                    credential_source: run.credential_source.clone(),
                    execution_mode: run.execution_mode.clone(),
                    replay_boundary: run.replay_boundary.clone(),
                    trace_origin: run.trace_origin.clone(),
                })
                .collect();
            let run_contract_path = harness_ref.temp_path("live_provider_results.contract.jsonl");
            write_jsonl_records(&run_contract_path, &run_records)
                .unwrap_or_else(|err| panic!("write live provider result contract jsonl: {err}"));
            harness_ref.record_artifact("live_provider_results.contract.jsonl", &run_contract_path);

            let model_cost_lookup = build_model_cost_lookup(&registry);
            let thresholds = default_cost_thresholds();
            let mut cost_records = Vec::new();
            let mut cost_failures = Vec::new();

            for run in &runs {
                let threshold = thresholds.iter().find(|item| item.provider == run.provider);
                let model_cost = run.model.as_ref().and_then(|model| {
                    model_cost_lookup.get(&(run.provider.clone(), model.clone()))
                });

                let usage = &run.usage;
                let has_usage_fields = usage.input > 0
                    || usage.output > 0
                    || usage.cache_read > 0
                    || usage.cache_write > 0
                    || usage.total_tokens > 0;
                let breakdown_total = usage.cost.input
                    + usage.cost.output
                    + usage.cost.cache_read
                    + usage.cost.cache_write;

                let provider_reported_cost = if usage.cost.total > 0.0 {
                    Some(usage.cost.total)
                } else {
                    None
                };

                let estimated_cost = model_cost.and_then(|rates| {
                    let estimate = estimate_from_model_rates(usage, rates);
                    if estimate > 0.0 { Some(estimate) } else { None }
                });

                let (total_cost, usage_fallback, cost_source) = if run.status == "skipped" {
                    (0.0, "skipped".to_string(), "not_applicable".to_string())
                } else if let Some(total) = provider_reported_cost {
                    (
                        total,
                        "none".to_string(),
                        "provider_reported_total".to_string(),
                    )
                } else if breakdown_total > 0.0 {
                    (
                        breakdown_total,
                        "none".to_string(),
                        "provider_reported_breakdown_sum".to_string(),
                    )
                } else if let Some(estimate) = estimated_cost {
                    (
                        estimate,
                        "missing_vendor_cost_fields".to_string(),
                        "estimated_from_model_rates".to_string(),
                    )
                } else if has_usage_fields {
                    (
                        0.0,
                        "missing_vendor_cost_fields".to_string(),
                        "no_cost_available".to_string(),
                    )
                } else {
                    (
                        0.0,
                        "missing_vendor_usage_fields".to_string(),
                        "no_usage_or_cost_available".to_string(),
                    )
                };

                let budget_outcome = if run.status == "skipped" {
                    "ok".to_string()
                } else {
                    let outcome = check_cost_budget(&run.provider, total_cost, &thresholds);
                    match &outcome {
                        common::CostBudgetOutcome::Ok => "ok".to_string(),
                        common::CostBudgetOutcome::Warn { .. } => {
                            harness_ref.log().warn("live_e2e_cost", outcome.to_string());
                            "warn".to_string()
                        }
                        common::CostBudgetOutcome::Fail { .. } => {
                            let message = outcome.to_string();
                            harness_ref.log().error("live_e2e_cost", &message);
                            cost_failures.push(message);
                            "fail".to_string()
                        }
                    }
                };

                cost_records.push(LiveProviderCostRecord {
                    schema: LIVE_PROVIDER_COST_SCHEMA.to_string(),
                    record_type: "cost_summary".to_string(),
                    provider: run.provider.clone(),
                    model: run.model.clone(),
                    status: run.status.clone(),
                    usage_fallback,
                    cost_source,
                    input_tokens: usage.input,
                    output_tokens: usage.output,
                    cache_read_tokens: usage.cache_read,
                    cache_write_tokens: usage.cache_write,
                    total_tokens: usage.total_tokens,
                    provider_reported_cost_usd: provider_reported_cost,
                    estimated_cost_usd: estimated_cost,
                    total_cost_usd: total_cost,
                    budget_outcome,
                    warn_threshold_usd: threshold.map(|value| value.warn_dollars),
                    fail_threshold_usd: threshold.map(|value| value.fail_dollars),
                });
            }

            let cost_path = harness_ref.temp_path("live_provider_costs.jsonl");
            write_jsonl_records(&cost_path, &cost_records)
                .unwrap_or_else(|err| panic!("write live provider cost jsonl: {err}"));
            harness_ref.record_artifact("live_provider_costs.jsonl", &cost_path);

            let log_path = harness_ref.temp_path("live_provider_log.jsonl");
            harness_ref
                .write_jsonl_logs(&log_path)
                .unwrap_or_else(|err| panic!("write live provider results jsonl: {err}"));
            harness_ref.record_artifact("live_provider_log.jsonl", &log_path);
            let normalized_log_path = normalize_jsonl_path(&log_path);
            harness_ref
                .write_jsonl_logs_normalized(&normalized_log_path)
                .unwrap_or_else(|err| panic!("write normalized live provider JSONL log: {err}"));
            harness_ref.record_artifact("live_provider_log.normalized.jsonl", &normalized_log_path);

            let artifact_path = harness_ref.temp_path("live_provider_artifacts.jsonl");
            harness_ref
                .write_artifact_index_jsonl(&artifact_path)
                .unwrap_or_else(|err| panic!("write live provider artifact index: {err}"));
            harness_ref.record_artifact("live_provider_artifacts.jsonl", &artifact_path);
            let normalized_artifact_path = normalize_jsonl_path(&artifact_path);
            harness_ref
                .write_artifact_index_jsonl_normalized(&normalized_artifact_path)
                .unwrap_or_else(|err| {
                    panic!("write normalized live provider artifact index: {err}")
                });
            harness_ref.record_artifact(
                "live_provider_artifacts.normalized.jsonl",
                &normalized_artifact_path,
            );

            if let Ok(export_dir) = std::env::var(LIVE_EXPORT_DIR_ENV) {
                let export_root = PathBuf::from(export_dir);
                std::fs::create_dir_all(&export_root)
                    .unwrap_or_else(|err| panic!("create {LIVE_EXPORT_DIR_ENV} dir: {err}"));

                let export_pairs = [
                    ("live_provider_results.raw.jsonl", &raw_results_path),
                    ("live_provider_results.contract.jsonl", &run_contract_path),
                    ("live_provider_costs.jsonl", &cost_path),
                    ("live_provider_log.jsonl", &log_path),
                    ("live_provider_log.normalized.jsonl", &normalized_log_path),
                    ("live_provider_artifacts.jsonl", &artifact_path),
                    (
                        "live_provider_artifacts.normalized.jsonl",
                        &normalized_artifact_path,
                    ),
                ];

                for (name, source_path) in export_pairs {
                    let destination = export_root.join(name);
                    std::fs::copy(source_path, &destination).unwrap_or_else(|err| {
                        panic!(
                            "export {name} to {} via {LIVE_EXPORT_DIR_ENV}: {err}",
                            destination.display()
                        )
                    });
                    harness_ref.record_artifact(format!("export/{name}"), &destination);
                }
            }

            let log_content = std::fs::read_to_string(&log_path).unwrap_or_else(|err| {
                panic!("read live provider log {}: {err}", log_path.display())
            });
            let log_errors = validate_jsonl(&log_content);
            assert!(
                log_errors.is_empty(),
                "live provider log schema validation failed: {log_errors:?}",
            );

            let artifact_content = std::fs::read_to_string(&artifact_path).unwrap_or_else(|err| {
                panic!(
                    "read live provider artifact index {}: {err}",
                    artifact_path.display()
                )
            });
            let artifact_errors = validate_jsonl(&artifact_content);
            assert!(
                artifact_errors.is_empty(),
                "live provider artifact schema validation failed: {artifact_errors:?}",
            );

            let normalized_log_content = std::fs::read_to_string(&normalized_log_path)
                .unwrap_or_else(|err| {
                    panic!(
                        "read normalized live provider log {}: {err}",
                        normalized_log_path.display()
                    )
                });
            let normalized_log_errors = validate_jsonl(&normalized_log_content);
            assert!(
                normalized_log_errors.is_empty(),
                "normalized live provider log schema validation failed: {normalized_log_errors:?}",
            );

            let normalized_artifact_content = std::fs::read_to_string(&normalized_artifact_path)
                .unwrap_or_else(|err| {
                    panic!(
                        "read normalized live provider artifact index {}: {err}",
                        normalized_artifact_path.display()
                    )
                });
            let normalized_artifact_errors = validate_jsonl(&normalized_artifact_content);
            assert!(
                normalized_artifact_errors.is_empty(),
                "normalized live provider artifact schema validation failed: {normalized_artifact_errors:?}",
            );

            let temp_root = harness_ref.temp_dir().display().to_string();
            assert!(
                normalized_log_content.contains("<TIMESTAMP>"),
                "normalized live provider log should contain normalized timestamps",
            );
            assert!(
                !normalized_log_content.contains(&temp_root),
                "normalized live provider log should not contain absolute temp paths",
            );
            assert!(
                normalized_artifact_content.contains("<TIMESTAMP>"),
                "normalized artifact index should contain normalized timestamps",
            );
            assert!(
                !normalized_artifact_content.contains(&temp_root),
                "normalized artifact index should not contain absolute temp paths",
            );

            let run_contract_records = load_jsonl_records::<LiveProviderResultRecord>(
                &run_contract_path,
                "live result contract",
            );
            validate_live_result_records(&run_contract_records, runs.len());

            let cost_contract_records =
                load_jsonl_records::<LiveProviderCostRecord>(&cost_path, "live cost contract");
            validate_live_cost_records(&cost_contract_records, runs.len());

            let log_values = load_jsonl_values(&log_path, "live provider log");
            let artifact_values = load_jsonl_values(&artifact_path, "live provider artifact index");
            let normalized_log_values =
                load_jsonl_values(&normalized_log_path, "normalized live log");
            let run_contract_values = to_json_values(&run_contract_records, "run contract");
            let cost_contract_values = to_json_values(&cost_contract_records, "cost contract");

            assert_json_values_redacted(&log_values, "live provider log");
            assert_json_values_redacted(&normalized_log_values, "normalized live provider log");
            assert_json_values_redacted(&raw_result_values, "raw live provider results");
            assert_json_values_redacted(&run_contract_values, "live provider result contract");
            assert_json_values_redacted(&cost_contract_values, "live provider cost contract");
            assert_json_values_redacted(&artifact_values, "live provider artifact index");

            let total_cost_usd: f64 = cost_contract_records
                .iter()
                .map(|record| record.total_cost_usd)
                .sum();
            let cost_warnings = cost_contract_records
                .iter()
                .filter(|record| record.budget_outcome == "warn")
                .count();
            let cost_failures_count = cost_contract_records
                .iter()
                .filter(|record| record.budget_outcome == "fail")
                .count();
            harness_ref
                .log()
                .info_ctx("live_e2e_cost", "Cost telemetry summary", |ctx| {
                    ctx.push(("providers".into(), runs.len().to_string()));
                    ctx.push(("warnings".into(), cost_warnings.to_string()));
                    ctx.push(("failures".into(), cost_failures_count.to_string()));
                    ctx.push(("total_cost_usd".into(), format!("{total_cost_usd:.6}")));
                });

            let attempted = runs.iter().filter(|run| run.status != "skipped").count();
            let passed = runs.iter().filter(|run| run.status == "passed").count();
            let skipped = runs.iter().filter(|run| run.status == "skipped").count();
            let failed: Vec<String> = runs
                .iter()
                .filter(|run| run.status == "failed")
                .map(|run| {
                    format!(
                        "{}/{} ({})",
                        run.provider,
                        run.model.as_deref().unwrap_or("<none>"),
                        run.error.as_deref().unwrap_or("unknown error"),
                    )
                })
                .collect();

            harness_ref
                .log()
                .info_ctx("live_e2e", "Live harness suite summary", |ctx| {
                    ctx.push(("targets".into(), filtered_targets.len().to_string()));
                    ctx.push(("attempted".into(), attempted.to_string()));
                    ctx.push(("passed".into(), passed.to_string()));
                    ctx.push(("skipped".into(), skipped.to_string()));
                    ctx.push(("failed".into(), failed.len().to_string()));
                });

            if let Some(filter) = provider_filter.as_deref() {
                assert!(
                    runs.iter().all(|run| run.provider == filter),
                    "provider filter '{filter}' did not constrain all runs"
                );
            }

            assert!(
                attempted > 0,
                "CI_E2E_TESTS=1 but no providers were runnable. Ensure ~/.pi/agent/models.json and API keys are configured."
            );
            assert!(
                failed.is_empty(),
                "live provider harness failures: {}",
                failed.join("; ")
            );
            assert!(
                cost_failures.is_empty(),
                "live provider cost budget failures: {}",
                cost_failures.join("; ")
            );
        }
    });
}
