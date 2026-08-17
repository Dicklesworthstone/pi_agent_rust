//! Cross-model failover machinery (bd-cv653.3.2).
//!
//! When a provider throws a classified transient failure (429/quota/overload
//! after the same-provider retry budget), the next entry of the configured
//! fallback chain continues the turn; the primary is restored after a
//! cooldown. Round-robin credentials rotate multiple keys per provider with
//! session affinity and per-credential backoff. Path-scoped model sets pin
//! model lists per repository root.
//!
//! Classification is deliberately conservative: authentication failures
//! (401/403/invalid key) NEVER trigger failover — they are loud user errors,
//! not provider capacity problems.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Failure classes relevant to failover decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverClass {
    /// Rate limit / quota exhaustion (429, insufficient_quota, …).
    Quota,
    /// Provider overloaded / capacity (529, service_unavailable, overloaded).
    Overload,
    /// Other transient failures after the retry budget is spent.
    Transient,
}

/// Classify an error text for failover. `None` = never fail over (auth and
/// other loud errors). Ordering matters: auth patterns are checked FIRST so
/// a "401 ... quota" message never fails over.
pub fn classify_failover(error_text: &str) -> Option<FailoverClass> {
    const AUTH_PATTERNS: &[&str] = &[
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "invalid_api_key",
        "incorrect api key",
        "authentication failed",
        "permission denied",
        "expired token",
        "missing api key",
    ];
    const QUOTA_PATTERNS: &[&str] = &[
        "429",
        "rate limit",
        "rate_limit",
        "too many requests",
        "quota",
        "insufficient_quota",
        "billing",
        "spending limit",
    ];
    const OVERLOAD_PATTERNS: &[&str] = &[
        "529",
        "503",
        "502",
        "500",
        "overloaded",
        "service unavailable",
        "service_unavailable",
        "capacity",
        "temporarily unavailable",
        "server error",
        "internal error",
    ];

    let text = error_text.to_ascii_lowercase();

    // Auth: loud, user-actionable, never a failover trigger.
    if AUTH_PATTERNS.iter().any(|p| text.contains(p)) {
        return None;
    }

    if QUOTA_PATTERNS.iter().any(|p| text.contains(p)) {
        return Some(FailoverClass::Quota);
    }

    if OVERLOAD_PATTERNS.iter().any(|p| text.contains(p)) {
        return Some(FailoverClass::Overload);
    }

    if crate::error::is_retryable_error(&text.to_ascii_lowercase(), None, None) {
        return Some(FailoverClass::Transient);
    }
    None
}

/// One resolved chain: the specs the controller walks on failure.
#[derive(Debug, Clone)]
pub struct FailoverChain {
    /// Ordered `provider/model` specs after the primary.
    pub entries: Vec<String>,
}

/// Resolve the chain for a role name or an exact `provider/model` spec from
/// `retry.fallbackChains`. Role keys take precedence over exact model specs.
pub fn chain_for<S: std::hash::BuildHasher>(
    chains: &HashMap<String, Vec<String>, S>,
    role: &str,
    provider: &str,
    model_id: &str,
) -> Option<FailoverChain> {
    if let Some(entries) = chains.get(role)
        && !entries.is_empty()
    {
        return Some(FailoverChain {
            entries: entries.clone(),
        });
    }
    let full = format!("{provider}/{model_id}");
    for (key, entries) in chains {
        if key.eq_ignore_ascii_case(&full) && !entries.is_empty() {
            return Some(FailoverChain {
                entries: entries.clone(),
            });
        }
    }
    None
}

/// Cooldown FSM for the primary after a failover.
///
/// The primary stays quiesced until the cooldown elapses; a successful
/// failover-chain turn records the failure time; a fresh `should_use_primary`
/// check restores the primary afterwards.
#[derive(Debug, Clone)]
pub struct CooldownTracker {
    failed_at: Option<Instant>,
    cooldown: Duration,
}

impl CooldownTracker {
    #[must_use]
    pub const fn new(cooldown_secs: u64) -> Self {
        Self {
            failed_at: None,
            cooldown: Duration::from_secs(cooldown_secs),
        }
    }

    /// Record that the primary failed at `now`.
    pub const fn record_primary_failure(&mut self, now: Instant) {
        self.failed_at = Some(now);
    }

    /// Whether the primary may be used again at `now`.
    #[must_use]
    pub fn should_use_primary(&self, now: Instant) -> bool {
        self.failed_at
            .is_none_or(|failed| now.duration_since(failed) >= self.cooldown)
    }

    /// Clear the tracker (primary succeeded).
    pub const fn reset(&mut self) {
        self.failed_at = None;
    }

    /// Deterministic test view of the failure timestamp.
    #[cfg(test)]
    pub(crate) fn failed_at(&self) -> Option<Instant> {
        self.failed_at
    }
}

