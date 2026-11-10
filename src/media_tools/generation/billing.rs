//! Billing evidence for native image generation and editing.
//!
//! These immutable image tariffs are separate from ordinary model text rates.
//! Only documented model IDs are recognized; future aliases and custom models
//! remain unpriced unless the response contains an explicit billed total.
//!
//! Standard USD tariffs and response contracts verified on 2026-11-10:
//! - <https://developers.openai.com/api/docs/models/gpt-image-1.5>
//! - <https://developers.openai.com/api/reference/resources/images/methods/generate>
//! - <https://ai.google.dev/gemini-api/docs/pricing/>
//! - <https://ai.google.dev/api/generate-content>
//! - <https://docs.x.ai/developers/cost-tracking>

use crate::error::{Error, Result};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

const NAME: &str = "generate_image";

/// Prefer the actual bill to catalog estimates, including an explicit zero.
/// Missing evidence or unsupported tariffs are unpriced; malformed evidence is
/// an error so the caller can quarantine the owning spending account.
pub(in crate::media_tools) fn response_cost(
    provider: &str,
    model: &str,
    response: &Value,
) -> Result<Option<f64>> {
    let usage = if matches!(provider, "openai" | "xai") {
        optional_object(response, "usage")?
    } else {
        None
    };
    if let Some(usage) = usage {
        let ticks = if provider == "xai" {
            usage.get("cost_in_usd_ticks")
        } else {
            None
        };
        let reported =
            crate::provider::reported_cost_total(usage.get("cost"), ticks).map_err(invalid)?;
        if reported.is_some() {
            return Ok(reported);
        }
    }

    match (provider, model) {
        ("openai", "gpt-image-1.5" | "gpt-image-1.5-2025-12-16") => {
            usage.map_or(Ok(None), openai_cost)
        }
        ("gemini", "gemini-3.1-flash-image") => {
            optional_object(response, "usageMetadata")?.map_or(Ok(None), gemini_cost)
        }
        _ => Ok(None),
    }
}

fn invalid(reason: impl AsRef<str>) -> Error {
    Error::tool(
        NAME,
        format!("invalid image billing evidence: {}", reason.as_ref()),
    )
}

fn optional_object<'a>(value: &'a Value, name: &str) -> Result<Option<&'a Map<String, Value>>> {
    value
        .get(name)
        .map(|value| {
            value
                .as_object()
                .ok_or_else(|| invalid(format!("{name} must be an object")))
        })
        .transpose()
}

fn optional_count(fields: &Map<String, Value>, name: &str) -> Result<Option<u64>> {
    fields
        .get(name)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| invalid(format!("{name} must be a nonnegative integer")))
        })
        .transpose()
}

fn count(fields: &Map<String, Value>, name: &str) -> Result<u64> {
    optional_count(fields, name)?.ok_or_else(|| invalid(format!("missing {name}")))
}

fn sum(left: u64, right: u64) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| invalid("token counts overflow"))
}

fn check_total(reported: u64, expected: u64, name: &str) -> Result<()> {
    if reported != expected {
        return Err(invalid(format!(
            "{name} does not match its token partition"
        )));
    }
    Ok(())
}

#[derive(Default)]
struct Modalities {
    text: u64,
    image: u64,
}

impl Modalities {
    fn total(&self) -> Result<u64> {
        sum(self.text, self.image)
    }
}

fn openai_partition(value: &Value, aggregate: u64) -> Result<Modalities> {
    let fields = value
        .as_object()
        .ok_or_else(|| invalid("OpenAI token details must be an object"))?;
    let partition = Modalities {
        text: count(fields, "text_tokens")?,
        image: count(fields, "image_tokens")?,
    };
    for (name, value) in fields {
        if !matches!(name.as_str(), "text_tokens" | "image_tokens") && value.as_u64() != Some(0) {
            return Err(invalid("unsupported OpenAI token detail"));
        }
    }
    check_total(aggregate, partition.total()?, "OpenAI token total")?;
    Ok(partition)
}

