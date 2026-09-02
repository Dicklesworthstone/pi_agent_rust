use super::*;
use crate::agent::AgentConfig;
use crate::model::{ContentBlock, StreamEvent, TextContent};
use crate::provider::{Context, Provider, StreamOptions};
use crate::resources::{ResourceCliOptions, ResourceLoader};
use crate::tools::ToolRegistry;
use asupersync::channel::mpsc;
use asupersync::runtime::RuntimeBuilder;
use bubbletea::{KeyMsg, Message, WindowSizeMsg};
use futures::stream;
use serde_json::json;
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
        None,
        Vec::new(),
        event_tx,
        test_runtime_handle(),
        false,
        false,
        None,
        Some(KeyBindings::new()),
        Vec::new(),
        Usage::default(),
        None,
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
        || changelog,
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
        || "## 1.0.0\n- Added startup changelog notices\n\n## 0.9.0\n- Previous release\n",
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
fn is_inside_jj_repo_detects_root_directly() {
    let dir = tempdir();
    std::fs::create_dir(dir.path().join(".jj")).expect("mkdir .jj");
    assert!(super::is_inside_jj_repo(dir.path()));
}

#[test]
fn is_inside_jj_repo_walks_up_to_ancestor() {
    let dir = tempdir();
    let root = dir.path();
    std::fs::create_dir(root.join(".jj")).expect("mkdir .jj");
    let nested = root.join("a").join("b").join("c");
    std::fs::create_dir_all(&nested).expect("mkdir nested");
    assert!(super::is_inside_jj_repo(&nested));
}

#[test]
fn is_inside_jj_repo_false_when_no_dot_jj_anywhere() {
    let dir = tempdir();
    let nested = dir.path().join("a").join("b");
    std::fs::create_dir_all(&nested).expect("mkdir nested");
    assert!(!super::is_inside_jj_repo(&nested));
}

#[test]
fn is_inside_jj_repo_requires_dot_jj_to_be_a_directory() {
    // A file named `.jj` is a gitlink-like stub in some tooling; only a
    // real `.jj/` directory counts as a jj repo for display purposes.
    let dir = tempdir();
    std::fs::write(dir.path().join(".jj"), "not a dir").expect("write stub");
    assert!(!super::is_inside_jj_repo(dir.path()));
}

#[test]
fn read_jj_change_returns_none_outside_jj_repo() {
    // No `.jj` anywhere -> must short-circuit without forking a
    // subprocess and without touching $PATH for the `jj` binary.
    let dir = tempdir();
    assert!(super::read_jj_change(dir.path()).is_none());
}

#[test]
fn read_vcs_info_falls_back_to_git_when_no_jj() {
    // Seed a minimal `.git/HEAD` pointing at a branch. With no `.jj`
    // anywhere, read_vcs_info must return the git branch name unchanged.
    let dir = tempdir();
    let dot_git = dir.path().join(".git");
    std::fs::create_dir(&dot_git).expect("mkdir .git");
    std::fs::write(dot_git.join("HEAD"), "ref: refs/heads/feature/jj-demo\n").expect("seed HEAD");

    let vcs = super::read_vcs_info(dir.path());
    assert_eq!(vcs.as_deref(), Some("feature/jj-demo"));
}

#[test]
fn render_header_uses_cycle_thinking_binding_hint() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(200, 40);

    let header = app.render_header();

    assert!(header.contains("shift+tab: thinking"), "header: {header}");
    assert!(!header.contains("ctrl+t: thinking"), "header: {header}");
    assert!(
        header.contains("\x1b]0;Pi · openai/gpt-5.2 · ready\x07"),
        "live header must emit the delight terminal title: {header:?}"
    );
}

/// Issue #200: a named session (via `/name` or a resumed named session)
/// titles the terminal tab after itself; unnamed sessions keep the model
/// label (pinned by `render_header_uses_cycle_thinking_binding_hint`).
#[test]
fn render_header_titles_terminal_after_session_name() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(200, 40);
    {
        let mut session = app.session.try_lock().expect("session lock");
        session.set_name("refactor-plan");
    }

    let header = app.render_header();
    assert!(
        header.contains("\x1b]0;Pi · refactor-plan · ready\x07"),
        "named session must title the tab after itself: {header:?}"
    );
}

#[test]
fn live_view_renders_default_welcome_and_powerline_status() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(200, 40);
    app.startup_welcome.clear();

    let view = app.view();

    assert!(view.contains("Welcome to Pi!"), "view: {view}");
    assert!(view.contains("Tip: Type /help"), "view: {view}");
    assert!(view.contains("ACT"), "powerline mode missing: {view}");
    assert!(
        view.contains("ctx: 0%"),
        "powerline context missing: {view}"
    );
}

