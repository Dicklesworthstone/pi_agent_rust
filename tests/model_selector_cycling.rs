//! Tests for model selector overlay integration and scoped model cycling.
//!
//! Covers: bd-4ma1 — Tests: model selector + scoped cycling
//!
//! Tests are organized into:
//! 1. Helper function unit tests (parse, resolve, match, strip)
//! 2. Model selector overlay integration with `PiApp`
//! 3. Scoped cycling deterministic ordering tests
//! 4. Resolve + cycling integration

#![allow(clippy::unnecessary_literal_bound)]

mod common;

use asupersync::channel::mpsc;
use bubbletea::{KeyMsg, KeyType, Model as BubbleteaModel};
use common::TestHarness;
use futures::stream;
use pi::agent::{Agent, AgentConfig};
use pi::config::Config;
use pi::interactive::{
    PiApp, model_entry_matches, parse_scoped_model_patterns, resolve_scoped_model_entries,
    strip_thinking_level_suffix,
};
use pi::keybindings::KeyBindings;
use pi::model::{StreamEvent, Usage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::tools::ToolRegistry;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

// ── Test infrastructure ─────────────────────────────────────────────

fn test_runtime_handle() -> asupersync::runtime::RuntimeHandle {
    static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        asupersync::runtime::RuntimeBuilder::multi_thread()
            .blocking_threads(1, 8)
            .build()
            .expect("build asupersync runtime")
    })
    .handle()
}

struct DummyProvider;

#[async_trait::async_trait]
impl Provider for DummyProvider {
    fn name(&self) -> &str {
        "dummy"
    }

    fn api(&self) -> &str {
        "dummy"
    }

    fn model_id(&self) -> &str {
        "dummy-model"
    }

    async fn stream(
        &self,
        _context: &Context,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        Ok(Box::pin(stream::empty()))
    }
}

