//! Extension manager registration, runtime, policy, and event orchestration.

// `ExtensionManager` remains defined in the parent façade so its public type
// identity and import path stay stable. This private module owns its inherent
// implementation and therefore needs the manager's complete private state
// graph; narrower leaf modules use explicit imports instead.
use super::*;
impl ExtensionManager {
    fn validate_extension_identity_table(
        registrations: &[RegisterPayload],
        extension_ids: &[String],
        source: &str,
    ) -> Result<()> {
        if registrations.len() != extension_ids.len() {
            return Err(Error::extension(format!(
                "{source} identity table mismatch: {} principals for {} registrations",
                extension_ids.len(),
                registrations.len()
            )));
        }
        let mut seen = HashSet::with_capacity(extension_ids.len());
        for extension_id in extension_ids {
            if extension_id.trim().is_empty() {
                return Err(Error::extension(format!(
                    "{source} produced an empty authoritative extension id"
                )));
            }
            if !seen.insert(extension_id) {
                return Err(Error::extension(format!(
                    "{source} produced duplicate authoritative extension id: {extension_id}"
                )));
            }
        }
        Ok(())
    }

    fn record_extension_version(
        extension_versions: &mut HashMap<String, String>,
        extension_id: &str,
        extension_name: &str,
        version: &str,
    ) {
        extension_versions.insert(extension_id.to_string(), version.to_string());
        if extension_name != extension_id {
            extension_versions
                .entry(extension_name.to_string())
                .or_insert_with(|| version.to_string());
        }
    }

    /// Default cleanup budget for extension shutdown.
    pub const DEFAULT_CLEANUP_BUDGET: Duration = Duration::from_secs(5);

    /// Create a new extension manager.
    ///
    /// Loads persisted permission decisions from disk (if any) and seeds the
    /// in-memory policy prompt cache so that "Allow Always" / "Deny Always"
    /// choices survive across sessions.
    pub fn new() -> Self {
        let mut inner = ExtensionManagerInner::default();
        Self::load_persisted_permissions(&mut inner);
        let snapshot = Arc::new(RwLock::new(Arc::new(RegistrySnapshot::default())));
        let snapshot_version = Arc::new(AtomicU64::new(0));
        Self {
            inner: Arc::new(Mutex::new(inner)),
            snapshot,
            snapshot_version,
        }
    }

    /// Create a new extension manager with a specific operation budget.
    pub fn with_budget(budget: Budget) -> Self {
        let mut inner = ExtensionManagerInner {
            extension_budget: budget,
            ..Default::default()
        };
        Self::load_persisted_permissions(&mut inner);
        let snapshot = Arc::new(RwLock::new(Arc::new(RegistrySnapshot::default())));
        let snapshot_version = Arc::new(AtomicU64::new(0));
        Self {
            inner: Arc::new(Mutex::new(inner)),
            snapshot,
            snapshot_version,
        }
    }

    /// Load persisted permission decisions into the inner state.
    fn load_persisted_permissions(inner: &mut ExtensionManagerInner) {
        let path = Config::permissions_path();
        Self::load_persisted_permissions_from(inner, &path);
    }

    pub(super) fn load_persisted_permissions_from(inner: &mut ExtensionManagerInner, path: &Path) {
        match PermissionStore::open(path) {
            Ok(store) => {
                // Seed the in-memory cache from persisted decisions.
                inner.policy_prompt_cache = store.to_decision_cache();
                inner.permission_store = Some(store);
            }
            Err(e) => {
                tracing::warn!("Failed to load extension permissions: {e}");
                inner.permission_store = Some(PermissionStore::empty_at(path));
            }
        }
    }

    // ── RCU snapshot helpers ───────────────────────────────────────────

    /// Build a `RegistrySnapshot` from the current inner state.
    ///
    /// Caller must already hold the mutex on `inner`.
    fn build_snapshot_from_inner(inner: &ExtensionManagerInner) -> RegistrySnapshot {
        // Pre-compute derived views so readers never touch the mutex.
        let all_flags = Self::precompute_all_flags(inner);
        let all_commands = Self::precompute_all_commands(inner);
        let (all_shortcuts, shortcut_key_ids) = Self::precompute_all_shortcuts(inner);
        let all_event_hooks = Self::precompute_all_event_hooks(inner);
        let all_tool_defs = Self::precompute_all_tool_defs(inner);
        let command_names = Self::precompute_command_names(inner);

        RegistrySnapshot {
            extension_count: inner.extensions.len(),
            hook_bitmap: inner.hook_bitmap.clone(),
            has_any_hooks: !inner.hook_bitmap.is_empty(),
            session: inner.session.clone(),
            active_tools: inner.active_tools.clone(),
            providers: inner.providers.clone(),
            mcp_servers: inner.mcp_servers.clone(),
            flags: inner.flags.clone(),
            cwd: inner.cwd.clone(),
            model_registry_values: inner.model_registry_values.clone(),
            current_provider: inner.current_provider.clone(),
            current_model_id: inner.current_model_id.clone(),
            current_thinking_level: inner.current_thinking_level.clone(),
            hostcall_compat_kill_switch_global: inner.hostcall_compat_kill_switch_global,
            hostcall_compat_kill_switch_extensions: inner
                .hostcall_compat_kill_switch_extensions
                .clone(),
            version: inner.ctx_generation,
            all_flags,
            all_commands,
            all_shortcuts,
            shortcut_key_ids,
            all_event_hooks,
            all_tool_defs,
            command_names,
            has_ui: inner.ui_sender.is_some(),
        }
    }

    /// Pre-compute the merged flag list from dynamic flags + extension payloads.
    fn precompute_all_flags(inner: &ExtensionManagerInner) -> Vec<Value> {
        let mut flags = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        // Dynamic flags take priority.
        for flag in &inner.flags {
            let name = flag.get("name").and_then(Value::as_str).unwrap_or_default();
            if !name.is_empty() {
                seen.insert(name);
                let description = flag
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let flag_type = flag.get("type").and_then(Value::as_str).unwrap_or("string");
                let extension_id = flag.get("extension_id").and_then(Value::as_str);
                flags.push(json!({
                    "name": name,
                    "description": description,
                    "type": flag_type,
                    "default": flag.get("default").cloned(),
                    "extension_id": extension_id,
                    "source": "extension",
                }));
            }
        }
        // Extension-payload flags (skip duplicates).
        for ext in &inner.extensions {
            for flag in &ext.flags {
                let name = flag.get("name").and_then(Value::as_str).unwrap_or_default();
                if !name.is_empty() && seen.insert(name) {
                    let description = flag
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let flag_type = flag.get("type").and_then(Value::as_str).unwrap_or("string");
                    flags.push(json!({
                        "name": name,
                        "description": description,
                        "type": flag_type,
                        "default": flag.get("default").cloned(),
                        "extension_id": ext.name,
                        "source": "extension",
                    }));
                }
            }
        }
        flags
    }

    /// Pre-compute slash command list from all extensions.
    fn precompute_all_commands(inner: &ExtensionManagerInner) -> Vec<Value> {
        let mut commands = Vec::new();
        for ext in &inner.extensions {
            for cmd in &ext.slash_commands {
                if is_non_callable_compat_inferred(cmd) {
                    continue;
                }
                let Some(name) = extract_slash_command_name(cmd) else {
                    continue;
                };
                let description = cmd.get("description").and_then(Value::as_str);
                commands.push(json!({
                    "name": name,
                    "description": description,
                    "source": "extension",
                }));
            }
        }
        commands
    }

    /// Pre-compute shortcut list and `key_id` set from all extensions.
    fn precompute_all_shortcuts(inner: &ExtensionManagerInner) -> (Vec<Value>, HashSet<String>) {
        let mut shortcuts = Vec::new();
        let mut key_ids = HashSet::new();
        for ext in &inner.extensions {
            for shortcut in &ext.shortcuts {
                let key_id = shortcut
                    .get("key_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let description = shortcut.get("description").and_then(Value::as_str);
                shortcuts.push(json!({
                    "shortcut": key_id,
                    "key_id": key_id,
                    "key": shortcut.get("key"),
                    "description": description,
                    "source": "extension",
                }));
                if !key_id.is_empty() {
                    key_ids.insert(key_id.to_lowercase());
                }
            }
        }
        (shortcuts, key_ids)
    }

    /// Pre-compute deduplicated event hook names from all extensions.
    fn precompute_all_event_hooks(inner: &ExtensionManagerInner) -> Vec<String> {
        let mut hooks = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for ext in &inner.extensions {
            for hook in &ext.event_hooks {
                if seen.insert(hook.as_str()) {
                    hooks.push(hook.clone());
                }
            }
        }
        hooks
    }

    /// Pre-compute tool definitions from all extensions (flat list).
    fn precompute_all_tool_defs(inner: &ExtensionManagerInner) -> Vec<Value> {
        inner
            .extensions
            .iter()
            .flat_map(|ext| {
                ext.tools
                    .iter()
                    .filter(|tool| !is_non_callable_compat_inferred(tool))
                    .cloned()
            })
            .collect()
    }

    /// Pre-compute normalized command names for O(1) `has_command()` lookup.
    fn precompute_command_names(inner: &ExtensionManagerInner) -> HashSet<String> {
        inner
            .extensions
            .iter()
            .flat_map(|ext| ext.slash_commands.iter())
            .filter(|cmd| !is_non_callable_compat_inferred(cmd))
            .filter_map(extract_slash_command_name)
            .map(|cmd| normalize_command(&cmd))
            .collect()
    }

    /// Atomically publish a new snapshot, replacing the old one.
    ///
    /// Previous readers keep their `Arc` alive until they drop it.
    fn publish_snapshot(&self, snap: RegistrySnapshot) {
        let version = snap.version;
        {
            let mut guard = match self.snapshot.write() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *guard = Arc::new(snap);
        }
        self.snapshot_version.store(version, StdOrdering::Release);
    }

    /// Grab the current snapshot without touching the mutex.
    ///
    /// Cost: one `RwLock::read()` (uncontended fast-path) + `Arc::clone`.
    pub(super) fn read_snapshot(&self) -> Arc<RegistrySnapshot> {
        let guard = match self.snapshot.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        Arc::clone(&guard)
    }

    /// Current snapshot version (seqlock counter).
    ///
    /// Cheap atomic load — useful for staleness checks without cloning.
    pub fn snapshot_version(&self) -> u64 {
        self.snapshot_version.load(StdOrdering::Acquire)
    }

    /// Rebuild and publish the snapshot from current inner state.
    ///
    /// Call this after any mutation to fields captured in `RegistrySnapshot`.
    /// Caller must already hold the mutex.
    #[allow(dead_code)]
    fn refresh_snapshot_locked(&self, inner: &ExtensionManagerInner) {
        let snap = Self::build_snapshot_from_inner(inner);
        self.publish_snapshot(snap);
    }

    /// Rebuild and publish the snapshot, releasing the mutex guard before
    /// publishing to avoid prolonging lock hold time.
    fn refresh_snapshot_with_guard_release(
        &self,
        guard: std::sync::MutexGuard<'_, ExtensionManagerInner>,
    ) {
        let snap = Self::build_snapshot_from_inner(&guard);
        drop(guard);
        self.publish_snapshot(snap);
    }

    /// Set the budget for extension operations.
    pub fn set_budget(&self, budget: Budget) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extension_budget = budget;
    }

    /// Get the current extension operation budget.
    pub fn budget(&self) -> Budget {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extension_budget
    }

    /// Create a `Cx` for extension operations using the configured budget.
    ///
    /// If a budget with constraints is set, returns a budget-constrained Cx.
    /// Otherwise returns a standard request-scoped Cx.
    pub fn extension_cx(&self) -> Cx {
        let budget = self.budget();
        if budget.deadline.is_some() || budget.poll_quota < u32::MAX || budget.cost_quota.is_some()
        {
            Cx::for_request_with_budget(budget)
        } else {
            Cx::for_request()
        }
    }

    /// Compute the effective timeout for an operation, taking the minimum of
    /// the per-operation timeout and the remaining manager-level budget deadline.
    ///
    /// When the manager has a constrained budget (e.g. during shutdown), this
    /// ensures individual operations don't outlast the overall budget.
    pub(super) fn effective_timeout(&self, operation_timeout_ms: u64) -> u64 {
        let budget = self.budget();
        budget.deadline.map_or(operation_timeout_ms, |deadline| {
            let now = wall_now();
            let remaining_ms = deadline.as_millis().saturating_sub(now.as_millis());
            operation_timeout_ms.min(remaining_ms)
        })
    }

    fn runtime_risk_extension_key(extension_id: Option<&str>) -> String {
        extension_id.unwrap_or("<unknown>").to_string()
    }

    pub(super) fn record_hostcall_marshalling_fallback_count(
        &self,
        extension_id: Option<&str>,
        fallback_reason: Option<&str>,
    ) -> u64 {
        let ext_key = Self::runtime_risk_extension_key(extension_id);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard
            .hostcall_marshalling_fallback_counts
            .entry(ext_key)
            .or_insert(0);
        if fallback_reason.is_some() {
            *entry = entry.saturating_add(1);
        }
        let result = *entry;
        drop(guard);
        result
    }