#[test]
fn quiet_startup_does_not_render_welcome_screen_fallback() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(200, 40);
    app.config.quiet_startup = Some(true);
    app.startup_welcome.clear();

    let view = app.view();

    assert!(!view.contains("Welcome to Pi!"), "view: {view}");
    assert!(!view.contains("Tip: Type /help"), "view: {view}");
}

#[test]
fn live_view_applies_rich_markdown_enhancements() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(200, 40);
    app.messages.push(ConversationMessage::new(
        MessageRole::Assistant,
        "Use #ff5500 for \\alpha.".to_string(),
        None,
    ));

    let view = app.view();

    assert!(view.contains('α'), "LaTeX enhancement missing: {view}");
    assert!(view.contains("■ #ff5500"), "hex swatch missing: {view}");
}

#[test]
fn enter_accepts_highlighted_autocomplete_item() {
    // Regression for issue #61: with the slash dropdown open and an entry
    // highlighted (e.g. user pressed Down to select `/model`), pressing Enter
    // must accept the highlighted item — matching the dropdown's own footer
    // hint "Enter/Tab accept" — not submit the raw `/` typed so far.
    use crate::autocomplete::{AutocompleteItem, AutocompleteItemKind};
    use bubbletea::{KeyMsg, KeyType, Message};

    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());

    app.input.set_value("/");
    app.autocomplete.open = true;
    app.autocomplete.items = vec![AutocompleteItem {
        kind: AutocompleteItemKind::SlashCommand,
        label: "/model".to_string(),
        insert: "/model ".to_string(),
        description: None,
    }];
    app.autocomplete.selected = Some(0);
    app.autocomplete.replace_range = 0..1;

    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Enter)));

    assert_eq!(
        app.input.value(),
        "/model ",
        "Enter with a highlighted dropdown entry must accept the item"
    );
    assert!(
        !app.autocomplete.open,
        "Accepting via Enter should close the dropdown"
    );
}

#[test]
fn enter_submits_when_no_autocomplete_item_highlighted() {
    // The dual contract for issue #61: when the dropdown is open but the
    // user has not navigated to any item (selected.is_none()), Enter must
    // still submit the raw editor contents — i.e. behavior is unchanged
    // for users who never pressed Down.
    use bubbletea::{KeyMsg, KeyType, Message};

    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());

    app.input.set_value("/foo");
    app.autocomplete.open = true;
    app.autocomplete.items.clear();
    app.autocomplete.selected = None;
    app.autocomplete.replace_range = 0..4;

    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Enter)));

    assert!(
        !app.autocomplete.open,
        "Enter with no selection should still close the dropdown"
    );
}

#[derive(Default)]
struct TuiDegradationDrillTrace {
    event_count: usize,
    redraw_count: usize,
    coalesced_count: usize,
    preserved_input_count: usize,
    max_rendered_rows: usize,
}

impl TuiDegradationDrillTrace {
    fn event(&mut self) {
        self.event_count += 1;
    }

    fn render(&mut self, app: &PiApp) -> String {
        let frame = app.view();
        self.redraw_count += 1;
        self.max_rendered_rows = self.max_rendered_rows.max(frame.lines().count());
        frame
    }

    fn record_input_preserved(&mut self, before_len: usize, after: &str) {
        self.preserved_input_count += after.len().saturating_sub(before_len);
    }
}

fn pressure_tool_block(label: &str) -> ContentBlock {
    let mut output = String::new();
    for line in 0..32 {
        output.push_str(label);
        output.push_str(" line ");
        output.push_str(&line.to_string());
        output.push('\n');
    }
    ContentBlock::Text(TextContent::new(output))
}

fn semantic_visible(frame: &str, marker: &str) -> bool {
    frame.contains(marker)
}

const PROVIDER_DELTA_COUNT: usize = 72;
const THINKING_DELTA_COUNT: usize = 12;
const TOOL_UPDATE_COUNT: usize = 18;
const SESSION_BURST_COUNT: usize = 10;
const FINAL_MARKER: &str = "semantic-provider-delta-71";
const TOOL_MARKER: &str = "semantic-tool-final";
const SESSION_MARKER: &str = "session write burst 9 committed";