/// Round-robin credential ring (bd-cv653.3.2): multiple keys per provider,
/// stable session affinity by hash, per-credential exponential backoff on 429.
#[derive(Debug, Clone)]
pub struct CredentialRing {
    keys: Vec<String>,
    backoff_until: Vec<Option<Instant>>,
    /// Stable affinity index derived from the session hash at construction.
    affinity: usize,
}

impl CredentialRing {
    /// Build a ring from a non-empty key list with session-affinity index.
    #[must_use]
    pub fn new(keys: Vec<String>, session_hash: u64) -> Option<Self> {
        if keys.is_empty() {
            return None;
        }
        // Hash-first modulo keeps the affinity index within pointer width on
        // every target (no u64→usize truncation).
        let affinity = usize::try_from(session_hash % keys.len() as u64).unwrap_or(0);
        let backoff_until = vec![None; keys.len()];
        Some(Self {
            keys,
            backoff_until,
            affinity,
        })
    }

    /// The current usable key at `now`: the affinity key when healthy, else
    /// the next key without an active backoff; `None` when all are cooling.
    #[must_use]
    pub fn current_key(&self, now: Instant) -> Option<&str> {
        let usable = |idx: usize| self.backoff_until[idx].is_none_or(|until| now >= until);
        if usable(self.affinity) {
            return Some(self.keys[self.affinity].as_str());
        }
        (0..self.keys.len())
            .find(|&idx| usable(idx))
            .map(|idx| self.keys[idx].as_str())
    }

    /// Report a 429 for `key`: exponential backoff `base * 2^strikes` clamped
    /// to `max`. Returns the new backoff expiry for observability.
    pub fn report_rate_limited(
        &mut self,
        key: &str,
        now: Instant,
        base: Duration,
        max: Duration,
    ) -> Option<Instant> {
        let idx = self.keys.iter().position(|k| k == key)?;
        let previous = self.backoff_until[idx];
        let strikes = previous.filter(|until| now < *until).map_or(0, |_| 1);
        let delay = (base * 2_u32.pow(strikes)).min(max);
        let expiry = now + delay;
        self.backoff_until[idx] = Some(expiry);
        Some(expiry)
    }

    /// All keys currently cooling (observability/testing).
    #[must_use]
    pub(crate) fn cooling_count(&self, now: Instant) -> usize {
        self.backoff_until
            .iter()
            .filter(|until| until.is_some_and(|u| now < u))
            .count()
    }

    /// Masked key fingerprints for diagnostics (never logs raw secrets).
    #[must_use]
    pub(crate) fn key_fingerprints(&self) -> Vec<String> {
        self.keys
            .iter()
            .map(|key| {
                let len = key.len();
                let tail: String = key
                    .chars()
                    .rev()
                    .take(2)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                format!("len{len}/..{tail}")
            })
            .collect()
    }
}

/// Stable per-session hash for credential affinity (FNV-1a over the id).
#[must_use]
pub fn session_affinity_hash(session_id: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in session_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Path-scope resolution (bd-cv653.3.2): given cwd and the configured
/// overrides, return the winning override (longest matching prefix), if any.
pub fn best_scope_override<'a>(
    overrides: &'a [crate::config::ModelScopeOverride],
    cwd: &std::path::Path,
) -> Option<&'a crate::config::ModelScopeOverride> {
    overrides
        .iter()
        .filter(|ov| {
            let scope = expand_tilde(&ov.path);
            cwd.starts_with(&scope)
        })
        .max_by_key(|ov| ov.path.len())
}

fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return std::path::PathBuf::from(home).join(rest);
    }
    std::path::PathBuf::from(path)
}