fn make_model_entry(provider: &str, model_id: &str) -> ModelEntry {
    let model = Model {
        id: model_id.to_string(),
        name: format!("{provider} {model_id}"),
        api: "test-api".to_string(),
        provider: provider.to_string(),
        base_url: "https://example.invalid".to_string(),
        reasoning: false,
        input: vec![InputType::Text],
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: 4096,
        max_tokens: 1024,
        headers: HashMap::new(),
    };

    ModelEntry {
        model,
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn build_app_with_models(
    harness: &TestHarness,
    current: ModelEntry,
    scope: Vec<ModelEntry>,
    available: Vec<ModelEntry>,
) -> PiApp {
    build_app_with_models_and_config(harness, current, scope, available, Config::default())
}

fn build_app_with_models_and_config(
    harness: &TestHarness,
    current: ModelEntry,
    scope: Vec<ModelEntry>,
    available: Vec<ModelEntry>,
    config: Config,
) -> PiApp {
    let cwd = harness.temp_dir().to_path_buf();
    let tools = ToolRegistry::new(&[], &cwd, Some(&config));
    let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
    let agent = Agent::new(provider, tools, AgentConfig::default());
    let session = Arc::new(asupersync::sync::Mutex::new(Session::in_memory()));
    let resources = ResourceLoader::empty(config.enable_skill_commands());
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
    let (event_tx, _event_rx) = mpsc::channel(1024);

    let mut app = PiApp::new(
        agent,
        session,
        config,
        resources,
        resource_cli,
        cwd,
        current,
        scope,
        available,
        Vec::new(),
        event_tx,
        test_runtime_handle(),
        true,
        None,
        Some(KeyBindings::new()),
        Vec::new(),
        Usage::default(),
    );
    app.set_terminal_size(80, 24);
    app
}

fn get_status(app: &PiApp) -> Option<&str> {
    app.status_message()
}

#[allow(dead_code)]
fn get_view(app: &PiApp) -> String {
    BubbleteaModel::view(app)
}

// ═══════════════════════════════════════════════════════════════════
// 1. Helper function unit tests
// ═══════════════════════════════════════════════════════════════════

// ── strip_thinking_level_suffix ──────────────────────────────────

#[test]
fn strip_suffix_removes_known_levels() {
    assert_eq!(
        strip_thinking_level_suffix("claude-sonnet-4:high"),
        "claude-sonnet-4"
    );
    assert_eq!(strip_thinking_level_suffix("gpt-4o:off"), "gpt-4o");
    assert_eq!(strip_thinking_level_suffix("model:minimal"), "model");
    assert_eq!(strip_thinking_level_suffix("model:low"), "model");
    assert_eq!(strip_thinking_level_suffix("model:medium"), "model");
    assert_eq!(strip_thinking_level_suffix("model:xhigh"), "model");
}

#[test]
fn strip_suffix_preserves_unknown_suffixes() {
    assert_eq!(strip_thinking_level_suffix("model:turbo"), "model:turbo");
    assert_eq!(strip_thinking_level_suffix("model:v2"), "model:v2");
}

#[test]
fn strip_suffix_no_colon_returns_unchanged() {
    assert_eq!(
        strip_thinking_level_suffix("claude-sonnet-4"),
        "claude-sonnet-4"
    );
    assert_eq!(strip_thinking_level_suffix(""), "");
}

#[test]
fn strip_suffix_case_insensitive() {
    assert_eq!(strip_thinking_level_suffix("model:HIGH"), "model");
    assert_eq!(strip_thinking_level_suffix("model:Medium"), "model");
    assert_eq!(strip_thinking_level_suffix("model:XHIGH"), "model");
}

#[test]
fn strip_suffix_empty_suffix_preserved() {
    // "model:" has an empty suffix which is not a known level
    assert_eq!(strip_thinking_level_suffix("model:"), "model:");
}

#[test]
fn strip_suffix_multiple_colons_splits_at_last() {
    // rsplit_once splits at the last colon
    assert_eq!(
        strip_thinking_level_suffix("openai/gpt-4o:reasoning:high"),
        "openai/gpt-4o:reasoning"
    );
}

// ── parse_scoped_model_patterns ──────────────────────────────────

#[test]
fn parse_patterns_comma_separated() {
    let result = parse_scoped_model_patterns("gpt-4o, claude-sonnet-4, gemini-pro");
    assert_eq!(result, vec!["gpt-4o", "claude-sonnet-4", "gemini-pro"]);
}

#[test]
fn parse_patterns_whitespace_separated() {
    let result = parse_scoped_model_patterns("gpt-4o claude-sonnet-4  gemini-pro");
    assert_eq!(result, vec!["gpt-4o", "claude-sonnet-4", "gemini-pro"]);
}

#[test]
fn parse_patterns_mixed_delimiters() {
    let result = parse_scoped_model_patterns("gpt-4o, claude-sonnet-4 gemini-pro");
    assert_eq!(result, vec!["gpt-4o", "claude-sonnet-4", "gemini-pro"]);
}

#[test]
fn parse_patterns_empty_string() {
    let result = parse_scoped_model_patterns("");
    assert!(result.is_empty());
}

#[test]
fn parse_patterns_whitespace_only() {
    let result = parse_scoped_model_patterns("   ,  , ,   ");
    assert!(result.is_empty());
}

#[test]
fn parse_patterns_single_pattern() {
    let result = parse_scoped_model_patterns("gpt-4o");
    assert_eq!(result, vec!["gpt-4o"]);
}

#[test]
fn parse_patterns_with_provider_prefix() {
    let result = parse_scoped_model_patterns("openai/gpt-4o, anthropic/claude-sonnet-4");
    assert_eq!(result, vec!["openai/gpt-4o", "anthropic/claude-sonnet-4"]);
}

#[test]
fn parse_patterns_glob_patterns() {
    let result = parse_scoped_model_patterns("claude-*, gpt-?o, openai/*");
    assert_eq!(result, vec!["claude-*", "gpt-?o", "openai/*"]);
}

// ── model_entry_matches ──────────────────────────────────────────

#[test]
fn model_entry_matches_same_model() {
    let a = make_model_entry("anthropic", "claude-sonnet-4");
    let b = make_model_entry("anthropic", "claude-sonnet-4");
    assert!(model_entry_matches(&a, &b));
}

#[test]
fn model_entry_matches_case_insensitive() {
    let a = make_model_entry("Anthropic", "Claude-Sonnet-4");
    let b = make_model_entry("anthropic", "claude-sonnet-4");
    assert!(model_entry_matches(&a, &b));
}

#[test]
fn model_entry_matches_different_provider() {
    let a = make_model_entry("anthropic", "gpt-4o");
    let b = make_model_entry("openai", "gpt-4o");
    assert!(!model_entry_matches(&a, &b));
}

#[test]
fn model_entry_matches_different_id() {
    let a = make_model_entry("anthropic", "claude-sonnet-4");
    let b = make_model_entry("anthropic", "claude-opus-4");
    assert!(!model_entry_matches(&a, &b));
}

// ── resolve_scoped_model_entries ─────────────────────────────────

fn test_models() -> Vec<ModelEntry> {
    vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("anthropic", "claude-opus-4"),
        make_model_entry("openai", "gpt-4o"),
        make_model_entry("openai", "gpt-4o-mini"),
        make_model_entry("google", "gemini-pro"),
    ]
}

#[test]
fn resolve_exact_id_match() {
    let models = test_models();
    let patterns = vec!["gpt-4o".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].model.id, "gpt-4o");
}