fn seed_normal_tui_load(app: &mut PiApp, trace: &mut TuiDegradationDrillTrace) {
    app.messages.push(ConversationMessage::new(
        MessageRole::User,
        "normal-load prompt remains readable".to_string(),
        None,
    ));
    app.messages.push(ConversationMessage::new(
        MessageRole::Assistant,
        "normal-load assistant reply remains readable".to_string(),
        None,
    ));
    app.scroll_to_bottom();
    trace.event_count += 2;
    let normal_frame = trace.render(app);
    assert!(semantic_visible(
        &normal_frame,
        "normal-load assistant reply"
    ));
}

fn drive_provider_pressure(app: &mut PiApp, trace: &mut TuiDegradationDrillTrace) {
    app.tui_pressure_frame_p99_us.store(
        TuiPressureController::HIGH_FRAME_P99_US,
        std::sync::atomic::Ordering::Relaxed,
    );
    app.handle_pi_message(PiMsg::AgentStart);
    trace.event();
    for idx in 0..PROVIDER_DELTA_COUNT {
        let delta = format!("semantic-provider-delta-{idx} ");
        app.handle_pi_message(PiMsg::TextDelta(delta));
        trace.event();
        if idx % 16 == 0 {
            let frame = trace.render(app);
            assert!(
                semantic_visible(&frame, &format!("semantic-provider-delta-{idx}")),
                "streaming provider delta must stay visible at idx {idx}"
            );
        }
    }
    for idx in 0..THINKING_DELTA_COUNT {
        app.handle_pi_message(PiMsg::ThinkingDelta(format!("thinking-step-{idx} ")));
        trace.event();
    }
}

fn drive_tool_pressure(app: &mut PiApp, trace: &mut TuiDegradationDrillTrace) {
    app.handle_pi_message(PiMsg::ToolStart {
        name: "bash".to_string(),
        tool_id: "tool-pressure".to_string(),
    });
    trace.event();
    for idx in 0..TOOL_UPDATE_COUNT {
        let label = if idx + 1 == TOOL_UPDATE_COUNT {
            TOOL_MARKER
        } else {
            "low-value-tool-noise"
        };
        app.handle_pi_message(PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-pressure".to_string(),
            content: vec![pressure_tool_block(label)],
            details: Some(json!({
                "line_count": (idx + 1) * 32,
                "byte_count": (idx + 1) * 512,
            })),
        });
        trace.event();
    }
    trace.coalesced_count += TOOL_UPDATE_COUNT.saturating_sub(1);
    app.handle_pi_message(PiMsg::ToolEnd {
        name: "bash".to_string(),
        tool_id: "tool-pressure".to_string(),
        is_error: false,
        output: None,
    });
    trace.event();
}

fn drive_session_write_bursts(app: &mut PiApp, trace: &mut TuiDegradationDrillTrace) {
    for idx in 0..SESSION_BURST_COUNT {
        app.handle_pi_message(PiMsg::SystemNote(format!(
            "session write burst {idx} committed"
        )));
        trace.event();
    }
}

fn drive_resize_pressure(app: &mut PiApp, trace: &mut TuiDegradationDrillTrace) {
    let _ = app.update(Message::new(WindowSizeMsg {
        width: 92,
        height: 26,
    }));
    trace.event();
    let compact_frame = trace.render(app);
    assert!(
        compact_frame.lines().count() <= app.term_height,
        "compact resize frame must not exceed terminal height"
    );

    let _ = app.update(Message::new(WindowSizeMsg {
        width: 120,
        height: 64,
    }));
    trace.event();
}

fn finish_agent_and_preserve_input(app: &mut PiApp, trace: &mut TuiDegradationDrillTrace) {
    app.handle_pi_message(PiMsg::AgentDone {
        usage: None,
        stop_reason: StopReason::Stop,
        error_message: None,
    });
    trace.event();

    for key in ['o', 'k'] {
        let before_len = app.input.value().len();
        let _ = app.update(Message::new(KeyMsg::from_char(key)));
        trace.event();
        trace.record_input_preserved(before_len, &app.input.value());
    }
}