/// Whether a provider id is disabled by the effective configuration for cwd.
pub fn provider_is_disabled(
    disabled: &[String],
    scope: Option<&crate::config::ModelScopeOverride>,
    provider: &str,
) -> bool {
    let in_list = |list: &[String]| {
        list.iter()
            .any(|entry| crate::provider_metadata::provider_ids_match(entry.trim(), provider))
    };
    if let Some(scope_list) = scope.and_then(|ov| ov.disabled_providers.as_deref())
        && in_list(scope_list)
    {
        return true;
    }
    in_list(disabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn classify_quota_and_overload_failover() {
        assert_eq!(
            classify_failover("429 Too Many Requests: rate limit exceeded"),
            Some(FailoverClass::Quota)
        );
        assert_eq!(
            classify_failover("insufficient_quota: you exceeded your current quota"),
            Some(FailoverClass::Quota)
        );
        assert_eq!(
            classify_failover("529 the model is overloaded"),
            Some(FailoverClass::Overload)
        );
        assert_eq!(
            classify_failover("503 service unavailable"),
            Some(FailoverClass::Overload)
        );
    }

    #[test]
    fn classify_auth_never_fails_over() {
        assert_eq!(classify_failover("401 unauthorized: invalid api key"), None);
        assert_eq!(classify_failover("403 forbidden"), None);
        // Auth wins even when quota words appear in the same message.
        assert_eq!(classify_failover("401 unauthorized: quota exceeded"), None);
    }

    #[test]
    fn classify_other_errors_do_not_failover() {
        assert_eq!(classify_failover("the model produced invalid JSON"), None);
        assert_eq!(classify_failover("context window exceeded"), None);
    }

    #[test]
    fn chain_lookup_prefers_role_then_exact_model() {
        let mut chains = HashMap::new();
        chains.insert("default".to_string(), vec!["openai/gpt-5-mini".to_string()]);
        chains.insert(
            "anthropic/claude-opus-4-7".to_string(),
            vec!["google/gemini-3-pro".to_string()],
        );
        assert_eq!(
            chain_for(&chains, "default", "anthropic", "claude-opus-4-7")
                .unwrap()
                .entries,
            vec!["openai/gpt-5-mini".to_string()]
        );
        // Non-default role name that is not configured: falls to the exact model key.
        assert_eq!(
            chain_for(&chains, "task", "anthropic", "claude-opus-4-7")
                .unwrap()
                .entries,
            vec!["google/gemini-3-pro".to_string()]
        );
        // The "default" role key matches any model by design; an UNCONFIGURED
        // role + unconfigured exact model yields no chain.
        assert!(chain_for(&chains, "task", "openai", "gpt-5.5").is_none());
    }

    #[test]
    fn cooldown_blocks_primary_until_elapsed() {
        let start = Instant::now();
        let mut tracker = CooldownTracker::new(60);
        assert!(tracker.should_use_primary(start));
        tracker.record_primary_failure(start);
        assert!(!tracker.should_use_primary(start + Duration::from_secs(59)));
        assert!(tracker.should_use_primary(start + Duration::from_secs(60)));
        tracker.record_primary_failure(start);
        tracker.reset();
        assert!(tracker.should_use_primary(start));
    }

    #[test]
    fn credential_ring_affinity_and_backoff() {
        let start = Instant::now();
        let keys = vec!["k1".to_string(), "k2".to_string(), "k3".to_string()];
        let mut ring = CredentialRing::new(keys, session_affinity_hash("sess-1")).unwrap();
        let first = ring.current_key(start).unwrap().to_string();
        // Affinity is stable for the same session id.
        let ring2 = CredentialRing::new(
            vec!["k1".to_string(), "k2".to_string(), "k3".to_string()],
            session_affinity_hash("sess-1"),
        )
        .unwrap();
        assert_eq!(ring2.current_key(start).unwrap(), first);

        // Rate-limit the current key: rotation moves to another key.
        ring.report_rate_limited(
            &first,
            start,
            Duration::from_secs(1),
            Duration::from_secs(60),
        );
        let next = ring.current_key(start).unwrap().to_string();
        assert_ne!(next, first);
        assert_eq!(ring.cooling_count(start), 1);

        // Backoff expiry restores the original key.
        let later = start + Duration::from_secs(2);
        assert_eq!(ring.current_key(later).unwrap(), first);

        // Rate-limit every key: no usable key remains.
        ring.report_rate_limited(
            &first,
            later,
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        ring.report_rate_limited(
            &next,
            later,
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        let third = ["k1", "k2", "k3"]
            .into_iter()
            .find(|k| k != &first && k != &next)
            .unwrap();
        ring.report_rate_limited(
            third,
            later,
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        assert!(ring.current_key(later).is_none());
    }

    #[test]
    fn scope_override_longest_prefix_wins() {
        let overrides = vec![
            crate::config::ModelScopeOverride {
                path: "/repo".to_string(),
                enabled_models: None,
                disabled_providers: None,
            },
            crate::config::ModelScopeOverride {
                path: "/repo/a".to_string(),
                enabled_models: Some(vec!["openai/gpt-5.5".to_string()]),
                disabled_providers: None,
            },
        ];
        let winner = best_scope_override(&overrides, Path::new("/repo/a/sub")).unwrap();
        assert_eq!(winner.path, "/repo/a");
        assert!(best_scope_override(&overrides, Path::new("/elsewhere")).is_none());
    }

    #[test]
    fn provider_disable_checks_global_then_scope() {
        let disabled = vec!["anthropic".to_string()];
        assert!(provider_is_disabled(&disabled, None, "anthropic"));
        assert!(provider_is_disabled(&disabled, None, "ANTHROPIC")); // case-insensitive
        assert!(!provider_is_disabled(&disabled, None, "openai"));
        let scope = crate::config::ModelScopeOverride {
            path: "/repo".to_string(),
            enabled_models: None,
            disabled_providers: Some(vec!["openai".to_string()]),
        };
        assert!(provider_is_disabled(&disabled, Some(&scope), "openai"));
        assert!(provider_is_disabled(&disabled, Some(&scope), "anthropic"));
    }
}
