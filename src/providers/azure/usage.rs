//! Azure Chat usage normalization and billing evidence.
//!
//! Cache writes replace ordinary input billing; they are not an extra charge
//! on the same token. The catalog's single write rate cannot represent mixed
//! cache lifetimes or audio, so those require a provider-reported USD bill.
//! References:
//! - <https://learn.microsoft.com/azure/foundry/openai/how-to/prompt-caching>
//! - <https://developers.openai.com/api/docs/guides/prompt-caching>

use crate::model::{BillingTokenCounts, BillingUsage, Cost, Usage};
use crate::provider::ModelCost;
use serde::Deserialize;
use serde_json::{Value, value::RawValue};

#[derive(Default)]
struct UsageEnvelope {
    usage: Option<Box<RawValue>>,
    duplicate_usage: bool,
}

struct UsageEnvelopeVisitor;

impl<'de> serde::de::Visitor<'de> for UsageEnvelopeVisitor {
    type Value = UsageEnvelope;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an Azure stream object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut envelope = UsageEnvelope::default();
        let mut seen_usage = false;
        while let Some(key) = map.next_key::<String>()? {
            if key == "usage" {
                if seen_usage {
                    envelope.duplicate_usage = true;
                    map.next_value::<serde::de::IgnoredAny>()?;
                } else {
                    seen_usage = true;
                    envelope.usage = map.next_value()?;
                }
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(envelope)
    }
}

impl<'de> Deserialize<'de> for UsageEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(UsageEnvelopeVisitor)
    }
}

#[derive(Deserialize)]
struct ReportedCost {
    #[serde(default, deserialize_with = "deserialize_reported_cost")]
    cost: Option<Value>,
}

/// A supplied null bill is invalid, rather than an absent optional bill.
fn deserialize_reported_cost<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

fn deserialize_optional_object<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let raw = Option::<Box<RawValue>>::deserialize(deserializer)?;
    raw.map(|raw| {
        if !raw.get().trim_start().starts_with('{') {
            return Err(serde::de::Error::custom(
                "expected an object for Azure usage details",
            ));
        }
        serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)
    })
    .transpose()
}

#[derive(Deserialize)]
struct AzureUsage {
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: Option<u64>,
    total_tokens: u64,
    #[serde(default, deserialize_with = "deserialize_optional_object")]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default, deserialize_with = "deserialize_optional_object")]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    // Older Azure deployments emit null for these optional detail fields.
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    audio_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: u64,
    #[serde(default, deserialize_with = "deserialize_optional_object")]
    cache_write_tokens_details: Option<CacheWriteDetails>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheWriteDetails {
    ttl_30m_tokens: u64,
    ttl_12h_tokens: u64,
}

#[derive(Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    audio_tokens: Option<u64>,
}

fn unavailable(reason: &str) -> BillingUsage {
    BillingUsage::Unavailable {
        reason: reason.to_string(),
    }
}

fn billing_fallback(usage: &mut Usage, reported_total: Option<f64>, reason: &str) {
    usage.billing = Some(reported_total.map_or_else(
        || unavailable(reason),
        |total| BillingUsage::ReportedTotal { total },
    ));
    if let Some(total) = reported_total {
        usage.cost.total = total;
    }
}

/// Acknowledge usage before parsing unrelated content fields. Duplicate outer
/// usage keys are ambiguous even when one is null; never choose either bill.
/// Wholly malformed JSON still follows the ordinary stream parser error path.
pub(super) fn from_chunk(data: &str, rates: Option<&ModelCost>) -> Option<Usage> {
    let envelope = serde_json::from_str::<UsageEnvelope>(data).ok()?;
    if envelope.duplicate_usage {
        return Some(Usage {
            billing: Some(unavailable("Azure reported duplicate usage metadata")),
            ..Usage::default()
        });
    }
    envelope.usage.map(|raw| normalize(&raw, rates))
}