fn openai_cost(usage: &Map<String, Value>) -> Result<Option<f64>> {
    let input = count(usage, "input_tokens")?;
    let output = count(usage, "output_tokens")?;
    let total = sum(input, output)?;
    if let Some(reported) = optional_count(usage, "total_tokens")? {
        check_total(reported, total, "total_tokens")?;
    }
    let input = match usage.get("input_tokens_details") {
        Some(details) => openai_partition(details, input)?,
        None if input == 0 => Modalities::default(),
        None => return Err(invalid("missing OpenAI input modality partition")),
    };
    // Historical direct Images responses report only image output tokens. Newer
    // responses can supply an explicit text/image output partition.
    let output = match usage.get("output_tokens_details") {
        Some(details) => openai_partition(details, output)?,
        None => Modalities {
            text: 0,
            image: output,
        },
    };
    if openai_has_cached_input(usage, &input)? {
        // Direct Images does not document cached usage. Do not silently apply
        // full input prices to cache evidence from a different API or gateway.
        return Ok(None);
    }
    Ok(Some(
        token_cost(input.text, 5.0)
            + token_cost(input.image, 8.0)
            + token_cost(output.text, 10.0)
            + token_cost(output.image, 32.0),
    ))
}

fn openai_has_cached_input(usage: &Map<String, Value>, input: &Modalities) -> Result<bool> {
    let reported = optional_count(usage, "cached_tokens")?;
    if let Some(reported) = reported
        && reported > input.total()?
    {
        return Err(invalid("cached_tokens exceeds input_tokens"));
    }
    let Some(details) = usage.get("cached_tokens_details") else {
        return Ok(reported.is_some_and(|count| count > 0));
    };
    let fields = details
        .as_object()
        .ok_or_else(|| invalid("cached_tokens_details must be an object"))?;
    let text = count(fields, "cached_text_tokens")?;
    let image = count(fields, "cached_image_tokens")?;
    let total = sum(text, image)?;
    if text > input.text || image > input.image {
        return Err(invalid("cached modality tokens exceed input tokens"));
    }
    if let Some(reported) = reported {
        check_total(reported, total, "cached_tokens")?;
    }
    for (name, value) in fields {
        if !matches!(name.as_str(), "cached_text_tokens" | "cached_image_tokens")
            && value.as_u64() != Some(0)
        {
            return Err(invalid("unsupported cached OpenAI token detail"));
        }
    }
    Ok(total > 0)
}

fn gemini_partition(
    usage: &Map<String, Value>,
    name: &str,
    aggregate: u64,
) -> Result<Option<Modalities>> {
    let Some(value) = usage.get(name) else {
        return Ok(None);
    };
    let rows = value
        .as_array()
        .ok_or_else(|| invalid(format!("{name} must be an array")))?;
    let mut seen = BTreeSet::new();
    let mut partition = Modalities::default();
    for row in rows {
        let fields = row
            .as_object()
            .ok_or_else(|| invalid("Gemini modality entries must be objects"))?;
        let modality = fields
            .get("modality")
            .and_then(Value::as_str)
            .filter(|modality| !modality.is_empty())
            .ok_or_else(|| invalid("missing Gemini token modality"))?;
        if !seen.insert(modality) {
            return Err(invalid("duplicate Gemini token modality"));
        }
        let tokens = count(fields, "tokenCount")?;
        match modality {
            "TEXT" => partition.text = tokens,
            "IMAGE" => partition.image = tokens,
            _ if tokens == 0 => {}
            _ => return Err(invalid("unsupported Gemini token modality")),
        }
    }
    check_total(aggregate, partition.total()?, name)?;
    Ok(Some(partition))
}

