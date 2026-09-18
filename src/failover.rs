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

/// Refusal shared by same-provider retry and failover. The input is lowercase.
/// Authentication wins even when the provider also mentions a transient status.
fn is_auth_failure(lower: &str) -> bool {
    const AUTH_PATTERNS: &[&str] = &[
        "unauthorized",
        "unauthenticated",
        "forbidden",
        "invalid api key",
        "invalid_api_key",
        "incorrect api key",
        "authentication failed",
        "authentication error",
        "authentication_error",
        "permission denied",
        "permission_denied",
        "expired token",
        "invalid token",
        "invalid_token",
        "invalid credentials",
        "invalid_credentials",
        "missing api key",
    ];
    if AUTH_PATTERNS.iter().any(|pattern| lower.contains(pattern)) {
        return true;
    }

    // Prefer an explicit response status to unrelated numbers inside the body.
    // Reuse the existing parser rather than making recovery and diagnostics
    // disagree about HTTP/status markers. Credential wording above still wins.
    if let Some(status) =
        crate::error::ProviderErrorSummary::from_error_text(None, lower).http_status
    {
        return matches!(status, 401 | 403);
    }

    // Some providers emit a bare status instead. Match a complete token, never
    // the middle of a request id, a token count, or a duration such as 1403ms.
    lower
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .any(|token| matches!(token, "401" | "403"))
}

/// Classify an error text for failover. `None` = never fail over (auth and
/// other loud errors). Terminal markers and auth are checked before transient
/// patterns so a "401 ... quota" message never fails over.
pub fn classify_failover(error_text: &str) -> Option<FailoverClass> {
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

    // Durability failures may follow completed side effects. Context overflow
    // needs compaction, not a replay against another provider.
    if marks_session_persistence(error_text)
        || crate::error::is_context_overflow(error_text, None, None)
    {
        return None;
    }
    let text = error_text.to_ascii_lowercase();

    // Auth: loud, user-actionable, never a failover trigger.
    if is_auth_failure(&text) {
        return None;
    }

    if QUOTA_PATTERNS.iter().any(|p| text.contains(p)) {
        return Some(FailoverClass::Quota);
    }

    if OVERLOAD_PATTERNS.iter().any(|p| text.contains(p)) {
        return Some(FailoverClass::Overload);
    }

    if crate::error::is_retryable_error(&text, None, None) {
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
/// Provider aliases and surrounding spec whitespace follow model resolution.
/// An exact key wins over equivalent spellings; otherwise the lexicographically
/// first matching key wins, independent of `HashMap` iteration order.
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
    if let Some(entries) = chains.get(&full)
        && !entries.is_empty()
    {
        return Some(FailoverChain {
            entries: entries.clone(),
        });
    }
    chains
        .iter()
        .filter(|(key, entries)| !entries.is_empty() && model_spec_matches(key, provider, model_id))
        .min_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, entries)| FailoverChain {
            entries: entries.clone(),
        })
}

/// Use the same identity relation as `resolve_chain_spec`, including provider
/// aliases and nested model ids. Raw string equality is not model identity.
fn model_spec_matches(spec: &str, provider: &str, model_id: &str) -> bool {
    crate::provider_metadata::split_provider_model_spec(spec).is_some_and(
        |(candidate_provider, candidate_model)| {
            crate::provider_metadata::provider_ids_match(provider, candidate_provider)
                && model_id.eq_ignore_ascii_case(candidate_model)
        },
    )
}