fn assert_tui_degradation_evidence(app: &PiApp, trace: &mut TuiDegradationDrillTrace) {
    let final_frame = trace.render(app);
    let collapsed_tool_messages = app
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool && message.collapsed)
        .count();
    let semantic_visible_count = [
        semantic_visible(&final_frame, FINAL_MARKER),
        semantic_visible(&final_frame, TOOL_MARKER),
        semantic_visible(&final_frame, SESSION_MARKER),
        app.input.value() == "ok",
    ]
    .into_iter()
    .filter(|visible| *visible)
    .count();
    let frame_pressure = TuiPressureController::decide(
        TuiPressureController::HIGH_FRAME_P99_US,
        TuiPressureController::HIGH_TOOL_OUTPUT_BYTES,
        TOOL_UPDATE_COUNT,
    );
    let evidence = json!({
        "schema": "pi.tui.degradation_drill.v1",
        "fixture": "sustained_event_pressure",
        "event_count": trace.event_count,
        "redraw_count": trace.redraw_count,
        "coalesced_count": trace.coalesced_count,
        "max_frame_budget_pressure": format!("{:?}", frame_pressure.level),
        "max_rendered_rows": trace.max_rendered_rows,
        "terminal_height": app.term_height,
        "preserved_input_count": trace.preserved_input_count,
        "semantic_visible_count": semantic_visible_count,
        "collapsed_tool_message_count": collapsed_tool_messages,
        "verdict": if semantic_visible_count == 4
            && collapsed_tool_messages == 1
            && trace.preserved_input_count == 2
            && trace.max_rendered_rows <= app.term_height
        {
            "pass"
        } else {
            "fail_closed"
        },
    });

    assert_eq!(evidence["schema"], "pi.tui.degradation_drill.v1");
    assert_eq!(evidence["event_count"], 122);
    assert_eq!(evidence["redraw_count"], 8);
    assert_eq!(evidence["coalesced_count"], TOOL_UPDATE_COUNT - 1);
    assert_eq!(evidence["max_frame_budget_pressure"], "High");
    assert_eq!(evidence["preserved_input_count"], 2);
    assert_eq!(evidence["collapsed_tool_message_count"], 1);
    assert_eq!(evidence["semantic_visible_count"], 4);
    assert_eq!(
        evidence["verdict"], "pass",
        "degradation evidence: {evidence}"
    );
}

#[test]
fn tui_degradation_drill_preserves_input_and_semantics_under_pressure() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    let mut trace = TuiDegradationDrillTrace::default();
    app.enable_frame_timing_for_test();
    app.reset_frame_timing_for_test();
    app.set_terminal_size(120, 48);

    seed_normal_tui_load(&mut app, &mut trace);
    drive_provider_pressure(&mut app, &mut trace);
    drive_tool_pressure(&mut app, &mut trace);
    drive_session_write_bursts(&mut app, &mut trace);
    drive_resize_pressure(&mut app, &mut trace);
    finish_agent_and_preserve_input(&mut app, &mut trace);
    assert_tui_degradation_evidence(&app, &mut trace);
}

/// bd-cv653.3.1: `/model <role> <spec>` assigns a session-scoped role
/// override and records a role-tagged ModelChange entry; `/model <role>`
/// alone reports the assignment; unknown role-like tokens never assign.
#[test]
fn slash_model_role_assignment_and_query() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());

    // Assign: exact provider/model match against available_models.
    let result = app.handle_slash_model("advisor openai/gpt-5.2");
    assert!(result.is_none(), "role assignment is a status-only action");
    assert_eq!(
        app.role_model_overrides
            .get(&crate::models::ModelRole::Advisor),
        Some(&("openai".to_string(), "gpt-5.2".to_string())),
        "override recorded for the advisor role"
    );

    // Session carries a role-tagged ModelChange entry.
    let guard = app.session.try_lock().expect("session lock");
    let role_entries: Vec<_> = guard
        .entries_for_current_path()
        .iter()
        .filter_map(|e| match e {
            crate::session::SessionEntry::ModelChange(mc) => Some(mc),
            _ => None,
        })
        .collect();
    assert!(
        role_entries
            .iter()
            .any(|mc| mc.role.as_deref() == Some("advisor")
                && mc.provider == "openai"
                && mc.model_id == "gpt-5.2"),
        "role-tagged ModelChange entry present"
    );
    drop(guard);

    // Query: `/model advisor` reports the assignment without changing state.
    app.status_message = None;
    let result = app.handle_slash_model("advisor");
    assert!(result.is_none());
    let status = app.status_message.clone().unwrap_or_default();
    assert!(
        status.contains("advisor") && status.contains("openai/gpt-5.2"),
        "query reports assignment, got: {status}"
    );

    // Planted negative: a two-token pattern whose first token is NOT a role
    // must not create any override.
    let before = app.role_model_overrides.len();
    let _ = app.handle_slash_model("notarole openai/gpt-5.2");
    assert_eq!(
        app.role_model_overrides.len(),
        before,
        "non-role first token must not assign"
    );
}