fn gemini_cost(usage: &Map<String, Value>) -> Result<Option<f64>> {
    let prompt = count(usage, "promptTokenCount")?;
    // Proto JSON can omit zero candidate and thought counters. The required
    // aggregate still proves that omitted candidates contain no billed tokens.
    let candidates = optional_count(usage, "candidatesTokenCount")?.unwrap_or(0);
    let thoughts = optional_count(usage, "thoughtsTokenCount")?.unwrap_or(0);
    let total = count(usage, "totalTokenCount")?;
    let expected = sum(sum(prompt, candidates)?, thoughts)?;
    let prompt_details = gemini_partition(usage, "promptTokensDetails", prompt)?;
    let output = gemini_partition(usage, "candidatesTokensDetails", candidates)?;
    if candidates > 0 && output.is_none() {
        return Err(invalid("missing Gemini output modality partition"));
    }
    let output = output.unwrap_or_default();

    let cached = optional_count(usage, "cachedContentTokenCount")?.unwrap_or(0);
    if cached > prompt {
        return Err(invalid("cachedContentTokenCount exceeds promptTokenCount"));
    }
    let cache_details = gemini_partition(usage, "cacheTokensDetails", cached)?;
    if let (Some(prompt), Some(cache)) = (prompt_details, cache_details)
        && (cache.text > prompt.text || cache.image > prompt.image)
    {
        return Err(invalid("cached Gemini modalities exceed prompt modalities"));
    }
    let tools = optional_count(usage, "toolUsePromptTokenCount")?.unwrap_or(0);
    let _ = gemini_partition(usage, "toolUsePromptTokensDetails", tools)?;
    let standard_tier = match usage.get("serviceTier") {
        None => true,
        Some(value) => {
            let tier = value
                .as_str()
                .filter(|tier| !tier.is_empty())
                .ok_or_else(|| invalid("serviceTier must be a nonempty string"))?;
            matches!(tier, "STANDARD" | "standard" | "SERVICE_TIER_UNSPECIFIED")
        }
    };
    if tools > 0 {
        // This tariff does not cover tool charges. Validate the supported
        // components without assuming how an unsupported tool contributes to
        // the provider's aggregate token count.
        sum(expected, tools)?;
        if total < expected {
            return Err(invalid("totalTokenCount omits reported token usage"));
        }
        return Ok(None);
    }
    check_total(total, expected, "totalTokenCount")?;
    if cached > 0 || !standard_tier {
        return Ok(None);
    }
    Ok(Some(
        token_cost(prompt, 0.5)
            + token_cost(sum(output.text, thoughts)?, 3.0)
            + token_cost(output.image, 60.0),
    ))
}

