use super::commands::{model_entry_matches, resolve_model_key_from_default_auth};
use super::*;
use crate::models::model_requires_configured_credential;

impl PiApp {
    fn normalize_model_key(entry: &ModelEntry) -> (String, String) {
        let canonical_provider =
            crate::provider_metadata::canonical_provider_id(entry.model.provider.as_str())
                .unwrap_or(entry.model.provider.as_str());
        (
            canonical_provider.to_ascii_lowercase(),
            entry.model.id.to_ascii_lowercase(),
        )
    }

    fn unique_model_count(models: &[ModelEntry]) -> usize {
        models
            .iter()
            .map(Self::normalize_model_key)
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    fn available_models_with_credentials(&self) -> Vec<ModelEntry> {
        let auth = crate::auth::AuthStorage::load(crate::config::Config::auth_path()).ok();
        let mut provider_has_credential: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        let mut filtered = Vec::new();
        for entry in &self.available_models {
            let provider = entry.model.provider.as_str();
            let canonical = crate::provider_metadata::canonical_provider_id(provider)
                .unwrap_or(provider)
                .to_ascii_lowercase();
            let requires_configured_credential = model_requires_configured_credential(entry);
            let has_inline_key = entry
                .api_key
                .as_ref()
                .is_some_and(|key| !key.trim().is_empty());
            let has_auth_key = auth.as_ref().is_some_and(|storage| {
                *provider_has_credential
                    .entry(canonical.clone())
                    .or_insert_with(|| storage.resolve_api_key(&canonical, None).is_some())
            });
            if !requires_configured_credential || has_inline_key || has_auth_key {
                filtered.push(entry.clone());
            }
        }

        filtered.sort_by_key(Self::normalize_model_key);
        filtered.dedup_by(|left, right| model_entry_matches(left, right));
        filtered
    }

    /// Open the model selector overlay.
    pub fn open_model_selector(&mut self) {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot switch models while processing".to_string());
            return;
        }

        if self.available_models.is_empty() {
            self.status_message = Some("No models available".to_string());
            return;
        }

        let mut overlay = crate::model_selector::ModelSelectorOverlay::new(&self.available_models);
        overlay.set_max_visible(super::overlay_max_visible(self.term_height));
        self.model_selector = Some(overlay);
    }

    pub(super) fn open_model_selector_configured_only(&mut self) {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot switch models while processing".to_string());
            return;
        }

        if self.available_models.is_empty() {
            self.status_message = Some("No models available".to_string());
            return;
        }

        let filtered = self.available_models_with_credentials();
        if filtered.is_empty() {
            self.status_message = Some(
                "No models are ready to use. Configure credentials with /login <provider>."
                    .to_string(),
            );
            return;
        }