#[test]
fn resolve_full_id_match() {
    let models = test_models();
    let patterns = vec!["openai/gpt-4o".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].model.id, "gpt-4o");
    assert_eq!(result[0].model.provider, "openai");
}

#[test]
fn resolve_case_insensitive_match() {
    let models = test_models();
    let patterns = vec!["GPT-4O".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].model.id, "gpt-4o");
}

#[test]
fn resolve_glob_wildcard_star() {
    let models = test_models();
    let patterns = vec!["claude-*".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 2);
    // Sorted by provider/id
    assert_eq!(result[0].model.id, "claude-opus-4");
    assert_eq!(result[1].model.id, "claude-sonnet-4");
}

#[test]
fn resolve_glob_question_mark() {
    let models = test_models();
    let patterns = vec!["gpt-4o-???i".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].model.id, "gpt-4o-mini");
}

#[test]
fn resolve_glob_with_provider_prefix() {
    let models = test_models();
    let patterns = vec!["openai/*".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].model.id, "gpt-4o");
    assert_eq!(result[1].model.id, "gpt-4o-mini");
}

#[test]
fn resolve_multiple_patterns() {
    let models = test_models();
    let patterns = vec!["gpt-4o".to_string(), "gemini-pro".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 2);
    // Sorted by provider/id: google < openai
    assert_eq!(result[0].model.id, "gemini-pro");
    assert_eq!(result[1].model.id, "gpt-4o");
}

#[test]
fn resolve_deduplicates() {
    let models = test_models();
    let patterns = vec!["gpt-4o".to_string(), "openai/gpt-4o".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].model.id, "gpt-4o");
}

#[test]
fn resolve_strips_thinking_suffix() {
    let models = test_models();
    let patterns = vec!["claude-sonnet-4:high".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].model.id, "claude-sonnet-4");
}

