//! Explicit, invocation-owned auxiliary model recovery policy.
//!
//! The host catalog and trusted settings choose every destination before a
//! retained job starts. Guest input cannot supply a fallback or a credential.

use super::{Config, Judge, MAX_CALLS, ModelEntry, ModelRegistry, Selection};
use crate::failover::{FallbackOn, chain_for};
use crate::model::ThinkingLevel;
use std::path::Path;

#[derive(Clone)]
pub(super) struct BoundModels {
    pub(super) primary: Judge,
    pub(super) fallbacks: Vec<Judge>,
    pub(super) policy: FallbackOn,
}

impl From<Judge> for BoundModels {
    fn from(primary: Judge) -> Self {
        Self {
            primary,
            fallbacks: Vec::new(),
            policy: FallbackOn::Any,
        }
    }
}

impl BoundModels {
    pub(super) fn with_cost_budget(mut self, budget: Option<crate::cost::CostBudget>) -> Self {
        self.primary = self.primary.with_cost_budget(budget.clone());
        self.fallbacks = self
            .fallbacks
            .into_iter()
            .map(|judge| judge.with_cost_budget(budget.clone()))
            .collect();
        self
    }

    pub(super) fn for_request(mut self, allow_fallback: bool, images: bool) -> Self {
        if allow_fallback {
            // The primary is still checked explicitly before any image read.
            // A text-only secondary cannot receive another model's images.
            self.fallbacks
                .retain(|judge| judge.check_image_support(images).is_ok());
        } else {
            self.fallbacks.clear();
        }
        self
    }

    pub(super) fn len(&self) -> usize {
        1 + self.fallbacks.len()
    }
}

/// Depth-first expansion matches configured chain order and terminates cycles
/// by model AND effective effort. A literal registered ID wins over `:effort`
/// parsing, using the same resolver as conversation fallback. Only registered,
/// enabled models are eligible; defaults never invent a secondary route.
pub(super) fn entries(
    cwd: &Path,
    selection: &Selection,
    primary: &ModelEntry,
    thinking: Option<ThinkingLevel>,
    config: &Config,
    registry: &ModelRegistry,
) -> Vec<(ModelEntry, Option<ThinkingLevel>)> {
    if !config.retry_enabled()
        || !config.model_fallback_enabled()
        || config.retry_max_retries() == 0
    {
        return Vec::new();
    }
    let Some(chains) = config
        .retry
        .as_ref()
        .and_then(|retry| retry.fallback_chains.as_ref())
    else {
        return Vec::new();
    };
    let role = match selection {
        Selection::Role { role } => role.as_str(),
        Selection::Named { .. } => "",
    };
    let Some(chain) = chain_for(
        chains,
        role,
        &primary.model.provider,
        &primary.model.id,
        registry.models(),
    ) else {
        return Vec::new();
    };
    let scope = config
        .model_scope_overrides
        .as_deref()
        .and_then(|overrides| crate::failover::best_scope_override(overrides, cwd));
    let mut pending: Vec<_> = chain.entries.into_iter().rev().collect();
    let mut seen = vec![(
        primary.model.provider.clone(),
        primary.model.id.clone(),
        super::thinking_for_entry(primary, thinking),
    )];
    let mut candidates = Vec::new();
    while let Some(spec) = pending.pop() {
        let Some((entry, thinking)) =
            crate::failover::resolve_chain_selection(&spec, registry.models())
        else {
            continue;
        };
        // The general failover resolver can synthesize native models. Eval's
        // explicit binding contract requires a registered catalog entry.
        let Some(entry) = registry.find(&entry.model.provider, &entry.model.id) else {
            continue;
        };
        if crate::failover::provider_is_disabled(
            config.disabled_providers.as_deref().unwrap_or_default(),
            scope,
            &entry.model.provider,
        ) {
            continue;
        }
        let thinking = super::thinking_for_entry(&entry, thinking);
        let identity = (
            // ubs:ignore -- The bounded route keeps owned model identities to detect cycles after each candidate moves.
            entry.model.provider.clone(),
            entry.model.id.clone(),
            thinking,
        );
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity);
        let next = chain_for(
            chains,
            "",
            &entry.model.provider,
            &entry.model.id,
            registry.models(),
        );
        candidates.push((entry, thinking));
        // Bound provider preparation even when configuration contains a long
        // graph of unusable credentials. Each cell still reserves at most 16
        // real requests; keyless candidates consume no request slot.
        if candidates.len() >= MAX_CALLS * 4 {
            break;
        }
        if let Some(next) = next {
            pending.extend(next.entries.into_iter().rev());
        }
    }
    candidates
}