#[allow(clippy::cast_precision_loss)] // Costs use the same f64 USD representation as the ledger.
fn token_cost(tokens: u64, per_million: f64) -> f64 {
    (tokens as f64 / 1_000_000.0) * per_million
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn openai_usage() -> Value {
        json!({"usage": {
            "input_tokens": 300,
            "input_tokens_details": {"text_tokens": 200, "image_tokens": 100},
            "output_tokens": 1000,
            "total_tokens": 1300
        }})
    }

    fn gemini_usage() -> Value {
        json!({"usageMetadata": {
            "promptTokenCount": 1000,
            "promptTokensDetails": [
                {"modality": "TEXT", "tokenCount": 100},
                {"modality": "IMAGE", "tokenCount": 900}
            ],
            "candidatesTokenCount": 1140,
            "candidatesTokensDetails": [
                {"modality": "TEXT", "tokenCount": 20},
                {"modality": "IMAGE", "tokenCount": 1120}
            ],
            "thoughtsTokenCount": 100,
            "totalTokenCount": 2240
        }})
    }

    fn assert_bill(provider: &str, model: &str, response: &Value, expected: f64) {
        // ubs:ignore[rust.ownership.unwrap-expect] -- Valid priced fixtures must produce a bill.
        let actual = response_cost(provider, model, response)
            .expect("valid billing evidence")
            .expect("priced billing evidence");
        // ubs:ignore[rust.panic.assert-macros] -- A mismatched charge must fail the regression.
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected ${expected}, got ${actual}"
        );
    }

    #[test]
    fn openai_prices_text_and_image_input_with_native_image_output() {
        for model in ["gpt-image-1.5", "gpt-image-1.5-2025-12-16"] {
            assert_bill("openai", model, &openai_usage(), 0.0338);
        }
    }

    #[test]
    fn openai_output_partition_uses_distinct_text_and_image_rates() {
        let mut response = openai_usage();
        // ubs:ignore[rust.panic.direct-indexing] -- openai_usage constructs this usage object.
        response["usage"]["output_tokens_details"] =
            json!({"text_tokens": 100, "image_tokens": 900});
        assert_bill("openai", "gpt-image-1.5", &response, 0.0316);
    }

    #[test]
    fn gemini_prices_image_output_and_adds_thinking_once() {
        assert_bill("gemini", "gemini-3.1-flash-image", &gemini_usage(), 0.06806);
    }

    #[test]
    fn gemini_can_price_prompt_only_usage_without_zero_candidate_fields() {
        assert_bill(
            "gemini",
            "gemini-3.1-flash-image",
            &json!({"usageMetadata": {"promptTokenCount": 100, "totalTokenCount": 100}}),
            0.00005,
        );
    }

    #[test]
    fn explicit_totals_override_token_partitions_and_unknown_tariffs() {
        for provider in ["openai", "xai"] {
            assert_bill(
                provider,
                "custom-image-model",
                &json!({"usage": {"cost": 0.25, "input_tokens": "unusable"}}),
                0.25,
            );
        }
        let mut response = openai_usage();
        // ubs:ignore[rust.panic.direct-indexing] -- Set the locally constructed fixture's bill.
        response["usage"]["cost"] = json!(0.125);
        // ubs:ignore[rust.panic.direct-indexing] -- Deliberately invalidate the fixture partition.
        response["usage"]["input_tokens_details"] = Value::Null;
        assert_bill("openai", "gpt-image-1.5", &response, 0.125);
        assert_bill(
            "xai",
            "grok-imagine-image-2.0",
            &json!({"usage": {"cost_in_usd_ticks": 400_000_000}}),
            0.04,
        );
    }

    #[test]
    fn gemini_native_usage_cannot_be_overridden_by_openai_compatible_totals() {
        let mut response = gemini_usage();
        // ubs:ignore[rust.panic.direct-indexing] -- Add a false bill to a local object fixture.
        response["usage"] = json!({"cost": 0});
        assert_bill("gemini", "gemini-3.1-flash-image", &response, 0.06806);
        // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Check an unpriced fixture.
        assert!(
            response_cost("gemini", "custom-image-model", &response)
                .unwrap()
                .is_none()
        );
        // ubs:ignore[rust.panic.direct-indexing] -- Deliberately invalidate known fixture metadata.
        response["usageMetadata"]["candidatesTokensDetails"] = Value::Null;
        // ubs:ignore[rust.panic.assert-macros] -- Malformed billing must fail this regression.
        assert!(response_cost("gemini", "gemini-3.1-flash-image", &response).is_err());
    }

    #[test]
    fn explicit_and_partitioned_zero_are_priceable() {
        assert_bill("openai", "custom", &json!({"usage": {"cost": 0}}), 0.0);
        assert_bill(
            "xai",
            "custom",
            &json!({"usage": {"cost_in_usd_ticks": 0}}),
            0.0,
        );
        assert_bill(
            "openai",
            "gpt-image-1.5",
            &json!({"usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}}),
            0.0,
        );
        assert_bill(
            "gemini",
            "gemini-3.1-flash-image",
            &json!({"usageMetadata": {"promptTokenCount": 0, "totalTokenCount": 0}}),
            0.0,
        );
    }

    #[test]
    fn unknown_models_and_absent_usage_stay_unpriced() {
        for (provider, model, response) in [
            ("openai", "gpt-image-1.5-future", openai_usage()),
            ("xai", "gpt-image-1.5", openai_usage()),
            ("gemini", "gemini-3.1-flash-image-preview", gemini_usage()),
            ("gemini", "gemini-3.1-flash-image-future", gemini_usage()),
            ("openai", "gpt-image-1.5", json!({})),
            ("gemini", "gemini-3.1-flash-image", json!({})),
            ("xai", "grok-imagine-image-2.0", json!({"data": []})),
        ] {
            // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- These fixtures stay unpriced.
            assert!(response_cost(provider, model, &response).unwrap().is_none());
        }
    }

    #[test]
    fn malformed_explicit_bills_never_fall_back_to_estimates() {
        for cost in [Value::Null, json!(-1), json!("0.01"), json!({}), json!([])] {
            let mut response = openai_usage();
            // ubs:ignore[rust.panic.direct-indexing] -- Mutate the fixture with each invalid bill.
            response["usage"]["cost"] = cost;
            // ubs:ignore[rust.panic.assert-macros] -- Invalid explicit bills must be rejected.
            assert!(response_cost("openai", "gpt-image-1.5", &response).is_err());
        }
        for ticks in [Value::Null, json!(-1), json!(1.5), json!("10")] {
            // ubs:ignore[rust.panic.assert-macros] -- Invalid tick representations must be rejected.
            assert!(
                response_cost(
                    "xai",
                    "custom",
                    &json!({"usage": {"cost_in_usd_ticks": ticks}})
                )
                .is_err()
            );
        }
        // ubs:ignore[rust.panic.assert-macros] -- Conflicting explicit bills must be rejected.
        assert!(
            response_cost(
                "xai",
                "custom",
                &json!({"usage": {"cost": 0, "cost_in_usd_ticks": 400_000_000}})
            )
            .is_err()
        );
    }

    #[test]
    fn openai_rejects_incomplete_malformed_or_unsupported_partitions() {
        for details in [
            Value::Null,
            json!({"text_tokens": 200}),
            json!({"text_tokens": 200, "image_tokens": 99}),
            json!({"text_tokens": -1, "image_tokens": 301}),
            json!({"text_tokens": 200.0, "image_tokens": 100}),
            json!({"text_tokens": 200, "image_tokens": 100, "audio_tokens": 1}),
            json!({"text_tokens": u64::MAX, "image_tokens": 1}),
        ] {
            let mut response = openai_usage();
            // ubs:ignore[rust.panic.direct-indexing] -- Install a deliberately malformed partition.
            response["usage"]["input_tokens_details"] = details;
            // ubs:ignore[rust.panic.assert-macros] -- Malformed partitions must fail the regression.
            assert!(response_cost("openai", "gpt-image-1.5", &response).is_err());
        }
        let mut response = openai_usage();
        // ubs:ignore[rust.panic.direct-indexing] -- Give the local fixture contradictory output counts.
        response["usage"]["output_tokens_details"] =
            json!({"text_tokens": 100, "image_tokens": 1000});
        // ubs:ignore[rust.panic.assert-macros] -- Contradictory output counts must be rejected.
        assert!(response_cost("openai", "gpt-image-1.5", &response).is_err());
    }

    #[test]
    fn aggregate_counts_must_agree_without_overflow() {
        let mut openai = openai_usage();
        // ubs:ignore[rust.panic.direct-indexing] -- Deliberately disagree with the fixture's partition.
        openai["usage"]["total_tokens"] = json!(1301);
        // ubs:ignore[rust.panic.assert-macros] -- Inconsistent totals must be rejected.
        assert!(response_cost("openai", "gpt-image-1.5", &openai).is_err());
        // ubs:ignore[rust.panic.direct-indexing] -- Exercise checked arithmetic on a local fixture.
        openai["usage"]["input_tokens"] = json!(u64::MAX);
        // ubs:ignore[rust.panic.assert-macros] -- Overflowing totals must be rejected.
        assert!(response_cost("openai", "gpt-image-1.5", &openai).is_err());

        let mut gemini = gemini_usage();
        // ubs:ignore[rust.panic.direct-indexing] -- Omit thinking tokens from the fixture's total.
        gemini["usageMetadata"]["totalTokenCount"] = json!(2140);
        // ubs:ignore[rust.panic.assert-macros] -- Thinking must be included exactly once.
        assert!(response_cost("gemini", "gemini-3.1-flash-image", &gemini).is_err());
        // ubs:ignore[rust.panic.direct-indexing] -- Exercise checked arithmetic on a local fixture.
        gemini["usageMetadata"]["promptTokenCount"] = json!(u64::MAX);
        // ubs:ignore[rust.panic.assert-macros] -- Overflowing Gemini totals must be rejected.
        assert!(response_cost("gemini", "gemini-3.1-flash-image", &gemini).is_err());
    }

    #[test]
    fn gemini_requires_a_complete_nonoverlapping_output_partition() {
        for details in [
            Value::Null,
            json!([]),
            json!([{"modality": "IMAGE", "tokenCount": 1120}]),
            json!([
                {"modality": "IMAGE", "tokenCount": 570},
                {"modality": "IMAGE", "tokenCount": 570}
            ]),
            json!([{"modality": "AUDIO", "tokenCount": 1140}]),
            json!([{"modality": "IMAGE", "tokenCount": 1140.0}]),
            json!([{"modality": "IMAGE", "tokenCount": -1}]),
            json!([{"modality": "IMAGE"}]),
            json!([
                {"modality": "TEXT", "tokenCount": u64::MAX},
                {"modality": "IMAGE", "tokenCount": 1}
            ]),
        ] {
            let mut response = gemini_usage();
            // ubs:ignore[rust.panic.direct-indexing] -- Install deliberately invalid fixture details.
            response["usageMetadata"]["candidatesTokensDetails"] = details;
            // ubs:ignore[rust.panic.assert-macros] -- Invalid modality partitions must be rejected.
            assert!(response_cost("gemini", "gemini-3.1-flash-image", &response).is_err());
        }
        // ubs:ignore[rust.panic.assert-macros] -- Positive output needs a complete modality partition.
        assert!(
            response_cost(
                "gemini",
                "gemini-3.1-flash-image",
                &json!({"usageMetadata": {
                    "promptTokenCount": 1, "candidatesTokenCount": 1120,
                    "totalTokenCount": 1121
                }})
            )
            .is_err()
        );
    }

    #[test]
    fn unsupported_valid_cache_tools_and_tiers_remain_unpriced() {
        for (field, value) in [
            ("cachedContentTokenCount", json!(20)),
            ("toolUsePromptTokenCount", json!(20)),
            ("serviceTier", json!("FLEX")),
        ] {
            let mut response = gemini_usage();
            // ubs:ignore[rust.panic.direct-indexing] -- Each field comes from this fixed test table.
            response["usageMetadata"][field] = value;
            // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Unsupported units stay unpriced.
            assert!(
                response_cost("gemini", "gemini-3.1-flash-image", &response)
                    .unwrap()
                    .is_none()
            );
        }
        let mut response = openai_usage();
        // ubs:ignore[rust.panic.direct-indexing] -- Add cache evidence to the local usage fixture.
        response["usage"]["cached_tokens"] = json!(10);
        // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Unsupported cache stays unpriced.
        assert!(
            response_cost("openai", "gpt-image-1.5", &response)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_cache_tools_and_tiers_are_errors() {
        for (field, value) in [
            ("cachedContentTokenCount", json!(1001)),
            ("toolUsePromptTokenCount", json!(-1)),
            ("serviceTier", Value::Null),
            (
                "cacheTokensDetails",
                json!([{"modality": "IMAGE", "tokenCount": 1}]),
            ),
        ] {
            let mut response = gemini_usage();
            // ubs:ignore[rust.panic.direct-indexing] -- Install each fixed malformed metadata case.
            response["usageMetadata"][field] = value;
            // ubs:ignore[rust.panic.assert-macros] -- Malformed optional evidence must be rejected.
            assert!(response_cost("gemini", "gemini-3.1-flash-image", &response).is_err());
        }
        let mut response = openai_usage();
        // ubs:ignore[rust.panic.direct-indexing] -- Cache exceeds the known local fixture input.
        response["usage"]["cached_tokens_details"] =
            json!({"cached_text_tokens": 201, "cached_image_tokens": 0});
        // ubs:ignore[rust.panic.assert-macros] -- Impossible cached counts must be rejected.
        assert!(response_cost("openai", "gpt-image-1.5", &response).is_err());
    }
}