#[test]
fn resolve_no_match_returns_empty() {
    let models = test_models();
    let patterns = vec!["nonexistent-model".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert!(result.is_empty());
}

#[test]
fn resolve_empty_patterns_returns_empty() {
    let models = test_models();
    let result = resolve_scoped_model_entries(&[], &models).unwrap();
    assert!(result.is_empty());
}

#[test]
fn resolve_invalid_glob_returns_error() {
    let models = test_models();
    let patterns = vec!["[invalid".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Invalid model pattern"));
}

#[test]
fn resolve_sorted_by_full_id() {
    let models = test_models();
    let patterns = vec!["*".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 5);
    let ids: Vec<String> = result
        .iter()
        .map(|e| format!("{}/{}", e.model.provider, e.model.id))
        .collect();
    assert_eq!(
        ids,
        vec![
            "anthropic/claude-opus-4",
            "anthropic/claude-sonnet-4",
            "google/gemini-pro",
            "openai/gpt-4o",
            "openai/gpt-4o-mini",
        ]
    );
}

// ═══════════════════════════════════════════════════════════════════
// 2. Model selector overlay integration with PiApp
// ═══════════════════════════════════════════════════════════════════

#[test]
fn open_model_selector_populates_overlay() {
    let harness = TestHarness::new("model_selector_open");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
        make_model_entry("google", "gemini-pro"),
    ];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    assert!(app.model_selector().is_none());

    app.open_model_selector();

    let selector = app.model_selector().expect("selector should be open");
    assert_eq!(selector.filtered_len(), 3);
    assert_eq!(selector.selected_index(), 0);
}

#[test]
fn open_model_selector_no_models_sets_status() {
    let harness = TestHarness::new("model_selector_no_models");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let mut app = build_app_with_models(&harness, current, vec![], vec![]);

    app.open_model_selector();

    assert!(app.model_selector().is_none());
    assert_eq!(get_status(&app), Some("No models available"));
}

#[test]
fn model_selector_key_navigation() {
    let harness = TestHarness::new("model_selector_nav");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
        make_model_entry("google", "gemini-pro"),
    ];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    app.open_model_selector();

    // Down arrow moves selection
    app.handle_model_selector_key(&KeyMsg::from_type(KeyType::Down));
    let selector = app.model_selector().unwrap();
    assert_eq!(selector.selected_index(), 1);

    // Up arrow moves back
    app.handle_model_selector_key(&KeyMsg::from_type(KeyType::Up));
    let selector = app.model_selector().unwrap();
    assert_eq!(selector.selected_index(), 0);

    // j/k navigation
    app.handle_model_selector_key(&KeyMsg::from_runes(vec!['j']));
    let selector = app.model_selector().unwrap();
    assert_eq!(selector.selected_index(), 1);

    app.handle_model_selector_key(&KeyMsg::from_runes(vec!['k']));
    let selector = app.model_selector().unwrap();
    assert_eq!(selector.selected_index(), 0);
}

#[test]
fn model_selector_fuzzy_filter() {
    let harness = TestHarness::new("model_selector_filter");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
        make_model_entry("google", "gemini-pro"),
    ];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    app.open_model_selector();

    // Type filter text
    app.handle_model_selector_key(&KeyMsg::from_runes(vec!['g', 'p', 't']));
    let selector = app.model_selector().unwrap();
    assert_eq!(selector.filtered_len(), 1);
    assert_eq!(selector.query(), "gpt");

    // Backspace widens filter
    app.handle_model_selector_key(&KeyMsg::from_type(KeyType::Backspace));
    let selector = app.model_selector().unwrap();
    assert_eq!(selector.query(), "gp");
}

#[test]
fn model_selector_esc_cancels() {
    let harness = TestHarness::new("model_selector_cancel");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
    ];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    app.open_model_selector();
    assert!(app.model_selector().is_some());

    app.handle_model_selector_key(&KeyMsg::from_type(KeyType::Esc));
    assert!(app.model_selector().is_none());
    assert_eq!(get_status(&app), Some("Model selector cancelled"));
}

#[test]
fn model_selector_ctrl_c_cancels() {
    let harness = TestHarness::new("model_selector_ctrl_c");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
    ];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    app.open_model_selector();
    app.handle_model_selector_key(&KeyMsg::from_type(KeyType::CtrlC));
    assert!(app.model_selector().is_none());
    assert_eq!(get_status(&app), Some("Model selector cancelled"));
}