/// bd-cv653.3.9: the todo footer is state-driven — a TodoSummary message
/// renders the compact line in the next frame's view; `None` clears it and
/// the chrome height accounting tracks both states.
#[test]
fn todo_summary_message_drives_footer_line() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(100, 30);

    let base_height = app.view_effective_conversation_height();
    assert!(app.todo_summary.is_none());
    assert!(!app.view().contains("1/2 · implement"));

    app.handle_pi_message(PiMsg::TodoSummary {
        summary: Some("1/2 · implement".to_string()),
    });
    let view = app.view();
    assert!(view.contains("todo"), "footer label rendered");
    assert!(view.contains("1/2 · implement"), "summary rendered");
    assert_eq!(
        app.view_effective_conversation_height(),
        base_height.saturating_sub(2),
        "todo footer consumes two chrome rows"
    );

    app.handle_pi_message(PiMsg::TodoSummary { summary: None });
    assert!(app.todo_summary.is_none());
    assert!(!app.view().contains("1/2 · implement"));
    assert_eq!(app.view_effective_conversation_height(), base_height);
}

/// bd-cv653.3.8: an ask card owns the input line — the question renders as a
/// system card, numbered answers advance through the questions, and answers
/// Issue #197: `/new` must restore the configured default thinking level
/// (clamped to the model), not hard-code `off`.
#[test]
fn new_session_thinking_level_resolves_configured_default() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());

    app.config.default_thinking_level = Some("max".to_string());
    assert_eq!(
        app.new_session_thinking_level(),
        app.model_entry
            .clamp_thinking_level(crate::model::ThinkingLevel::Max)
    );

    app.config.default_thinking_level = Some("low".to_string());
    assert_eq!(
        app.new_session_thinking_level(),
        crate::model::ThinkingLevel::Low
    );

    // Unset resolves exactly like launch: XHigh clamped to the model.
    app.config.default_thinking_level = None;
    assert_eq!(
        app.new_session_thinking_level(),
        app.model_entry
            .clamp_thinking_level(crate::model::ThinkingLevel::XHigh)
    );
}

/// route to AskTool::respond_ui (an expired request surfaces a status).
#[test]
fn ask_card_consumes_input_and_advances_questions() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(100, 30);
    let tool = crate::ask::AskTool::new(crate::ask::AskPolicy::Recommended);
    app.ask_tool = Some(tool);

    let request: crate::ask::AskRequest = serde_json::from_value(serde_json::json!({
        "questions": [
            {"id": "q1", "question": "Pick one?", "recommended": 0,
             "options": [{"label": "Alpha"}, {"label": "Beta"}]},
            {"id": "q2", "question": "And another?",
             "options": [{"label": "Left"}, {"label": "Right"}]}
        ]
    }))
    .expect("ask request");
    if let Some(tool) = app.ask_tool.as_ref() {
        tool.register_channel_ui_request_for_tests("req-1");
    }
    app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
        id: "req-1".to_string(),
        request,
    }));

    let view = app.view();
    assert!(view.contains("question 1 of 2"), "card rendered");
    assert!(view.contains("Alpha (recommended)"), "recommended badge");

    // First answer advances to the second card.
    app.submit_message("2");
    assert!(app.view().contains("question 2 of 2"));

    // Second answer completes; with no pending reply slot (this request was
    // injected directly, not via install_channel_ui) the expiry status shows.
    app.submit_message("left");
    assert!(app.active_ask_ui.is_none());
    assert_eq!(
        app.status_message.as_deref(),
        Some("Ask request expired before the answer")
    );

    // A fresh card can be dismissed with 'cancel'.
    let request: crate::ask::AskRequest = serde_json::from_value(serde_json::json!({
        "questions": [{"question": "Again?", "options": [{"label": "A"}, {"label": "B"}]}]
    }))
    .expect("ask request 2");
    if let Some(tool) = app.ask_tool.as_ref() {
        tool.register_channel_ui_request_for_tests("req-2");
    }
    app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
        id: "req-2".to_string(),
        request,
    }));
    app.submit_message("cancel");
    assert!(app.active_ask_ui.is_none());
}