fn model_specs_match(left: &str, right: &str) -> bool {
    crate::provider_metadata::split_provider_model_spec(right).map_or_else(
        || left.eq_ignore_ascii_case(right),
        |(provider, model_id)| model_spec_matches(left, provider, model_id),
    )
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

    /// Create a cooldown tracker restored from a restart-safe deadline (bd-gm481.2).
    /// If `now < deadline`, the remaining cooldown is tracked. If `now >= deadline`,
    /// the cooldown has elapsed.
    #[must_use]
    pub fn from_deadline(
        cooldown_secs: u64,
        deadline: chrono::DateTime<chrono::Utc>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let mut tracker = Self::new(cooldown_secs);
        if now < deadline {
            let remaining_millis = (deadline - now).num_milliseconds().max(0) as u64;
            let total_millis = cooldown_secs.saturating_mul(1000);
            let elapsed_millis = total_millis.saturating_sub(remaining_millis);
            let failed_at = Instant::now()
                .checked_sub(Duration::from_millis(elapsed_millis))
                .unwrap_or_else(Instant::now);
            tracker.record_primary_failure(failed_at);
        } else {
            let past = Instant::now()
                .checked_sub(Duration::from_secs(cooldown_secs.saturating_add(1)))
                .unwrap_or_else(Instant::now);
            tracker.record_primary_failure(past);
        }
        tracker
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

/// The identity a fallback chain started from, recorded so a cooldown has
/// something to return to.
///
/// Neither the Session header nor the newest `ModelChange` can answer this
/// after a committed failover: both advance to the fallback. The thinking level
/// is the one the user actually asked for, not the level a fallback clamped it
/// to, so restoring gives back what they configured rather than what the detour
/// allowed (bd-gm481.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverPrimary {
    /// Provider id the chain started from.
    pub provider: String,
    /// Model id the chain started from.
    pub model_id: String,
    /// The level requested before any swap clamped it.
    pub requested_thinking_level: crate::model::ThinkingLevel,
}

/// Cross-turn failover bookkeeping: where the chain walk left off, what to
/// return to, and when the cooldown on returning started.
///
/// Print mode and the RPC server each grew their own copy of these four fields
/// and the operations over them, down to the same "record the primary only on
/// the FIRST swap" rule. Keeping one definition is what lets a third surface —
/// the interactive stacks, where a configured chain is currently inert — adopt
/// failover without becoming a third copy (bd-u2qv4).
///
/// Durable restart-safe failover provenance is recorded on Session `ModelChange`
/// entries and restored upon session open (bd-gm481.2).
#[derive(Debug, Default)]
pub struct FailoverState {
    cooldown: Option<CooldownTracker>,
    primary: Option<FailoverPrimary>,
    active: Option<(String, String)>,
    chain_position: usize,
    lifecycle_id: Option<String>,
}

impl FailoverState {
    /// Build from configuration. The cooldown tracker is absent when no
    /// fallback chain is configured: with no chain there is nothing to fail
    /// over to and nothing to restore from.
    #[must_use]
    pub fn new(config: &crate::config::Config) -> Self {
        Self {
            cooldown: config
                .retry
                .as_ref()
                .and_then(|retry| retry.fallback_chains.as_ref())
                .map(|_| CooldownTracker::new(config.failover_cooldown_secs())),
            primary: None,
            active: None,
            chain_position: 0,
            lifecycle_id: None,
        }
    }

    /// Build with no cooldown tracker, for a session that has no chain
    /// configured: there is nothing to fail over to and nothing to restore.
    #[must_use]
    pub const fn new_empty() -> Self {
        Self {
            cooldown: None,
            primary: None,
            active: None,
            chain_position: 0,
            lifecycle_id: None,
        }
    }

    /// Build with an explicit cooldown, for a caller that configures failover
    /// directly rather than from a [`crate::config::Config`].
    #[must_use]
    pub fn with_cooldown_secs(cooldown_secs: u64) -> Self {
        Self {
            cooldown: Some(CooldownTracker::new(cooldown_secs)),
            ..Self::default()
        }
    }

    /// Reconstruct cross-turn failover bookkeeping from a persisted session
    /// record (bd-gm481.2). If the session's active branch path ends on an
    /// unrestored failover swap, its primary, active fallback, chain position,
    /// and restart-safe cooldown deadline are restored.
    #[must_use]
    pub fn reconstruct_from_session(
        session: &crate::session::Session,
        configured_cooldown_secs: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let Some(provenance) = session.active_failover_provenance_for_current_path() else {
            return Self::with_cooldown_secs(configured_cooldown_secs);
        };
        Self::reconstruct_from_provenance(provenance, configured_cooldown_secs, now)
    }

    /// Reconstruct cross-turn failover bookkeeping from explicit provenance
    /// (bd-gm481.2).
    #[must_use]
    pub fn reconstruct_from_provenance(
        provenance: &crate::session::ModelChangeFailover,
        configured_cooldown_secs: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let primary = FailoverPrimary {
            provider: provenance.primary_provider.clone(),
            model_id: provenance.primary_model_id.clone(),
            requested_thinking_level: provenance
                .primary_thinking_level
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default(),
        };
        let active = (
            provenance.fallback_provider.clone(),
            provenance.fallback_model_id.clone(),
        );
        let chain_position = provenance.chain_position.unwrap_or(0);
        let cooldown_secs = provenance.cooldown_secs.unwrap_or(configured_cooldown_secs);
        let cooldown = provenance
            .cooldown_deadline
            .as_deref()
            .and_then(|dl| chrono::DateTime::parse_from_rfc3339(dl).ok())
            .map_or_else(
                || CooldownTracker::new(cooldown_secs),
                |deadline| {
                    CooldownTracker::from_deadline(
                        cooldown_secs,
                        deadline.with_timezone(&chrono::Utc),
                        now,
                    )
                },
            );
        Self {
            cooldown: Some(cooldown),
            primary: Some(primary),
            active: Some(active),
            chain_position,
            lifecycle_id: provenance.lifecycle_id.clone(),
        }
    }

    /// The identity the chain started from, once a swap has committed.
    #[must_use]
    pub const fn primary(&self) -> Option<&FailoverPrimary> {
        self.primary.as_ref()
    }

    /// The fallback currently installed, if any.
    #[must_use]
    pub const fn active(&self) -> Option<&(String, String)> {
        self.active.as_ref()
    }

    /// Where the next walk resumes, carried across turns so a later turn
    /// continues the chain instead of restarting it.
    #[must_use]
    pub const fn chain_position(&self) -> usize {
        self.chain_position
    }

    /// Record where a walk finished, whether or not it swapped.
    pub const fn set_chain_position(&mut self, position: usize) {
        self.chain_position = position;
    }

    /// Active failover lifecycle ID, if any.
    #[must_use]
    pub fn lifecycle_id(&self) -> Option<&str> {
        self.lifecycle_id.as_deref()
    }

    /// Set or update the active failover lifecycle ID.
    pub fn set_lifecycle_id(&mut self, id: Option<String>) {
        self.lifecycle_id = id;
    }

    /// Cooldown tracker, if configured.
    #[must_use]
    pub const fn cooldown(&self) -> Option<&CooldownTracker> {
        self.cooldown.as_ref()
    }

    /// Deconstruct into constituent state components.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Option<CooldownTracker>,
        Option<FailoverPrimary>,
        Option<(String, String)>,
        usize,
        Option<String>,
    ) {
        (
            self.cooldown,
            self.primary,
            self.active,
            self.chain_position,
            self.lifecycle_id,
        )
    }

    /// The identity a fresh swap should record as its primary: the one already
    /// recorded when a chain is in flight, else the live identity.
    ///
    /// A second hop moves away from a FALLBACK, and the identity to return to
    /// is still the model the chain started from (bd-oqo03.1).
    #[must_use]
    pub fn primary_for_swap(&self, live: FailoverPrimary) -> FailoverPrimary {
        self.primary.clone().unwrap_or(live)
    }

    /// Record a committed swap away from `primary` onto `active`, starting the
    /// cooldown on returning to the primary.
    pub fn record_swap(
        &mut self,
        primary: FailoverPrimary,
        active: (String, String),
        now: std::time::Instant,
    ) {
        if self.primary.is_none() {
            self.primary = Some(primary);
        }
        self.active = Some(active);
        if let Some(tracker) = self.cooldown.as_mut() {
            tracker.record_primary_failure(now);
        }
    }

    /// Whether the primary may be used again at `now`. False with no chain
    /// configured and nothing to restore.
    #[must_use]
    pub fn should_restore_primary(&self, now: std::time::Instant) -> bool {
        self.primary.is_some()
            && self
                .cooldown
                .as_ref()
                .is_some_and(|tracker| tracker.should_use_primary(now))
    }

    /// The primary is back; the chain starts over from the top next time.
    pub fn clear(&mut self) {
        self.primary = None;
        self.active = None;
        self.chain_position = 0;
        self.lifecycle_id = None;
        if let Some(tracker) = self.cooldown.as_mut() {
            tracker.reset();
        }
    }
}

/// Cursor over a fallback chain that yields only the specs worth considering.
///
/// The live model and any spec already walked earlier in the same chain are
/// skipped: installing either emits a phantom `FailoverStart`/`FailoverEnd`
/// pair and spends a unit of `max_failovers_per_turn` on a no-op (bd-oqo03.1).
/// The walk is bounded by the chain, never by that per-turn cap — bounding the
/// cursor by the cap let malformed, uncredentialed, unconstructible, current
/// or duplicate entries consume the budget and hide a later valid entry.
///
/// Print mode and the RPC server each had their own copy of exactly this
/// cursor arithmetic, and both off-by-one bugs in the family (bd-oqo03,
/// bd-oqo03.1) had to be found and fixed twice. One definition now, so the
/// interactive surfaces can walk a chain without inheriting a third copy
/// (bd-u2qv4).
///
/// [`Self::position`] is the resume point for the NEXT turn: one past the
/// entry last yielded. Callers persist it only once a swap actually commits.
pub struct FailoverWalk<'a> {
    entries: &'a [String],
    position: usize,
    current_provider: &'a str,
    current_model: &'a str,
}

impl<'a> FailoverWalk<'a> {
    /// Start (or resume, via `position`) a walk of `chain` while
    /// `current_provider`/`current_model` are live.
    #[must_use]
    pub const fn new(
        chain: &'a FailoverChain,
        position: usize,
        current_provider: &'a str,
        current_model: &'a str,
    ) -> Self {
        Self {
            entries: chain.entries.as_slice(),
            position,
            current_provider,
            current_model,
        }
    }

    /// The next candidate spec and the chain index it occupies, advancing past
    /// it. The index is captured BEFORE the advance: reporting the post-advance
    /// cursor as the chain index is off by one (bd-oqo03).
    pub fn next_spec(&mut self) -> Option<(usize, &'a str)> {
        while self.position < self.entries.len() {
            let index = self.position;
            let spec = self.entries[index].as_str();
            self.position += 1;
            let is_current = model_spec_matches(spec, self.current_provider, self.current_model);
            let is_duplicate = self.entries[..index]
                .iter()
                .any(|earlier| model_specs_match(earlier, spec));
            if is_current || is_duplicate {
                continue;
            }
            return Some((index, spec));
        }
        None
    }

    /// Where the next turn resumes: one past the entry last yielded.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.position
    }
}