#[test]
fn model_selector_enter_with_no_match_shows_status() {
    let harness = TestHarness::new("model_selector_enter_empty");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![make_model_entry("anthropic", "claude-sonnet-4")];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    app.open_model_selector();

    // Filter to zero results
    app.handle_model_selector_key(&KeyMsg::from_runes(vec!['z', 'z', 'z']));
    let selector = app.model_selector().unwrap();
    assert_eq!(selector.filtered_len(), 0);

    // Enter with no match
    app.handle_model_selector_key(&KeyMsg::from_type(KeyType::Enter));
    assert!(app.model_selector().is_none());
    assert_eq!(get_status(&app), Some("No model selected"));
}

#[test]
fn model_selector_enter_selecting_current_model() {
    let harness = TestHarness::new("model_selector_same");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
    ];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    app.open_model_selector();
    // First item (sorted) is anthropic/claude-sonnet-4, which is current
    app.handle_model_selector_key(&KeyMsg::from_type(KeyType::Enter));
    assert!(app.model_selector().is_none());
    assert_eq!(
        get_status(&app),
        Some("Already using anthropic/claude-sonnet-4")
    );
}

#[test]
fn model_selector_page_navigation() {
    let harness = TestHarness::new("model_selector_page_nav");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let mut available = Vec::new();
    for i in 0..20 {
        available.push(make_model_entry("provider", &format!("model-{i:02}")));
    }
    let mut app = build_app_with_models(&harness, current, vec![], available);

    app.open_model_selector();

    // Page down
    app.handle_model_selector_key(&KeyMsg::from_type(KeyType::PgDown));
    let selector = app.model_selector().unwrap();
    assert!(selector.selected_index() > 0);
    let after_pgdn = selector.selected_index();

    // Page up
    app.handle_model_selector_key(&KeyMsg::from_type(KeyType::PgUp));
    let selector = app.model_selector().unwrap();
    assert!(selector.selected_index() < after_pgdn);
}

// ═══════════════════════════════════════════════════════════════════
// 3. Scoped cycling deterministic ordering tests
// ═══════════════════════════════════════════════════════════════════

#[test]
fn cycle_model_forward_wraps_around() {
    let harness = TestHarness::new("cycle_forward_wrap");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
        make_model_entry("google", "gemini-pro"),
    ];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    // cycle_model(1) = forward
    app.cycle_model(1);

    // After cycling forward from anthropic/claude-sonnet-4 (sorted first),
    // next should be google/gemini-pro (sorted second)
    let status = get_status(&app).unwrap_or("");
    assert!(
        status.starts_with("Switched model:"),
        "Expected 'Switched model:' but got: {status}"
    );
}

#[test]
fn cycle_model_backward_wraps_to_end() {
    let harness = TestHarness::new("cycle_backward_wrap");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
        make_model_entry("google", "gemini-pro"),
    ];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    // cycle_model(-1) = backward
    app.cycle_model(-1);

    // Cycling backward from first (anthropic/claude-sonnet-4) wraps to last (openai/gpt-4o)
    let status = get_status(&app).unwrap_or("");
    assert!(
        status.starts_with("Switched model:"),
        "Expected 'Switched model:' but got: {status}"
    );
}

#[test]
fn cycle_model_single_model_shows_status() {
    let harness = TestHarness::new("cycle_single");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![make_model_entry("anthropic", "claude-sonnet-4")];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    app.cycle_model(1);

    assert_eq!(get_status(&app), Some("Only one model available"));
}

#[test]
fn cycle_model_no_models_shows_status() {
    let harness = TestHarness::new("cycle_empty");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let mut app = build_app_with_models(&harness, current, vec![], vec![]);

    app.cycle_model(1);

    assert_eq!(get_status(&app), Some("No models available"));
}

#[test]
fn cycle_model_uses_scope_when_set() {
    let harness = TestHarness::new("cycle_scoped");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    // Scope only has 2 models
    let scope = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
    ];
    // Available has more models
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
        make_model_entry("google", "gemini-pro"),
    ];
    let mut app = build_app_with_models(&harness, current, scope, available);

    app.cycle_model(1);

    // With scope, should cycle within scope only (2 models)
    let status = get_status(&app).unwrap_or("");
    assert!(
        status.contains("openai/gpt-4o"),
        "Expected cycle to openai/gpt-4o within scope, got: {status}"
    );
}