/// gh #184: ask cards arrive while the ask tool is still executing, i.e.
/// while the agent is busy. Enter must answer the card instead of queueing
/// the text as a steering message, and Escape must dismiss the card rather
/// than abort the turn.
#[test]
fn ask_card_answer_is_not_queued_as_steering_while_agent_busy() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(100, 30);
    app.ask_tool = Some(crate::ask::AskTool::new(crate::ask::AskPolicy::Recommended));

    let request: crate::ask::AskRequest = serde_json::from_value(json!({
        "questions": [
            {"id": "q1", "question": "Retry or dump?",
             "options": [{"label": "Retry"}, {"label": "Dump"}]},
            {"id": "q2", "question": "Where?",
             "options": [{"label": "Here"}, {"label": "There"}]}
        ]
    }))
    .expect("ask request");
    if let Some(tool) = app.ask_tool.as_ref() {
        tool.register_channel_ui_request_for_tests("req-busy");
    }
    app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
        id: "req-busy".to_string(),
        request,
    }));
    app.agent_state = AgentState::ToolRunning;
    assert!(app.view().contains("question 1 of 2"));

    // Number selection through the real Enter keybinding path.
    app.input.set_value("2");
    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Enter)));
    assert!(
        app.view().contains("question 2 of 2"),
        "Enter must advance the card while busy"
    );
    assert!(
        app.input.value().is_empty(),
        "the first answer must be cleared before the next question"
    );
    let queued = app
        .message_queue
        .lock()
        .expect("message queue")
        .pop_steering();
    assert!(
        queued.is_empty(),
        "card answer must not be queued as steering"
    );
    assert_ne!(
        app.status_message.as_deref(),
        Some("Queued steering message")
    );

    // Free text is accepted the same way.
    app.input.set_value("somewhere else");
    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Enter)));
    assert!(
        app.active_ask_ui.is_none(),
        "card completes after last answer"
    );
    assert!(
        app.input.value().is_empty(),
        "editor cleared after answering"
    );

    // Escape dismisses a fresh card without aborting the turn.
    let request: crate::ask::AskRequest = serde_json::from_value(json!({
        "questions": [{"question": "Again?", "options": [{"label": "A"}, {"label": "B"}]}]
    }))
    .expect("ask request 2");
    if let Some(tool) = app.ask_tool.as_ref() {
        tool.register_channel_ui_request_for_tests("req-busy-2");
    }
    app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
        id: "req-busy-2".to_string(),
        request,
    }));
    assert!(app.active_ask_ui.is_some());
    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Esc)));
    assert!(app.active_ask_ui.is_none(), "Escape dismisses the card");
    assert_ne!(
        app.status_message.as_deref(),
        Some("Aborting request..."),
        "Escape on a card must not abort the turn"
    );
    assert_eq!(app.agent_state, AgentState::ToolRunning);
}

/// bd-1qol9 harness helpers: a minimal text-answer extension card.
fn ext_input_card(id: &str, prompt_text: &str) -> ExtensionUiRequest {
    ExtensionUiRequest::new(
        id,
        "input",
        serde_json::json!({
            "title": "Ext",
            "message": prompt_text,
            "extension_id": "ext-cards"
        }),
    )
    .with_extension_id(Some("ext-cards".to_string()))
}

/// bd-1qol9 mixed arrival order (extension FIRST, ask SECOND): exactly one
/// card may own the editor; the later ask queues behind and promotes only
/// after the extension resolves, so answers can never be reordered.
#[test]
fn mixed_cards_extension_then_ask_serialize_in_arrival_order() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(100, 30);
    app.ask_tool = Some(crate::ask::AskTool::new(crate::ask::AskPolicy::Recommended));

    app.handle_pi_message(PiMsg::ExtensionUiRequest(ext_input_card("e1", "Say?")));
    assert_eq!(
        app.active_input_card_kind,
        Some(InputCardKind::Extension),
        "extension activates first"
    );

    let request: crate::ask::AskRequest = serde_json::from_value(json!({
        "questions": [{"question": "Which?", "options": [{"label": "A"}]}]
    }))
    .expect("ask request");
    if let Some(tool) = app.ask_tool.as_ref() {
        tool.register_channel_ui_request_for_tests("a1");
    }
    app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
        id: "a1".to_string(),
        request,
    }));
    assert_eq!(
        app.active_input_card_kind,
        Some(InputCardKind::Extension),
        "later ask must NOT steal the editor from the active card"
    );
    assert!(app.active_ask_ui.is_none());

    // The ext-active answer routes to the EXTENSION parser, not the ask one:
    // an arbitrary string would fail numeric ask parsing but succeed here.
    app.input.set_value("free-form reply");
    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Enter)));
    assert!(app.active_extension_ui.is_none(), "ext resolved");
    assert_eq!(app.active_input_card_kind, Some(InputCardKind::Ask));
    assert!(app.view().contains("question 1 of 1"), "ask promoted");
    assert!(
        app.card_draft_snapshot.is_none(),
        "the consumed extension answer must not become a draft"
    );

    app.input.set_value("A");
    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Enter)));
    assert!(app.active_ask_ui.is_none());
    assert!(app.active_input_card_kind.is_none());
}