/// Resolve one `provider/model` chain spec against the configured model list.
///
/// A well-formed pair that is simply not configured falls back to an ad-hoc
/// entry. `None` means the spec names nothing usable and the walk should move
/// on.
#[must_use]
pub fn resolve_chain_spec(
    spec: &str,
    available_models: &[crate::models::ModelEntry],
) -> Option<crate::models::ModelEntry> {
    let (provider, model_id) = crate::provider_metadata::split_provider_model_spec(spec)?;
    available_models
        .iter()
        .find(|entry| {
            crate::provider_metadata::provider_ids_match(&entry.model.provider, provider)
                && entry.model.id.eq_ignore_ascii_case(model_id)
        })
        .cloned()
        .or_else(|| crate::models::ad_hoc_model_entry(provider, model_id))
}

// ---------------------------------------------------------------------------
// Shared retry policy (bd-u2qv4)
//
// Print mode (`src/main.rs`) and the RPC server (`src/rpc.rs`) each grew their
// own copy of this policy, and the copies have already drifted. The functions
// below are the single definition both surfaces call, so a third consumer —
// the interactive stacks, which today have no provider retry or failover at
// all — can adopt the same policy instead of becoming a fourth copy.
//
// Everything here is pure: no I/O, no surface types, no session mutation.
// Whether to sleep, what to emit, and how to resume the turn stay with the
// caller, because those genuinely differ per surface.
// ---------------------------------------------------------------------------

/// Exponential backoff delay for same-provider retry `attempt` (1-based).
///
/// `attempt` 0 and 1 both yield `base_delay_ms`; each later attempt doubles,
/// capped at `max_delay_ms`. Saturating throughout so a large attempt count
/// clamps at the cap rather than overflowing.
#[must_use]
pub fn retry_delay_ms(base_delay_ms: u32, max_delay_ms: u32, attempt: u32) -> u32 {
    let base = u64::from(base_delay_ms);
    let max = u64::from(max_delay_ms);
    let shift = attempt.saturating_sub(1);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let delay = base.saturating_mul(multiplier).min(max);
    u32::try_from(delay).unwrap_or(u32::MAX)
}

/// Terminal marker check (bd-8188r): does this error text describe a
/// session-persistence failure?
///
/// Such a failure means provider or tool side effects may already have
/// happened while the durable record is missing or stale. Re-entering the
/// provider — retry, credential rotation, or model failover — could repeat
/// those effects, so callers must treat this as final regardless of what the
/// wrapped prose looks like.
///
/// `contains`, not `starts_with`: the flattened `Display` form embeds the
/// marker after `thiserror`'s own "Session error: " prefix. A false positive
/// merely refuses a retry, which is the safe direction.
#[must_use]
pub fn marks_session_persistence(error_text: &str) -> bool {
    error_text.contains(crate::error::Error::SESSION_PERSISTENCE_PREFIX)
}

/// Whether a completed turn that ended in [`StopReason::Error`] should be
/// retried against the same provider.
///
/// `context_window` is the active model's context window when the caller knows
/// it; supplying it lets [`crate::error::is_retryable_error`] recognise a
/// context overflow, which is never retryable. Print mode passed `None` here
/// while RPC supplied the real window, so the same overflow was retried on one
/// surface and refused on the other; callers that can resolve the window
/// should pass it.
#[must_use]
pub fn error_result_is_retryable(
    message: &crate::model::AssistantMessage,
    context_window: Option<u32>,
) -> bool {
    if !matches!(message.stop_reason, crate::model::StopReason::Error) {
        return false;
    }
    let error_text = message.error_message.as_deref().unwrap_or("Request error");
    // Terminal markers and authentication outrank transient-looking prose.
    // In particular, a mixed 401/429 response must not spend the retry budget.
    if marks_session_persistence(error_text) || is_auth_failure(&error_text.to_ascii_lowercase()) {
        return false;
    }
    crate::error::is_retryable_error(error_text, Some(message.usage.input), context_window)
}

/// Only provider/transport failures can justify another provider call. A local
/// tool, configuration, extension or session failure cannot be repaired by
/// re-entering the provider, even when its diagnostic quotes a transient error.
fn is_provider_call_error(error: &crate::error::Error) -> bool {
    matches!(
        error,
        crate::error::Error::Api(_)
            | crate::error::Error::Provider { .. }
            | crate::error::Error::Io(_)
    )
}

/// Whether a failed provider call reported through [`crate::error::Error`]
/// should be retried against the same provider.
///
/// Local errors and terminal markers are refused before classification.
/// For provider/transport errors, [`crate::error::Error::is_transient`] walks
/// the typed source chain without depending on flattened message text, then
/// prose-only failures use text matching (pi_agent_rust#118).
#[must_use]
pub fn call_error_is_retryable(error: &crate::error::Error) -> bool {
    if !is_provider_call_error(error) {
        return false;
    }
    let error_text = error.to_string();
    if marks_session_persistence(&error_text) || is_auth_failure(&error_text.to_ascii_lowercase()) {
        return false;
    }
    error.is_transient() || crate::error::is_retryable_error(&error_text, None, None)
}

/// The outcome of one provider attempt, borrowed for classification.
#[derive(Debug, Clone, Copy)]
pub enum TurnOutcome<'a> {
    /// The provider returned a message, which may itself carry an error stop.
    Completed(&'a crate::model::AssistantMessage),
    /// The call failed before a message could be produced.
    Failed(&'a crate::error::Error),
}

/// Why a turn must end without another provider call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReason {
    /// bd-8188r: provider or tool side effects may already have happened while
    /// the durable session record is missing or stale, so re-entering the
    /// provider could repeat them. Final regardless of how transient the
    /// wrapped prose reads.
    SessionPersistence,
    /// Aborted locally. The user asked for this; do not retry it at them.
    Aborted,
}

/// What a surface should do after one provider attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDecision {
    /// Hand the outcome back unchanged.
    Finish {
        /// False for an errored turn that has exhausted retry and failover.
        success: bool,
    },
    /// Re-issue against the same provider as attempt `attempt`, after
    /// `delay_ms`. Resume the turn rather than replaying it: only the failed
    /// request's incomplete output is stripped, so completed tool cycles are
    /// neither re-run nor re-billed (pi_agent_rust#125).
    Retry {
        /// 1-based attempt number, for the surface's retry events.
        attempt: u32,
        /// Backoff before re-entry.
        delay_ms: u32,
    },
    /// Walk the fallback chain. The caller resolves the next entry with
    /// [`FailoverWalk`]; a swap that commits resets the retry budget and spends
    /// one unit of `max_failovers_per_turn`.
    FailOver,
    /// Neither retry nor fail over, whatever the error text looks like.
    Terminal(TerminalReason),
}

/// The configured limits a turn is decided against.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Same-provider retries allowed per chain entry.
    pub max_retries: u32,
    /// Successful fallback swaps allowed in one turn. The chain walk itself is
    /// bounded by the chain, not by this (bd-oqo03.1).
    pub max_failovers_per_turn: u32,
    /// First retry delay; each later attempt doubles it.
    pub base_delay_ms: u32,
    /// Ceiling for the doubling.
    pub max_delay_ms: u32,
}

impl RetryPolicy {
    /// Read the policy a surface should apply out of configuration.
    ///
    /// `None` means the user turned retry off, and a surface that gets `None`
    /// must hand a failed turn straight back rather than quietly substituting a
    /// default — the whole point of `retry.enabled = false` is that nobody
    /// re-enters the provider on the user's behalf.
    ///
    /// One reader for the four config keys, so a surface adopting this policy
    /// cannot accidentally consult a different set (bd-u2qv4). Print mode
    /// supplies its own `max_retries` from the CLI and so builds its policy
    /// directly.
    #[must_use]
    pub fn from_config(config: &crate::config::Config) -> Option<Self> {
        config.retry_enabled().then(|| Self {
            max_retries: config.retry_max_retries(),
            max_failovers_per_turn: config.max_failovers_per_turn(),
            base_delay_ms: config.retry_base_delay_ms(),
            max_delay_ms: config.retry_max_delay_ms(),
        })
    }
}