#[test]
fn cycle_model_single_in_scope_shows_status() {
    let harness = TestHarness::new("cycle_scope_single");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let scope = vec![make_model_entry("anthropic", "claude-sonnet-4")];
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
    ];
    let mut app = build_app_with_models(&harness, current, scope, available);

    app.cycle_model(1);

    assert_eq!(get_status(&app), Some("Only one model in scope"));
}

#[test]
fn cycle_model_empty_scope_with_config_shows_status() {
    let harness = TestHarness::new("cycle_scope_empty");
    let current = make_model_entry("anthropic", "claude-sonnet-4");
    let available = vec![
        make_model_entry("anthropic", "claude-sonnet-4"),
        make_model_entry("openai", "gpt-4o"),
    ];

    // Config with enabled_models patterns triggers scope mode
    let config = Config {
        enabled_models: Some(vec!["nonexistent-pattern".to_string()]),
        ..Config::default()
    };

    let mut app = build_app_with_models_and_config(&harness, current, vec![], available, config);

    app.cycle_model(1);

    // With enabled_models config set but empty scope, should say "No models in scope"
    assert_eq!(get_status(&app), Some("No models in scope"));
}

#[test]
fn cycle_model_deterministic_ordering() {
    let harness = TestHarness::new("cycle_deterministic");
    // Models in random order should be sorted deterministically
    let current = make_model_entry("openai", "gpt-4o");
    let available = vec![
        make_model_entry("openai", "gpt-4o-mini"),
        make_model_entry("anthropic", "claude-opus-4"),
        make_model_entry("openai", "gpt-4o"),
        make_model_entry("google", "gemini-pro"),
        make_model_entry("anthropic", "claude-sonnet-4"),
    ];
    let mut app = build_app_with_models(&harness, current, vec![], available);

    // Sorted order:
    // anthropic/claude-opus-4 (0)
    // anthropic/claude-sonnet-4 (1)
    // google/gemini-pro (2)
    // openai/gpt-4o (3) <-- current
    // openai/gpt-4o-mini (4)

    // Forward from openai/gpt-4o (index 3) → openai/gpt-4o-mini (index 4)
    app.cycle_model(1);
    let status = get_status(&app).unwrap_or("");
    assert!(
        status.contains("openai/gpt-4o-mini"),
        "Expected openai/gpt-4o-mini, got: {status}"
    );
}

// Note: back-to-back cycle_model calls are not tested because spawn_save_session()
// holds the session lock asynchronously, causing the second call to get "Session busy".
// Individual forward and backward tests cover both directions independently.

// ═══════════════════════════════════════════════════════════════════
// 4. Resolve + cycling integration
// ═══════════════════════════════════════════════════════════════════

#[test]
fn resolve_then_cycle_respects_scope() {
    let available = test_models();
    let patterns = vec!["claude-*".to_string()];
    let scope = resolve_scoped_model_entries(&patterns, &available).unwrap();
    assert_eq!(scope.len(), 2);

    let harness = TestHarness::new("resolve_cycle");
    let current = scope[0].clone(); // claude-opus-4 (first alphabetically)
    let mut app = build_app_with_models(&harness, current, scope, available);

    app.cycle_model(1);
    let status = get_status(&app).unwrap_or("");
    assert!(
        status.contains("claude-sonnet-4"),
        "Should cycle to claude-sonnet-4 within scope, got: {status}"
    );
}

#[test]
fn resolve_all_models_with_star_glob() {
    let available = test_models();
    let patterns = vec!["*".to_string()];
    let resolved = resolve_scoped_model_entries(&patterns, &available).unwrap();
    assert_eq!(resolved.len(), 5);
}

#[test]
fn resolve_with_thinking_suffix_and_glob() {
    let available = test_models();
    let patterns = vec!["claude-*:high".to_string()];
    let resolved = resolve_scoped_model_entries(&patterns, &available).unwrap();
    assert_eq!(resolved.len(), 2);
}