    fn runtime_risk_push_ledger(
        guard: &mut ExtensionManagerInner,
        mut entry: RuntimeRiskLedgerEntry,
    ) -> RuntimeRiskLedgerEntry {
        let prev_hash = guard.runtime_risk_last_hash.clone();
        entry.prev_ledger_hash.clone_from(&prev_hash);
        entry.ledger_hash = runtime_risk_compute_ledger_hash(&entry, prev_hash.as_deref());

        guard.runtime_risk_last_hash = Some(entry.ledger_hash.clone());
        guard.runtime_risk_ledger.push_back(entry.clone());
        while guard.runtime_risk_ledger.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.runtime_risk_ledger.pop_front();
        }
        entry
    }

    fn runtime_risk_push_telemetry(
        guard: &mut ExtensionManagerInner,
        entry: RuntimeHostcallTelemetryEvent,
    ) {
        guard.runtime_hostcall_telemetry.push_back(entry);
        while guard.runtime_hostcall_telemetry.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.runtime_hostcall_telemetry.pop_front();
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn set_runtime_risk_config(&self, config: RuntimeRiskConfig) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clamped = RuntimeRiskConfig {
            enabled: config.enabled,
            enforce: config.enforce,
            alpha: config.alpha.clamp(1.0e-6, 0.5),
            window_size: config.window_size.clamp(8, 4096),
            ledger_limit: config.ledger_limit.clamp(32, 20_000),
            decision_timeout_ms: config.decision_timeout_ms.clamp(1, 2_000),
            fail_closed: config.fail_closed,
        };
        guard.runtime_risk_config = clamped;
    }

    pub fn runtime_risk_config(&self) -> RuntimeRiskConfig {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime_risk_config
            .clone()
    }

    // ── SEC-7.2: Graduated enforcement rollout ──────────────────────────

    /// Get the current rollout phase.
    pub fn rollout_phase(&self) -> RolloutPhase {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rollout_tracker
            .phase
    }

    /// Set the rollout phase explicitly (operator override).
    pub fn set_rollout_phase(&self, phase: RolloutPhase) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.rollout_tracker.set_phase(phase);
        // Sync the `enforce` flag with the phase.
        guard.runtime_risk_config.enforce = phase.is_enforcing();
    }

    /// Advance the rollout to the next phase. Returns `true` if changed.
    pub fn advance_rollout(&self) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let advanced = guard.rollout_tracker.advance();
        if advanced {
            guard.runtime_risk_config.enforce = guard.rollout_tracker.phase.is_enforcing();
        }
        advanced
    }

    /// Record a risk decision for rollback trigger evaluation.
    /// Returns `true` if a rollback was triggered.
    pub fn record_rollout_decision(
        &self,
        latency_ms: u64,
        was_error: bool,
        was_false_positive: bool,
    ) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let triggered =
            guard
                .rollout_tracker
                .record_decision(latency_ms, was_error, was_false_positive);
        if triggered {
            guard.runtime_risk_config.enforce = false;
        }
        triggered
    }

    /// Configure the rollback trigger thresholds.
    pub fn set_rollback_trigger(&self, trigger: &RollbackTrigger) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Validate inputs to prevent misconfiguration that could silently
        // disable rollback triggers (NaN, 0 window, negative rates).
        guard.rollout_tracker.trigger = RollbackTrigger {
            max_false_positive_rate: if trigger.max_false_positive_rate.is_nan() {
                RollbackTrigger::default().max_false_positive_rate
            } else {
                trigger.max_false_positive_rate.clamp(0.0, 1.0)
            },
            max_error_rate: if trigger.max_error_rate.is_nan() {
                RollbackTrigger::default().max_error_rate
            } else {
                trigger.max_error_rate.clamp(0.0, 1.0)
            },
            window_size: trigger.window_size.clamp(10, 10_000),
            max_latency_ms: trigger.max_latency_ms.max(1),
        };
    }

    /// Get a snapshot of the current rollout state for operator inspection.
    pub fn rollout_state(&self) -> RolloutState {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let phase = guard.rollout_tracker.phase;
        let enforce = guard.runtime_risk_config.enforce;
        let enabled = guard.runtime_risk_config.enabled;
        let last_transition_ms = guard.rollout_tracker.last_transition_ms;
        let transition_count = guard.rollout_tracker.transition_count;
        let rolled_back_from = guard.rollout_tracker.rolled_back_from;
        let window_stats = guard.rollout_tracker.window_stats();
        drop(guard);
        RolloutState {
            phase,
            enforce,
            enabled,
            last_transition_ms,
            transition_count,
            rolled_back_from,
            window_stats,
        }
    }

    // ── SEC-4.1: Per-extension resource quota check ──────────────────────

    /// Check per-extension resource quotas before dispatching a hostcall.
    /// Returns [`QuotaCheckResult::Exceeded`] if any configured limit is breached.
    ///
    /// Quota config resolution: per-extension override (from policy) > global default.
    pub(super) fn check_quota(
        &self,
        extension_id: Option<&str>,
        capability: &str,
        now_ms: i64,
        policy: &ExtensionPolicy,
    ) -> QuotaCheckResult {
        let Some(ext_id) = extension_id else {
            return QuotaCheckResult::Allowed;
        };
        let Ok(mut guard) = self.inner.lock() else {
            return QuotaCheckResult::Allowed;
        };

        // Resolve quota config: per-extension override > global default.
        let quota_config = policy
            .per_extension
            .get(ext_id)
            .and_then(|ovr| ovr.quota.as_ref())
            .cloned()
            .unwrap_or_else(|| guard.quota_config.clone());

        let state = guard.quota_states.entry(ext_id.to_string()).or_default();

        let result = check_extension_quota(&quota_config, state, now_ms, capability);

        // Record breach telemetry if quota was exceeded.
        if let QuotaCheckResult::Exceeded { ref reason } = result {
            guard.quota_breach_events.push_back(QuotaBreachEvent {
                ts_ms: now_ms,
                extension_id: ext_id.to_string(),
                capability: capability.to_string(),
                reason: reason.clone(),
                quota_config_source: if policy
                    .per_extension
                    .get(ext_id)
                    .and_then(|ovr| ovr.quota.as_ref())
                    .is_some()
                {
                    "per_extension"
                } else {
                    "global"
                }
                .to_string(),
            });
        }

        result
    }

    /// Record subprocess spawn (increments active subprocess counter).
    pub fn record_subprocess_spawn(&self, extension_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            let state = guard
                .quota_states
                .entry(extension_id.to_string())
                .or_default();
            state.active_subprocesses += 1;
        }
    }

    /// Record subprocess exit (decrements active subprocess counter).
    pub fn record_subprocess_exit(&self, extension_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            let state = guard
                .quota_states
                .entry(extension_id.to_string())
                .or_default();
            state.active_subprocesses = state.active_subprocesses.saturating_sub(1);
        }
    }

    /// Record bytes written by an extension (for write quota tracking).
    pub fn record_write_bytes(&self, extension_id: &str, bytes: u64) {
        if let Ok(mut guard) = self.inner.lock() {
            let state = guard
                .quota_states
                .entry(extension_id.to_string())
                .or_default();
            state.write_bytes_total = state.write_bytes_total.saturating_add(bytes);
        }
    }

    /// Get the current quota state for an extension (for telemetry/inspection).
    pub fn quota_state(&self, extension_id: &str) -> Option<(u64, u32, u64, u64)> {
        let guard = self.inner.lock().ok()?;
        guard.quota_states.get(extension_id).map(|s| {
            (
                s.hostcalls_total,
                s.active_subprocesses,
                s.write_bytes_total,
                s.http_requests_total,
            )
        })
    }

    /// Update the budget-controller configuration.
    #[allow(clippy::too_many_lines)]
    pub fn set_budget_controller_config(&self, config: ExtensionBudgetControllerConfig) {
        if let Ok(mut guard) = self.inner.lock() {
            let mut clamped = config;
            let default_regime_shift = RegimeShiftConfig::for_tier(clamped.tier);
            let default_safety = SafetyEnvelopeConfig::for_tier(clamped.tier);
            let default_oco = OcoTunerConfig::for_tier(clamped.tier);

            clamped.overload_window_ms = clamped.overload_window_ms.max(100);
            clamped.overload_signals_to_fallback = clamped.overload_signals_to_fallback.max(1);
            clamped.recovery_successes_to_exit = clamped.recovery_successes_to_exit.max(1);

            if !clamped.regime_shift.cusum_k.is_finite() || clamped.regime_shift.cusum_k <= 0.0 {
                clamped.regime_shift.cusum_k = default_regime_shift.cusum_k;
            }
            if !clamped.regime_shift.cusum_h.is_finite() || clamped.regime_shift.cusum_h <= 0.0 {
                clamped.regime_shift.cusum_h = default_regime_shift.cusum_h;
            }
            if !clamped.regime_shift.bocpd_lambda.is_finite()
                || clamped.regime_shift.bocpd_lambda <= 0.0
            {
                clamped.regime_shift.bocpd_lambda = default_regime_shift.bocpd_lambda;
            }
            clamped.regime_shift.bocpd_threshold =
                if clamped.regime_shift.bocpd_threshold.is_finite() {
                    clamped.regime_shift.bocpd_threshold.clamp(0.01, 0.99)
                } else {
                    default_regime_shift.bocpd_threshold
                };
            clamped.regime_shift.bocpd_max_run_length =
                clamped.regime_shift.bocpd_max_run_length.clamp(8, 10_000);

            clamped.safety_envelope.conformal_confidence =
                if clamped.safety_envelope.conformal_confidence.is_finite() {
                    clamped
                        .safety_envelope
                        .conformal_confidence
                        .clamp(0.5, 0.999)
                } else {
                    default_safety.conformal_confidence
                };
            clamped.safety_envelope.conformal_calibration_size = clamped
                .safety_envelope
                .conformal_calibration_size
                .clamp(16, 10_000);
            clamped.safety_envelope.pac_bayes_delta =
                if clamped.safety_envelope.pac_bayes_delta.is_finite() {
                    clamped.safety_envelope.pac_bayes_delta.clamp(1.0e-6, 0.5)
                } else {
                    default_safety.pac_bayes_delta
                };
            clamped.safety_envelope.pac_bayes_prior_weight =
                if clamped.safety_envelope.pac_bayes_prior_weight.is_finite() {
                    clamped
                        .safety_envelope
                        .pac_bayes_prior_weight
                        .clamp(0.01, 100.0)
                } else {
                    default_safety.pac_bayes_prior_weight
                };
            clamped.safety_envelope.safety_error_threshold =
                if clamped.safety_envelope.safety_error_threshold.is_finite() {
                    clamped
                        .safety_envelope
                        .safety_error_threshold
                        .clamp(0.0, 1.0)
                } else {
                    default_safety.safety_error_threshold
                };
            clamped.safety_envelope.min_observations =
                clamped.safety_envelope.min_observations.max(1);

            if !clamped.oco_tuner.learning_rate.is_finite()
                || clamped.oco_tuner.learning_rate <= 0.0
            {
                clamped.oco_tuner.learning_rate = default_oco.learning_rate;
            }
            clamped.oco_tuner.learning_rate = clamped.oco_tuner.learning_rate.clamp(1.0e-4, 1.0);

            if !clamped.oco_tuner.min_queue_budget.is_finite() {
                clamped.oco_tuner.min_queue_budget = default_oco.min_queue_budget;
            }
            if !clamped.oco_tuner.max_queue_budget.is_finite() {
                clamped.oco_tuner.max_queue_budget = default_oco.max_queue_budget;
            }
            if clamped.oco_tuner.min_queue_budget <= 0.0 {
                clamped.oco_tuner.min_queue_budget = default_oco.min_queue_budget;
            }
            if clamped.oco_tuner.max_queue_budget < clamped.oco_tuner.min_queue_budget {
                clamped.oco_tuner.max_queue_budget = clamped.oco_tuner.min_queue_budget;
            }

            if !clamped.oco_tuner.min_batch_budget.is_finite() {
                clamped.oco_tuner.min_batch_budget = default_oco.min_batch_budget;
            }
            if !clamped.oco_tuner.max_batch_budget.is_finite() {
                clamped.oco_tuner.max_batch_budget = default_oco.max_batch_budget;
            }
            if clamped.oco_tuner.min_batch_budget <= 0.0 {
                clamped.oco_tuner.min_batch_budget = default_oco.min_batch_budget;
            }
            if clamped.oco_tuner.max_batch_budget < clamped.oco_tuner.min_batch_budget {
                clamped.oco_tuner.max_batch_budget = clamped.oco_tuner.min_batch_budget;
            }

            if !clamped.oco_tuner.min_time_slice_ms.is_finite() {
                clamped.oco_tuner.min_time_slice_ms = default_oco.min_time_slice_ms;
            }
            if !clamped.oco_tuner.max_time_slice_ms.is_finite() {
                clamped.oco_tuner.max_time_slice_ms = default_oco.max_time_slice_ms;
            }
            if clamped.oco_tuner.min_time_slice_ms <= 0.0 {
                clamped.oco_tuner.min_time_slice_ms = default_oco.min_time_slice_ms;
            }
            if clamped.oco_tuner.max_time_slice_ms < clamped.oco_tuner.min_time_slice_ms {
                clamped.oco_tuner.max_time_slice_ms = clamped.oco_tuner.min_time_slice_ms;
            }

            clamped.oco_tuner.initial_queue_budget =
                if clamped.oco_tuner.initial_queue_budget.is_finite() {
                    clamped.oco_tuner.initial_queue_budget
                } else {
                    default_oco.initial_queue_budget
                }
                .clamp(
                    clamped.oco_tuner.min_queue_budget,
                    clamped.oco_tuner.max_queue_budget,
                );
            clamped.oco_tuner.initial_batch_budget =
                if clamped.oco_tuner.initial_batch_budget.is_finite() {
                    clamped.oco_tuner.initial_batch_budget
                } else {
                    default_oco.initial_batch_budget
                }
                .clamp(
                    clamped.oco_tuner.min_batch_budget,
                    clamped.oco_tuner.max_batch_budget,
                );
            clamped.oco_tuner.initial_time_slice_ms =
                if clamped.oco_tuner.initial_time_slice_ms.is_finite() {
                    clamped.oco_tuner.initial_time_slice_ms
                } else {
                    default_oco.initial_time_slice_ms
                }
                .clamp(
                    clamped.oco_tuner.min_time_slice_ms,
                    clamped.oco_tuner.max_time_slice_ms,
                );
            clamped.oco_tuner.rollback_loss_threshold =
                if clamped.oco_tuner.rollback_loss_threshold.is_finite() {
                    clamped.oco_tuner.rollback_loss_threshold.clamp(0.1, 10.0)
                } else {
                    default_oco.rollback_loss_threshold
                };

            if !clamped.enabled || !clamped.safety_envelope.enabled {
                for state in guard.budget_fallback_states.values_mut() {
                    state.safety_envelope.clear_veto();
                }
            }

            guard.budget_controller_config = clamped;
        }
    }

    /// Snapshot the budget-controller configuration.
    pub fn budget_controller_config(&self) -> ExtensionBudgetControllerConfig {
        self.inner
            .lock()
            .map(|guard| guard.budget_controller_config.clone())
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn record_budget_overload_signal(
        &self,
        extension_id: Option<&str>,
        reason: &str,
        queue_depth: Option<usize>,
        queue_capacity: Option<usize>,
    ) {
        let Some(ext_id) = extension_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return;
        };
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        let config = guard.budget_controller_config.clone();
        if !config.enabled {
            return;
        }

        let state = guard
            .budget_fallback_states
            .entry(ext_id.to_string())
            .or_default();
        if config.oco_tuner.enabled && state.oco_tuner.is_none() {
            state.oco_tuner = Some(OcoTunerState::from_config(&config.oco_tuner));
        }
        let now_ms = runtime_risk_now_ms();
        let horizon = now_ms.saturating_sub(i64::try_from(config.overload_window_ms).unwrap_or(0));
        while state
            .overload_timestamps_ms
            .front()
            .is_some_and(|ts| *ts < horizon)
        {
            let _ = state.overload_timestamps_ms.pop_front();
        }

        // Feed regime-shift detectors with inter-arrival interval.
        #[allow(clippy::cast_precision_loss)]
        let regime_shift_triggered = if config.regime_shift.enabled {
            let interval_ms = state
                .regime_shift
                .cusum
                .last_observation_ms
                .map_or(0.0, |prev| (now_ms - prev) as f64);
            state.regime_shift.cusum.last_observation_ms = Some(now_ms);

            let cusum_alarm = state.regime_shift.cusum.observe(
                interval_ms,
                config.regime_shift.cusum_k,
                config.regime_shift.cusum_h,
            );
            let bocpd_alarm = state.regime_shift.bocpd.observe(
                interval_ms,
                config.regime_shift.bocpd_lambda,
                config.regime_shift.bocpd_threshold,
                config.regime_shift.bocpd_max_run_length,
            );

            if cusum_alarm || bocpd_alarm {
                let source = if cusum_alarm { "cusum" } else { "bocpd" };
                state.regime_shift.triggered = true;
                state.regime_shift.trigger_source = Some(source);
                state.regime_shift.trigger_count += 1;
                true
            } else {
                false
            }
        } else {
            false
        };

        // Feed safety envelope with the overload signal (failure, latency = inter-arrival).
        #[allow(clippy::cast_precision_loss)]
        let safety_veto = if config.safety_envelope.enabled {
            let latency_proxy = state
                .regime_shift
                .cusum
                .last_observation_ms
                .map_or(0.0, |prev| (now_ms - prev) as f64);
            state
                .safety_envelope
                .evaluate(latency_proxy, false, &config.safety_envelope)
        } else {
            false
        };

        state.overload_timestamps_ms.push_back(now_ms);
        state.healthy_success_streak = 0;
        state.last_trigger_reason = Some(reason.to_string());
        let oco_update = state
            .oco_tuner
            .as_mut()
            .filter(|_| config.oco_tuner.enabled)
            .map(|state| state.update(true, queue_depth, queue_capacity, &config.oco_tuner));
        let adaptive_threshold = state
            .oco_tuner
            .as_ref()
            .filter(|_| config.oco_tuner.enabled)
            .map_or_else(
                || config.overload_signals_to_fallback.max(1),
                |state| state.adaptive_overload_threshold(config.overload_signals_to_fallback),
            );

        let signal_count = u32::try_from(state.overload_timestamps_ms.len()).unwrap_or(u32::MAX);
        let utilization_pct = if adaptive_threshold == 0 {
            0.0
        } else {
            (f64::from(signal_count) / f64::from(adaptive_threshold)) * 100.0
        };

        // Enter fallback if the classic signal count threshold is met,
        // the regime-shift detector fires, OR the safety envelope vetoes.
        let count_trigger = signal_count >= adaptive_threshold;
        let oco_guardrail_triggered = oco_update.is_some_and(|update| update.rolled_back);
        if !state.in_fallback
            && (count_trigger || regime_shift_triggered || safety_veto || oco_guardrail_triggered)
        {
            let trigger_kind = if safety_veto && !count_trigger && !regime_shift_triggered {
                state
                    .safety_envelope
                    .veto_reason
                    .unwrap_or("safety_envelope")
            } else if oco_guardrail_triggered && !count_trigger && !regime_shift_triggered {
                "oco_guardrail"
            } else if regime_shift_triggered && !count_trigger {
                state.regime_shift.trigger_source.unwrap_or("regime_shift")
            } else {
                "count_threshold"
            };
            let oco_snapshot = state.oco_tuner.as_ref().map(OcoTunerState::snapshot);
            state.in_fallback = true;
            tracing::warn!(
                event = "host_call.budget_controller.fallback_entered",
                extension_id = %ext_id,
                budget_tier = config.tier.as_str(),
                trigger_reason = %reason,
                trigger_kind,
                overload_signal_count = signal_count,
                overload_signal_threshold = adaptive_threshold,
                overload_signal_threshold_base = config.overload_signals_to_fallback,
                overload_window_ms = config.overload_window_ms,
                recovery_successes_to_exit = config.recovery_successes_to_exit,
                queue_depth,
                queue_capacity,
                overload_utilization_pct = utilization_pct,
                regime_shift_triggered,
                safety_veto,
                oco_guardrail_triggered,
                oco_enabled = config.oco_tuner.enabled,
                oco_queue_budget = ?oco_snapshot.as_ref().map(|s| s.queue_budget),
                oco_batch_budget = ?oco_snapshot.as_ref().map(|s| s.batch_budget),
                oco_time_slice_ms = ?oco_snapshot.as_ref().map(|s| s.time_slice_ms),
                oco_cumulative_regret = ?oco_snapshot.as_ref().map(|s| s.cumulative_regret),
                oco_instantaneous_loss = ?oco_update.as_ref().map(|u| u.instantaneous_loss),
                oco_update_cumulative_regret = ?oco_update.as_ref().map(|u| u.cumulative_regret),
                oco_guardrail_rollbacks = ?oco_snapshot.as_ref().map(|s| s.guardrail_rollbacks),
                fallback_lane = "compat",
                "Budget controller entered compatibility fallback mode"
            );
            return;
        }

        let oco_snapshot = state.oco_tuner.as_ref().map(OcoTunerState::snapshot);
        tracing::debug!(
            event = "host_call.budget_controller.signal",
            extension_id = %ext_id,
            budget_tier = config.tier.as_str(),
            trigger_reason = %reason,
            overload_signal_count = signal_count,
            overload_signal_threshold = adaptive_threshold,
            overload_signal_threshold_base = config.overload_signals_to_fallback,
            overload_window_ms = config.overload_window_ms,
            queue_depth,
            queue_capacity,
            overload_utilization_pct = utilization_pct,
            fallback_active = state.in_fallback,
            regime_shift_triggered,
            safety_veto,
            oco_enabled = config.oco_tuner.enabled,
            oco_queue_budget = ?oco_snapshot.as_ref().map(|s| s.queue_budget),
            oco_batch_budget = ?oco_snapshot.as_ref().map(|s| s.batch_budget),
            oco_time_slice_ms = ?oco_snapshot.as_ref().map(|s| s.time_slice_ms),
            oco_cumulative_regret = ?oco_snapshot.as_ref().map(|s| s.cumulative_regret),
            oco_instantaneous_loss = ?oco_update.as_ref().map(|u| u.instantaneous_loss),
            oco_update_cumulative_regret = ?oco_update.as_ref().map(|u| u.cumulative_regret),
            oco_guardrail_rollbacks = ?oco_snapshot.as_ref().map(|s| s.guardrail_rollbacks),
            "Budget controller recorded overload/anomaly signal"
        );
    }

    pub(super) fn record_budget_recovery_sample(&self, extension_id: Option<&str>, success: bool) {
        let Some(ext_id) = extension_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return;
        };
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        let config = guard.budget_controller_config.clone();
        if !config.enabled {
            return;
        }
        let Some(state) = guard.budget_fallback_states.get_mut(ext_id) else {
            return;
        };
        if !state.in_fallback {
            return;
        }

        let oco_update = state
            .oco_tuner
            .as_mut()
            .filter(|_| config.oco_tuner.enabled)
            .map(|state| state.update(!success, None, None, &config.oco_tuner));

        // Feed recovery outcome to the safety envelope (latency=0 for success).
        state
            .safety_envelope
            .evaluate(0.0, success, &config.safety_envelope);

        if !success {
            state.healthy_success_streak = 0;
            return;
        }

        state.healthy_success_streak = state.healthy_success_streak.saturating_add(1);
        if state.healthy_success_streak < config.recovery_successes_to_exit {
            return;
        }

        state.in_fallback = false;
        state.healthy_success_streak = 0;
        state.overload_timestamps_ms.clear();
        // Reset regime-shift detectors so the next regime starts fresh.
        state.regime_shift.cusum.reset_cumsum();
        state.regime_shift.bocpd.reset();
        state.regime_shift.triggered = false;
        state.regime_shift.trigger_source = None;
        // Reset safety envelope so the next regime starts fresh.
        state.safety_envelope.reset();
        let oco_snapshot = state.oco_tuner.as_ref().map(OcoTunerState::snapshot);
        tracing::info!(
            event = "host_call.budget_controller.recovered",
            extension_id = %ext_id,
            budget_tier = config.tier.as_str(),
            recovery_successes = config.recovery_successes_to_exit,
            oco_enabled = config.oco_tuner.enabled,
            oco_queue_budget = ?oco_snapshot.as_ref().map(|s| s.queue_budget),
            oco_batch_budget = ?oco_snapshot.as_ref().map(|s| s.batch_budget),
            oco_time_slice_ms = ?oco_snapshot.as_ref().map(|s| s.time_slice_ms),
            oco_cumulative_regret = ?oco_snapshot.as_ref().map(|s| s.cumulative_regret),
            oco_instantaneous_loss = ?oco_update.as_ref().map(|u| u.instantaneous_loss),
            oco_update_cumulative_regret = ?oco_update.as_ref().map(|u| u.cumulative_regret),
            fallback_lane = "fast",
            "Budget controller exited compatibility fallback mode"
        );
    }

    /// Snapshot the regime-shift detector state for an extension.
    pub fn regime_shift_snapshot(&self, extension_id: &str) -> Option<RegimeShiftSnapshot> {
        let guard = self.inner.lock().ok()?;
        guard
            .budget_fallback_states
            .get(extension_id)
            .map(|state| state.regime_shift.snapshot())
    }

    /// Check if any extension has an active safety envelope veto.
    ///
    /// When any extension is in a vetoed state, aggressive optimization
    /// (e.g. AMAC interleaving) should be disabled to remain conservative.
    #[must_use]
    pub fn any_safety_envelope_vetoing(&self) -> bool {
        let Ok(guard) = self.inner.lock() else {
            return false;
        };
        guard
            .budget_fallback_states
            .values()
            .any(|state| state.safety_envelope.vetoing)
    }

    /// Snapshot the safety envelope state for an extension.
    pub fn safety_envelope_snapshot(&self, extension_id: &str) -> Option<SafetyEnvelopeSnapshot> {
        let guard = self.inner.lock().ok()?;
        let config = &guard.budget_controller_config;
        guard
            .budget_fallback_states
            .get(extension_id)
            .map(|state| state.safety_envelope.snapshot(&config.safety_envelope))
    }

    /// Snapshot OCO tuner state for an extension.
    pub fn oco_tuner_snapshot(&self, extension_id: &str) -> Option<OcoTunerSnapshot> {
        let guard = self.inner.lock().ok()?;
        guard
            .budget_fallback_states
            .get(extension_id)
            .and_then(|state| state.oco_tuner.as_ref().map(OcoTunerState::snapshot))
    }

    #[cfg(test)]
    pub(super) fn budget_fallback_state_snapshot(
        &self,
        extension_id: &str,
    ) -> Option<(bool, u32, usize, Option<String>)> {
        let guard = self.inner.lock().ok()?;
        guard.budget_fallback_states.get(extension_id).map(|state| {
            (
                state.in_fallback,
                state.healthy_success_streak,
                state.overload_timestamps_ms.len(),
                state.last_trigger_reason.clone(),
            )
        })
    }

    /// Update the global quota configuration.
    pub fn set_quota_config(&self, config: ExtensionQuotaConfig) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.quota_config = config;
        }
    }

    /// Drain and return all quota breach telemetry events.
    pub fn drain_quota_breach_events(&self) -> Vec<QuotaBreachEvent> {
        self.inner.lock().ok().map_or_else(Vec::new, |mut guard| {
            guard.quota_breach_events.drain(..).collect()
        })
    }

    /// Get the count of recorded quota breach events (for inspection).
    pub fn quota_breach_count(&self) -> usize {
        self.inner
            .lock()
            .ok()
            .map_or(0, |guard| guard.quota_breach_events.len())
    }

    /// Reset quota counters for a specific extension (e.g. on extension reload).
    /// The sliding window timestamps and monotonic counters are cleared.
    pub fn reset_quota_state(&self, extension_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.quota_states.remove(extension_id);
        }
    }

    // ── Replay trace integration ────────────────────────────────────

    /// Enable replay trace recording with the given budget/config.
    pub fn enable_replay(&self, config: crate::extension_replay::ReplayLaneConfig) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.replay_config = Some(config);
        }
    }

    /// Disable replay trace recording.
    pub fn disable_replay(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.replay_config = None;
        }
    }

    /// Store a completed replay trace bundle from a dispatch cycle.
    pub fn store_replay_bundle(&self, bundle: crate::extension_replay::ReplayTraceBundle) {
        if let Ok(mut guard) = self.inner.lock() {
            // Keep at most 64 recent bundles to bound memory.
            while guard.replay_bundles.len() >= 64 {
                guard.replay_bundles.pop_front();
            }
            guard.replay_bundles.push_back(bundle);
        }
    }

    /// Drain and return all stored replay trace bundles.
    pub fn drain_replay_bundles(&self) -> Vec<crate::extension_replay::ReplayTraceBundle> {
        self.inner.lock().ok().map_or_else(Vec::new, |mut guard| {
            guard.replay_bundles.drain(..).collect()
        })
    }

    /// Get the current replay lane config (if enabled).
    #[must_use]
    pub fn replay_config(&self) -> Option<crate::extension_replay::ReplayLaneConfig> {
        self.inner
            .lock()
            .ok()
            .and_then(|guard| guard.replay_config.clone())
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::significant_drop_tightening,
        clippy::cast_precision_loss,
        clippy::suboptimal_flops
    )]
    pub(super) fn evaluate_runtime_risk(
        &self,
        extension_id: Option<&str>,
        _call_id: &str,
        capability: &str,
        method: &str,
        params_hash: &str,
        meta: RuntimeRiskCallMetadata<'_>,
        policy_reason: &str,
    ) -> Option<RuntimeRiskDecision> {
        let started = Instant::now();
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = guard.runtime_risk_config.clone();
        if !config.enabled {
            return None;
        }

        let ext_key = Self::runtime_risk_extension_key(extension_id);
        let state = guard.runtime_risk_states.entry(ext_key).or_default();

        let now_ms = runtime_risk_now_ms();
        let sequence_context = runtime_hostcall_sequence_context(state, now_ms);
        let argument_signals = runtime_hostcall_argument_signals(
            capability,
            method,
            meta.params,
            meta.resource_target_class,
        );
        let base = runtime_risk_clamp01(
            runtime_risk_base_score(capability, method, policy_reason)
                + argument_signals.risk_delta,
        );
        let recent_mean = if state.recent_scores.is_empty() {
            0.0
        } else {
            state.recent_scores.iter().sum::<f64>() / state.recent_scores.len() as f64
        };

        let feature_started = Instant::now();
        let features = runtime_hostcall_extract_features(
            base,
            recent_mean,
            &sequence_context,
            capability,
            policy_reason,
            meta.timeout_ms,
        );
        let feature_extraction_latency_us =
            u64::try_from(feature_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let feature_budget_exceeded =
            feature_extraction_latency_us > RUNTIME_HOSTCALL_FEATURE_BUDGET_US;

        if state.quarantined {
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let mut triggers = vec!["quarantined".to_string()];
            if feature_budget_exceeded {
                triggers.push("feature_budget_exceeded".to_string());
            }
            let posterior = RuntimeRiskPosterior {
                safe_fast: 0.0,
                suspicious: 0.0,
                unsafe_: 1.0,
            };
            let expected_loss = RuntimeRiskExpectedLoss {
                allow: 120.0,
                harden: 35.0,
                deny: 2.0,
                terminate: 1.0,
            };
            let (explanation_level, explanation_summary, top_contributors, budget_state) =
                runtime_risk_build_explanation(
                    RuntimeRiskAction::Terminate,
                    1.0,
                    &posterior,
                    &expected_loss,
                    &features,
                    &triggers,
                    None,
                    RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
                    RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
                );
            return Some(RuntimeRiskDecision {
                action: RuntimeRiskAction::Terminate,
                reason: "quarantined".to_string(),
                capability: capability.to_string(),
                method: method.to_string(),
                params_hash: params_hash.to_string(),
                args_shape_hash: meta.args_shape_hash.to_string(),
                resource_target_class: meta.resource_target_class.to_string(),
                policy_profile: meta.policy_profile.to_string(),
                timeout_ms: meta.timeout_ms,
                risk_score: 1.0,
                posterior,
                expected_loss,
                e_process: f64::INFINITY,
                e_threshold: 1.0 / config.alpha,
                conformal_residual: 1.0,
                conformal_quantile: state.previous_residual_quantile,
                drift_detected: true,
                triggers,
                explanation_schema: RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION.to_string(),
                explanation_level,
                explanation_summary,
                top_contributors,
                budget_state,
                fallback_reason: None,
                elapsed_ms,
                state_label: RuntimeRiskStateLabel::Unsafe,
                sequence_context,
                features,
                feature_extraction_latency_us,
                feature_budget_exceeded,
            });
        }

        let mut risk_score = runtime_risk_clamp01((0.50 * base) + (0.30 * recent_mean));
        risk_score = runtime_risk_clamp01(
            risk_score
                + (0.12 * features.recent_error_rate)
                + (0.08 * features.burst_density_1s)
                + (0.05 * features.prior_failure_streak_norm),
        );
        if runtime_risk_is_dangerous(capability)
            && matches!(state.last_decision, Some(RuntimeRiskAction::Harden))
        {
            let escalation_bonus = if argument_signals.risk_delta >= 0.18 {
                0.10
            } else {
                0.02
            };
            risk_score = runtime_risk_clamp01(risk_score + escalation_bonus);
        }

        state.recent_scores.push_back(risk_score);
        while state.recent_scores.len() > config.window_size {
            let _ = state.recent_scores.pop_front();
        }

        // Soft Bayesian evidence update.
        let safe_evidence = (1.0 - risk_score).max(0.05);
        let suspicious_evidence = (risk_score * 0.9).max(0.01);
        let unsafe_evidence = if runtime_risk_is_dangerous(capability) {
            (risk_score * 0.8).max(0.01)
        } else {
            (risk_score * 0.35).max(0.01)
        };

        state.alpha_safe += safe_evidence;
        state.alpha_suspicious += suspicious_evidence;
        state.alpha_unsafe += unsafe_evidence;

        let denom = state.alpha_safe + state.alpha_suspicious + state.alpha_unsafe;
        let posterior = RuntimeRiskPosterior {
            safe_fast: runtime_risk_clamp01(state.alpha_safe / denom),
            suspicious: runtime_risk_clamp01(state.alpha_suspicious / denom),
            unsafe_: runtime_risk_clamp01(state.alpha_unsafe / denom),
        };

        // Anytime-valid sequential evidence (likelihood-ratio style e-process).
        let x = if risk_score >= 0.65 { 1.0 } else { 0.0 };
        let p0: f64 = 0.10;
        let p1: f64 = 0.45;
        let log_lr = if x > 0.5 {
            f64::ln(p1 / p0)
        } else {
            f64::ln((1.0 - p1) / (1.0 - p0))
        };
        state.log_e_process = (state.log_e_process + log_lr).clamp(-120.0, 120.0);
        let e_process = state.log_e_process.exp();
        let e_threshold = 1.0 / config.alpha;
        let e_process_breach = e_process >= e_threshold;

        // BOCPD-lite drift: compare first/second half moving means.
        let mut drift_detected = false;
        if state.recent_scores.len() >= config.window_size / 2 {
            let len = state.recent_scores.len();
            let split = len / 2;
            if split > 0 {
                let first_mean = state.recent_scores.iter().take(split).sum::<f64>() / split as f64;
                let second_mean =
                    state.recent_scores.iter().skip(split).sum::<f64>() / (len - split) as f64;
                drift_detected = (second_mean - first_mean).abs() >= 0.22;
            }
        }

        let conformal_residual = (risk_score - recent_mean).abs();
        let conformal_quantile = if state.residual_window.is_empty() {
            state.previous_residual_quantile
        } else {
            runtime_risk_quantile(
                state.residual_window.iter().copied().collect(),
                1.0 - config.alpha,
            )
        };
        if conformal_quantile > 0.0 && conformal_residual > conformal_quantile * 1.5 {
            drift_detected = true;
        }

        let (mut action, expected_loss, mut triggers, state_label) =
            runtime_risk_choose_action(&posterior, e_process_breach, drift_detected);

        if state.consecutive_unsafe >= 3 && posterior.unsafe_ >= 0.45 {
            action = RuntimeRiskAction::Terminate;
            triggers.push("unsafe_streak".to_string());
        }
        if feature_budget_exceeded {
            triggers.push("feature_budget_exceeded".to_string());
        }

        // SEC-3.3: Deterministic reason codes for specific feature anomalies.
        if features.burst_density_1s >= 0.5 {
            triggers.push("burst_rate_anomaly".to_string());
        }
        if features.recent_error_rate >= 0.4 {
            triggers.push("high_error_rate".to_string());
        }
        if features.prior_failure_streak_norm >= 0.25 {
            triggers.push("consecutive_failure_escalation".to_string());
        }
        if argument_signals.has(ARG_FLAG_SUSPICIOUS_EXEC) {
            triggers.push("suspicious_exec_detail".to_string());
        }
        if argument_signals.has(ARG_FLAG_DCG_PATTERN_HIT) {
            triggers.push("dcg_rule_hit".to_string());
        }
        if argument_signals.has(ARG_FLAG_DCG_HEREDOC_HIT) {
            triggers.push("dcg_heredoc_hit".to_string());
        }
        if argument_signals.has(ARG_FLAG_SENSITIVE_PATH) {
            triggers.push("sensitive_path_target".to_string());
        }
        if argument_signals.has(ARG_FLAG_PUBLIC_NETWORK) {
            triggers.push("public_network_target".to_string());
        }
        if argument_signals.has(ARG_FLAG_SECRET_ENV_ACCESS) {
            triggers.push("secret_env_access".to_string());
        }
        if runtime_risk_is_dangerous(capability)
            && matches!(state.last_decision, Some(RuntimeRiskAction::Harden))
        {
            triggers.push("dangerous_capability_escalation".to_string());
        }
        if let Some(ref prev_cap) = state.last_capability
            && prev_cap != capability
            && runtime_risk_is_dangerous(capability)
            && !runtime_risk_is_dangerous(prev_cap)
        {
            triggers.push("unseen_capability_transition".to_string());
        }
        if (meta.resource_target_class.starts_with("filesystem.")
            || meta.resource_target_class.starts_with("subprocess.")
            || meta.resource_target_class.starts_with("network.")
            || meta.resource_target_class.starts_with("credential."))
            && features.dangerous_capability > 0.5
        {
            triggers.push("sensitive_target_mismatch".to_string());
        }

        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut fallback_reason = None;
        if elapsed_ms > config.decision_timeout_ms {
            fallback_reason = Some("decision_timeout".to_string());
            action = if config.fail_closed {
                RuntimeRiskAction::Harden
            } else {
                RuntimeRiskAction::Allow
            };
            triggers.push("decision_timeout".to_string());
        }

        let (explanation_level, explanation_summary, top_contributors, budget_state) =
            runtime_risk_build_explanation(
                action,
                risk_score,
                &posterior,
                &expected_loss,
                &features,
                &triggers,
                fallback_reason.as_deref(),
                RUNTIME_RISK_EXPLANATION_TERM_BUDGET,
                RUNTIME_RISK_EXPLANATION_TIME_BUDGET_MS,
            );

        state.last_decision = Some(action);

        Some(RuntimeRiskDecision {
            action,
            reason: "runtime_risk".to_string(),
            capability: capability.to_string(),
            method: method.to_string(),
            params_hash: params_hash.to_string(),
            args_shape_hash: meta.args_shape_hash.to_string(),
            resource_target_class: meta.resource_target_class.to_string(),
            policy_profile: meta.policy_profile.to_string(),
            timeout_ms: meta.timeout_ms,
            risk_score,
            posterior,
            expected_loss,
            e_process,
            e_threshold,
            conformal_residual,
            conformal_quantile,
            drift_detected,
            triggers,
            explanation_schema: RUNTIME_RISK_EXPLANATION_SCHEMA_VERSION.to_string(),
            explanation_level,
            explanation_summary,
            top_contributors,
            budget_state,
            fallback_reason,
            elapsed_ms,
            state_label,
            sequence_context,
            features,
            feature_extraction_latency_us,
            feature_budget_exceeded,
        })
    }

    #[allow(
        clippy::too_many_lines,
        clippy::significant_drop_tightening,
        clippy::too_many_arguments
    )]
    pub(super) fn record_runtime_risk_outcome(
        &self,
        extension_id: Option<&str>,
        call_id: &str,
        policy_reason: &str,
        decision: &RuntimeRiskDecision,
        outcome_error_code: Option<&str>,
        duration_ms: u64,
        lane_execution: Option<&HostcallLaneExecution>,
        marshalling: &HostcallMarshallingTelemetry,
    ) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.runtime_risk_config.enabled {
            return;
        }
        let window_size = guard.runtime_risk_config.window_size;
        let alpha = guard.runtime_risk_config.alpha;

        let ext_key = Self::runtime_risk_extension_key(extension_id);
        let (telemetry, entry) = {
            let Some(state) = guard.runtime_risk_states.get_mut(&ext_key) else {
                return;
            };

            let realized_risk = if outcome_error_code.is_some() {
                1.0
            } else {
                0.0
            };
            let predicted =
                runtime_risk_clamp01(decision.posterior.suspicious + decision.posterior.unsafe_);
            let residual = (predicted - realized_risk).abs();

            state.residual_window.push_back(residual);
            while state.residual_window.len() > window_size {
                let _ = state.residual_window.pop_front();
            }
            state.previous_residual_quantile =
                runtime_risk_quantile(state.residual_window.iter().copied().collect(), 1.0 - alpha);

            let denied_dangerous = outcome_error_code.is_some_and(|code| code == "denied")
                && runtime_risk_is_dangerous(&decision.capability);
            if decision.posterior.unsafe_ >= 0.55
                || matches!(decision.action, RuntimeRiskAction::Terminate)
                || denied_dangerous
            {
                state.consecutive_unsafe = state.consecutive_unsafe.saturating_add(1);
            } else {
                state.consecutive_unsafe = 0;
            }
            if state.consecutive_unsafe >= 3 {
                state.quarantined = true;
            }

            // Outcome-conditioned Bayesian reinforcement.
            if let Some(code) = outcome_error_code {
                if code == "denied" || code == "timeout" || code == "io" {
                    state.alpha_unsafe += 0.9;
                    state.alpha_suspicious += 0.4;
                } else {
                    state.alpha_suspicious += 0.35;
                }
            } else {
                state.alpha_safe += 0.6;
            }

            let ts_ms = runtime_risk_now_ms();
            let is_error = outcome_error_code.is_some();
            let sequence_window = window_size.max(RUNTIME_HOSTCALL_SEQUENCE_WINDOW);
            state.sequence_counter = state.sequence_counter.saturating_add(1);
            state.recent_call_timestamps_ms.push_back(ts_ms);
            while state.recent_call_timestamps_ms.len() > sequence_window {
                let _ = state.recent_call_timestamps_ms.pop_front();
            }
            state.recent_outcome_errors.push_back(is_error);
            while state.recent_outcome_errors.len() > sequence_window {
                let _ = state.recent_outcome_errors.pop_front();
            }
            state.consecutive_failures = if is_error {
                state.consecutive_failures.saturating_add(1)
            } else {
                0
            };
            state.last_capability = Some(decision.capability.clone());
            state.last_method = Some(decision.method.clone());
            state.last_resource_target_class = Some(decision.resource_target_class.clone());
            let (
                lane,
                lane_decision_reason,
                lane_fallback_reason,
                lane_matrix_key,
                lane_dispatch_latency_ms,
            ) = lane_execution.map_or_else(
                || {
                    let method = decision.method.trim().to_ascii_lowercase();
                    let capability = decision.capability.trim().to_ascii_lowercase();
                    let capability_class = hostcall_capability_class_from_capability(&capability);
                    let lane_reason = if outcome_error_code.is_some() {
                        "no_dispatch_runtime_risk"
                    } else {
                        "missing_lane_execution_metadata"
                    };
                    let lane_reason_owned = lane_reason.to_string();
                    (
                        if outcome_error_code.is_some() {
                            "compat".to_string()
                        } else {
                            "unknown".to_string()
                        },
                        lane_reason_owned.clone(),
                        Some(lane_reason_owned),
                        format!("{method}|fallback|{capability_class}"),
                        0,
                    )
                },
                |meta| {
                    (
                        meta.lane.as_str().to_string(),
                        meta.decision_reason.clone(),
                        meta.fallback_reason.clone(),
                        meta.matrix_key.to_string(),
                        meta.dispatch_latency_ms,
                    )
                },
            );
            let lane_latency_share_bps = lane_dispatch_latency_ms
                .saturating_mul(10_000)
                .checked_div(duration_ms)
                .unwrap_or(0)
                .min(10_000);

            let telemetry = RuntimeHostcallTelemetryEvent {
                schema: RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION.to_string(),
                ts_ms,
                extension_id: ext_key.clone(),
                call_id: call_id.to_string(),
                capability: decision.capability.clone(),
                method: decision.method.clone(),
                params_hash: decision.params_hash.clone(),
                args_shape_hash: decision.args_shape_hash.clone(),
                resource_target_class: decision.resource_target_class.clone(),
                policy_reason: policy_reason.to_string(),
                policy_profile: decision.policy_profile.clone(),
                risk_score: decision.risk_score,
                timeout_ms: decision.timeout_ms,
                latency_ms: duration_ms,
                lane,
                lane_decision_reason,
                lane_fallback_reason,
                lane_matrix_key,
                lane_dispatch_latency_ms,
                lane_latency_share_bps,
                marshalling_path: marshalling.path.clone(),
                marshalling_latency_us: marshalling.latency_us,
                marshalling_fallback_reason: marshalling.fallback_reason.clone(),
                marshalling_fallback_count: marshalling.fallback_count,
                marshalling_superinstruction_trace_signature: marshalling
                    .superinstruction_trace_signature
                    .clone(),
                marshalling_superinstruction_plan_id: marshalling.superinstruction_plan_id.clone(),
                marshalling_superinstruction_expected_cost_delta: marshalling
                    .superinstruction_expected_cost_delta,
                marshalling_superinstruction_observed_cost_delta: marshalling
                    .superinstruction_observed_cost_delta,
                marshalling_superinstruction_deopt_reason: marshalling
                    .superinstruction_deopt_reason
                    .clone(),
                marshalling_superinstruction_jit_hit: marshalling.superinstruction_jit_hit,
                marshalling_superinstruction_jit_cost_delta: marshalling
                    .superinstruction_jit_cost_delta,
                outcome: if is_error {
                    "error".to_string()
                } else {
                    "success".to_string()
                },
                outcome_error_code: outcome_error_code.map(ToString::to_string),
                selected_action: RuntimeRiskActionValue::from(decision.action),
                reason_codes: decision.triggers.clone(),
                explanation_level: decision.explanation_level,
                explanation_summary: decision.explanation_summary.clone(),
                top_contributors: decision.top_contributors.clone(),
                budget_state: decision.budget_state.clone(),
                sequence: decision.sequence_context.clone(),
                features: decision.features.clone(),
                extraction_latency_us: decision.feature_extraction_latency_us,
                extraction_budget_us: RUNTIME_HOSTCALL_FEATURE_BUDGET_US,
                extraction_budget_exceeded: decision.feature_budget_exceeded,
                redaction_summary: "params redacted via hash-only telemetry".to_string(),
            };

            let entry = RuntimeRiskLedgerEntry {
                ts_ms,
                extension_id: ext_key.clone(),
                call_id: call_id.to_string(),
                capability: decision.capability.clone(),
                method: decision.method.clone(),
                params_hash: decision.params_hash.clone(),
                policy_reason: policy_reason.to_string(),
                risk_score: decision.risk_score,
                posterior: decision.posterior.clone(),
                expected_loss: decision.expected_loss.clone(),
                selected_action: decision.action,
                derived_state: decision.state_label,
                triggers: decision.triggers.clone(),
                fallback_reason: decision.fallback_reason.clone(),
                e_process: decision.e_process,
                e_threshold: decision.e_threshold,
                conformal_residual: residual,
                conformal_quantile: state.previous_residual_quantile,
                drift_detected: decision.drift_detected,
                outcome_error_code: outcome_error_code.map(ToString::to_string),
                explanation_schema: decision.explanation_schema.clone(),
                explanation_level: decision.explanation_level,
                explanation_summary: decision.explanation_summary.clone(),
                top_contributors: decision.top_contributors.clone(),
                budget_state: decision.budget_state.clone(),
                ledger_hash: String::new(),
                prev_ledger_hash: None,
            };
            (telemetry, entry)
        };

        Self::runtime_risk_push_telemetry(&mut guard, telemetry);

        let entry = Self::runtime_risk_push_ledger(&mut guard, entry);
        let top_contributor_codes = entry
            .top_contributors
            .iter()
            .map(|contributor| contributor.code.clone())
            .collect::<Vec<_>>();

        if matches!(
            decision.action,
            RuntimeRiskAction::Deny | RuntimeRiskAction::Terminate | RuntimeRiskAction::Harden
        ) {
            tracing::warn!(
                event = "runtime_risk.decision",
                extension_id = %entry.extension_id,
                call_id = %entry.call_id,
                capability = %entry.capability,
                method = %entry.method,
                selected_action = ?entry.selected_action,
                state = ?entry.derived_state,
                risk_score = entry.risk_score,
                e_process = entry.e_process,
                e_threshold = entry.e_threshold,
                conformal_residual = entry.conformal_residual,
                conformal_quantile = entry.conformal_quantile,
                triggers = ?entry.triggers,
                fallback_reason = ?entry.fallback_reason,
                explanation_level = ?entry.explanation_level,
                explanation_budget_exhausted = entry.budget_state.exhausted,
                explanation_terms = entry.budget_state.terms_emitted,
                top_contributors = ?top_contributor_codes,
                outcome_error_code = ?entry.outcome_error_code,
                ledger_hash = %entry.ledger_hash,
                "Runtime risk controller applied defensive action"
            );
        } else {
            tracing::info!(
                event = "runtime_risk.decision",
                extension_id = %entry.extension_id,
                call_id = %entry.call_id,
                capability = %entry.capability,
                method = %entry.method,
                selected_action = ?entry.selected_action,
                state = ?entry.derived_state,
                risk_score = entry.risk_score,
                e_process = entry.e_process,
                e_threshold = entry.e_threshold,
                conformal_residual = entry.conformal_residual,
                conformal_quantile = entry.conformal_quantile,
                triggers = ?entry.triggers,
                explanation_level = ?entry.explanation_level,
                explanation_budget_exhausted = entry.budget_state.exhausted,
                explanation_terms = entry.budget_state.terms_emitted,
                top_contributors = ?top_contributor_codes,
                ledger_hash = %entry.ledger_hash,
                "Runtime risk controller allowed hostcall"
            );
        }
    }

    pub fn runtime_risk_ledger_artifact(&self) -> RuntimeRiskLedgerArtifact {
        let entries = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .runtime_risk_ledger
                .iter()
                .map(RuntimeRiskLedgerArtifactEntry::from)
                .collect::<Vec<_>>()
        };
        let head_ledger_hash = entries.first().map(|entry| entry.ledger_hash.clone());
        let tail_ledger_hash = entries.last().map(|entry| entry.ledger_hash.clone());
        let data_hash = runtime_risk_ledger_data_hash(&entries);
        RuntimeRiskLedgerArtifact {
            schema: RUNTIME_RISK_LEDGER_SCHEMA_VERSION.to_string(),
            generated_at_ms: runtime_risk_now_ms(),
            entry_count: entries.len(),
            head_ledger_hash,
            tail_ledger_hash,
            data_hash,
            entries,
        }
    }

    pub fn runtime_hostcall_telemetry_artifact(&self) -> RuntimeHostcallTelemetryArtifact {
        let entries = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .runtime_hostcall_telemetry
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        };
        RuntimeHostcallTelemetryArtifact {
            schema: RUNTIME_HOSTCALL_TELEMETRY_SCHEMA_VERSION.to_string(),
            generated_at_ms: runtime_risk_now_ms(),
            entry_count: entries.len(),
            entries,
        }
    }

    pub fn runtime_risk_verify_ledger(&self) -> RuntimeRiskLedgerVerificationReport {
        let artifact = self.runtime_risk_ledger_artifact();
        verify_runtime_risk_ledger_artifact(&artifact)
    }

    pub fn runtime_risk_replay_ledger(&self) -> Result<RuntimeRiskReplayArtifact> {
        let artifact = self.runtime_risk_ledger_artifact();
        replay_runtime_risk_ledger_artifact(&artifact)
    }

    pub fn runtime_risk_calibrate_ledger(
        &self,
        config: &RuntimeRiskCalibrationConfig,
    ) -> Result<RuntimeRiskCalibrationReport> {
        let artifact = self.runtime_risk_ledger_artifact();
        calibrate_runtime_risk_from_ledger(&artifact, config)
    }

    /// Build a baseline model for the given extension from the current ledger.
    pub fn build_baseline(&self, extension_id: &str) -> Result<RuntimeRiskBaselineModel> {
        let artifact = self.runtime_risk_ledger_artifact();
        build_baseline_from_ledger(&artifact, extension_id)
    }

    #[cfg(test)]
    pub(super) fn runtime_risk_ledger_snapshot(&self) -> Vec<RuntimeRiskLedgerEntry> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime_risk_ledger
            .iter()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(super) fn runtime_hostcall_telemetry_snapshot(&self) -> Vec<RuntimeHostcallTelemetryEvent> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime_hostcall_telemetry
            .iter()
            .cloned()
            .collect()
    }

    // -----------------------------------------------------------------------
    // SEC-4.3: Exec mediation + secret broker ledger accumulation & export
    // -----------------------------------------------------------------------

    /// Record an exec mediation decision into the SEC-4.3 ledger.
    pub fn record_exec_mediation(&self, entry: ExecMediationLedgerEntry) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.exec_mediation_ledger.push_back(entry);
        // Cap at same limit as runtime-risk ledger.
        while guard.exec_mediation_ledger.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.exec_mediation_ledger.pop_front();
        }
        drop(guard);
    }

    /// Record a secret broker decision into the SEC-4.3 ledger.
    pub fn record_secret_broker(&self, entry: SecretBrokerLedgerEntry) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.secret_broker_ledger.push_back(entry);
        while guard.secret_broker_ledger.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.secret_broker_ledger.pop_front();
        }
        drop(guard);
    }

    /// Export the exec mediation ledger as a structured artifact.
    pub fn exec_mediation_artifact(&self) -> ExecMediationArtifact {
        let entries: Vec<_> = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .exec_mediation_ledger
            .iter()
            .cloned()
            .collect();
        ExecMediationArtifact {
            schema: "pi.ext.exec_mediation_ledger.v1".to_string(),
            generated_at_ms: runtime_risk_now_ms(),
            entry_count: entries.len(),
            entries,
        }
    }

    /// Export the secret broker ledger as a structured artifact.
    pub fn secret_broker_artifact(&self) -> SecretBrokerArtifact {
        let entries: Vec<_> = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .secret_broker_ledger
            .iter()
            .cloned()
            .collect();
        SecretBrokerArtifact {
            schema: "pi.ext.secret_broker_ledger.v1".to_string(),
            generated_at_ms: runtime_risk_now_ms(),
            entry_count: entries.len(),
            entries,
        }
    }

    /// Snapshot of exec mediation entries (test helper).
    #[cfg(test)]
    fn exec_mediation_snapshot(&self) -> Vec<ExecMediationLedgerEntry> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .exec_mediation_ledger
            .iter()
            .cloned()
            .collect()
    }

    /// Snapshot of secret broker entries (test helper).
    #[cfg(test)]
    fn secret_broker_snapshot(&self) -> Vec<SecretBrokerLedgerEntry> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .secret_broker_ledger
            .iter()
            .cloned()
            .collect()
    }

    // ------------------------------------------------------------------
    // SEC-5.1: Security alert recording and export
    // ------------------------------------------------------------------

    /// Record a security alert into the SEC-5.1 alert stream.
    pub fn record_security_alert(&self, mut alert: SecurityAlert) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.security_alert_seq += 1;
        alert.sequence_id = guard.security_alert_seq;
        guard.security_alerts.push_back(alert);
        while guard.security_alerts.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.security_alerts.pop_front();
        }
        drop(guard);
    }

    /// Export the security alert stream as a structured artifact.
    pub fn security_alert_artifact(&self) -> SecurityAlertArtifact {
        let alerts: Vec<_> = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .security_alerts
            .iter()
            .cloned()
            .collect();
        let mut category_counts = SecurityAlertCategoryCounts::default();
        let mut severity_counts = SecurityAlertSeverityCounts::default();
        for a in &alerts {
            category_counts.increment(a.category);
            severity_counts.increment(a.severity);
        }
        SecurityAlertArtifact {
            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
            generated_at_ms: runtime_risk_now_ms(),
            alert_count: alerts.len(),
            category_counts,
            severity_counts,
            alerts,
        }
    }

    /// Return the current count of recorded security alerts.
    pub fn security_alert_count(&self) -> usize {
        self.inner
            .lock()
            .ok()
            .map_or(0, |guard| guard.security_alerts.len())
    }

    /// Snapshot of security alerts (test helper).
    #[cfg(test)]
    pub(super) fn security_alert_snapshot(&self) -> Vec<SecurityAlert> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .security_alerts
            .iter()
            .cloned()
            .collect()
    }

    // ------------------------------------------------------------------
    // Hostcall lane emergency controls
    // ------------------------------------------------------------------

    /// Enable or disable the global hostcall compatibility-lane kill-switch.
    ///
    /// When enabled, all hostcalls that would normally use the fast lane are
    /// deterministically routed through the compatibility lane.
    #[allow(clippy::significant_drop_tightening)]
    pub fn set_hostcall_compat_kill_switch_global(&self, enabled: bool) {
        let Ok(mut guard) = self.inner.lock() else {
            tracing::error!(
                event = "host_call.compat_kill_switch.global.lock_poisoned",
                enabled,
                "Cannot set global kill-switch: lock poisoned"
            );
            return;
        };
        guard.hostcall_compat_kill_switch_global = enabled;
        self.refresh_snapshot_with_guard_release(guard);

        if enabled {
            tracing::warn!(
                event = "host_call.compat_kill_switch.global",
                enabled,
                "Enabled global hostcall compatibility-lane kill-switch"
            );
        } else {
            tracing::info!(
                event = "host_call.compat_kill_switch.global",
                enabled,
                "Disabled global hostcall compatibility-lane kill-switch"
            );
        }
    }

    /// Enable or disable per-extension hostcall compatibility-lane kill-switch.
    ///
    /// When enabled for `extension_id`, fast-lane-eligible hostcalls from that
    /// extension are routed through the compatibility lane.
    #[allow(clippy::significant_drop_tightening)]
    pub fn set_hostcall_compat_kill_switch_for_extension(&self, extension_id: &str, enabled: bool) {
        let extension_id = extension_id.trim();
        if extension_id.is_empty() {
            return;
        }

        let Ok(mut guard) = self.inner.lock() else {
            tracing::error!(
                event = "host_call.compat_kill_switch.extension.lock_poisoned",
                %extension_id,
                enabled,
                "Cannot set per-extension kill-switch: lock poisoned"
            );
            return;
        };
        if enabled {
            guard
                .hostcall_compat_kill_switch_extensions
                .insert(extension_id.to_string());
        } else {
            guard
                .hostcall_compat_kill_switch_extensions
                .remove(extension_id);
        }
        self.refresh_snapshot_with_guard_release(guard);

        if enabled {
            tracing::warn!(
                event = "host_call.compat_kill_switch.extension",
                extension_id = %extension_id,
                enabled,
                "Enabled per-extension hostcall compatibility-lane kill-switch"
            );
        } else {
            tracing::info!(
                event = "host_call.compat_kill_switch.extension",
                extension_id = %extension_id,
                enabled,
                "Disabled per-extension hostcall compatibility-lane kill-switch"
            );
        }
    }

    pub fn hostcall_compat_kill_switch_global(&self) -> bool {
        self.inner
            .lock()
            .is_ok_and(|guard| guard.hostcall_compat_kill_switch_global)
    }

    pub fn hostcall_compat_kill_switch_for_extension(&self, extension_id: &str) -> bool {
        self.inner.lock().is_ok_and(|guard| {
            guard
                .hostcall_compat_kill_switch_extensions
                .contains(extension_id)
        })
    }

    pub(super) fn hostcall_compat_kill_switch_reason(
        &self,
        extension_id: Option<&str>,
    ) -> Option<&'static str> {
        let guard = self.inner.lock().ok()?;
        let forced_global = guard.hostcall_compat_kill_switch_global;
        let forced_extension = extension_id
            .is_some_and(|id| guard.hostcall_compat_kill_switch_extensions.contains(id));
        let forced_budget = extension_id.is_some_and(|id| {
            guard.budget_controller_config.enabled
                && guard
                    .budget_fallback_states
                    .get(id)
                    .is_some_and(|state| state.in_fallback)
        });
        drop(guard);

        if forced_global {
            return Some("forced_compat_global_kill_switch");
        }
        if forced_extension {
            return Some("forced_compat_extension_kill_switch");
        }
        if forced_budget {
            return Some("forced_compat_budget_controller");
        }
        None
    }

    // ------------------------------------------------------------------
    // Hostcall Reactor Mesh (bd-3ar8v.4.20)
    // ------------------------------------------------------------------

    /// Enable the hostcall reactor mesh with the given configuration.
    ///
    /// Fast-lane opcodes will be routed through per-shard SPSC lanes
    /// for reduced cross-core contention.
    pub fn enable_hostcall_reactor(&self, config: HostcallReactorConfig) {
        let configured_shard_count = config.shard_count;
        let lane_capacity = config.lane_capacity;
        let reactor = HostcallReactorMesh::new(config);
        let shard_count = reactor.shard_count();
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if shard_count == 0 {
            guard.hostcall_reactor = None;
            drop(guard);
            tracing::warn!(
                event = "hostcall_reactor.invalid_config",
                configured_shard_count = configured_shard_count,
                lane_capacity = lane_capacity,
                "Invalid hostcall reactor config leaves reactor disabled"
            );
            return;
        }
        guard.hostcall_reactor = Some(reactor);
        drop(guard);
        tracing::info!(
            event = "hostcall_reactor.enabled",
            shard_count,
            "Hostcall reactor mesh enabled"
        );
    }

    /// Disable the hostcall reactor mesh.
    pub fn disable_hostcall_reactor(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.hostcall_reactor = None;
        drop(guard);
        tracing::info!(
            event = "hostcall_reactor.disabled",
            "Hostcall reactor mesh disabled"
        );
    }

    /// Check if the reactor mesh is enabled.
    #[must_use]
    pub fn hostcall_reactor_enabled(&self) -> bool {
        self.inner
            .lock()
            .is_ok_and(|guard| guard.hostcall_reactor.is_some())
    }

    /// Submit a fast-lane hostcall to the reactor mesh for shard-local dispatch.
    ///
    /// Returns `None` if the reactor is not enabled (caller should dispatch directly).
    /// Returns `Some(Ok(request))` on successful submission.
    /// Returns `Some(Err(backpressure))` if the target shard lane is full.
    pub(crate) fn reactor_submit(
        &self,
        call_id: String,
        opcode: CommonHostcallOpcode,
        params: Value,
    ) -> Option<std::result::Result<HostcallReactorRequest, HostcallReactorBackpressure>> {
        let mut guard = self.inner.lock().ok()?;
        let reactor = guard.hostcall_reactor.as_mut()?;
        let result = reactor.submit(call_id, opcode, params);
        drop(guard);
        Some(result)
    }

    /// Record completion for a fast-lane hostcall that was dispatched directly.
    pub(crate) fn reactor_record_completion(&self, shard_id: usize, global_seq: u64) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|mut guard| {
                guard
                    .hostcall_reactor
                    .as_mut()
                    .map(|r| r.record_completion(shard_id, global_seq))
            })
            .unwrap_or(false)
    }

    /// Re-enable the reactor with sizing derived from host parallelism and current telemetry.
    ///
    /// Returns the applied configuration, or `None` if no reactor is currently enabled.
    #[must_use]
    pub fn retune_hostcall_reactor_from_telemetry(&self) -> Option<HostcallReactorConfig> {
        let telemetry = self.reactor_telemetry()?;
        let config = HostcallReactorConfig::auto_sized_for(
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            Some(&telemetry),
        );
        self.enable_hostcall_reactor(config.clone());
        Some(config)
    }

    /// Drain pending requests from a specific reactor shard.
    pub fn reactor_drain_shard(
        &self,
        shard_id: usize,
        budget: usize,
    ) -> Vec<HostcallReactorRequest> {
        self.inner
            .lock()
            .ok()
            .and_then(|mut guard| {
                guard
                    .hostcall_reactor
                    .as_mut()
                    .map(|r| r.drain_shard(shard_id, budget))
            })
            .unwrap_or_default()
    }

    /// Drain pending requests in deterministic global sequence order.
    pub fn reactor_drain_global(&self, budget: usize) -> Vec<HostcallReactorRequest> {
        self.inner
            .lock()
            .ok()
            .and_then(|mut guard| {
                guard
                    .hostcall_reactor
                    .as_mut()
                    .map(|r| r.drain_global_order(budget))
            })
            .unwrap_or_default()
    }

    /// Get reactor mesh telemetry snapshot.
    #[must_use]
    pub fn reactor_telemetry(&self) -> Option<HostcallReactorTelemetry> {
        self.inner.lock().ok().and_then(|guard| {
            guard
                .hostcall_reactor
                .as_ref()
                .map(HostcallReactorMesh::telemetry)
        })
    }

    // ------------------------------------------------------------------
    // SEC-5.2: Kill-switch and trust onboarding
    // ------------------------------------------------------------------

    /// Activate the kill-switch for an extension.
    ///
    /// Immediately sets the extension's trust state to `Killed` and
    /// quarantines it in the runtime risk controller so all future
    /// hostcalls are rejected.  Emits a Critical security alert and
    /// records an audit entry.
    pub fn kill_switch(
        &self,
        extension_id: &str,
        reason: &str,
        operator: &str,
    ) -> KillSwitchResult {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = guard
            .trust_states
            .get(extension_id)
            .copied()
            .unwrap_or(ExtensionTrustState::Pending);

        if previous == ExtensionTrustState::Killed {
            return KillSwitchResult {
                success: false,
                previous_state: previous,
                new_state: ExtensionTrustState::Killed,
                message: format!("Extension `{extension_id}` is already killed"),
            };
        }

        // Set trust state.
        guard
            .trust_states
            .insert(extension_id.to_string(), ExtensionTrustState::Killed);

        // Quarantine in runtime risk controller.
        let risk_state = guard
            .runtime_risk_states
            .entry(extension_id.to_string())
            .or_default();
        risk_state.quarantined = true;

        // Record audit entry.
        let entry = KillSwitchAuditEntry {
            ts_ms: runtime_risk_now_ms(),
            extension_id: extension_id.to_string(),
            activated: true,
            reason: reason.to_string(),
            operator: operator.to_string(),
            previous_state: previous,
            new_state: ExtensionTrustState::Killed,
        };
        guard.kill_switch_audit.push_back(entry);
        while guard.kill_switch_audit.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.kill_switch_audit.pop_front();
        }

        // Emit security alert.
        guard.security_alert_seq += 1;
        let seq = guard.security_alert_seq;
        let mut alert = SecurityAlert::from_quarantine(extension_id, reason, 1.0);
        alert.sequence_id = seq;
        alert.summary =
            format!("Kill-switch activated for `{extension_id}` by {operator}: {reason}");
        alert.policy_source = "kill_switch".to_string();
        guard.security_alerts.push_back(alert);
        while guard.security_alerts.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.security_alerts.pop_front();
        }

        drop(guard);

        tracing::error!(
            extension_id = %extension_id,
            operator = %operator,
            reason = %reason,
            "KILL-SWITCH activated"
        );

        KillSwitchResult {
            success: true,
            previous_state: previous,
            new_state: ExtensionTrustState::Killed,
            message: format!("Kill-switch activated for `{extension_id}`"),
        }
    }

    /// Lift the kill-switch for an extension.
    ///
    /// Requires explicit acknowledgment.  Moves the trust state back to
    /// `Acknowledged` and clears the quarantine flag.  Records an audit
    /// entry and emits an Info-level security alert.
    pub fn lift_kill_switch(
        &self,
        extension_id: &str,
        reason: &str,
        operator: &str,
    ) -> KillSwitchResult {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = guard
            .trust_states
            .get(extension_id)
            .copied()
            .unwrap_or(ExtensionTrustState::Pending);

        if previous != ExtensionTrustState::Killed {
            return KillSwitchResult {
                success: false,
                previous_state: previous,
                new_state: previous,
                message: format!("Extension `{extension_id}` is not killed (state: {previous})"),
            };
        }

        // Restore trust state.
        guard
            .trust_states
            .insert(extension_id.to_string(), ExtensionTrustState::Acknowledged);

        // Clear quarantine in runtime risk controller.
        if let Some(risk_state) = guard.runtime_risk_states.get_mut(extension_id) {
            risk_state.quarantined = false;
            risk_state.consecutive_unsafe = 0;
        }

        // Record audit entry.
        let entry = KillSwitchAuditEntry {
            ts_ms: runtime_risk_now_ms(),
            extension_id: extension_id.to_string(),
            activated: false,
            reason: reason.to_string(),
            operator: operator.to_string(),
            previous_state: ExtensionTrustState::Killed,
            new_state: ExtensionTrustState::Acknowledged,
        };
        guard.kill_switch_audit.push_back(entry);
        while guard.kill_switch_audit.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.kill_switch_audit.pop_front();
        }

        // Emit info alert.
        guard.security_alert_seq += 1;
        let seq = guard.security_alert_seq;
        guard.security_alerts.push_back(SecurityAlert {
            schema: SECURITY_ALERT_SCHEMA_VERSION.to_string(),
            ts_ms: runtime_risk_now_ms(),
            sequence_id: seq,
            extension_id: extension_id.to_string(),
            category: SecurityAlertCategory::Quarantine,
            severity: SecurityAlertSeverity::Info,
            capability: String::new(),
            method: String::new(),
            reason_codes: vec!["kill_switch_lifted".to_string()],
            summary: format!("Kill-switch lifted for `{extension_id}` by {operator}: {reason}"),
            policy_source: "kill_switch".to_string(),
            action: SecurityAlertAction::Allow,
            remediation: String::new(),
            risk_score: 0.0,
            risk_state: None,
            context_hash: String::new(),
        });
        while guard.security_alerts.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.security_alerts.pop_front();
        }

        drop(guard);

        tracing::info!(
            extension_id = %extension_id,
            operator = %operator,
            reason = %reason,
            "Kill-switch lifted"
        );

        KillSwitchResult {
            success: true,
            previous_state: ExtensionTrustState::Killed,
            new_state: ExtensionTrustState::Acknowledged,
            message: format!("Kill-switch lifted for `{extension_id}`"),
        }
    }

    /// Check whether an extension is currently killed.
    pub fn is_killed(&self, extension_id: &str) -> bool {
        self.inner.lock().is_ok_and(|guard| {
            guard
                .trust_states
                .get(extension_id)
                .copied()
                .unwrap_or(ExtensionTrustState::Pending)
                == ExtensionTrustState::Killed
        })
    }

    /// Get the trust state for an extension.
    pub fn trust_state(&self, extension_id: &str) -> ExtensionTrustState {
        self.inner
            .lock()
            .ok()
            .and_then(|guard| guard.trust_states.get(extension_id).copied())
            .unwrap_or(ExtensionTrustState::Pending)
    }

    /// Record a trust onboarding decision.
    ///
    /// If `accepted` is `true`, the extension moves to `Acknowledged`.
    /// If `accepted` is `false`, the extension is killed (rejected).
    pub fn record_trust_onboarding(
        &self,
        extension_id: &str,
        risk_level: &str,
        accepted: bool,
        operator: &str,
    ) -> ExtensionTrustState {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let resulting_state = if accepted {
            ExtensionTrustState::Acknowledged
        } else {
            ExtensionTrustState::Killed
        };

        guard
            .trust_states
            .insert(extension_id.to_string(), resulting_state);

        // If rejected, also quarantine.
        if !accepted {
            let risk_state = guard
                .runtime_risk_states
                .entry(extension_id.to_string())
                .or_default();
            risk_state.quarantined = true;
        }

        let decision = TrustOnboardingDecision {
            ts_ms: runtime_risk_now_ms(),
            extension_id: extension_id.to_string(),
            acknowledged_risk_level: risk_level.to_string(),
            accepted,
            operator: operator.to_string(),
            resulting_state,
        };
        guard.trust_onboarding_log.push_back(decision);
        while guard.trust_onboarding_log.len() > guard.runtime_risk_config.ledger_limit {
            let _ = guard.trust_onboarding_log.pop_front();
        }

        drop(guard);

        if accepted {
            tracing::info!(
                extension_id = %extension_id,
                risk_level = %risk_level,
                operator = %operator,
                "Trust onboarding: extension accepted"
            );
        } else {
            tracing::warn!(
                extension_id = %extension_id,
                risk_level = %risk_level,
                operator = %operator,
                "Trust onboarding: extension rejected"
            );
        }

        resulting_state
    }

    /// Promote an extension to `Trusted` state.
    ///
    /// Only extensions currently in `Acknowledged` state can be promoted.
    pub fn promote_trust(&self, extension_id: &str) -> ExtensionTrustState {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = guard
            .trust_states
            .get(extension_id)
            .copied()
            .unwrap_or(ExtensionTrustState::Pending);
        if current == ExtensionTrustState::Acknowledged {
            guard
                .trust_states
                .insert(extension_id.to_string(), ExtensionTrustState::Trusted);
            ExtensionTrustState::Trusted
        } else {
            current
        }
    }

    /// Return the kill-switch audit trail.
    pub fn kill_switch_audit_log(&self) -> Vec<KillSwitchAuditEntry> {
        self.inner
            .lock()
            .ok()
            .map(|guard| guard.kill_switch_audit.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Return the trust onboarding decision log.
    pub fn trust_onboarding_decisions(&self) -> Vec<TrustOnboardingDecision> {
        self.inner
            .lock()
            .ok()
            .map(|guard| guard.trust_onboarding_log.iter().cloned().collect())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // SEC-5.3: Incident Evidence Bundle export
    // ------------------------------------------------------------------
    /// Export a complete incident evidence bundle combining all security
    /// artifacts with optional filtering and redaction.
    ///
    /// Delegates to [`build_incident_evidence_bundle`] after collecting
    /// all sub-artifacts from the manager.
    pub fn export_incident_bundle(
        &self,
        filter: &IncidentBundleFilter,
        redaction: &IncidentBundleRedactionPolicy,
    ) -> IncidentEvidenceBundle {
        let risk_ledger = self.runtime_risk_ledger_artifact();
        let exec_mediation = self.exec_mediation_artifact();
        let secret_broker = self.secret_broker_artifact();
        let hostcall_telemetry = self.runtime_hostcall_telemetry_artifact();
        let security_alerts = self.security_alert_artifact();
        let quota_breaches = self.drain_quota_breach_events();
        let now_ms = runtime_risk_now_ms();

        build_incident_evidence_bundle(
            &risk_ledger,
            &security_alerts,
            &hostcall_telemetry,
            &exec_mediation,
            &secret_broker,
            &quota_breaches,
            filter,
            redaction,
            now_ms,
        )
    }

    /// Shut down the extension runtime with a cleanup budget.
    ///
    /// Sends a graceful shutdown to the configured extension runtime thread and waits up to
    /// `budget` for it to exit.  Returns `true` if the runtime exited
    /// cleanly within the budget.
    pub async fn shutdown(&self, budget: Duration) -> bool {
        let runtime = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.runtime.clone()
        };

        if let Some(runtime) = runtime {
            let ok = runtime.shutdown(budget).await;
            // Clear the runtime handle so subsequent calls are no-ops.
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.runtime = None;
            ok
        } else {
            true
        }
    }

    pub fn set_ui_sender(&self, sender: mpsc::Sender<ExtensionUiRequest>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.ui_sender = Some(sender);
        self.refresh_snapshot_with_guard_release(guard);
    }

    pub fn clear_ui_sender(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.ui_sender = None;
        self.refresh_snapshot_with_guard_release(guard);
    }

    pub fn set_runtime(&self, runtime: ExtensionRuntimeHandle) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.runtime = Some(runtime);
        drop(guard);
    }

    pub fn set_js_runtime(&self, runtime: JsExtensionRuntimeHandle) {
        self.set_runtime(ExtensionRuntimeHandle::Js(runtime));
    }

    pub fn set_native_runtime(&self, runtime: NativeRustExtensionRuntimeHandle) {
        self.set_runtime(ExtensionRuntimeHandle::NativeRust(runtime));
    }

    pub fn set_cwd(&self, cwd: String) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.cwd = Some(cwd);
        guard.ctx_generation = guard.ctx_generation.wrapping_add(1);
        self.refresh_snapshot_with_guard_release(guard);
    }

    pub fn set_model_registry_values(&self, values: HashMap<String, String>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.model_registry_values = values;
        guard.ctx_generation = guard.ctx_generation.wrapping_add(1);
        self.refresh_snapshot_with_guard_release(guard);
    }

    #[cfg(feature = "wasm-host")]
    fn handle(&self) -> ExtensionManagerHandle {
        ExtensionManagerHandle::new(self)
    }

    pub fn set_host_actions(&self, actions: Arc<dyn ExtensionHostActions>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.host_actions = Some(actions);
    }

    pub fn runtime(&self) -> Option<ExtensionRuntimeHandle> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.runtime.clone()
    }

    pub fn js_runtime(&self) -> Option<JsExtensionRuntimeHandle> {
        match self.runtime() {
            Some(ExtensionRuntimeHandle::Js(runtime)) => Some(runtime),
            _ => None,
        }
    }

    pub fn native_runtime(&self) -> Option<NativeRustExtensionRuntimeHandle> {
        match self.runtime() {
            Some(ExtensionRuntimeHandle::NativeRust(runtime)) => Some(runtime),
            _ => None,
        }
    }

    pub(super) fn host_actions(&self) -> Option<Arc<dyn ExtensionHostActions>> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.host_actions.clone()
    }

    #[allow(clippy::significant_drop_tightening)]
    pub fn cached_policy_prompt_decision(
        &self,
        extension_id: &str,
        capability: &str,
    ) -> Option<bool> {
        let (decision, extension_version) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let decision = guard
                .policy_prompt_cache
                .get(extension_id)
                .and_then(|by_cap| by_cap.get(capability))
                .cloned();
            let extension_version = guard.extension_versions.get(extension_id).cloned();
            drop(guard);
            (decision, extension_version)
        };
        let dec = decision?;

        if let Some(range) = &dec.version_range {
            let version = extension_version?;
            if !check_version_constraint(&version, range) {
                return None;
            }
        }

        Some(dec.allow)
    }

    pub fn cache_policy_prompt_decision(&self, extension_id: &str, capability: &str, allow: bool) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let version_range = guard
            .extension_versions
            .get(extension_id)
            .map(|version| format!("^{version}"));

        let decision = PersistedDecision {
            capability: capability.to_string(),
            allow,
            decided_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            expires_at: None,
            version_range: version_range.clone(),
        };

        guard
            .policy_prompt_cache
            .entry(extension_id.to_string())
            .or_default()
            .insert(capability.to_string(), decision);

        // Persist to disk so the decision survives across sessions.
        if let Some(ref mut store) = guard.permission_store {
            let res = if let Some(range) = version_range {
                store.record_with_version(extension_id, capability, allow, &range)
            } else {
                store.record(extension_id, capability, allow)
            };
            if let Err(e) = res {
                tracing::warn!("Failed to persist permission decision: {e}");
            }
        }
    }

    /// Revoke all persisted permission decisions for an extension.
    pub fn revoke_extension_permissions(&self, extension_id: &str) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.policy_prompt_cache.remove(extension_id);
        if let Some(ref mut store) = guard.permission_store
            && let Err(e) = store.revoke_extension(extension_id)
        {
            tracing::warn!("Failed to revoke extension permissions: {e}");
        }
    }

    /// Reset all persisted permission decisions.
    pub fn reset_all_permissions(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.policy_prompt_cache.clear();
        if let Some(ref mut store) = guard.permission_store
            && let Err(e) = store.reset()
        {
            tracing::warn!("Failed to reset all permissions: {e}");
        }
    }

    /// List all persisted permission decisions.
    pub fn list_permissions(&self) -> HashMap<String, HashMap<String, PersistedDecision>> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.policy_prompt_cache.clone()
    }

    /// Lock-free: reads from the RCU snapshot.
    pub fn active_tools(&self) -> Option<Vec<String>> {
        self.read_snapshot().active_tools.clone()
    }

    fn extension_roots(&self) -> Vec<PathBuf> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.extension_roots.clone()
    }

    fn resolve_resource_paths(
        cwd: &Path,
        roots: &[PathBuf],
        raw_paths: Vec<String>,
    ) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();

        for raw in raw_paths {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }

            let candidate = PathBuf::from(trimmed);
            let resolved = if candidate.is_absolute() {
                candidate
            } else {
                let mut resolved = None;
                for root in roots {
                    let joined = root.join(&candidate);
                    if joined.exists() {
                        resolved = Some(joined);
                        break;
                    }
                }
                resolved.unwrap_or_else(|| cwd.join(&candidate))
            };

            let key = safe_canonicalize(&resolved);
            if seen.insert(key) {
                out.push(resolved);
            }
        }

        out
    }

    #[allow(clippy::significant_drop_tightening, clippy::too_many_lines)]
    pub async fn load_js_extensions(&self, specs: Vec<JsExtensionLoadSpec>) -> Result<()> {
        let runtime = self
            .runtime()
            .ok_or_else(|| Error::extension("Extension runtime not configured"))?;

        let entry_paths = specs
            .iter()
            .map(|spec| spec.entry_path.clone())
            .collect::<Vec<_>>();
        let extension_roots = collect_extension_roots_from_paths(&entry_paths);

        let compat_hints_by_extension = if runtime.compat_scan_mode() {
            Some(build_compat_registration_hints(&specs))
        } else {
            None
        };

        let snapshots = runtime.load_js_extensions_snapshots(specs).await?;

        let mut payloads = Vec::new();
        let mut extension_ids = Vec::new();
        let mut extension_versions = HashMap::new();
        let mut active_tools: Option<Vec<String>> = None;
        let mut all_providers = Vec::new();
        let mut all_mcp_servers = Vec::new();
        let mut all_flags = Vec::new();
        for snapshot in snapshots {
            let JsExtensionSnapshot {
                id,
                name,
                version,
                api_version,
                mut tools,
                mut slash_commands,
                providers,
                mcp_servers,
                shortcuts,
                flags,
                event_hooks,
                active_tools: ext_active_tools,
            } = snapshot;
            if let Some(hints_by_extension) = compat_hints_by_extension.as_ref()
                && let Some(hints) = hints_by_extension.get(&id)
            {
                apply_compat_registration_hints(
                    &id,
                    if name.is_empty() { &id } else { &name },
                    &mut tools,
                    &mut slash_commands,
                    hints,
                );
            }
            all_providers.extend(providers.into_iter().map(|mut provider| {
                if let Some(obj) = provider.as_object_mut() {
                    obj.insert("extension_id".to_string(), Value::String(id.clone()));
                }
                provider
            }));
            all_mcp_servers.extend(mcp_servers.into_iter().map(|mut server| {
                if let Some(obj) = server.as_object_mut() {
                    obj.entry("extension_id".to_string())
                        .or_insert_with(|| Value::String(id.clone()));
                }
                server
            }));
            let extension_name = if name.is_empty() {
                id.clone()
            } else {
                name.clone()
            };
            Self::record_extension_version(&mut extension_versions, &id, &extension_name, &version);
            all_flags.extend(flags.iter().cloned().map(|mut flag| {
                if let Some(obj) = flag.as_object_mut() {
                    obj.entry("extension_id".to_string())
                        .or_insert_with(|| Value::String(id.clone()));
                }
                flag
            }));
            if let Some(list) = ext_active_tools {
                active_tools = Some(list);
            }
            extension_ids.push(id);
            payloads.push(RegisterPayload {
                name: extension_name,
                version,
                api_version: if api_version.is_empty() {
                    PROTOCOL_VERSION.to_string()
                } else {
                    api_version
                },
                capabilities: Vec::new(),
                capability_manifest: None,
                tools,
                slash_commands,
                shortcuts,
                flags,
                event_hooks,
            });
        }

        Self::validate_extension_identity_table(&payloads, &extension_ids, "QuickJS load")?;
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.extensions = payloads;
            guard.extension_ids = extension_ids;
            guard.extension_roots = extension_roots;
            guard.extension_versions = extension_versions;
            guard.active_tools = active_tools;
            guard.providers = all_providers;
            guard.mcp_servers = all_mcp_servers;
            guard.flags = all_flags;
            // Rebuild hook_bitmap from the freshly-loaded extensions so that
            // dispatch_tool_result / dispatch_event can find registered hooks.
            guard.hook_bitmap.clear();
            let hooks: Vec<String> = guard
                .extensions
                .iter()
                .flat_map(|ext| ext.event_hooks.iter().cloned())
                .collect();
            for hook in hooks {
                guard.hook_bitmap.insert(hook);
            }
            let active_extension_ids = guard
                .extension_versions
                .keys()
                .cloned()
                .collect::<HashSet<_>>();
            guard
                .runtime_risk_states
                .retain(|ext_id, _| active_extension_ids.contains(ext_id));
            self.refresh_snapshot_with_guard_release(guard);
        }
        Ok(())
    }

    #[allow(clippy::significant_drop_tightening, clippy::too_many_lines)]
    pub async fn load_native_extensions(
        &self,
        specs: Vec<NativeRustExtensionLoadSpec>,
    ) -> Result<()> {
        let runtime = self
            .runtime()
            .ok_or_else(|| Error::extension("Extension runtime not configured"))?;

        let entry_paths = specs
            .iter()
            .map(|spec| spec.entry_path.clone())
            .collect::<Vec<_>>();
        let extension_roots = collect_extension_roots_from_paths(&entry_paths);

        let snapshots = runtime.load_native_extensions_snapshots(specs).await?;
        let mut payloads = Vec::new();
        let mut extension_ids = Vec::new();
        let mut extension_versions = HashMap::new();
        let mut active_tools: Option<Vec<String>> = None;
        let mut all_providers = Vec::new();
        let mut all_mcp_servers = Vec::new();
        let mut all_flags = Vec::new();

        for snapshot in snapshots {
            let JsExtensionSnapshot {
                id,
                name,
                version,
                api_version,
                tools,
                slash_commands,
                providers,
                mcp_servers,
                shortcuts,
                flags,
                event_hooks,
                active_tools: ext_active_tools,
            } = snapshot;
            all_providers.extend(providers.into_iter().map(|mut provider| {
                if let Some(obj) = provider.as_object_mut() {
                    obj.insert("extension_id".to_string(), Value::String(id.clone()));
                }
                provider
            }));
            all_mcp_servers.extend(mcp_servers.into_iter().map(|mut server| {
                if let Some(obj) = server.as_object_mut() {
                    obj.entry("extension_id".to_string())
                        .or_insert_with(|| Value::String(id.clone()));
                }
                server
            }));
            let extension_name = if name.is_empty() {
                id.clone()
            } else {
                name.clone()
            };
            Self::record_extension_version(&mut extension_versions, &id, &extension_name, &version);
            all_flags.extend(flags.iter().cloned().map(|mut flag| {
                if let Some(obj) = flag.as_object_mut() {
                    obj.entry("extension_id".to_string())
                        .or_insert_with(|| Value::String(id.clone()));
                }
                flag
            }));
            if let Some(list) = ext_active_tools {
                active_tools = Some(list);
            }

            extension_ids.push(id);
            payloads.push(RegisterPayload {
                name: extension_name,
                version,
                api_version: if api_version.is_empty() {
                    PROTOCOL_VERSION.to_string()
                } else {
                    api_version
                },
                capabilities: Vec::new(),
                capability_manifest: None,
                tools,
                slash_commands,
                shortcuts,
                flags,
                event_hooks,
            });
        }

        Self::validate_extension_identity_table(&payloads, &extension_ids, "native-rust load")?;
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.extensions = payloads;
            guard.extension_ids = extension_ids;
            guard.extension_roots = extension_roots;
            guard.extension_versions = extension_versions;
            guard.active_tools = active_tools;
            guard.providers = all_providers;
            guard.mcp_servers = all_mcp_servers;
            guard.flags = all_flags;
            guard.hook_bitmap.clear();
            let hooks: Vec<String> = guard
                .extensions
                .iter()
                .flat_map(|ext| ext.event_hooks.iter().cloned())
                .collect();
            for hook in hooks {
                guard.hook_bitmap.insert(hook);
            }
            let active_extension_ids = guard
                .extension_versions
                .keys()
                .cloned()
                .collect::<HashSet<_>>();
            guard
                .runtime_risk_states
                .retain(|ext_id, _| active_extension_ids.contains(ext_id));
            self.refresh_snapshot_with_guard_release(guard);
        }
        Ok(())
    }

    #[cfg(feature = "wasm-host")]
    pub async fn load_wasm_extensions(
        &self,
        host: &WasmExtensionHost,
        specs: Vec<WasmExtensionLoadSpec>,
        tools: Arc<ToolRegistry>,
    ) -> Result<()> {
        let entry_paths = specs
            .iter()
            .map(|spec| spec.entry_path.clone())
            .collect::<Vec<_>>();
        let extension_roots = collect_extension_roots_from_paths(&entry_paths);

        let mut wasm_handles = Vec::new();
        let mut registrations = Vec::new();
        let mut registration_ids = Vec::new();

        for spec in specs {
            let extension_id = spec.manifest.extension_id.clone();
            let extension = host.load_from_path(&spec.entry_path)?;
            let mut instance = host
                .instantiate_with(&extension, Arc::clone(&tools), Some(self.handle()))
                .await?;

            let registration_json = instance.init(&spec.manifest_json).await?;
            let mut registration: RegisterPayload = serde_json::from_str(&registration_json)
                .map_err(|err| {
                    Error::extension(format!(
                        "WASM init returned invalid registration payload: {err}"
                    ))
                })?;
            if registration.capability_manifest.is_none() {
                registration
                    .capability_manifest
                    .clone_from(&spec.manifest.capability_manifest);
            }
            validate_register(&registration)?;

            wasm_handles.push(WasmExtensionHandle::new(instance, registration.clone()));
            registrations.push(registration);
            registration_ids.push(extension_id);
        }

        Self::validate_extension_identity_table(&registrations, &registration_ids, "WASM load")?;
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self::validate_extension_identity_table(
                &guard.extensions,
                &guard.extension_ids,
                "existing manager state",
            )?;
            if registration_ids
                .iter()
                .any(|extension_id| guard.extension_ids.contains(extension_id))
            {
                return Err(Error::extension(
                    "WASM load would duplicate an authoritative extension id",
                ));
            }
            if !extension_roots.is_empty() {
                let mut seen = HashSet::new();
                for root in &guard.extension_roots {
                    seen.insert(safe_canonicalize(root));
                }
                for root in extension_roots {
                    let key = safe_canonicalize(&root);
                    if seen.insert(key) {
                        guard.extension_roots.push(root);
                    }
                }
            }
            for (registration, extension_id) in registrations.iter().zip(&registration_ids) {
                Self::record_extension_version(
                    &mut guard.extension_versions,
                    extension_id,
                    &registration.name,
                    &registration.version,
                );
            }
            guard.extensions.extend(registrations);
            guard.extension_ids.extend(registration_ids);
            guard.wasm_extensions.extend(wasm_handles);
            drop(guard);
        }
        Ok(())
    }

    #[cfg(feature = "wasm-host")]
    pub fn wasm_extensions(&self) -> Vec<WasmExtensionHandle> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.wasm_extensions.clone()
    }

    #[allow(clippy::significant_drop_tightening)]
    pub fn set_session(&self, session: Arc<dyn ExtensionSession>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.session = Some(session);
        guard.ctx_generation = guard.ctx_generation.wrapping_add(1);
        self.refresh_snapshot_with_guard_release(guard);
    }

    /// Lock-free: reads from the RCU snapshot.
    pub fn session_handle(&self) -> Option<Arc<dyn ExtensionSession>> {
        self.read_snapshot().session.clone()
    }

    #[allow(clippy::significant_drop_tightening)]
    pub fn set_active_tools(&self, tools: Vec<String>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.active_tools = Some(tools);
        self.refresh_snapshot_with_guard_release(guard);
    }

    /// Lock-free: reads from the RCU snapshot.
    pub fn current_model(&self) -> (Option<String>, Option<String>) {
        let snap = self.read_snapshot();
        (snap.current_provider.clone(), snap.current_model_id.clone())
    }

    #[allow(clippy::significant_drop_tightening)]
    pub fn set_current_model(&self, provider: Option<String>, model_id: Option<String>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.current_provider = provider;
        guard.current_model_id = model_id;
        guard.ctx_generation = guard.ctx_generation.wrapping_add(1);
        self.refresh_snapshot_with_guard_release(guard);
    }

    /// Lock-free: reads from the RCU snapshot.
    pub fn current_thinking_level(&self) -> Option<String> {
        self.read_snapshot().current_thinking_level.clone()
    }

    pub fn set_current_thinking_level(&self, level: Option<String>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.current_thinking_level = level;
        guard.ctx_generation = guard.ctx_generation.wrapping_add(1);
        self.refresh_snapshot_with_guard_release(guard);
    }

    /// Collect tool definitions from all registered extensions.
    ///
    /// Uses the pre-computed snapshot (RCU) instead of locking the mutex.
    pub fn extension_tool_defs(&self) -> Vec<Value> {
        self.read_snapshot().all_tool_defs.clone()
    }

    /// Whether any extensions are currently loaded into this manager.
    pub fn has_loaded_extensions(&self) -> bool {
        self.read_snapshot().extension_count > 0
    }

    pub fn register(&self, payload: RegisterPayload) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let extension_id = payload.name.clone();
        // Update the hook bitmap with any new event hooks.
        for hook in &payload.event_hooks {
            guard.hook_bitmap.insert(hook.clone());
        }
        guard
            .extension_versions
            .insert(payload.name.clone(), payload.version.clone());
        guard.extensions.push(payload);
        guard.extension_ids.push(extension_id);
        self.refresh_snapshot_with_guard_release(guard);
    }

    pub fn has_command(&self, name: &str) -> bool {
        let needle = normalize_command(name);
        self.read_snapshot().command_names.contains(&needle)
    }

    /// Dynamically register a slash command at runtime (from a hostcall).
    pub fn register_command(&self, name: &str, description: Option<&str>) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = json!({
            "name": name,
            "description": description,
        });
        if let Some(ext) = guard.extensions.first_mut() {
            ext.slash_commands.push(entry);
        } else {
            guard
                .extension_versions
                .insert("__dynamic__".to_string(), "1.0.0".to_string());
            guard.extensions.push(RegisterPayload {
                name: "__dynamic__".to_string(),
                version: "1.0.0".to_string(),
                api_version: PROTOCOL_VERSION.to_string(),
                capabilities: Vec::new(),
                capability_manifest: None,
                tools: Vec::new(),
                slash_commands: vec![entry],
                shortcuts: Vec::new(),
                flags: Vec::new(),
                event_hooks: Vec::new(),
            });
            guard.extension_ids.push("__dynamic__".to_string());
        }
        self.refresh_snapshot_with_guard_release(guard);
    }

    fn extension_index_for_owner(
        inner: &ExtensionManagerInner,
        extension_id: &str,
    ) -> Result<usize> {
        if inner.extension_ids.len() != inner.extensions.len() {
            return Err(Error::extension(format!(
                "Extension identity table invariant violated: {} principals for {} registrations",
                inner.extension_ids.len(),
                inner.extensions.len()
            )));
        }
        inner
            .extension_ids
            .iter()
            .position(|candidate| candidate == extension_id)
            .ok_or_else(|| {
                Error::extension(format!(
                    "Unknown authoritative extension owner: {extension_id}"
                ))
            })
    }

    /// Register a slash command against its authoritative runtime principal.
    pub(super) fn register_command_for_extension(
        &self,
        extension_id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<()> {
        let normalized_name = js_command_route_name(name);
        let entry = json!({
            "name": normalized_name,
            "description": description,
            "extension_id": extension_id,
        });
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let target_index = Self::extension_index_for_owner(&guard, extension_id)
            .map_err(|err| Error::extension(format!("registerCommand: {err}")))?;
        for (index, extension) in guard.extensions.iter().enumerate() {
            if index == target_index {
                continue;
            }
            if extension.slash_commands.iter().any(|command| {
                extract_slash_command_name(command)
                    .is_some_and(|existing| js_command_route_name(&existing) == normalized_name)
            }) {
                return Err(Error::extension(format!(
                    "registerCommand: command name collision: {normalized_name}"
                )));
            }
        }
        let target = &mut guard.extensions[target_index];
        target.slash_commands.retain(|command| {
            extract_slash_command_name(command)
                .is_none_or(|existing| js_command_route_name(&existing) != normalized_name)
        });
        target.slash_commands.push(entry);
        let snapshot = Self::build_snapshot_from_inner(&guard);
        drop(guard);
        self.publish_snapshot(snapshot);
        Ok(())
    }

    /// Dynamically register a provider at runtime (from a hostcall).
    pub fn register_provider(&self, payload: Value) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.providers.push(payload);
        self.refresh_snapshot_with_guard_release(guard);
    }

    /// Register a provider against its authoritative runtime principal.
    pub(super) fn register_provider_for_extension(
        &self,
        extension_id: &str,
        mut payload: Value,
    ) -> Result<()> {
        let provider_id = payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let payload_is_object = payload.as_object_mut().is_some_and(|object| {
            object.insert(
                "extension_id".to_string(),
                Value::String(extension_id.to_string()),
            );
            true
        });
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _target_index = Self::extension_index_for_owner(&guard, extension_id)
            .map_err(|err| Error::extension(format!("registerProvider: {err}")))?;
        if guard.providers.iter().any(|provider| {
            provider.get("id").and_then(Value::as_str) == Some(provider_id.as_str())
                && provider
                    .get("extension_id")
                    .and_then(Value::as_str)
                    .is_some_and(|owner| owner != extension_id)
        }) {
            return Err(Error::extension(format!(
                "registerProvider: provider id collision: {provider_id}"
            )));
        }
        if !payload_is_object {
            return Err(Error::extension(
                "registerProvider: provider spec must be an object",
            ));
        }
        guard.providers.retain(|provider| {
            provider.get("id").and_then(Value::as_str) != Some(provider_id.as_str())
                || provider.get("extension_id").and_then(Value::as_str) != Some(extension_id)
        });
        guard.providers.push(payload);
        let snapshot = Self::build_snapshot_from_inner(&guard);
        drop(guard);
        self.publish_snapshot(snapshot);
        Ok(())
    }

    /// Dynamically register an MCP server at runtime (from a hostcall).
    pub fn register_mcp_server(&self, mut spec: Value) {
        let name = spec
            .get("name")
            .and_then(Value::as_str)
            .map_or("", str::trim);
        if name.is_empty() {
            tracing::warn!(
                event = "pi.extensions.mcp_invalid_spec",
                "Skipping MCP server registration with missing name"
            );
            return;
        }
        let name = name.to_string();
        if let Some(obj) = spec.as_object_mut()
            && obj.get("name").and_then(Value::as_str) != Some(name.as_str())
        {
            obj.insert("name".to_string(), Value::String(name.clone()));
        }
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.mcp_servers.retain(|s| {
            s.get("name")
                .and_then(Value::as_str)
                .is_none_or(|existing| existing != name.as_str())
        });
        guard.mcp_servers.push(spec);
        self.refresh_snapshot_with_guard_release(guard);
    }

    /// Dynamically register a flag at runtime (from a hostcall).
    pub fn register_flag(&self, spec: Value) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let name = spec.get("name").and_then(Value::as_str).unwrap_or_default();
        // Deduplicate: replace existing flag with the same name.
        guard
            .flags
            .retain(|f| f.get("name").and_then(Value::as_str).unwrap_or_default() != name);
        guard.flags.push(spec);
        self.refresh_snapshot_with_guard_release(guard);
    }

    /// Register a flag against its authoritative runtime principal.
    pub(super) fn register_flag_for_extension(
        &self,
        extension_id: &str,
        mut spec: Value,
    ) -> Result<()> {
        let flag_name = spec
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let spec_is_object = spec.as_object_mut().is_some_and(|object| {
            object.insert(
                "extension_id".to_string(),
                Value::String(extension_id.to_string()),
            );
            true
        });
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _target_index = Self::extension_index_for_owner(&guard, extension_id)
            .map_err(|err| Error::extension(format!("registerFlag: {err}")))?;
        if guard.flags.iter().any(|flag| {
            flag.get("name").and_then(Value::as_str) == Some(flag_name.as_str())
                && flag
                    .get("extension_id")
                    .and_then(Value::as_str)
                    .is_some_and(|owner| owner != extension_id)
        }) {
            return Err(Error::extension(format!(
                "registerFlag: flag name collision: {flag_name}"
            )));
        }
        if !spec_is_object {
            return Err(Error::extension(
                "registerFlag: flag spec must be an object",
            ));
        }
        guard.flags.retain(|flag| {
            flag.get("name").and_then(Value::as_str) != Some(flag_name.as_str())
                || flag.get("extension_id").and_then(Value::as_str) != Some(extension_id)
        });
        guard.flags.push(spec);
        let snapshot = Self::build_snapshot_from_inner(&guard);
        drop(guard);
        self.publish_snapshot(snapshot);
        Ok(())
    }

    /// Execute an extension slash command via the JS runtime.
    pub async fn execute_command(
        &self,
        command_name: &str,
        args: &str,
        timeout_ms: u64,
    ) -> Result<Value> {
        let timeout_ms = self.effective_timeout(timeout_ms);
        let runtime = self
            .runtime()
            .ok_or_else(|| Error::extension("Extension runtime not configured"))?;
        runtime
            .execute_command(
                command_name.to_string(),
                args.to_string(),
                Arc::new(json!({})),
                timeout_ms,
            )
            .await
    }

    /// Return extension-registered providers as raw JSON specs.
    ///
    /// Uses the pre-computed snapshot (RCU) instead of locking the mutex.
    pub fn extension_providers(&self) -> Vec<Value> {
        self.read_snapshot().providers.clone()
    }

    /// Return extension-registered MCP server specs as raw JSON.
    ///
    /// Uses the pre-computed snapshot (RCU) instead of locking the mutex.
    pub fn extension_mcp_servers(&self) -> Vec<Value> {
        self.read_snapshot().mcp_servers.clone()
    }

    /// Return true if an extension provider is backed by a JS `streamSimple` handler.
    pub fn provider_has_stream_simple(&self, provider_id: &str) -> bool {
        let needle = provider_id.trim();
        if needle.is_empty() {
            return false;
        }

        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.providers.iter().any(|provider_spec| {
            provider_spec
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == needle)
                && provider_spec
                    .get("hasStreamSimple")
                    .and_then(Value::as_bool)
                    .or_else(|| provider_spec.get("streamSimple").and_then(Value::as_bool))
                    .unwrap_or(false)
        })
    }

    /// Convert extension-registered providers into model entries suitable for
    /// merging into the `ModelRegistry`.
    #[allow(clippy::too_many_lines)]
    pub fn extension_model_entries(&self) -> Vec<crate::models::ModelEntry> {
        use crate::provider::{InputType, Model, ModelCost};
        use std::collections::HashMap;

        let snap = self.read_snapshot();
        let mut entries = Vec::new();

        for provider_spec in &snap.providers {
            let provider_id = provider_spec
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if provider_id.is_empty() {
                continue;
            }
            let base_url = provider_spec
                .get("baseUrl")
                .or_else(|| provider_spec.get("base_url"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let api_key_ref = provider_spec
                .get("apiKey")
                .or_else(|| provider_spec.get("api_key"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let api = provider_spec
                .get("api")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            // Resolve API key (supports env var names).
            let resolved_key = if api_key_ref.is_empty() {
                None
            } else {
                std::env::var(api_key_ref)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .or_else(|| Some(api_key_ref.to_string()))
            };

            // Extract OAuth config if present.
            let oauth_config = provider_spec
                .get("oauth")
                .and_then(Value::as_object)
                .and_then(|oauth| {
                    let auth_url = oauth.get("authUrl")?.as_str()?.to_string();
                    let token_url = oauth.get("tokenUrl")?.as_str()?.to_string();
                    let client_id = oauth.get("clientId")?.as_str()?.to_string();
                    let scopes = oauth
                        .get("scopes")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(Value::as_str)
                                .map(ToString::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    let redirect_uri = oauth
                        .get("redirectUri")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    Some(crate::models::OAuthConfig {
                        auth_url,
                        token_url,
                        client_id,
                        scopes,
                        redirect_uri,
                    })
                });

            let models = provider_spec
                .get("models")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            for model_spec in &models {
                let model_id = model_spec
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if model_id.is_empty() {
                    continue;
                }
                let model_name = model_spec
                    .get("name")
                    .and_then(Value::as_str)
                    .map_or_else(|| model_id.clone(), ToString::to_string);
                let model_api = model_spec
                    .get("api")
                    .and_then(Value::as_str)
                    .map_or_else(|| api.clone(), ToString::to_string);
                let reasoning = model_spec
                    .get("reasoning")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                #[allow(clippy::cast_possible_truncation)]
                let context_window = model_spec
                    .get("contextWindow")
                    .or_else(|| model_spec.get("context_window"))
                    .and_then(Value::as_u64)
                    .unwrap_or(128_000) as u32;
                #[allow(clippy::cast_possible_truncation)]
                let max_tokens = model_spec
                    .get("maxTokens")
                    .or_else(|| model_spec.get("max_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(16_384) as u32;

                let input = model_spec
                    .get("input")
                    .and_then(Value::as_array)
                    .map_or_else(
                        || vec![InputType::Text],
                        |arr| {
                            arr.iter()
                                .filter_map(Value::as_str)
                                .filter_map(|s| match s {
                                    "text" => Some(InputType::Text),
                                    "image" => Some(InputType::Image),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                        },
                    );

                entries.push(crate::models::ModelEntry {
                    model: Model {
                        id: model_id,
                        name: model_name,
                        api: model_api,
                        provider: provider_id.clone(),
                        base_url: base_url.clone(),
                        reasoning,
                        input,
                        cost: ModelCost {
                            input: 0.0,
                            output: 0.0,
                            cache_read: 0.0,
                            cache_write: 0.0,
                        },
                        context_window,
                        max_tokens,
                        headers: HashMap::new(),
                    },
                    api_key: resolved_key.clone(),
                    headers: HashMap::new(),
                    auth_header: true,
                    compat: None,
                    oauth_config: oauth_config.clone(),
                });
            }
        }
        entries
    }

    pub fn list_commands(&self) -> Vec<Value> {
        self.read_snapshot().all_commands.clone()
    }

    pub fn has_shortcut(&self, key_id: &str) -> bool {
        let needle = key_id.to_lowercase();
        self.read_snapshot().shortcut_key_ids.contains(&needle)
    }

    pub fn list_shortcuts(&self) -> Vec<Value> {
        self.read_snapshot().all_shortcuts.clone()
    }

    pub fn list_flags(&self) -> Vec<Value> {
        self.read_snapshot().all_flags.clone()
    }

    /// List all event hook names registered by all loaded extensions.
    pub fn list_event_hooks(&self) -> Vec<String> {
        self.read_snapshot().all_event_hooks.clone()
    }

    /// Execute an extension shortcut via the JS runtime.
    pub async fn execute_shortcut(
        &self,
        key_id: &str,
        ctx_payload: Value,
        timeout_ms: u64,
    ) -> Result<Value> {
        let timeout_ms = self.effective_timeout(timeout_ms);
        let runtime = self
            .runtime()
            .ok_or_else(|| Error::extension("Extension runtime not configured"))?;
        runtime
            .execute_shortcut(key_id.to_string(), Arc::new(ctx_payload), timeout_ms)
            .await
    }

    /// Set a flag value in the JS runtime for a specific extension.
    pub async fn set_flag_value(
        &self,
        extension_id: &str,
        flag_name: &str,
        value: Value,
    ) -> Result<()> {
        let runtime = self
            .runtime()
            .ok_or_else(|| Error::extension("Extension runtime not configured"))?;
        runtime
            .set_flag_value(extension_id.to_string(), flag_name.to_string(), value)
            .await
    }

    pub async fn request_ui(
        &self,
        mut request: ExtensionUiRequest,
    ) -> Result<Option<ExtensionUiResponse>> {
        let cx = Cx::for_request();
        if request.id.trim().is_empty() {
            request.id = Uuid::new_v4().to_string();
        }

        let (ui_sender, expects_response) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (guard.ui_sender.clone(), request.expects_response())
        };

        let Some(ui_sender) = ui_sender else {
            return Err(Error::extension("Extension UI sender not configured"));
        };

        if !expects_response {
            ui_sender
                .send(&cx, request)
                .await
                .map_err(|_| Error::extension("Extension UI channel closed"))?;
            return Ok(None);
        }

        let (tx, mut rx) = oneshot::channel();
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.pending_ui.insert(request.id.clone(), tx);
        }

        if ui_sender.send(&cx, request.clone()).await.is_err() {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending_ui
                .remove(&request.id);
            return Err(Error::extension("Extension UI channel closed"));
        }

        let response = if let Some(timeout_ms) = request.effective_timeout_ms() {
            match timeout(wall_now(), Duration::from_millis(timeout_ms), rx.recv(&cx)).await {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(_)) => Err(Error::extension("Extension UI response dropped")),
                Err(_) => Err(Error::extension("Extension UI request timed out")),
            }
        } else {
            rx.recv(&cx)
                .await
                .map_err(|_| Error::extension("Extension UI response dropped"))
        };

        match response {
            Ok(resp) => Ok(Some(resp)),
            Err(err) => {
                self.inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending_ui
                    .remove(&request.id);
                Err(err)
            }
        }
    }

    pub fn respond_ui(&self, response: ExtensionUiResponse) -> bool {
        let cx = Cx::for_request();
        let tx = {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.pending_ui.remove(&response.id)
        };
        tx.is_some_and(|sender| sender.send(&cx, response).is_ok())
    }

    /// Build the context payload from the current inner state.
    ///
    /// This is extracted so that it can be called once and the result cached
    /// across multiple rapid-fire event dispatches.
    async fn build_ctx_payload(
        has_ui: bool,
        session: Option<Arc<dyn ExtensionSession>>,
        cwd_override: Option<String>,
        model_registry_values: &HashMap<String, String>,
    ) -> Value {
        let mut ctx = serde_json::Map::new();
        ctx.insert("hasUI".into(), Value::Bool(has_ui));
        if let Some(cwd) = cwd_override.or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        }) {
            ctx.insert("cwd".into(), Value::String(cwd));
        }

        if !model_registry_values.is_empty() {
            let mut map = serde_json::Map::new();
            for (key, value) in model_registry_values {
                map.insert(key.clone(), Value::String(value.clone()));
            }
            ctx.insert("modelRegistry".into(), Value::Object(map));
        }

        if let Some(session) = session {
            let state = session.get_state().await;
            let entries = session.get_entries().await;
            let branch = session.get_branch().await;
            let leaf_entry = entries.last().cloned().unwrap_or(Value::Null);
            ctx.insert("sessionState".into(), state);
            ctx.insert("sessionEntries".into(), Value::Array(entries));
            ctx.insert("sessionBranch".into(), Value::Array(branch));
            ctx.insert("sessionLeafEntry".into(), leaf_entry);
        }

        Value::Object(ctx)
    }

    /// Obtain the context payload, using the cache when the generation matches.
    ///
    /// On cache miss the context is rebuilt from the current inner state and
    /// stored for future dispatches within the same generation.
    ///
    /// Returns `Arc<Value>` so callers can share the payload cheaply.
    async fn get_or_build_ctx_payload(&self) -> Arc<Value> {
        // Seqlock fast-path: read version without any lock.
        let version = self.snapshot_version();

        // Check cache under a brief mutex lock (Arc clone = atomic increment).
        let cached = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.ctx_cache.clone()
        };

        // Cache hit: version matches → return Arc (no deep clone).
        if let Some(ref c) = cached
            && c.generation == version
        {
            return Arc::clone(&c.payload);
        }

        // Cache miss: read state from the RCU snapshot (no mutex needed).
        let snap = self.read_snapshot();
        let has_ui = snap.has_ui;
        let session = snap.session.clone();
        let cwd = snap.cwd.clone();
        // Rebuild directly from the snapshot to avoid cloning the full
        // model-registry map on cache misses.
        let payload = Arc::new(
            Self::build_ctx_payload(has_ui, session, cwd, &snap.model_registry_values).await,
        );
        drop(snap);

        // Store in cache (best-effort; if another thread updated generation
        // between our snapshot and now, the cache will simply be stale and
        // rebuilt on the next call).
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Only store if our generation is still current.
            if guard.ctx_generation == version {
                guard.ctx_cache = Some(CachedEventContext {
                    generation: version,
                    payload: Arc::clone(&payload),
                });
            }
        }

        payload
    }

    #[allow(clippy::too_many_lines)]
    async fn dispatch_event_value(
        &self,
        event: ExtensionEventName,
        data: Option<Value>,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let started_at = Instant::now();
        let timeout_ms = self.effective_timeout(timeout_ms);
        let event_name = event.to_string();

        // --- Fast path: O(1) hook bitmap check via snapshot (no mutex) ---
        let snap = self.read_snapshot();
        let has_hook = snap.hook_bitmap.contains(&event_name);
        drop(snap);
        let runtime = if has_hook {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.runtime.clone()
        } else {
            None
        };

        #[cfg(feature = "wasm-host")]
        let (wasm_extensions, has_hook_wasm) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let has_hook_wasm = guard
                .wasm_extensions
                .iter()
                .any(|ext| ext.event_hooks().iter().any(|hook| hook == &event_name));
            (guard.wasm_extensions.clone(), has_hook_wasm)
        };

        let has_any_hook = {
            #[cfg(feature = "wasm-host")]
            {
                has_hook || has_hook_wasm
            }
            #[cfg(not(feature = "wasm-host"))]
            {
                has_hook
            }
        };

        if !has_any_hook {
            return Ok(None);
        }

        tracing::info!(
            event = "ext.event.start",
            event_name = %event_name,
            timeout_ms,
            "Extension event dispatch start"
        );

        // --- Use cached context when generation hasn't changed ---
        let ctx_payload = self.get_or_build_ctx_payload().await;

        let event_payload = match data {
            None => json!({ "type": event_name }),
            Some(Value::Object(mut map)) => {
                map.insert("type".into(), Value::String(event_name.clone()));
                Value::Object(map)
            }
            Some(other) => json!({ "type": event_name, "data": other }),
        };

        let response = if let Some(runtime) = runtime
            && has_hook
        {
            #[cfg(feature = "wasm-host")]
            let runtime_event_payload = event_payload.clone();
            #[cfg(not(feature = "wasm-host"))]
            let runtime_event_payload = event_payload;

            let js_response = runtime
                .dispatch_event(
                    event_name.clone(),
                    runtime_event_payload,
                    Arc::clone(&ctx_payload),
                    timeout_ms,
                )
                .await?;
            Some(js_response)
        } else {
            None
        };

        #[cfg(feature = "wasm-host")]
        let response = if has_hook_wasm {
            let mut wasm_payload = event_payload;
            if let Value::Object(map) = &mut wasm_payload {
                map.insert("ctx".into(), (*ctx_payload).clone());
            }
            Self::dispatch_wasm_event_value(
                &wasm_extensions,
                &event_name,
                &wasm_payload,
                timeout_ms,
            )
            .await?
            .or(response)
        } else {
            response
        };

        let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            event = "ext.event.end",
            event_name = %event_name,
            duration_ms,
            has_response = response.is_some(),
            "Extension event dispatch end"
        );

        Ok(response)
    }

    #[cfg(feature = "wasm-host")]
    async fn dispatch_wasm_event_value(
        extensions: &[WasmExtensionHandle],
        event_name: &str,
        event_payload: &Value,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        // Fan out across subscribed extensions concurrently. A single
        // slow or deadlocked extension bounds to its own per-instance
        // timeout and cannot block its peers — each `WasmExtensionHandle`
        // carries its own `Arc<AsyncMutex<Instance>>`, so these futures
        // don't share a lock.
        //
        // Results are still aggregated with "last-wins" semantics (in
        // stable iteration order over the filtered extension list), so
        // behavior matches the prior sequential loop when extensions
        // return a response deterministically.
        let calls = extensions
            .iter()
            .filter(|ext| ext.event_hooks().iter().any(|hook| hook == event_name))
            .map(|ext| async move {
                let ext_name = ext.registration().name.clone();
                let result = ext.handle_event_value(event_payload, timeout_ms).await;
                (ext_name, result)
            })
            .collect::<Vec<_>>();

        if calls.is_empty() {
            return Ok(None);
        }

        let results = futures::future::join_all(calls).await;

        // Walk every result so each failing extension gets its own
        // attributed warn! line before we bubble. The sequential code
        // also short-circuited on the first error, but because it was
        // sequential the later extensions never ran — here they did,
        // and we want the operator to see which ones also failed.
        let mut response = None;
        let mut first_err: Option<Error> = None;
        for (ext_name, result) in results {
            match result {
                Ok(Some(value)) => {
                    if first_err.is_none() {
                        response = Some(value);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        event = "ext.wasm.dispatch.failed",
                        extension_name = %ext_name,
                        event_name = %event_name,
                        error = %err,
                        "WASM extension event dispatch failed"
                    );
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }
        if let Some(err) = first_err {
            return Err(err);
        }
        Ok(response)
    }

    /// Dispatch an event to all registered extensions.
    ///
    /// Uses the per-event default deadline (see
    /// [`ExtensionEventName::default_timeout_ms`]): informational
    /// fire-and-forget events like `turn_start` get
    /// [`EXTENSION_INFO_EVENT_TIMEOUT_MS`], while actionable events
    /// (`tool_call`, `session_before_*`, etc.) keep the full
    /// [`EXTENSION_EVENT_TIMEOUT_MS`].
    pub async fn dispatch_event(
        &self,
        event: ExtensionEventName,
        data: Option<Value>,
    ) -> Result<()> {
        let _ = self
            .dispatch_event_value(event, data, event.default_timeout_ms())
            .await?;
        Ok(())
    }

    /// Dispatch an event to all registered extensions and return the raw response (if any).
    pub async fn dispatch_event_with_response(
        &self,
        event: ExtensionEventName,
        data: Option<Value>,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        self.dispatch_event_value(event, data, timeout_ms).await
    }

    pub async fn discover_resources(&self, cwd: &Path, reason: &str) -> ExtensionResourcePaths {
        let payload = json!({
            "cwd": cwd.display().to_string(),
            "reason": reason,
        });

        let response = match self
            .dispatch_event_with_response(
                ExtensionEventName::ResourcesDiscover,
                Some(payload),
                EXTENSION_EVENT_TIMEOUT_MS,
            )
            .await
        {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    event = "ext.resources_discover.failed",
                    error = %err,
                    "Failed to dispatch resources_discover"
                );
                return ExtensionResourcePaths::default();
            }
        };

        let Some(value) = response else {
            return ExtensionResourcePaths::default();
        };
        let Some(obj) = value.as_object() else {
            return ExtensionResourcePaths::default();
        };

        let collect_paths = |keys: &[&str]| -> Vec<String> {
            let mut out = Vec::new();
            for key in keys {
                let Some(value) = obj.get(*key) else {
                    continue;
                };
                match value {
                    Value::Array(values) => {
                        for entry in values {
                            if let Some(path) = entry.as_str() {
                                let trimmed = path.trim();
                                if !trimmed.is_empty() {
                                    out.push(trimmed.to_string());
                                }
                            }
                        }
                    }
                    Value::String(path) => {
                        let trimmed = path.trim();
                        if !trimmed.is_empty() {
                            out.push(trimmed.to_string());
                        }
                    }
                    _ => {}
                }
            }
            out
        };

        let skill_paths = collect_paths(&["skillPaths", "skill_paths"]);
        let prompt_paths = collect_paths(&["promptPaths", "prompt_paths"]);
        let theme_paths = collect_paths(&["themePaths", "theme_paths"]);

        if skill_paths.is_empty() && prompt_paths.is_empty() && theme_paths.is_empty() {
            return ExtensionResourcePaths::default();
        }

        let roots = self.extension_roots();
        ExtensionResourcePaths {
            skill_paths: Self::resolve_resource_paths(cwd, &roots, skill_paths),
            prompt_paths: Self::resolve_resource_paths(cwd, &roots, prompt_paths),
            theme_paths: Self::resolve_resource_paths(cwd, &roots, theme_paths),
        }
    }

    /// Dispatch a cancellable event to all registered extensions.
    pub async fn dispatch_cancellable_event(
        &self,
        event: ExtensionEventName,
        data: Option<Value>,
        timeout_ms: u64,
    ) -> Result<bool> {
        let Some(response) = self.dispatch_event_value(event, data, timeout_ms).await? else {
            return Ok(false);
        };

        Ok(response.as_bool() == Some(false)
            || response
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || response
                .get("cancel")
                .and_then(Value::as_bool)
                .unwrap_or(false))
    }

    /// Dispatch multiple fire-and-forget events in a single JS bridge call.
    ///
    /// Events that have no registered hooks are filtered out before crossing
    /// the bridge.  Returns `Ok(())` — individual per-event errors are logged
    /// but do not fail the batch.
    pub async fn dispatch_event_batch(
        &self,
        events: Vec<(ExtensionEventName, Option<Value>)>,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let timeout_ms = self.effective_timeout(EXTENSION_EVENT_TIMEOUT_MS);

        // Filter to events with hooks using snapshot (no mutex) for hook check.
        let snap = self.read_snapshot();
        let mut filtered = Vec::with_capacity(events.len());
        for (event, data) in &events {
            let event_name = event.to_string();
            if snap.hook_bitmap.contains(&event_name) {
                let event_payload = match data {
                    None => json!({ "type": event_name }),
                    Some(Value::Object(map)) => {
                        let mut map = map.clone();
                        map.insert("type".into(), Value::String(event_name.clone()));
                        Value::Object(map)
                    }
                    Some(other) => json!({ "type": event_name, "data": other }),
                };
                filtered.push((event_name, event_payload));
            }
        }
        drop(snap);
        let filtered_events = filtered;

        // Only lock mutex for js_runtime if there are events to dispatch.
        let runtime = if filtered_events.is_empty() {
            None
        } else {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.runtime.clone()
        };

        if filtered_events.is_empty() {
            return Ok(());
        }

        let ctx_payload = self.get_or_build_ctx_payload().await;

        if let Some(runtime) = runtime {
            let results = runtime
                .dispatch_event_batch(filtered_events, ctx_payload, timeout_ms)
                .await;
            if let Err(err) = &results {
                tracing::warn!(
                    event = "ext.event_batch.error",
                    error = %err,
                    "Batch event dispatch failed"
                );
            }
        }

        Ok(())
    }

    /// Dispatch a `tool_call` event to registered extensions and return the first
    /// blocking response (if any).
    #[allow(clippy::too_many_lines)]
    pub async fn dispatch_tool_call(
        &self,
        tool_call: &crate::model::ToolCall,
        timeout_ms: u64,
    ) -> Result<Option<ToolCallEventResult>> {
        let timeout_ms = self.effective_timeout(timeout_ms);
        let event_name = "tool_call";
        // O(1) hook bitmap check.
        let (runtime, has_hook_js) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let has_hook = guard.hook_bitmap.contains(event_name);
            (guard.runtime.clone(), has_hook)
        };

        #[cfg(feature = "wasm-host")]
        let (wasm_extensions, has_hook_wasm) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let has_hook_wasm = guard
                .wasm_extensions
                .iter()
                .any(|ext| ext.event_hooks().iter().any(|hook| hook == event_name));
            (guard.wasm_extensions.clone(), has_hook_wasm)
        };

        let has_any_hook = {
            #[cfg(feature = "wasm-host")]
            {
                has_hook_js || has_hook_wasm
            }
            #[cfg(not(feature = "wasm-host"))]
            {
                has_hook_js
            }
        };

        if !has_any_hook {
            return Ok(None);
        }

        // Reuse cached event context payload instead of rebuilding session state
        // on every tool_call dispatch.
        let ctx_payload = self.get_or_build_ctx_payload().await;
        let event_payload = json!({
            "type": "tool_call",
            "toolName": tool_call.name.clone(),
            "toolCallId": tool_call.id.clone(),
            "input": tool_call.arguments.clone()
        });

        let mut response: Option<ToolCallEventResult> = None;

        if let Some(runtime) = runtime
            && has_hook_js
        {
            let js_response = runtime
                .dispatch_event(
                    event_name.to_string(),
                    event_payload.clone(),
                    Arc::clone(&ctx_payload),
                    timeout_ms,
                )
                .await?;
            if !js_response.is_null() {
                let parsed: ToolCallEventResult = serde_json::from_value(js_response)
                    .map_err(|err| Error::extension(err.to_string()))?;
                if parsed.block {
                    return Ok(Some(parsed));
                }
                response = Some(parsed);
            }
        }

        #[cfg(feature = "wasm-host")]
        if has_hook_wasm {
            let mut wasm_payload = event_payload;
            if let Value::Object(map) = &mut wasm_payload {
                map.insert("ctx".to_string(), (*ctx_payload).clone());
            }
            if let Some(value) = Self::dispatch_wasm_event_value(
                &wasm_extensions,
                event_name,
                &wasm_payload,
                timeout_ms,
            )
            .await?
            {
                let parsed: ToolCallEventResult = serde_json::from_value(value)
                    .map_err(|err| Error::extension(err.to_string()))?;
                if parsed.block {
                    return Ok(Some(parsed));
                }
                response = response.or(Some(parsed));
            }
        }

        Ok(response)
    }

    /// Dispatch a `tool_result` event to registered extensions and return the
    /// last handler response (if any).
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::significant_drop_tightening)]
    pub async fn dispatch_tool_result(
        &self,
        tool_call: &crate::model::ToolCall,
        output: &crate::tools::ToolOutput,
        is_error: bool,
        timeout_ms: u64,
    ) -> Result<Option<ToolResultEventResult>> {
        let timeout_ms = self.effective_timeout(timeout_ms);
        let event_name = "tool_result";

        // O(1) hook bitmap check.
        let (runtime, has_hook_js) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let has_hook = guard.hook_bitmap.contains(event_name);
            (guard.runtime.clone(), has_hook)
        };

        #[cfg(feature = "wasm-host")]
        let (wasm_extensions, has_hook_wasm) = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let has_hook_wasm = guard
                .wasm_extensions
                .iter()
                .any(|ext| ext.event_hooks().iter().any(|hook| hook == event_name));
            (guard.wasm_extensions.clone(), has_hook_wasm)
        };

        let has_any_hook = {
            #[cfg(feature = "wasm-host")]
            {
                has_hook_js || has_hook_wasm
            }
            #[cfg(not(feature = "wasm-host"))]
            {
                has_hook_js
            }
        };

        if !has_any_hook {
            return Ok(None);
        }

        // Use cached context payload.
        let ctx_payload = self.get_or_build_ctx_payload().await;

        let event_payload = json!({
            "type": "tool_result",
            "toolName": tool_call.name.clone(),
            "toolCallId": tool_call.id.clone(),
            "input": tool_call.arguments.clone(),
            "content": output.content.clone(),
            "details": output.details.clone(),
            "isError": is_error
        });

        let mut response: Option<ToolResultEventResult> = None;

        if let Some(runtime) = runtime
            && has_hook_js
        {
            let js_response = runtime
                .dispatch_event(
                    event_name.to_string(),
                    event_payload.clone(),
                    Arc::clone(&ctx_payload),
                    timeout_ms,
                )
                .await?;
            if !js_response.is_null() {
                response = Some(
                    serde_json::from_value(js_response)
                        .map_err(|err| Error::extension(err.to_string()))?,
                );
            }
        }

        #[cfg(feature = "wasm-host")]
        if has_hook_wasm {
            let mut wasm_payload = event_payload;
            if let Value::Object(map) = &mut wasm_payload {
                map.insert("ctx".into(), (*ctx_payload).clone());
            }
            if let Some(value) = Self::dispatch_wasm_event_value(
                &wasm_extensions,
                event_name,
                &wasm_payload,
                timeout_ms,
            )
            .await?
            {
                response = Some(
                    serde_json::from_value(value)
                        .map_err(|err| Error::extension(err.to_string()))?,
                );
            }
        }

        Ok(response)
    }

    /// Invalidate the context cache, forcing the next dispatch to rebuild it.
    ///
    /// Call this when session content changes outside the normal setter flow
    /// (e.g. after appending messages to a session).
    pub fn invalidate_ctx_cache(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.ctx_generation = guard.ctx_generation.wrapping_add(1);
        self.refresh_snapshot_with_guard_release(guard);
    }

    /// Check whether any extension has registered a hook for the given event
    /// name.  O(1) lookup via pre-computed bitmap.
    ///
    /// Lock-free: reads from the RCU snapshot.
    pub fn has_hook_for(&self, event_name: &str) -> bool {
        let snap = self.read_snapshot();
        snap.hook_bitmap.contains(event_name)
    }

    /// Returns `true` if at least one event hook is registered across all
    /// extensions.  Use this as a fast-path gate to skip event serialization
    /// entirely when no hooks are present.
    ///
    /// Lock-free: reads from the RCU snapshot.
    pub fn has_any_event_hooks(&self) -> bool {
        self.read_snapshot().has_any_hooks
    }
}