/// Where this turn has got to.
#[derive(Debug, Clone, Copy)]
pub struct TurnProgress {
    /// Same-provider retries already spent against the current entry.
    pub retry_count: u32,
    /// Fallback swaps already committed in this turn.
    pub failovers_this_turn: u32,
    /// Whether the surface can still retry without corrupting what the user
    /// has already seen. Print mode refuses once visible bytes have been
    /// streamed to a terminal; surfaces with no such constraint pass `true`.
    pub stream_can_retry: bool,
}

/// Decide what to do after one provider attempt.
///
/// This is the whole retry/failover policy, and it is pure: no I/O, no session
/// mutation, no surface types. Sleeping, emitting events, restoring the turn
/// tail and swapping the provider stay with the caller, because those genuinely
/// differ between print mode, RPC and the interactive stacks — the policy does
/// not, and three hand-rolled copies of it would drift the way the first two
/// already have (bd-u2qv4).
///
/// `context_window` is the active model's window when the caller can resolve
/// it. Supplying it lets a context overflow be recognised as never-retryable;
/// omitting it means an overflow is retried until the budget is spent.
#[must_use]
pub fn decide(
    outcome: TurnOutcome<'_>,
    progress: &TurnProgress,
    policy: &RetryPolicy,
    context_window: Option<u32>,
) -> TurnDecision {
    let retry = || TurnDecision::Retry {
        attempt: progress.retry_count.saturating_add(1),
        delay_ms: retry_delay_ms(
            policy.base_delay_ms,
            policy.max_delay_ms,
            progress.retry_count.saturating_add(1),
        ),
    };
    let budget_left = progress.retry_count < policy.max_retries && progress.stream_can_retry;
    let may_fail_over = progress.failovers_this_turn < policy.max_failovers_per_turn;

    // Each outcome shape decides only what is specific to it — whether the turn
    // is terminal, and whether its failure is retryable. The budget arithmetic
    // that follows is the same for both, and keeping it in one place is what
    // stops the two from drifting again.
    let same_provider_is_worth_another_try = match outcome {
        TurnOutcome::Completed(message) => {
            match message.stop_reason {
                crate::model::StopReason::Aborted => {
                    return TurnDecision::Terminal(TerminalReason::Aborted);
                }
                crate::model::StopReason::Error => {}
                _ => return TurnDecision::Finish { success: true },
            }
            let error_text = message.error_message.as_deref().unwrap_or("Request error");
            if marks_session_persistence(error_text) {
                return TurnDecision::Terminal(TerminalReason::SessionPersistence);
            }
            if is_auth_failure(&error_text.to_ascii_lowercase()) {
                return TurnDecision::Finish { success: false };
            }
            error_result_is_retryable(message, context_window)
        }
        TurnOutcome::Failed(error) => {
            if matches!(error, crate::error::Error::Aborted) {
                return TurnDecision::Terminal(TerminalReason::Aborted);
            }
            // A persistence marker remains terminal after a boundary flattens
            // and rewraps it as Api, Provider, Io, or another Session error.
            let error_text = error.to_string();
            if marks_session_persistence(&error_text) {
                return TurnDecision::Terminal(TerminalReason::SessionPersistence);
            }
            if !is_provider_call_error(error) || is_auth_failure(&error_text.to_ascii_lowercase()) {
                return TurnDecision::Finish { success: false };
            }
            call_error_is_retryable(error)
        }
    };

    if budget_left && same_provider_is_worth_another_try {
        return retry();
    }
    if may_fail_over {
        return TurnDecision::FailOver;
    }
    TurnDecision::Finish { success: false }
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
    fn chain_lookup_resolves_provider_aliases_and_spec_whitespace() {
        let chains = HashMap::from([(
            " gemini / GEMINI-Z ".to_string(),
            vec!["openai/gpt-y".to_string()],
        )]);
        for provider in ["google", "gemini", "GOOGLE"] {
            let resolved = chain_for(&chains, "task", provider, "gemini-z")
                .expect("configured aliases must resolve like model selection");
            assert_eq!(resolved.entries, vec!["openai/gpt-y".to_string()]);
        }
    }

    #[test]
    fn chain_lookup_keeps_role_then_exact_key_precedence() {
        let chains = HashMap::from([
            ("task".to_string(), vec!["role/winner".to_string()]),
            (
                "google/gemini-z".to_string(),
                vec!["exact/winner".to_string()],
            ),
            (
                "GOOGLE/GEMINI-Z".to_string(),
                vec!["case/alternative".to_string()],
            ),
            (
                "gemini/gemini-z".to_string(),
                vec!["alias/alternative".to_string()],
            ),
        ]);
        assert_eq!(
            chain_for(&chains, "task", "google", "gemini-z")
                .unwrap()
                .entries,
            vec!["role/winner".to_string()]
        );
        assert_eq!(
            chain_for(&chains, "unconfigured", "google", "gemini-z")
                .unwrap()
                .entries,
            vec!["exact/winner".to_string()]
        );
    }

    #[test]
    fn equivalent_chain_keys_have_a_stable_tie_break() {
        for keys in [
            ["gemini/gemini-z", "GEMINI/GEMINI-Z"],
            ["GEMINI/GEMINI-Z", "gemini/gemini-z"],
        ] {
            let chains: HashMap<_, _> = keys
                .into_iter()
                .map(|key| (key.to_string(), vec![key.to_string()]))
                .collect();
            assert_eq!(
                chain_for(&chains, "task", "google", "gemini-z")
                    .unwrap()
                    .entries,
                vec!["GEMINI/GEMINI-Z".to_string()]
            );
        }
    }

    #[test]
    fn empty_role_and_exact_chains_do_not_hide_a_nonempty_alias_chain() {
        let chains = HashMap::from([
            ("task".to_string(), Vec::new()),
            ("google/gemini-z".to_string(), Vec::new()),
            (
                "gemini/gemini-z".to_string(),
                vec!["openai/gpt-y".to_string()],
            ),
        ]);
        assert_eq!(
            chain_for(&chains, "task", "google", "gemini-z")
                .unwrap()
                .entries,
            vec!["openai/gpt-y".to_string()]
        );
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

    // -- cross-turn bookkeeping (bd-u2qv4) ---------------------------------

    fn primary(provider: &str, model_id: &str) -> FailoverPrimary {
        FailoverPrimary {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            requested_thinking_level: crate::model::ThinkingLevel::High,
        }
    }

    fn state_with_chain() -> FailoverState {
        let mut config = crate::config::Config::default();
        config.retry = Some(crate::config::RetrySettings {
            fallback_chains: Some(std::collections::HashMap::from([(
                "default".to_string(),
                vec!["openai/gpt-y".to_string()],
            )])),
            failover_cooldown_secs: Some(300),
            ..crate::config::RetrySettings::default()
        });
        FailoverState::new(&config)
    }

    #[test]
    fn a_second_hop_still_records_the_model_the_chain_started_from() {
        // bd-oqo03.1: a later hop moves away from a FALLBACK, and the identity
        // to return to is the primary, not the fallback being left.
        let mut state = state_with_chain();
        let now = Instant::now();
        state.record_swap(
            state.primary_for_swap(primary("anthropic", "claude-x")),
            ("openai".to_string(), "gpt-y".to_string()),
            now,
        );
        let second = state.primary_for_swap(primary("openai", "gpt-y"));
        assert_eq!(second, primary("anthropic", "claude-x"));
        state.record_swap(second, ("google".to_string(), "gemini-z".to_string()), now);
        assert_eq!(state.primary(), Some(&primary("anthropic", "claude-x")));
        assert_eq!(
            state.active(),
            Some(&("google".to_string(), "gemini-z".to_string()))
        );
    }

    #[test]
    fn the_primary_is_restorable_only_after_the_cooldown_elapses() {
        let mut state = state_with_chain();
        let now = Instant::now();
        assert!(
            !state.should_restore_primary(now),
            "nothing has failed over, so there is nothing to restore"
        );
        state.record_swap(
            primary("anthropic", "claude-x"),
            ("openai".to_string(), "gpt-y".to_string()),
            now,
        );
        assert!(!state.should_restore_primary(now));
        assert!(state.should_restore_primary(now + Duration::from_secs(301)));

        state.clear();
        assert_eq!(state.primary(), None);
        assert_eq!(state.chain_position(), 0);
        assert!(!state.should_restore_primary(now + Duration::from_secs(301)));
    }

    #[test]
    fn with_no_chain_configured_there_is_no_cooldown_and_nothing_to_restore() {
        let mut state = FailoverState::new(&crate::config::Config::default());
        let now = Instant::now();
        state.record_swap(
            primary("anthropic", "claude-x"),
            ("openai".to_string(), "gpt-y".to_string()),
            now,
        );
        assert!(
            !state.should_restore_primary(now + Duration::from_secs(100_000)),
            "with no chain there is nothing to fail over to and nothing to restore from"
        );
    }

    #[test]
    fn failover_state_reconstructs_from_session_with_cooldown_active() {
        use crate::session::{ModelChangeFailover, Session};
        let mut session = Session::in_memory();
        let now = chrono::Utc::now();
        // Cooldown deadline is 40 seconds in the future
        let deadline = (now + chrono::Duration::seconds(40))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let provenance = ModelChangeFailover {
            primary_provider: "anthropic".to_string(),
            primary_model_id: "claude-3-7-sonnet".to_string(),
            primary_thinking_level: Some("high".to_string()),
            fallback_provider: "openai".to_string(),
            fallback_model_id: "gpt-4o".to_string(),
            chain_position: Some(2),
            cooldown_deadline: Some(deadline),
            cooldown_secs: Some(60),
            lifecycle_id: Some("lifecycle-active-123".to_string()),
        };

        session.append_model_change_with_role_and_failover(
            "openai".to_string(),
            "gpt-4o".to_string(),
            Some("failover".to_string()),
            Some(provenance),
        );

        let state = FailoverState::reconstruct_from_session(&session, 60, now);
        assert_eq!(
            state.primary(),
            Some(&primary("anthropic", "claude-3-7-sonnet"))
        );
        assert_eq!(
            state.active(),
            Some(&("openai".to_string(), "gpt-4o".to_string()))
        );
        assert_eq!(state.chain_position(), 2);
        assert_eq!(state.lifecycle_id(), Some("lifecycle-active-123"));

        // Cooldown has 40s left: right now it should NOT restore
        let inst_now = Instant::now();
        assert!(!state.should_restore_primary(inst_now));
        // After 41s, it SHOULD restore
        assert!(state.should_restore_primary(inst_now + Duration::from_secs(41)));
    }

    #[test]
    fn failover_state_reconstructs_from_session_with_cooldown_elapsed() {
        use crate::session::{ModelChangeFailover, Session};
        let mut session = Session::in_memory();
        let now = chrono::Utc::now();
        // Cooldown deadline is in the past (10 seconds ago)
        let deadline = (now - chrono::Duration::seconds(10))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let provenance = ModelChangeFailover {
            primary_provider: "anthropic".to_string(),
            primary_model_id: "claude-3-7-sonnet".to_string(),
            primary_thinking_level: Some("high".to_string()),
            fallback_provider: "openai".to_string(),
            fallback_model_id: "gpt-4o".to_string(),
            chain_position: Some(1),
            cooldown_deadline: Some(deadline),
            cooldown_secs: Some(300),
            lifecycle_id: Some("lifecycle-elapsed-456".to_string()),
        };

        session.append_model_change_with_role_and_failover(
            "openai".to_string(),
            "gpt-4o".to_string(),
            Some("failover".to_string()),
            Some(provenance),
        );

        let state = FailoverState::reconstruct_from_session(&session, 300, now);
        assert_eq!(
            state.primary(),
            Some(&primary("anthropic", "claude-3-7-sonnet"))
        );
        assert_eq!(
            state.active(),
            Some(&("openai".to_string(), "gpt-4o".to_string()))
        );
        assert_eq!(state.lifecycle_id(), Some("lifecycle-elapsed-456"));
        // Cooldown already elapsed: should restore immediately!
        assert!(state.should_restore_primary(Instant::now()));
    }

    #[test]
    fn failover_state_reconstruct_returns_clean_state_when_restored_or_explicitly_changed() {
        use crate::session::{ModelChangeFailover, Session};
        let mut session = Session::in_memory();
        let now = chrono::Utc::now();

        // 1. Unrestored failover
        let provenance = ModelChangeFailover {
            primary_provider: "anthropic".to_string(),
            primary_model_id: "claude-3-7-sonnet".to_string(),
            primary_thinking_level: None,
            fallback_provider: "openai".to_string(),
            fallback_model_id: "gpt-4o".to_string(),
            chain_position: Some(1),
            cooldown_deadline: None,
            cooldown_secs: Some(0),
            lifecycle_id: None,
        };
        session.append_model_change_with_role_and_failover(
            "openai".to_string(),
            "gpt-4o".to_string(),
            Some("failover".to_string()),
            Some(provenance),
        );

        // 2. Primary restored
        session.append_model_change_with_role(
            "anthropic".to_string(),
            "claude-3-7-sonnet".to_string(),
            Some("primary_restore".to_string()),
        );

        let state = FailoverState::reconstruct_from_session(&session, 300, now);
        assert_eq!(
            state.primary(),
            None,
            "restored session must not reconstruct failover primary"
        );
        assert_eq!(state.active(), None);

        // 3. Re-failover then explicit user model change
        session.append_model_change_with_role_and_failover(
            "openai".to_string(),
            "gpt-4o".to_string(),
            Some("failover".to_string()),
            Some(ModelChangeFailover {
                primary_provider: "anthropic".to_string(),
                primary_model_id: "claude-3-7-sonnet".to_string(),
                primary_thinking_level: None,
                fallback_provider: "openai".to_string(),
                fallback_model_id: "gpt-4o".to_string(),
                chain_position: Some(1),
                cooldown_deadline: None,
                cooldown_secs: Some(0),
                lifecycle_id: None,
            }),
        );
        // Explicit change (role None)
        session.append_model_change("cohere".to_string(), "command-r".to_string());

        let state2 = FailoverState::reconstruct_from_session(&session, 300, now);
        assert_eq!(
            state2.primary(),
            None,
            "explicit model change must supersede failover primary"
        );
        assert_eq!(state2.active(), None);
    }

    // -- shared chain walk (bd-u2qv4) --------------------------------------

    fn chain(specs: &[&str]) -> FailoverChain {
        FailoverChain {
            entries: specs.iter().map(|spec| (*spec).to_string()).collect(),
        }
    }

    #[test]
    fn the_walk_skips_the_live_model_and_earlier_duplicates() {
        let chain = chain(&[
            "anthropic/claude-x", // the live model: a no-op swap
            "openai/gpt-y",
            "OpenAI/GPT-Y", // duplicate of the previous, case-insensitively
            "google/gemini-z",
        ]);
        let mut walk = FailoverWalk::new(&chain, 0, "anthropic", "claude-x");
        assert_eq!(walk.next_spec(), Some((1, "openai/gpt-y")));
        assert_eq!(walk.next_spec(), Some((3, "google/gemini-z")));
        assert_eq!(walk.next_spec(), None);
        assert_eq!(walk.position(), 4);
    }

    #[test]
    fn the_walk_skips_alias_duplicates_after_intervening_models() {
        let chain = chain(&[
            "google/gemini-z",
            "openai/gpt-y",
            " GEMINI / GEMINI-Z ",
            "google/gemini-next",
        ]);
        let mut walk = FailoverWalk::new(&chain, 0, "anthropic", "claude-x");
        assert_eq!(walk.next_spec(), Some((0, "google/gemini-z")));
        assert_eq!(walk.next_spec(), Some((1, "openai/gpt-y")));
        assert_eq!(walk.next_spec(), Some((3, "google/gemini-next")));
        assert_eq!(walk.next_spec(), None);
        assert_eq!(walk.position(), 4);
    }

    #[test]
    fn a_resumed_walk_does_not_revisit_an_earlier_provider_alias() {
        let chain = chain(&[
            "gemini/gemini-z",
            "openai/gpt-y",
            "google/gemini-z",
            "anthropic/claude-x",
        ]);
        let mut walk = FailoverWalk::new(&chain, 2, "openai", "gpt-y");
        assert_eq!(walk.next_spec(), Some((3, "anthropic/claude-x")));
        assert_eq!(walk.position(), 4);
        assert_eq!(walk.next_spec(), None);
    }

    #[test]
    fn identity_matching_keeps_distinct_provider_routes_and_nested_models() {
        let chain = chain(&[
            "google/gemini-z",
            "google-gemini-cli/gemini-z",
            "openrouter/anthropic/claude-x",
            "anthropic/claude-x",
            " OPENROUTER / anthropic/CLAUDE-X ",
        ]);
        let mut walk = FailoverWalk::new(&chain, 0, "openai", "gpt-y");
        for (index, spec) in chain.entries[..4].iter().enumerate() {
            assert_eq!(walk.next_spec(), Some((index, spec.as_str())));
        }
        assert_eq!(walk.next_spec(), None);
        assert_eq!(walk.position(), 5);
    }

    #[test]
    fn the_yielded_index_is_the_entry_not_the_resume_point() {
        // bd-oqo03: reporting the post-advance cursor as the chain index is
        // off by one, and that index is what reaches the failover event.
        let chain = chain(&["openai/gpt-y", "google/gemini-z"]);
        let mut walk = FailoverWalk::new(&chain, 0, "anthropic", "claude-x");
        let (index, spec) = walk.next_spec().expect("first candidate");
        assert_eq!((index, spec), (0, "openai/gpt-y"));
        assert_eq!(
            walk.position(),
            1,
            "the resume point is one past the entry just yielded"
        );
    }

    #[test]
    fn a_resumed_walk_continues_past_the_persisted_position() {
        // bd-oqo03.1: `position` is durable across turns, so a per-turn cap of
        // one must still reach entry two on the next turn.
        let chain = chain(&["openai/gpt-y", "google/gemini-z"]);
        let mut walk = FailoverWalk::new(&chain, 1, "anthropic", "claude-x");
        assert_eq!(walk.next_spec(), Some((1, "google/gemini-z")));
        assert_eq!(walk.next_spec(), None);
    }

    #[test]
    fn a_malformed_spec_is_yielded_for_the_caller_to_reject() {
        // Resolution, credentials and provider construction stay with the
        // caller; the walk only decides what is worth looking at.
        let chain = chain(&["not-a-spec", "openai/gpt-y"]);
        let mut walk = FailoverWalk::new(&chain, 0, "anthropic", "claude-x");
        assert_eq!(walk.next_spec(), Some((0, "not-a-spec")));
        assert!(resolve_chain_spec("not-a-spec", &[]).is_none());
        assert_eq!(walk.next_spec(), Some((1, "openai/gpt-y")));
    }

    #[test]
    fn an_unconfigured_but_well_formed_spec_resolves_ad_hoc() {
        let resolved = resolve_chain_spec("openai/gpt-y", &[]);
        let entry = resolved.expect("a well-formed spec resolves even when unconfigured");
        assert!(crate::provider_metadata::provider_ids_match(
            &entry.model.provider,
            "openai"
        ));
        assert!(entry.model.id.eq_ignore_ascii_case("gpt-y"));
    }

    // -- shared retry policy (bd-u2qv4) ------------------------------------

    fn errored_message(
        error_message: Option<&str>,
        input_tokens: u64,
    ) -> crate::model::AssistantMessage {
        crate::model::AssistantMessage {
            content: Vec::new(),
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            usage: crate::model::Usage {
                input: input_tokens,
                ..crate::model::Usage::default()
            },
            stop_reason: crate::model::StopReason::Error,
            stop_details: None,
            error_message: error_message.map(str::to_string),
            timestamp: 0,
        }
    }

    #[test]
    fn retry_delay_doubles_from_base_and_caps() {
        assert_eq!(retry_delay_ms(500, 8_000, 0), 500);
        assert_eq!(retry_delay_ms(500, 8_000, 1), 500);
        assert_eq!(retry_delay_ms(500, 8_000, 2), 1_000);
        assert_eq!(retry_delay_ms(500, 8_000, 3), 2_000);
        assert_eq!(retry_delay_ms(500, 8_000, 4), 4_000);
        assert_eq!(retry_delay_ms(500, 8_000, 5), 8_000);
        // Saturates at the cap instead of overflowing the shift.
        assert_eq!(retry_delay_ms(500, 8_000, 30), 8_000);
        assert_eq!(retry_delay_ms(500, 8_000, u32::MAX), 8_000);
    }

    #[test]
    fn session_persistence_marker_is_recognized_after_a_display_prefix() {
        let flattened = format!(
            "Session error: {} could not write session",
            crate::error::Error::SESSION_PERSISTENCE_PREFIX
        );
        assert!(marks_session_persistence(&flattened));
        assert!(!marks_session_persistence("429 rate limit exceeded"));
    }

    #[test]
    fn only_errored_turns_are_retryable() {
        let mut ok = errored_message(None, 0);
        ok.stop_reason = crate::model::StopReason::Stop;
        assert!(!error_result_is_retryable(&ok, None));

        let mut aborted = errored_message(Some("connection reset by peer"), 0);
        aborted.stop_reason = crate::model::StopReason::Aborted;
        assert!(!error_result_is_retryable(&aborted, None));

        assert!(error_result_is_retryable(
            &errored_message(Some("connection reset by peer"), 0),
            None
        ));
    }

    #[test]
    fn a_session_persistence_turn_is_never_retryable_however_transient_it_reads() {
        // The prose alone would classify as retryable; the marker must win,
        // because repeating the turn could repeat side effects already made
        // against a session whose durable record is missing (bd-8188r).
        let text = format!(
            "{} connection reset by peer",
            crate::error::Error::SESSION_PERSISTENCE_PREFIX
        );
        assert!(crate::error::is_retryable_error(
            "connection reset by peer",
            None,
            None
        ));
        assert!(!error_result_is_retryable(
            &errored_message(Some(&text), 0),
            None
        ));
    }

    #[test]
    fn a_context_overflow_is_never_retryable_when_the_window_is_supplied() {
        // The drift this policy exists to remove: RPC supplies the active
        // model's context window here and print mode passes None. It only
        // changes the verdict for a SILENT overflow, where the prose reads
        // transient and input tokens > window is the only evidence — see
        // a_silent_context_overflow_stops_retrying_only_once_the_window_is_known.
        let overflow = errored_message(Some("prompt is too long: 250000 tokens > 200000"), 250_000);
        assert!(!error_result_is_retryable(&overflow, Some(200_000)));
    }

    // -- the decision itself (bd-u2qv4) ------------------------------------

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_retries: 2,
            max_failovers_per_turn: 1,
            base_delay_ms: 500,
            max_delay_ms: 8_000,
        }
    }

    fn progress(retry_count: u32, failovers_this_turn: u32) -> TurnProgress {
        TurnProgress {
            retry_count,
            failovers_this_turn,
            stream_can_retry: true,
        }
    }

    #[test]
    fn a_policy_is_read_from_config_and_absent_when_retry_is_disabled() {
        let settings = |enabled: bool| crate::config::Config {
            retry: Some(crate::config::RetrySettings {
                enabled: Some(enabled),
                max_retries: Some(4),
                base_delay_ms: Some(250),
                max_delay_ms: Some(9_000),
                max_failovers_per_turn: Some(2),
                ..crate::config::RetrySettings::default()
            }),
            ..crate::config::Config::default()
        };

        let policy = RetryPolicy::from_config(&settings(true)).expect("retry enabled");
        assert_eq!(policy.max_retries, 4);
        assert_eq!(policy.base_delay_ms, 250);
        assert_eq!(policy.max_delay_ms, 9_000);
        assert_eq!(policy.max_failovers_per_turn, 2);

        // "off" must mean off. A surface that substituted a default here would
        // re-enter the provider on behalf of a user who said not to.
        assert!(RetryPolicy::from_config(&settings(false)).is_none());
    }

    #[test]
    fn a_clean_turn_finishes_successfully() {
        let mut message = errored_message(None, 0);
        message.stop_reason = crate::model::StopReason::Stop;
        assert_eq!(
            decide(
                TurnOutcome::Completed(&message),
                &progress(0, 0),
                &policy(),
                None
            ),
            TurnDecision::Finish { success: true }
        );
    }

    #[test]
    fn an_abort_is_terminal_and_never_walks_the_chain() {
        // The user asked for this; retrying or failing over would re-enter a
        // provider they just stopped.
        let mut message = errored_message(Some("529 overloaded"), 0);
        message.stop_reason = crate::model::StopReason::Aborted;
        assert_eq!(
            decide(
                TurnOutcome::Completed(&message),
                &progress(0, 0),
                &policy(),
                None
            ),
            TurnDecision::Terminal(TerminalReason::Aborted)
        );
    }

    #[test]
    fn a_transient_error_retries_then_fails_over_then_finishes() {
        let message = errored_message(Some("503 service unavailable"), 0);
        let p = policy();
        assert_eq!(
            decide(TurnOutcome::Completed(&message), &progress(0, 0), &p, None),
            TurnDecision::Retry {
                attempt: 1,
                delay_ms: 500
            }
        );
        assert_eq!(
            decide(TurnOutcome::Completed(&message), &progress(1, 0), &p, None),
            TurnDecision::Retry {
                attempt: 2,
                delay_ms: 1_000
            }
        );
        // Retry budget spent: the chain is next, not the error.
        assert_eq!(
            decide(TurnOutcome::Completed(&message), &progress(2, 0), &p, None),
            TurnDecision::FailOver
        );
        // Failover budget spent too: now the error surfaces.
        assert_eq!(
            decide(TurnOutcome::Completed(&message), &progress(2, 1), &p, None),
            TurnDecision::Finish { success: false }
        );
    }

    #[test]
    fn a_surface_that_has_already_shown_output_does_not_retry_but_may_fail_over() {
        // Print mode refuses a retry once visible bytes have reached the
        // terminal; the chain is still admissible because a swap resumes the
        // turn rather than replaying it.
        let message = errored_message(Some("503 service unavailable"), 0);
        let mut stalled = progress(0, 0);
        stalled.stream_can_retry = false;
        assert_eq!(
            decide(TurnOutcome::Completed(&message), &stalled, &policy(), None),
            TurnDecision::FailOver
        );
    }

    #[test]
    fn session_persistence_is_terminal_on_both_outcome_shapes() {
        let text = format!(
            "{} connection reset by peer",
            crate::error::Error::SESSION_PERSISTENCE_PREFIX
        );
        let message = errored_message(Some(&text), 0);
        assert_eq!(
            decide(
                TurnOutcome::Completed(&message),
                &progress(0, 0),
                &policy(),
                None
            ),
            TurnDecision::Terminal(TerminalReason::SessionPersistence)
        );
        let error = crate::error::Error::session_persistence("write failed");
        assert_eq!(
            decide(
                TurnOutcome::Failed(&error),
                &progress(0, 0),
                &policy(),
                None
            ),
            TurnDecision::Terminal(TerminalReason::SessionPersistence)
        );
    }

    #[test]
    fn a_loud_auth_error_finishes_rather_than_failing_over_into_another_one() {
        // Refusal belongs to the shared decision, not only the caller's walk:
        // no surface may retry or open a failover lifecycle for bad credentials.
        let error = crate::error::Error::Api("401 unauthorized: invalid api key".to_string());
        assert_eq!(
            decide(
                TurnOutcome::Failed(&error),
                &progress(0, 0),
                &policy(),
                None
            ),
            TurnDecision::Finish { success: false }
        );
        assert_eq!(classify_failover(&error.to_string()), None);
    }

    #[test]
    fn a_failed_abort_is_terminal_with_any_remaining_budget() {
        let error = crate::error::Error::Aborted;
        assert!(!call_error_is_retryable(&error));
        for state in [progress(0, 0), progress(2, 0), progress(2, 1)] {
            assert_eq!(
                decide(TurnOutcome::Failed(&error), &state, &policy(), None),
                TurnDecision::Terminal(TerminalReason::Aborted)
            );
        }
    }

    #[test]
    fn local_errors_cannot_reenter_the_provider_due_to_their_prose() {
        use crate::error::Error;

        let errors = [
            Error::auth("429 quota exceeded"),
            Error::config("503 service unavailable"),
            Error::validation("connection reset by peer"),
            Error::tool("bash", "500 internal server error"),
            Error::extension("fetch failed"),
            Error::session("500 session write failed"),
            Error::SessionNotFound {
                path: "/sessions/500.jsonl".to_string(),
            },
            Error::Json(Box::new(serde_json::from_str::<bool>("503").unwrap_err())),
            Error::Sqlite(Box::new(fsqlite::FrankenError::Busy)),
        ];
        for error in &errors {
            assert!(!call_error_is_retryable(error), "{error}");
            for state in [progress(0, 0), progress(2, 0)] {
                assert_eq!(
                    decide(TurnOutcome::Failed(error), &state, &policy(), None),
                    TurnDecision::Finish { success: false },
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn auth_statuses_do_not_match_inside_request_ids_or_durations() {
        for detail in [
            "request=req401abcdef",
            "request=4037",
            "elapsed=1403ms",
            "trace=x_401_y",
        ] {
            let text = format!("503 service unavailable: {detail}");
            assert_eq!(classify_failover(&text), Some(FailoverClass::Overload));
            let message = errored_message(Some(&text), 0);
            let error = crate::error::Error::api(text);
            assert!(error_result_is_retryable(&message, None));
            assert!(call_error_is_retryable(&error));
            for outcome in [
                TurnOutcome::Completed(&message),
                TurnOutcome::Failed(&error),
            ] {
                assert_eq!(
                    decide(outcome, &progress(0, 0), &policy(), None),
                    TurnDecision::Retry {
                        attempt: 1,
                        delay_ms: 500
                    }
                );
            }
        }
    }

    #[test]
    fn explicit_response_status_outweighs_unlabelled_diagnostic_numbers() {
        for text in [
            "HTTP 503: service unavailable; request_id=401",
            "API error (status code: 503): upstream overloaded; attempt=403",
        ] {
            assert_eq!(classify_failover(text), Some(FailoverClass::Overload));
            let error = crate::error::Error::api(text);
            let message = errored_message(Some(text), 0);
            assert!(call_error_is_retryable(&error));
            assert!(error_result_is_retryable(&message, None));
        }
        // Explicit credential wording still wins over an outer 503 wrapper.
        assert_eq!(
            classify_failover("HTTP 503: invalid_api_key; retry delay"),
            None
        );
    }

    #[test]
    fn authentication_status_codes_support_provider_wire_spellings() {
        for status in [
            "401",
            "403",
            "HTTP401",
            "http: 403",
            "status code = 401",
            "{\"status\":403}",
        ] {
            let text = format!("{status}: 429 retry delay");
            assert_eq!(classify_failover(&text), None, "{text}");
            let error = crate::error::Error::api(text.clone());
            let message = errored_message(Some(&text), 0);
            assert!(!call_error_is_retryable(&error), "{text}");
            assert!(!error_result_is_retryable(&message, None), "{text}");
            assert_eq!(
                decide(
                    TurnOutcome::Failed(&error),
                    &progress(0, 0),
                    &policy(),
                    None
                ),
                TurnDecision::Finish { success: false },
                "{text}"
            );
        }
    }

    #[test]
    fn auth_refusal_wins_over_transient_words_on_both_outcome_shapes() {
        for text in [
            "401 unauthorized: 429 rate limit exceeded",
            "403 forbidden: service unavailable",
            "authentication_error: 503 overloaded",
            "invalid_api_key: connection reset by peer",
            "expired token: 500 server error",
            "invalid_token: upstream connect error",
            "permission_denied: 429 quota exceeded",
        ] {
            // Without the refusal guard, each message would spend the budget.
            assert!(crate::error::is_retryable_error(text, None, None), "{text}");
            assert_eq!(classify_failover(text), None, "{text}");
            let message = errored_message(Some(text), 0);
            assert!(!error_result_is_retryable(&message, None), "{text}");
            let error = crate::error::Error::api(text);
            assert!(!call_error_is_retryable(&error), "{text}");
            for outcome in [
                TurnOutcome::Completed(&message),
                TurnOutcome::Failed(&error),
            ] {
                assert_eq!(
                    decide(outcome, &progress(0, 0), &policy(), None),
                    TurnDecision::Finish { success: false },
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn repackaged_persistence_markers_remain_terminal_everywhere() {
        use crate::error::Error;

        for detail in [
            "503 connection reset by peer",
            "401 unauthorized: 429 quota",
        ] {
            let flattened = Error::session_persistence(detail).to_string();
            let errors = [
                Error::api(flattened.clone()),
                Error::provider("test", flattened.clone()),
                Error::session(format!("while committing turn: {flattened}")),
                Error::Io(Box::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    flattened.clone(),
                ))),
            ];
            assert_eq!(classify_failover(&flattened), None);
            for error in &errors {
                // None retains the original typed Session(prefix...) shape.
                assert!(!error.is_session_persistence());
                assert!(!call_error_is_retryable(error), "{error}");
                assert_eq!(classify_failover(&error.to_string()), None);
                assert_eq!(
                    decide(TurnOutcome::Failed(error), &progress(0, 0), &policy(), None),
                    TurnDecision::Terminal(TerminalReason::SessionPersistence),
                    "{error}"
                );
            }
            let message = errored_message(Some(&flattened), 0);
            assert!(!error_result_is_retryable(&message, None));
            assert_eq!(
                decide(
                    TurnOutcome::Completed(&message),
                    &progress(0, 0),
                    &policy(),
                    None
                ),
                TurnDecision::Terminal(TerminalReason::SessionPersistence)
            );
        }
    }

    #[test]
    fn explicit_context_overflow_is_not_failover_even_with_transient_statuses() {
        for text in [
            "503 service unavailable: prompt is too long",
            "429 rate limit: context_length_exceeded",
            "500 server error: too many tokens",
        ] {
            assert_eq!(classify_failover(text), None, "{text}");
            assert!(!error_result_is_retryable(
                &errored_message(Some(text), 0),
                None
            ));
        }
    }

    #[test]
    fn typed_transport_drops_still_retry_and_respect_both_budgets() {
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::TimedOut,
        ] {
            // Deliberately no transient substring: the typed kind must decide.
            let error = crate::error::Error::Io(Box::new(std::io::Error::new(kind, "wire")));
            assert!(call_error_is_retryable(&error), "{kind:?}");
            assert_eq!(
                decide(
                    TurnOutcome::Failed(&error),
                    &progress(0, 0),
                    &policy(),
                    None
                ),
                TurnDecision::Retry {
                    attempt: 1,
                    delay_ms: 500
                },
                "{kind:?}"
            );
            assert_eq!(
                decide(
                    TurnOutcome::Failed(&error),
                    &progress(2, 0),
                    &policy(),
                    None
                ),
                TurnDecision::FailOver,
                "{kind:?}"
            );
            assert_eq!(
                decide(
                    TurnOutcome::Failed(&error),
                    &progress(2, 1),
                    &policy(),
                    None
                ),
                TurnDecision::Finish { success: false },
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_silent_context_overflow_stops_retrying_only_once_the_window_is_known() {
        // A SILENT overflow: the provider's prose reads transient, and only
        // input tokens > context window reveals that the request can never fit.
        // This is the case the context window actually decides, and it is where
        // print mode and RPC diverge — RPC refuses the retry, print spends its
        // whole budget re-sending a prompt that cannot fit, billed each time.
        let silent = errored_message(Some("500 internal server error"), 250_000);
        let p = policy();
        assert_eq!(
            decide(
                TurnOutcome::Completed(&silent),
                &progress(0, 0),
                &p,
                Some(200_000)
            ),
            TurnDecision::FailOver,
            "a silent overflow must not burn the retry budget"
        );
        assert!(
            matches!(
                decide(TurnOutcome::Completed(&silent), &progress(0, 0), &p, None),
                TurnDecision::Retry { .. }
            ),
            "without the window the same message is indistinguishable from a 500"
        );
    }

    #[test]
    fn a_self_describing_overflow_needs_no_window() {
        // Text the provider spells out is caught either way, which is why the
        // print/RPC divergence went unnoticed: the common overflow says so.
        let spelled_out =
            errored_message(Some("prompt is too long: 250000 tokens > 200000"), 250_000);
        let p = policy();
        for window in [None, Some(200_000)] {
            assert_eq!(
                decide(
                    TurnOutcome::Completed(&spelled_out),
                    &progress(0, 0),
                    &p,
                    window
                ),
                TurnDecision::FailOver,
                "window={window:?}"
            );
        }
    }

    #[test]
    fn call_errors_classify_from_the_typed_error_before_its_prose() {
        let persistence = crate::error::Error::session_persistence("write failed");
        assert!(!call_error_is_retryable(&persistence));

        let transient = crate::error::Error::Api("503 service unavailable".to_string());
        assert!(call_error_is_retryable(&transient));

        let loud = crate::error::Error::Api("401 unauthorized: invalid api key".to_string());
        assert!(!call_error_is_retryable(&loud));
    }
}