        let mut overlay = crate::model_selector::ModelSelectorOverlay::new(&filtered);
        overlay.set_configured_only_scope(Self::unique_model_count(&self.available_models));
        overlay.set_max_visible(super::overlay_max_visible(self.term_height));
        self.model_selector = Some(overlay);
    }

    /// Handle keyboard input while the model selector is open.
    pub fn handle_model_selector_key(&mut self, key: &KeyMsg) -> Option<Cmd> {
        let selector = self.model_selector.as_mut()?;

        match key.key_type {
            KeyType::Up => selector.select_prev(),
            KeyType::Down => selector.select_next(),
            KeyType::Runes if key.runes == ['k'] => selector.select_prev(),
            KeyType::Runes if key.runes == ['j'] => selector.select_next(),
            KeyType::PgDown => selector.select_page_down(),
            KeyType::PgUp => selector.select_page_up(),
            KeyType::Backspace => selector.pop_char(),
            KeyType::Runes => selector.push_chars(key.runes.iter().copied()),
            KeyType::Enter => {
                let selected = selector.selected_item().cloned();
                self.model_selector = None;
                if let Some(selected) = selected {
                    self.apply_model_selection(&selected);
                } else {
                    self.status_message = Some("No model selected".to_string());
                }
                return None;
            }
            KeyType::Esc | KeyType::CtrlC => {
                self.model_selector = None;
                self.status_message = Some("Model selector cancelled".to_string());
            }
            _ => {} // consume all other input while selector is open
        }

        None
    }

    /// Apply a model selection from the model selector overlay.
    fn apply_model_selection(&mut self, selected: &crate::model_selector::ModelKey) {
        // Find the matching ModelEntry from available_models
        let entry = self
            .available_models
            .iter()
            .find(|e| {
                e.model.provider.eq_ignore_ascii_case(&selected.provider)
                    && e.model.id.eq_ignore_ascii_case(&selected.id)
            })
            .cloned();

        let Some(next) = entry else {
            self.status_message = Some(format!("Model {} not found", selected.full_id()));
            return;
        };

        if model_entry_matches(&next, &self.model_entry) {
            self.status_message = Some(format!("Already using {}", selected.full_id()));
            return;
        }

        let resolved_key_opt = resolve_model_key_from_default_auth(&next);
        if model_requires_configured_credential(&next) && resolved_key_opt.is_none() {
            self.status_message = Some(format!(
                "Missing credentials for provider {}. Run /login {}.",
                next.model.provider, next.model.provider
            ));
            return;
        }

        let provider_impl = match providers::create_provider(&next, self.extensions.as_ref()) {
            Ok(p) => p,
            Err(err) => {
                self.status_message = Some(err.to_string());
                return;
            }
        };

        if let Err(message) = self.switch_active_model(
            &next,
            provider_impl,
            resolved_key_opt.as_deref(),
            "selector",
        ) {
            self.status_message = Some(message);
            return;
        }
        self.status_message = Some(format!("Switched model: {}", self.model));
    }

    /// Render the model selector overlay.
    #[allow(clippy::too_many_lines)]
    pub(super) fn render_model_selector(
        &self,
        selector: &crate::model_selector::ModelSelectorOverlay,
    ) -> String {
        use std::fmt::Write;
        let mut output = String::new();

        let _ = writeln!(output, "\n  {}", self.styles.title.render("Select a model"));
        if selector.configured_only() {
            let _ = writeln!(
                output,
                "  {}",
                self.styles
                    .muted
                    .render("Only showing models that are ready to use (see README for details)")
            );
        }

        // Search field
        let query = selector.query();
        let search_line = if query.is_empty() {
            if selector.configured_only() {
                "  >".to_string()
            } else {
                "  > (type to filter)".to_string()
            }
        } else {
            format!("  > {query}")
        };
        let _ = writeln!(output, "{}", self.styles.muted.render(&search_line));

        let _ = writeln!(
            output,
            "  {}",
            self.styles.muted.render("─".repeat(50).as_str())
        );

        if selector.filtered_len() == 0 {
            let _ = writeln!(
                output,
                "  {}",
                self.styles.muted_italic.render("No matching models.")
            );
        } else {
            let offset = selector.scroll_offset();
            let visible_count = selector.max_visible().min(selector.filtered_len());
            let end = (offset + visible_count).min(selector.filtered_len());

            let current_full = format!(
                "{}/{}",
                self.model_entry.model.provider, self.model_entry.model.id
            );

            for idx in offset..end {
                let is_selected = idx == selector.selected_index();
                let prefix = if is_selected { ">" } else { " " };

                if let Some(key) = selector.item_at(idx) {
                    let full = key.full_id();
                    let is_current = full.eq_ignore_ascii_case(&current_full);
                    let marker = if is_current { " *" } else { "" };
                    let mut row =
                        String::with_capacity(prefix.len() + 1 + full.len() + marker.len() + 32);
                    row.push_str(prefix);
                    row.push(' ');
                    row.push_str(&full);
                    row.push_str(marker);
                    if let Some(badge) = selector
                        .routing_evidence_for(key)
                        .and_then(crate::model_routing::ModelRoutingEvidence::row_badge)
                    {
                        row.push(' ');
                        row.push_str(&badge);
                    }
                    let rendered = if is_selected {
                        self.styles.accent_bold.render(&row)
                    } else if is_current {
                        self.styles.accent.render(&row)
                    } else {
                        self.styles.muted.render(&row)
                    };
                    let _ = writeln!(output, "  {rendered}");
                }
            }

            if selector.filtered_len() > visible_count {
                let _ = writeln!(
                    output,
                    "  {}",
                    self.styles.muted.render(&format!(
                        "({}-{} of {})",
                        offset + 1,
                        end,
                        selector.filtered_len()
                    ))
                );
            }

            if selector.configured_only() {
                let _ = writeln!(
                    output,
                    "  {}",
                    self.styles.muted.render(&format!(
                        "({}/{})",
                        selector.filtered_len(),
                        selector.source_total()
                    ))
                );
            }

            if let Some(selected) = selector.selected_item()
                && let Some(entry) = self.available_models.iter().find(|entry| {
                    entry
                        .model
                        .provider
                        .eq_ignore_ascii_case(&selected.provider)
                        && entry.model.id.eq_ignore_ascii_case(&selected.id)
                })
            {
                let _ = writeln!(
                    output,
                    "\n  {}",
                    self.styles
                        .muted
                        .render(&format!("Model Name: {}", entry.model.name))
                );

                if let Some(evidence) = selector.routing_evidence_for(selected) {
                    let summary = evidence
                        .row_badge()
                        .unwrap_or_else(|| format!("[{}]", evidence.decision.short_label()));
                    let _ = writeln!(
                        output,
                        "  {}",
                        self.styles.muted.render(&format!("Routing: {summary}"))
                    );
                }
            }
        }

        let _ = writeln!(
            output,
            "\n  {}",
            self.styles
                .muted_italic
                .render("↑/↓/j/k/PgUp/PgDn: navigate  Enter: select  Esc: cancel  * = current")
        );
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig};
    use crate::model::{StreamEvent, Usage};
    use crate::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
    use crate::resources::{ResourceCliOptions, ResourceLoader};
    use crate::session::Session;
    use crate::tools::ToolRegistry;
    use asupersync::channel::mpsc;
    use asupersync::runtime::RuntimeBuilder;
    use futures::stream;
    use std::collections::HashMap;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::OnceLock;

    struct DummyProvider;

    #[async_trait::async_trait]
    impl Provider for DummyProvider {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn api(&self) -> &'static str {
            "dummy"
        }

        fn model_id(&self) -> &'static str {
            "dummy-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(stream::empty()))
        }
    }

    fn runtime_handle() -> asupersync::runtime::RuntimeHandle {
        static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
        RT.get_or_init(|| {
            RuntimeBuilder::multi_thread()
                .blocking_threads(1, 8)
                .build()
                .expect("build runtime")
        })
        .handle()
    }

    fn model_entry(provider: &str, id: &str, api_key: Option<&str>) -> ModelEntry {
        ModelEntry {
            model: Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "openai-completions".to_string(),
                provider: provider.to_string(),
                base_url: "https://example.invalid".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: api_key.map(str::to_string),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    }

    fn build_test_app(current: ModelEntry, available: Vec<ModelEntry>) -> PiApp {
        let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig::default(),
        );
        let session = Arc::new(asupersync::sync::Mutex::new(Session::in_memory()));
        let resources = ResourceLoader::empty(false);
        let resource_cli = ResourceCliOptions {
            no_skills: false,
            no_prompt_templates: false,
            no_extensions: false,
            no_themes: false,
            skill_paths: Vec::new(),
            prompt_paths: Vec::new(),
            extension_paths: Vec::new(),
            theme_paths: Vec::new(),
        };
        let (event_tx, _event_rx) = mpsc::channel(64);
        let config = Config {
            last_changelog_version: Some(crate::platform::VERSION.to_string()),
            ..Config::default()
        };
        PiApp::new(
            agent,
            session,
            config,
            resources,
            resource_cli,
            Path::new(".").to_path_buf(),
            current,
            Vec::new(),
            available,
            None,
            Vec::new(),
            event_tx,
            runtime_handle(),
            false,
            false,
            None,
            Some(KeyBindings::new()),
            Vec::new(),
            Usage::default(),
        )
    }

    #[test]
    fn apply_model_selection_replaces_stream_options_api_key_and_headers() {
        let current = model_entry("openai", "gpt-4o-mini", Some("old-key"));
        let mut next = model_entry("openrouter", "openai/gpt-4o-mini", Some("next-key"));
        next.headers
            .insert("x-provider-header".to_string(), "next".to_string());

        let mut app = build_test_app(current.clone(), vec![current, next.clone()]);

        {
            let mut guard = app.agent.try_lock().expect("agent lock");
            guard.stream_options_mut().api_key = Some("stale-token".to_string());
            guard
                .stream_options_mut()
                .headers
                .insert("x-stale".to_string(), "stale".to_string());
        }

        app.apply_model_selection(&crate::model_selector::ModelKey {
            provider: next.model.provider.clone(),
            id: next.model.id,
        });

        let mut guard = app.agent.try_lock().expect("agent lock");
        assert_eq!(
            guard.stream_options_mut().api_key.as_deref(),
            Some("next-key")
        );
        assert_eq!(
            guard
                .stream_options_mut()
                .headers
                .get("x-provider-header")
                .map(String::as_str),
            Some("next")
        );
        assert!(
            !guard.stream_options_mut().headers.contains_key("x-stale"),
            "switching models must replace stale provider headers"
        );
    }

    #[test]
    fn apply_model_selection_clears_stale_api_key_when_next_model_has_no_key() {
        let current = model_entry("openai", "gpt-4o-mini", Some("old-key"));
        let mut next = model_entry("ollama", "llama3.2", None);
        next.auth_header = false;
        let mut app = build_test_app(current.clone(), vec![current, next.clone()]);

        {
            let mut guard = app.agent.try_lock().expect("agent lock");
            guard.stream_options_mut().api_key = Some("stale-token".to_string());
        }

        app.apply_model_selection(&crate::model_selector::ModelKey {
            provider: next.model.provider.clone(),
            id: next.model.id,
        });

        let mut guard = app.agent.try_lock().expect("agent lock");
        assert!(
            guard.stream_options_mut().api_key.is_none(),
            "switching to a keyless model must clear stale API key"
        );
    }

    #[test]
    fn apply_model_selection_clamps_thinking_level_for_non_reasoning_targets() {
        let current = model_entry("openai", "gpt-5.2", Some("old-key"));
        let mut next = model_entry("ollama", "llama3.2", None);
        next.auth_header = false;
        next.model.reasoning = false;
        let mut app = build_test_app(current.clone(), vec![current, next.clone()]);

        {
            let mut guard = app.agent.try_lock().expect("agent lock");
            guard.stream_options_mut().thinking_level = Some(crate::model::ThinkingLevel::High);
        }
        {
            let mut guard = app.session.try_lock().expect("session lock");
            guard.header.thinking_level = Some(crate::model::ThinkingLevel::High.to_string());
        }

        app.apply_model_selection(&crate::model_selector::ModelKey {
            provider: next.model.provider.clone(),
            id: next.model.id,
        });

        let mut agent_guard = app.agent.try_lock().expect("agent lock");
        assert_eq!(
            agent_guard.stream_options_mut().thinking_level,
            Some(crate::model::ThinkingLevel::Off)
        );
        drop(agent_guard);

        let session_guard = app.session.try_lock().expect("session lock");
        assert_eq!(session_guard.header.thinking_level.as_deref(), Some("off"));
    }

    #[test]
    fn configured_only_selector_includes_keyless_ready_models() {
        let mut keyless = model_entry("ollama", "llama3.2", None);
        keyless.auth_header = false;

        let mut requires_creds = model_entry("acme-remote", "cloud-model", None);
        requires_creds.auth_header = true;

        let mut app = build_test_app(keyless.clone(), vec![keyless, requires_creds]);
        app.open_model_selector_configured_only();

        let selector = app
            .model_selector
            .as_ref()
            .expect("configured-only selector should open when keyless models are ready");
        let mut ids = Vec::new();
        for idx in 0..selector.filtered_len() {
            if let Some(item) = selector.item_at(idx) {
                ids.push(item.full_id());
            }
        }

        assert!(
            ids.iter().any(|id| id == "ollama/llama3.2"),
            "keyless local model must be considered ready"
        );
        assert!(
            !ids.iter().any(|id| id == "acme-remote/cloud-model"),
            "credentialed providers without configured auth should not appear"
        );
    }

    #[test]
    fn configured_only_selector_keeps_unknown_keyless_provider_models() {
        let mut unknown_keyless = model_entry("acme-local", "dev-model", None);
        unknown_keyless.auth_header = false;
        let mut unknown_requires = model_entry("acme-remote", "cloud-model", None);
        unknown_requires.auth_header = true;

        let mut app = build_test_app(
            unknown_keyless.clone(),
            vec![unknown_keyless, unknown_requires],
        );
        app.open_model_selector_configured_only();

        let selector = app
            .model_selector
            .as_ref()
            .expect("unknown keyless model should keep selector available");
        let mut ids = Vec::new();
        for idx in 0..selector.filtered_len() {
            if let Some(item) = selector.item_at(idx) {
                ids.push(item.full_id());
            }
        }

        assert!(ids.iter().any(|id| id == "acme-local/dev-model"));
        assert!(!ids.iter().any(|id| id == "acme-remote/cloud-model"));
    }

    #[test]
    fn configured_only_selector_renders_degraded_routing_evidence() {
        let mut keyless = model_entry("ollama", "llama3.2", None);
        keyless.auth_header = false;

        let mut requires_creds = model_entry("acme-remote", "cloud-model", None);
        requires_creds.auth_header = true;

        let mut app = build_test_app(keyless.clone(), vec![keyless.clone(), requires_creds]);
        app.open_model_selector_configured_only();

        let metrics = crate::model_routing::ProviderRoutingMetrics::new("ollama", 9_500, 12)
            .for_model("llama3.2")
            .with_latency_p95_ms(3_000)
            .with_error_rate(0.01);
        let evidence =
            crate::model_routing::evaluate_model_routing(crate::model_routing::RoutingEvaluation {
                model: &keyless,
                metrics: Some(&metrics),
                now_ms: 10_000,
                configured_only_scope: true,
                user_override: false,
                thresholds: crate::model_routing::ProviderRoutingThresholds {
                    degraded_latency_ms: 2_000,
                    ..crate::model_routing::ProviderRoutingThresholds::default()
                },
            });
        app.model_selector
            .as_mut()
            .expect("configured-only selector should open")
            .set_routing_evidence([evidence]);

        let selector = app.model_selector.as_ref().expect("selector");
        let rendered = app.render_model_selector(selector);
        assert!(rendered.contains("ollama/llama3.2 * [degraded: latency]"));
        assert!(rendered.contains("Routing: [degraded: latency]"));
        assert!(!rendered.contains("acme-remote/cloud-model"));
    }
}