/// Keep usage raw until this boundary so malformed or duplicate billing fields
/// quarantine the spending account rather than becoming an ordinary parse
/// error that permits another request. Typed decoding retains duplicate checks.
pub(super) fn normalize(raw: &RawValue, rates: Option<&ModelCost>) -> Usage {
    let mut normalized = Usage::default();
    // Derived structs also accept sequence input. A raw [0] must never be
    // interpreted as a free bill merely because cost is the first field.
    if !raw.get().trim_start().starts_with('{') {
        normalized.billing = Some(unavailable("Azure reported non-object token usage"));
        return normalized;
    }
    let reported_total = match serde_json::from_str::<ReportedCost>(raw.get()) {
        Ok(ReportedCost { cost: None }) => None,
        Ok(ReportedCost { cost: Some(cost) }) => {
            let Some(total) = cost.as_f64().filter(|total| total.is_finite() && *total >= 0.0)
            else {
                normalized.billing = Some(unavailable("Azure reported an invalid billed total"));
                return normalized;
            };
            Some(total)
        }
        Err(_) => {
            normalized.billing = Some(unavailable("Azure reported invalid billing metadata"));
            return normalized;
        }
    };
    let Ok(usage) = serde_json::from_str::<AzureUsage>(raw.get()) else {
        billing_fallback(
            &mut normalized,
            reported_total,
            "Azure reported invalid token usage",
        );
        return normalized;
    };
    let details = usage.prompt_tokens_details.as_ref();
    let cached = details.and_then(|details| details.cached_tokens).unwrap_or(0);
    let written = details.map_or(0, |details| details.cache_write_tokens);
    normalized = Usage {
        input: usage
            .prompt_tokens
            .saturating_sub(cached)
            .saturating_sub(written),
        output: usage.completion_tokens.unwrap_or(0),
        cache_read: cached,
        cache_write: written,
        total_tokens: usage.total_tokens,
        ..Usage::default()
    };

    if let Err(reason) = usage.validate(cached, written, rates) {
        billing_fallback(&mut normalized, reported_total, reason);
        return normalized;
    }

    normalized.cost = Cost {
        total: reported_total.unwrap_or(0.0),
        ..Cost::default()
    };
    // Preserve legacy unknown-price snapshots as estimates: positive token
    // counts alone cannot establish a zero-dollar bill or refund prior spend.
    let complete_priced_usage = usage.completion_tokens.is_some()
        && (usage.total_tokens == 0 || rates.is_some_and(ModelCost::is_priced));
    if reported_total.is_some() || complete_priced_usage {
        normalized.billing = Some(BillingUsage::Tokens {
            text: BillingTokenCounts {
                input: normalized.input,
                output: normalized.output,
                cache_read: cached,
                cache_write: written,
            },
            audio: BillingTokenCounts::default(),
            reported_total,
        });
    }
    normalized
}

impl AzureUsage {
    fn validate(
        &self,
        cached: u64,
        written: u64,
        rates: Option<&ModelCost>,
    ) -> Result<(), &'static str> {
        if cached
            .checked_add(written)
            .and_then(|cache| self.prompt_tokens.checked_sub(cache))
            .is_none()
        {
            return Err("Azure reported inconsistent cache token counts");
        }
        if let Some(output) = self.completion_tokens {
            if self.prompt_tokens.checked_add(output) != Some(self.total_tokens) {
                return Err("Azure reported inconsistent total token counts");
            }
        } else if written > 0 {
            return Err("Azure cache write usage is missing completion_tokens");
        }

        let details = self.prompt_tokens_details.as_ref();
        if let Some(lifetimes) =
            details.and_then(|details| details.cache_write_tokens_details.as_ref())
        {
            if lifetimes.ttl_30m_tokens.checked_add(lifetimes.ttl_12h_tokens) != Some(written) {
                return Err("Azure reported inconsistent cache write lifetimes");
            }
            if lifetimes.ttl_12h_tokens > 0 {
                return Err("Azure 12-hour cache writes require a billed total");
            }
        }
        if details.and_then(|details| details.audio_tokens).unwrap_or(0) > 0
            || self
                .completion_tokens_details
                .as_ref()
                .and_then(|details| details.audio_tokens)
                .unwrap_or(0)
                > 0
        {
            return Err("Azure audio usage requires a billed total");
        }
        if written > 0
            && !rates.is_some_and(|rates| rates.cache_write.is_finite() && rates.cache_write > 0.0)
        {
            // Legacy ModelCost cannot distinguish an omitted write rate from
            // an explicitly free one. A positive configured rate or a wire
            // bill (including zero) is required; never invent a multiplier.
            return Err(
                "Azure cache writes require a positive cost.cacheWrite rate or a billed total",
            );
        }
        Ok(())
    }
}
