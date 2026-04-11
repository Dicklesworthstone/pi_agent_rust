use super::*;
use crate::agent::AgentConfig;
use crate::model::StreamEvent;
use crate::provider::{Context, Provider, StreamOptions};
use crate::resources::{ResourceCliOptions, ResourceLoader};
use crate::tools::ToolRegistry;
use asupersync::channel::mpsc;
use asupersync::runtime::RuntimeBuilder;
use futures::stream;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

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

fn test_runtime_handle() -> asupersync::runtime::RuntimeHandle {
    static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        RuntimeBuilder::current_thread()
            .build()
            .expect("build asupersync runtime")
    })
    .handle()
}

fn test_model_entry() -> ModelEntry {
    ModelEntry {
        model: crate::provider::Model {
            id: "gpt-5.2".to_string(),
            name: "gpt-5.2".to_string(),
            api: "openai-responses".to_string(),
            provider: "openai".to_string(),
            base_url: "https://example.invalid".to_string(),
            reasoning: true,
            input: vec![crate::provider::InputType::Text],
            cost: crate::provider::ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 8_192,
            headers: std::collections::HashMap::new(),
        },
        api_key: None,
        headers: std::collections::HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn build_test_app(cwd: PathBuf) -> PiApp {
    let config = Config::default();
    let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&[], &cwd, Some(&config)),
        AgentConfig::default(),
    );
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
    let model_entry = test_model_entry();
    let (event_tx, _event_rx) = mpsc::channel(64);

    PiApp::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
        config,
        resources,
        resource_cli,
        cwd,
        model_entry.clone(),
        vec![model_entry.clone()],
        vec![model_entry],
        Vec::new(),
        event_tx,
        test_runtime_handle(),
        false,
        false,
        None,
        Some(KeyBindings::new()),
        Vec::new(),
        Usage::default(),
    )
}

fn tempdir() -> tempfile::TempDir {
    std::fs::create_dir_all(std::env::temp_dir()).expect("create temp root");
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn prepare_startup_changelog_skips_disk_write_when_persistence_disabled() {
    let dir = tempdir();
    let cwd = dir.path().join("workspace");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let settings_path = dir.path().join("settings.json");
    let mut config = Config {
        last_changelog_version: Some("0.9.0".to_string()),
        ..Config::default()
    };

    let changelog = "## 1.0.0\n- Added startup changelog notices\n\n## 0.9.0\n- Previous release\n";
    let startup = prepare_startup_changelog_with_roots(
        &mut config,
        dir.path(),
        &cwd,
        Some(&settings_path),
        false,
        false,
        "1.0.0",
        changelog,
    );

    assert_eq!(
        startup,
        Some(StartupChangelog::Full {
            markdown: "## 1.0.0\n- Added startup changelog notices".to_string(),
        })
    );
    assert!(
        !settings_path.exists(),
        "startup construction should not write settings"
    );
    assert_eq!(config.last_changelog_version.as_deref(), Some("1.0.0"));
}

#[test]
fn prepare_startup_changelog_writes_when_persistence_enabled() {
    let dir = tempdir();
    let cwd = dir.path().join("workspace");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let settings_path = dir.path().join("settings.json");
    let mut config = Config {
        last_changelog_version: Some("0.9.0".to_string()),
        ..Config::default()
    };

    let startup = prepare_startup_changelog_with_roots(
        &mut config,
        dir.path(),
        &cwd,
        Some(&settings_path),
        false,
        true,
        "1.0.0",
        "## 1.0.0\n- Added startup changelog notices\n\n## 0.9.0\n- Previous release\n",
    );

    assert!(matches!(startup, Some(StartupChangelog::Full { .. })));
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read settings"))
            .expect("parse settings");
    assert_eq!(saved["lastChangelogVersion"].as_str(), Some("1.0.0"));
}

#[test]
fn extract_file_references_removes_indented_ref_line_without_leaving_blank_whitespace() {
    let dir = tempdir();
    std::fs::write(dir.path().join("notes.txt"), "hi").expect("write file");
    let mut app = build_test_app(dir.path().to_path_buf());

    let (cleaned, refs) = app.extract_file_references("Summary:\n  @notes.txt\nNext line");

    assert_eq!(cleaned, "Summary:\nNext line");
    assert_eq!(refs, vec!["notes.txt".to_string()]);
}

#[test]
fn extract_file_references_preserves_newline_before_trailing_punctuation() {
    let dir = tempdir();
    std::fs::write(dir.path().join("notes.txt"), "hi").expect("write file");
    let mut app = build_test_app(dir.path().to_path_buf());

    let (cleaned, refs) = app.extract_file_references("Summary:\n@notes.txt.");

    assert_eq!(cleaned, "Summary:\n.");
    assert_eq!(refs, vec!["notes.txt".to_string()]);
}

#[test]
fn render_header_uses_cycle_thinking_binding_hint() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(200, 40);

    let header = app.render_header();

    assert!(header.contains("shift+tab: thinking"), "header: {header}");
    assert!(!header.contains("ctrl+t: thinking"), "header: {header}");
}