/// bd-1qol9 reverse order (ask FIRST, extension SECOND) plus draft
/// capture/restore across answer and Escape resolution paths.
#[test]
fn mixed_cards_ask_then_ext_preserve_drafts_across_resolution_paths() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(100, 30);
    app.ask_tool = Some(crate::ask::AskTool::new(crate::ask::AskPolicy::Recommended));

    // Preexisting steering draft gets snapshotted + cleared on activation.
    app.input.set_value("cargo test -- --filter bd_1");
    let request: crate::ask::AskRequest = serde_json::from_value(json!({
        "questions": [{"question": "Run tests?", "options": [{"label": "Yes"}, {"label": "No"}]}]
    }))
    .expect("ask request");
    if let Some(tool) = app.ask_tool.as_ref() {
        tool.register_channel_ui_request_for_tests("a-order");
    }
    app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
        id: "a-order".to_string(),
        request,
    }));
    assert!(
        app.input.value().is_empty(),
        "card activation clears the captured draft"
    );
    assert!(app.card_draft_snapshot.is_some());

    app.handle_pi_message(PiMsg::ExtensionUiRequest(ext_input_card("e-order", "Env?")));
    assert_eq!(app.active_input_card_kind, Some(InputCardKind::Ask));

    // Answer the ACTIVE ask through the REAL Enter path; the queued ext
    // promotes automatically afterward.
    app.input.set_value("Yes");
    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Enter)));
    assert_eq!(app.active_input_card_kind, Some(InputCardKind::Extension));

    // Generic Escape dismisses the promoted EXTENSION card without aborting
    // anything and restores the pre-card draft into the empty editor.
    app.agent_state = AgentState::ToolRunning;
    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Esc)));
    assert!(app.active_extension_ui.is_none());
    assert!(app.active_input_card_kind.is_none());
    assert_ne!(
        app.status_message.as_deref(),
        Some("Aborting request..."),
        "Escape on a card never aborts the turn"
    );
    assert_eq!(
        app.input.value(),
        "cargo test -- --filter bd_1",
        "explicit merge policy restores the draft after the last card settles"
    );
    assert_eq!(app.agent_state, AgentState::ToolRunning);
    app.agent_state = AgentState::Idle;
}

/// bd-q66i1: normal (non-Escape) completion clears each consumed answer
/// before successor activation, then restores only the genuine pre-card draft
/// after the final card settles.
#[test]
fn normal_card_resolution_restores_only_the_preexisting_draft() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(100, 30);
    app.ask_tool = Some(crate::ask::AskTool::new(crate::ask::AskPolicy::Recommended));
    app.input.set_value("keep this draft");

    app.handle_pi_message(PiMsg::ExtensionUiRequest(ext_input_card(
        "e-normal", "Env?",
    )));
    let request: crate::ask::AskRequest = serde_json::from_value(json!({
        "questions": [{"question": "Proceed?", "options": [{"label": "Yes"}]}]
    }))
    .expect("ask request");
    if let Some(tool) = app.ask_tool.as_ref() {
        tool.register_channel_ui_request_for_tests("a-normal");
    }
    app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
        id: "a-normal".to_string(),
        request,
    }));

    app.input.set_value("extension answer");
    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Enter)));
    assert_eq!(app.active_input_card_kind, Some(InputCardKind::Ask));
    assert!(
        app.input.value().is_empty(),
        "successor starts with an empty editor"
    );
    assert_eq!(app.card_draft_snapshot.as_deref(), Some("keep this draft"));

    app.input.set_value("Yes");
    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Enter)));
    assert!(app.active_input_card_kind.is_none());
    assert_eq!(app.input.value(), "keep this draft");
    assert!(app.card_draft_snapshot.is_none());
}

