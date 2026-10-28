//! Deterministic provider/model routing evidence.
//!
//! This module is intentionally advisory. It evaluates fixture or cached
//! provider health metrics into redaction-safe evidence that UI and JSON
//! surfaces can display without changing the live provider invocation path.

use crate::models::ModelEntry;
use crate::provider::ModelCost;
use crate::provider_metadata::canonical_provider_id;
use serde::{Deserialize, Serialize};

pub const ROUTING_EVIDENCE_SCHEMA: &str = "pi.provider_routing.evidence.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingDecision {
    Recommended,
    Degraded,
    TemporarilyAvoided,
    StaleMetrics,
    MissingMetrics,
}

impl RoutingDecision {
    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Recommended => "ok",
            Self::Degraded => "degraded",
            Self::TemporarilyAvoided => "avoid",
            Self::StaleMetrics => "stale",
            Self::MissingMetrics => "missing",
        }
    }

    const fn severity(self) -> u8 {
        match self {
            Self::Recommended => 0,
            Self::Degraded => 1,
            Self::MissingMetrics | Self::StaleMetrics | Self::TemporarilyAvoided => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingReason {
    Healthy,
    MissingMetrics,
    InvalidMetrics,
    StaleMetrics,
    LatencyDegraded,
    LatencyCircuitOpen,
    ErrorRateDegraded,
    ErrorCircuitOpen,
    CostHintHigh,
    ConfiguredOnlyScope,
    UserOverrideHonored,
}

impl RoutingReason {
    const fn warning_label(self) -> Option<&'static str> {
        match self {
            Self::Healthy | Self::ConfiguredOnlyScope => None,
            Self::MissingMetrics => Some("missing metrics"),
            Self::InvalidMetrics => Some("invalid metrics"),
            Self::StaleMetrics => Some("stale metrics"),
            Self::LatencyDegraded => Some("latency"),
            Self::LatencyCircuitOpen => Some("high latency"),
            Self::ErrorRateDegraded => Some("errors"),
            Self::ErrorCircuitOpen => Some("high error"),
            Self::CostHintHigh => Some("high cost"),
            Self::UserOverrideHonored => Some("override honored"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRoutingThresholds {
    pub stale_after_ms: u64,
    pub degraded_latency_ms: u64,
    pub avoid_latency_ms: u64,
    pub degraded_error_rate: f64,
    pub avoid_error_rate: f64,
    pub high_input_cost_per_million: f64,
    pub high_output_cost_per_million: f64,
}

impl Default for ProviderRoutingThresholds {
    fn default() -> Self {
        Self {
            stale_after_ms: 300_000,
            degraded_latency_ms: 4_000,
            avoid_latency_ms: 10_000,
            degraded_error_rate: 0.05,
            avoid_error_rate: 0.20,
            high_input_cost_per_million: 15.0,
            high_output_cost_per_million: 60.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRoutingMetrics {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub observed_at_ms: u64,
    pub sample_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_p95_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_rate: Option<f64>,
}

impl ProviderRoutingMetrics {
    #[must_use]
    pub fn new(provider: impl Into<String>, observed_at_ms: u64, sample_count: u64) -> Self {
        Self {
            provider: provider.into(),
            model: None,
            observed_at_ms,
            sample_count,
            latency_p95_ms: None,
            error_rate: None,
        }
    }

    #[must_use]
    pub fn for_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub const fn with_latency_p95_ms(mut self, latency_p95_ms: u64) -> Self {
        self.latency_p95_ms = Some(latency_p95_ms);
        self
    }

    #[must_use]
    pub const fn with_error_rate(mut self, error_rate: f64) -> Self {
        self.error_rate = Some(error_rate);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostHintClass {
    Normal,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingCostHint {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: f64,
    pub cache_write_per_million: f64,
    pub class: CostHintClass,
}

impl RoutingCostHint {
    fn from_cost(cost: &ModelCost, thresholds: ProviderRoutingThresholds) -> Option<Self> {
        if is_zero(cost.input)
            && is_zero(cost.output)
            && is_zero(cost.cache_read)
            && is_zero(cost.cache_write)
        {
            return None;
        }

        let class = if cost.input >= thresholds.high_input_cost_per_million
            || cost.output >= thresholds.high_output_cost_per_million
        {
            CostHintClass::High
        } else {
            CostHintClass::Normal
        };

        Some(Self {
            input_per_million: cost.input,
            output_per_million: cost.output,
            cache_read_per_million: cost.cache_read,
            cache_write_per_million: cost.cache_write,
            class,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingMetricsEvidence {
    pub observed_at_ms: u64,
    pub age_ms: u64,
    pub sample_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_p95_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_rate: Option<f64>,
}

impl RoutingMetricsEvidence {
    const fn from_metrics(metrics: &ProviderRoutingMetrics, now_ms: u64) -> Self {
        Self {
            observed_at_ms: metrics.observed_at_ms,
            age_ms: now_ms.saturating_sub(metrics.observed_at_ms),
            sample_count: metrics.sample_count,
            latency_p95_ms: metrics.latency_p95_ms,
            error_rate: metrics.error_rate,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRoutingEvidence {
    pub provider: String,
    pub model: String,
    pub decision: RoutingDecision,
    pub reasons: Vec<RoutingReason>,
    pub configured_only_scope: bool,
    pub user_override: bool,
    pub selection_honored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<RoutingMetricsEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_hint: Option<RoutingCostHint>,
}

impl ModelRoutingEvidence {
    #[must_use]
    pub fn row_badge(&self) -> Option<String> {
        if self.decision == RoutingDecision::Recommended {
            return None;
        }

        let labels = self
            .reasons
            .iter()
            .filter_map(|reason| reason.warning_label())
            .collect::<Vec<_>>();
        let summary = if labels.is_empty() {
            self.decision.short_label().to_string()
        } else {
            labels.join(", ")
        };
        Some(format!("[{}: {summary}]", self.decision.short_label()))
    }

    #[must_use]
    pub fn routing_key(&self) -> (String, String) {
        routing_key(&self.provider, &self.model)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingEvidenceSnapshot {
    pub schema: String,
    pub generated_at_ms: u64,
    pub entries: Vec<ModelRoutingEvidence>,
}

impl RoutingEvidenceSnapshot {
    #[must_use]
    pub fn new(generated_at_ms: u64, mut entries: Vec<ModelRoutingEvidence>) -> Self {
        entries.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then_with(|| left.model.cmp(&right.model))
        });
        Self {
            schema: ROUTING_EVIDENCE_SCHEMA.to_string(),
            generated_at_ms,
            entries,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RoutingEvaluation<'a> {
    pub model: &'a ModelEntry,
    pub metrics: Option<&'a ProviderRoutingMetrics>,
    pub now_ms: u64,
    pub configured_only_scope: bool,
    pub user_override: bool,
    pub thresholds: ProviderRoutingThresholds,
}

impl<'a> RoutingEvaluation<'a> {
    #[must_use]
    pub fn new(model: &'a ModelEntry, now_ms: u64) -> Self {
        Self {
            model,
            metrics: None,
            now_ms,
            configured_only_scope: false,
            user_override: false,
            thresholds: ProviderRoutingThresholds::default(),
        }
    }
}

#[must_use]
pub fn evaluate_model_routing(input: RoutingEvaluation<'_>) -> ModelRoutingEvidence {
    let provider = canonicalize_provider(input.model.model.provider.as_str());
    let model = input.model.model.id.clone();
    let cost_hint = RoutingCostHint::from_cost(&input.model.model.cost, input.thresholds);
    let mut evidence = ModelRoutingEvidence {
        provider: provider.clone(),
        model: model.clone(),
        decision: RoutingDecision::Recommended,
        reasons: Vec::new(),
        configured_only_scope: input.configured_only_scope,
        user_override: input.user_override,
        selection_honored: input.user_override,
        metrics: None,
        cost_hint,
    };

    match input
        .metrics
        .filter(|metrics| metrics_match_model(metrics, &provider, &model))
    {
        Some(metrics) if metrics.sample_count == 0 || metrics_have_invalid_values(metrics) => {
            evidence.decision = RoutingDecision::MissingMetrics;
            evidence.reasons.push(RoutingReason::InvalidMetrics);
        }
        Some(metrics) => {
            let metric_evidence = RoutingMetricsEvidence::from_metrics(metrics, input.now_ms);
            if metric_evidence.age_ms > input.thresholds.stale_after_ms {
                evidence.decision = RoutingDecision::StaleMetrics;
                evidence.reasons.push(RoutingReason::StaleMetrics);
            } else {
                evaluate_fresh_metrics(&mut evidence, metrics, input.thresholds);
                evaluate_cost_hint(&mut evidence);
                if evidence.reasons.is_empty() {
                    evidence.reasons.push(RoutingReason::Healthy);
                }
            }
            evidence.metrics = Some(metric_evidence);
        }
        None => {
            evidence.decision = RoutingDecision::MissingMetrics;
            evidence.reasons.push(RoutingReason::MissingMetrics);
        }
    }

    if input.configured_only_scope {
        evidence.reasons.push(RoutingReason::ConfiguredOnlyScope);
    }
    if input.user_override {
        evidence.reasons.push(RoutingReason::UserOverrideHonored);
    }

    evidence
}

#[must_use]
pub fn routing_key(provider: &str, model: &str) -> (String, String) {
    (canonicalize_provider(provider), model.to_ascii_lowercase())
}

fn evaluate_fresh_metrics(
    evidence: &mut ModelRoutingEvidence,
    metrics: &ProviderRoutingMetrics,
    thresholds: ProviderRoutingThresholds,
) {
    if let Some(latency_p95_ms) = metrics.latency_p95_ms {
        if latency_p95_ms >= thresholds.avoid_latency_ms {
            escalate(evidence, RoutingDecision::TemporarilyAvoided);
            evidence.reasons.push(RoutingReason::LatencyCircuitOpen);
        } else if latency_p95_ms >= thresholds.degraded_latency_ms {
            escalate(evidence, RoutingDecision::Degraded);
            evidence.reasons.push(RoutingReason::LatencyDegraded);
        }
    }

    if let Some(error_rate) = metrics.error_rate {
        if error_rate >= thresholds.avoid_error_rate {
            escalate(evidence, RoutingDecision::TemporarilyAvoided);
            evidence.reasons.push(RoutingReason::ErrorCircuitOpen);
        } else if error_rate >= thresholds.degraded_error_rate {
            escalate(evidence, RoutingDecision::Degraded);
            evidence.reasons.push(RoutingReason::ErrorRateDegraded);
        }
    }
}

fn evaluate_cost_hint(evidence: &mut ModelRoutingEvidence) {
    if evidence
        .cost_hint
        .as_ref()
        .is_some_and(|hint| hint.class == CostHintClass::High)
    {
        escalate(evidence, RoutingDecision::Degraded);
        evidence.reasons.push(RoutingReason::CostHintHigh);
    }
}

const fn escalate(evidence: &mut ModelRoutingEvidence, decision: RoutingDecision) {
    if decision.severity() > evidence.decision.severity() {
        evidence.decision = decision;
    }
}

fn metrics_match_model(metrics: &ProviderRoutingMetrics, provider: &str, model: &str) -> bool {
    if canonicalize_provider(metrics.provider.as_str()) != provider {
        return false;
    }
    metrics
        .model
        .as_deref()
        .is_none_or(|metric_model| metric_model.eq_ignore_ascii_case(model))
}

fn metrics_have_invalid_values(metrics: &ProviderRoutingMetrics) -> bool {
    let invalid_error_rate = metrics
        .error_rate
        .is_some_and(|rate| !rate.is_finite() || !(0.0..=1.0).contains(&rate));
    let missing_health_signal = metrics.latency_p95_ms.is_none() && metrics.error_rate.is_none();
    invalid_error_rate || missing_health_signal
}

fn canonicalize_provider(provider: &str) -> String {
    canonical_provider_id(provider)
        .unwrap_or(provider)
        .to_ascii_lowercase()
}

fn is_zero(value: f64) -> bool {
    value.abs() < f64::EPSILON
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{InputType, Model};
    use serde_json::json;
    use std::collections::HashMap;

    fn model_entry(provider: &str, id: &str, cost: ModelCost) -> ModelEntry {
        ModelEntry {
            model: Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "openai-completions".to_string(),
                provider: provider.to_string(),
                base_url: "https://example.invalid".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost,
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: Some("redacted-fixture-key".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    }

    fn zero_cost() -> ModelCost {
        ModelCost {
            audio: None,
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }

    fn thresholds() -> ProviderRoutingThresholds {
        ProviderRoutingThresholds {
            stale_after_ms: 1_000,
            degraded_latency_ms: 400,
            avoid_latency_ms: 900,
            degraded_error_rate: 0.05,
            avoid_error_rate: 0.20,
            high_input_cost_per_million: 5.0,
            high_output_cost_per_million: 15.0,
        }
    }

    fn metrics(latency_p95_ms: u64, error_rate: f64) -> ProviderRoutingMetrics {
        ProviderRoutingMetrics::new("openai", 9_500, 42)
            .for_model("gpt-test")
            .with_latency_p95_ms(latency_p95_ms)
            .with_error_rate(error_rate)
    }

    fn evaluate_with(metrics: Option<&ProviderRoutingMetrics>) -> ModelRoutingEvidence {
        let model = model_entry("openai", "gpt-test", zero_cost());
        evaluate_model_routing(RoutingEvaluation {
            model: &model,
            metrics,
            now_ms: 10_000,
            configured_only_scope: false,
            user_override: false,
            thresholds: thresholds(),
        })
    }

    #[test]
    fn healthy_metrics_are_recommended() {
        let metrics = metrics(120, 0.01);
        let evidence = evaluate_with(Some(&metrics));

        assert_eq!(evidence.decision, RoutingDecision::Recommended);
        assert_eq!(evidence.reasons, vec![RoutingReason::Healthy]);
        assert!(evidence.row_badge().is_none());
        assert!(evidence.cost_hint.is_none());
    }

    #[test]
    fn degraded_latency_or_errors_mark_provider_degraded() {
        let metrics = metrics(450, 0.07);
        let evidence = evaluate_with(Some(&metrics));

        assert_eq!(evidence.decision, RoutingDecision::Degraded);
        assert_eq!(
            evidence.reasons,
            vec![
                RoutingReason::LatencyDegraded,
                RoutingReason::ErrorRateDegraded
            ]
        );
        assert_eq!(
            evidence.row_badge().as_deref(),
            Some("[degraded: latency, errors]")
        );
    }

    #[test]
    fn stale_metrics_fail_closed() {
        let stale = ProviderRoutingMetrics::new("openai", 8_999, 42)
            .for_model("gpt-test")
            .with_latency_p95_ms(120)
            .with_error_rate(0.01);
        let evidence = evaluate_with(Some(&stale));

        assert_eq!(evidence.decision, RoutingDecision::StaleMetrics);
        assert_eq!(evidence.reasons, vec![RoutingReason::StaleMetrics]);
        assert_eq!(
            evidence.row_badge().as_deref(),
            Some("[stale: stale metrics]")
        );
    }

    #[test]
    fn missing_metrics_fail_closed() {
        let evidence = evaluate_with(None);

        assert_eq!(evidence.decision, RoutingDecision::MissingMetrics);
        assert_eq!(evidence.reasons, vec![RoutingReason::MissingMetrics]);
        assert!(evidence.metrics.is_none());
        assert_eq!(
            evidence.row_badge().as_deref(),
            Some("[missing: missing metrics]")
        );
    }

    #[test]
    fn high_latency_temporarily_opens_circuit() {
        let metrics = metrics(1_200, 0.01);
        let evidence = evaluate_with(Some(&metrics));

        assert_eq!(evidence.decision, RoutingDecision::TemporarilyAvoided);
        assert_eq!(evidence.reasons, vec![RoutingReason::LatencyCircuitOpen]);
        assert_eq!(
            evidence.row_badge().as_deref(),
            Some("[avoid: high latency]")
        );
    }

    #[test]
    fn high_error_rate_temporarily_opens_circuit() {
        let metrics = metrics(120, 0.30);
        let evidence = evaluate_with(Some(&metrics));

        assert_eq!(evidence.decision, RoutingDecision::TemporarilyAvoided);
        assert_eq!(evidence.reasons, vec![RoutingReason::ErrorCircuitOpen]);
        assert_eq!(evidence.row_badge().as_deref(), Some("[avoid: high error]"));
    }

    #[test]
    fn user_override_is_honored_but_warning_stays_visible() {
        let model = model_entry("openai", "gpt-test", zero_cost());
        let metrics = metrics(120, 0.30);
        let evidence = evaluate_model_routing(RoutingEvaluation {
            model: &model,
            metrics: Some(&metrics),
            now_ms: 10_000,
            configured_only_scope: false,
            user_override: true,
            thresholds: thresholds(),
        });

        assert_eq!(evidence.decision, RoutingDecision::TemporarilyAvoided);
        assert!(evidence.selection_honored);
        assert_eq!(
            evidence.reasons,
            vec![
                RoutingReason::ErrorCircuitOpen,
                RoutingReason::UserOverrideHonored
            ]
        );
        assert_eq!(
            evidence.row_badge().as_deref(),
            Some("[avoid: high error, override honored]")
        );
    }

    #[test]
    fn high_cost_hint_marks_model_degraded_without_secrets() {
        let model = model_entry(
            "openai",
            "gpt-test",
            ModelCost {
                audio: None,
                input: 6.0,
                output: 20.0,
                cache_read: 0.5,
                cache_write: 1.0,
            },
        );
        let metrics = metrics(120, 0.01);
        let evidence = evaluate_model_routing(RoutingEvaluation {
            model: &model,
            metrics: Some(&metrics),
            now_ms: 10_000,
            configured_only_scope: false,
            user_override: false,
            thresholds: thresholds(),
        });

        assert_eq!(evidence.decision, RoutingDecision::Degraded);
        assert_eq!(evidence.reasons, vec![RoutingReason::CostHintHigh]);
        assert_eq!(
            evidence.cost_hint.as_ref().map(|hint| hint.class),
            Some(CostHintClass::High)
        );
    }

    #[test]
    fn golden_json_projection_is_redaction_safe_and_stable() {
        let model = model_entry(
            "openai",
            "gpt-test",
            ModelCost {
                audio: None,
                input: 6.0,
                output: 20.0,
                cache_read: 0.5,
                cache_write: 1.0,
            },
        );
        let metrics = metrics(450, 0.07);
        let evidence = evaluate_model_routing(RoutingEvaluation {
            model: &model,
            metrics: Some(&metrics),
            now_ms: 10_000,
            configured_only_scope: true,
            user_override: true,
            thresholds: thresholds(),
        });
        let snapshot = RoutingEvidenceSnapshot::new(10_000, vec![evidence]);

        assert_eq!(
            serde_json::to_value(snapshot).expect("serialize routing evidence"),
            json!({
                "schema": ROUTING_EVIDENCE_SCHEMA,
                "generatedAtMs": 10_000,
                "entries": [{
                    "provider": "openai",
                    "model": "gpt-test",
                    "decision": "degraded",
                    "reasons": [
                        "latency_degraded",
                        "error_rate_degraded",
                        "cost_hint_high",
                        "configured_only_scope",
                        "user_override_honored"
                    ],
                    "configuredOnlyScope": true,
                    "userOverride": true,
                    "selectionHonored": true,
                    "metrics": {
                        "observedAtMs": 9_500,
                        "ageMs": 500,
                        "sampleCount": 42,
                        "latencyP95Ms": 450,
                        "errorRate": 0.07
                    },
                    "costHint": {
                        "inputPerMillion": 6.0,
                        "outputPerMillion": 20.0,
                        "cacheReadPerMillion": 0.5,
                        "cacheWritePerMillion": 1.0,
                        "class": "high"
                    }
                }]
            })
        );
    }
}