/// bd-q66i1: Escape resolves exactly one order-ledger entry. The historical
/// double-pop skipped the second Ask entry and stranded the following
/// extension card.
#[test]
fn escape_advances_one_card_at_a_time_across_ask_ask_extension() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(100, 30);
    app.ask_tool = Some(crate::ask::AskTool::new(crate::ask::AskPolicy::Recommended));

    for (id, label) in [("a-first", "First?"), ("a-second", "Second?")] {
        let request: crate::ask::AskRequest = serde_json::from_value(json!({
            "questions": [{"question": label, "options": [{"label": "Yes"}]}]
        }))
        .expect("ask request");
        if let Some(tool) = app.ask_tool.as_ref() {
            tool.register_channel_ui_request_for_tests(id);
        }
        app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
            id: id.to_string(),
            request,
        }));
    }
    app.handle_pi_message(PiMsg::ExtensionUiRequest(ext_input_card(
        "e-third", "Third?",
    )));

    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Esc)));
    assert_eq!(app.active_input_card_kind, Some(InputCardKind::Ask));
    assert_eq!(
        app.active_ask_ui
            .as_ref()
            .map(|card| card.request.id.as_str()),
        Some("a-second")
    );
    assert_eq!(app.input_card_order.front(), Some(&InputCardKind::Ask));

    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Esc)));
    assert_eq!(app.active_input_card_kind, Some(InputCardKind::Extension));
    assert_eq!(
        app.active_extension_ui
            .as_ref()
            .map(|request| request.id.as_str()),
        Some("e-third")
    );

    let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Esc)));
    assert!(app.active_input_card_kind.is_none());
    assert!(app.input_card_order.is_empty());
}

/// bd-q66i1: turn-end invalidation treats partial card input as consumed and
/// restores the genuine draft captured before the card burst.
#[test]
fn agent_done_discards_partial_extension_answer_and_restores_draft() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(100, 30);
    app.input.set_value("original draft");
    app.handle_pi_message(PiMsg::ExtensionUiRequest(ext_input_card(
        "e-invalidated",
        "Answer?",
    )));
    app.input.set_value("partial card answer");

    let _ = app.handle_pi_message(PiMsg::AgentDone {
        usage: None,
        stop_reason: StopReason::Aborted,
        error_message: None,
    });

    assert!(app.active_extension_ui.is_none());
    assert!(app.active_input_card_kind.is_none());
    assert_eq!(app.input.value(), "original draft");
    assert!(app.card_draft_snapshot.is_none());
}

/// bd-1qol9 terminal/abort cleanup: AgentDone invalidates the ACTIVE ask,
/// every QUEUED card of both kinds, and answers them fail-closed BEFORE any
/// RunPending/idle input can run. Mutation-sensitive: before this fix the
/// queues survived the turn boundary and could swallow the next idle prompt.
#[test]
fn agent_done_invalidates_all_outstanding_cards_before_idle() {
    let dir = tempdir();
    let mut app = build_test_app(dir.path().to_path_buf());
    app.set_terminal_size(100, 30);
    app.ask_tool = Some(crate::ask::AskTool::new(crate::ask::AskPolicy::Recommended));

    let request: crate::ask::AskRequest = serde_json::from_value(json!({
        "questions": [{"question": "Active?", "options": [{"label": "A"}]}]
    }))
    .expect("active ask");
    if let Some(tool) = app.ask_tool.as_ref() {
        tool.register_channel_ui_request_for_tests("a-done");
        tool.register_channel_ui_request_for_tests("a-queued");
    }
    app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
        id: "a-done".to_string(),
        request,
    }));
    app.handle_pi_message(PiMsg::AskUiRequest(crate::ask::AskUiRequest {
        id: "a-queued".to_string(),
        request: serde_json::from_value(json!({
            "questions": [{"question": "Queued?", "options": [{"label": "B"}]}]
        }))
        .expect("queued ask"),
    }));
    app.handle_pi_message(PiMsg::ExtensionUiRequest(ext_input_card("e-done", "late?")));

    assert!(!app.ask_ui_queue.is_empty());
    assert!(!app.extension_ui_queue.is_empty());

    app.pending_inputs
        .push_back(PendingInput::Text("queued user input".to_string()));
    let cmd = app.handle_pi_message(PiMsg::AgentDone {
        usage: None,
        stop_reason: StopReason::Aborted,
        error_message: None,
    });

    assert!(cmd.is_some(), "idle handoff still schedules RunPending");
    assert!(app.active_ask_ui.is_none());
    assert!(app.active_extension_ui.is_none());
    assert!(app.ask_ui_queue.is_empty(), "queued asks invalidated");
    assert!(app.extension_ui_queue.is_empty(), "queued exts invalidated");
    assert!(app.active_input_card_kind.is_none());
    assert!(
        app.status_message.as_deref() == Some("Question dismissed")
            || app
                .status_message
                .as_deref()
                .is_some_and(|msg| msg.starts_with("Ask request expired")),
        "dismissal surfaced: {:?}",
        app.status_message
    );
}
