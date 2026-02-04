#![allow(clippy::unnecessary_literal_bound)]

mod common;

use asupersync::channel::mpsc;
use bubbletea::{Cmd, KeyMsg, KeyType, Message, Model as BubbleteaModel, QuitMsg};
use common::TestHarness;
use futures::stream;
use pi::agent::{Agent, AgentConfig};
use pi::config::Config;
use pi::extensions::ExtensionUiRequest;
use pi::interactive::{ConversationMessage, MessageRole, PendingInput, PiApp, PiMsg};
use pi::keybindings::KeyBindings;
use pi::model::{ContentBlock, Cost, StopReason, StreamEvent, TextContent, Usage, UserContent};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::session::SessionMessage;
use pi::tools::ToolRegistry;
use regex::Regex;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

fn test_runtime_handle() -> asupersync::runtime::RuntimeHandle {
    static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        asupersync::runtime::RuntimeBuilder::current_thread()
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

fn dummy_model_entry() -> ModelEntry {
    let model = Model {
        id: "dummy-model".to_string(),
        name: "Dummy Model".to_string(),
        api: "dummy-api".to_string(),
        provider: "dummy".to_string(),
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
    }
}

fn build_app_with_session(
    harness: &TestHarness,
    pending_inputs: Vec<PendingInput>,
    session: Session,
) -> PiApp {
    let config = Config::default();
    let cwd = harness.temp_dir().to_path_buf();
    let tools = ToolRegistry::new(&[], &cwd, Some(&config));
    let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
    let agent = Agent::new(provider, tools, AgentConfig::default());
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
    let model_entry = dummy_model_entry();
    let model_scope = vec![model_entry.clone()];
    let available_models = vec![model_entry.clone()];
    let (event_tx, _event_rx) = mpsc::channel(1024);

    let mut app = PiApp::new(
        agent,
        session,
        config,
        resources,
        resource_cli,
        cwd,
        model_entry,
        model_scope,
        available_models,
        pending_inputs,
        event_tx,
        test_runtime_handle(),
        false,
        None,
        Some(KeyBindings::new()),
    );
    app.set_terminal_size(80, 24);
    app
}

fn build_app(harness: &TestHarness, pending_inputs: Vec<PendingInput>) -> PiApp {
    build_app_with_session(harness, pending_inputs, Session::in_memory())
}

fn strip_ansi(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("regex"));
    re.replace_all(input, "").replace('\r', "")
}

fn normalize_view(input: &str) -> String {
    let stripped = strip_ansi(input);
    stripped
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone)]
struct ViewDelta {
    before_lines: usize,
    after_lines: usize,
    changed_lines: usize,
    first_changed_line: Option<usize>,
    before_excerpt: String,
    after_excerpt: String,
}

fn compute_view_delta(before: &str, after: &str) -> ViewDelta {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let max_len = before_lines.len().max(after_lines.len());
    let mut changed_lines = 0usize;
    let mut first_changed_line = None;

    for idx in 0..max_len {
        let left = before_lines.get(idx).copied().unwrap_or("");
        let right = after_lines.get(idx).copied().unwrap_or("");
        if left != right {
            changed_lines += 1;
            if first_changed_line.is_none() {
                first_changed_line = Some(idx);
            }
        }
    }

    let (before_excerpt, after_excerpt) = first_changed_line.map_or_else(
        || (String::new(), String::new()),
        |idx| {
            let start = idx.saturating_sub(2);
            let end_before = (idx + 3).min(before_lines.len());
            let end_after = (idx + 3).min(after_lines.len());
            (
                before_lines[start..end_before].join("\\n"),
                after_lines[start..end_after].join("\\n"),
            )
        },
    );

    ViewDelta {
        before_lines: before_lines.len(),
        after_lines: after_lines.len(),
        changed_lines,
        first_changed_line,
        before_excerpt,
        after_excerpt,
    }
}

struct StepOutcome {
    label: String,
    before: String,
    after: String,
    cmd: Option<Cmd>,
    delta: ViewDelta,
}

fn log_initial_state(harness: &TestHarness, app: &PiApp) {
    let view = normalize_view(&BubbleteaModel::view(app));
    let mode = if view.contains("[multi-line]") {
        "multi"
    } else if view.contains("[single-line]") {
        "single"
    } else if view.contains("Processing...") {
        "processing"
    } else {
        "unknown"
    };

    harness.log().info_ctx("state", "initial", |ctx| {
        ctx.push(("mode".to_string(), mode.to_string()));
        ctx.push(("lines".to_string(), view.lines().count().to_string()));
    });
}

fn apply_msg(harness: &TestHarness, app: &mut PiApp, label: &str, msg: Message) -> StepOutcome {
    let before = normalize_view(&BubbleteaModel::view(app));
    harness.log().info_ctx("input", label, |ctx| {
        ctx.push((
            "before_lines".to_string(),
            before.lines().count().to_string(),
        ));
    });
    let cmd = BubbleteaModel::update(app, msg);
    let after = normalize_view(&BubbleteaModel::view(app));
    let delta = compute_view_delta(&before, &after);

    harness.log().info_ctx("delta", label, |ctx| {
        ctx.push(("before_lines".to_string(), delta.before_lines.to_string()));
        ctx.push(("after_lines".to_string(), delta.after_lines.to_string()));
        ctx.push(("changed_lines".to_string(), delta.changed_lines.to_string()));
        ctx.push((
            "first_changed".to_string(),
            delta
                .first_changed_line
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
        ));
        if !delta.before_excerpt.is_empty() || !delta.after_excerpt.is_empty() {
            ctx.push(("before_excerpt".to_string(), delta.before_excerpt.clone()));
            ctx.push(("after_excerpt".to_string(), delta.after_excerpt.clone()));
        }
    });

    StepOutcome {
        label: label.to_string(),
        before,
        after,
        cmd,
        delta,
    }
}

fn apply_pi(harness: &TestHarness, app: &mut PiApp, label: &str, msg: PiMsg) -> StepOutcome {
    apply_msg(harness, app, label, Message::new(msg))
}

fn apply_key(harness: &TestHarness, app: &mut PiApp, label: &str, key: KeyMsg) -> StepOutcome {
    apply_msg(harness, app, label, Message::new(key))
}

fn record_step_artifacts(harness: &TestHarness, step: &StepOutcome) {
    let slug = step
        .label
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();

    let before_path = harness.temp_path(format!("view-before-{slug}.txt"));
    fs::write(&before_path, &step.before).expect("write before view");
    harness.record_artifact(format!("view-before-{slug}"), &before_path);

    let after_path = harness.temp_path(format!("view-after-{slug}.txt"));
    fs::write(&after_path, &step.after).expect("write after view");
    harness.record_artifact(format!("view-after-{slug}"), &after_path);
}

fn fail_step(harness: &TestHarness, step: &StepOutcome, message: &str) -> ! {
    record_step_artifacts(harness, step);
    std::panic::panic_any(format!(
        "{message}\nlabel={}\nchanged_lines={}\nfirst_changed_line={:?}\n",
        step.label, step.delta.changed_lines, step.delta.first_changed_line
    ));
}

fn assert_after_contains(harness: &TestHarness, step: &StepOutcome, needle: &str) {
    if !step.after.contains(needle) {
        fail_step(
            harness,
            step,
            &format!("Expected view to contain: {needle}"),
        );
    }
}

fn assert_after_not_contains(harness: &TestHarness, step: &StepOutcome, needle: &str) {
    if step.after.contains(needle) {
        fail_step(
            harness,
            step,
            &format!("Expected view NOT to contain: {needle}"),
        );
    }
}

fn assert_cmd_is_quit(harness: &TestHarness, mut step: StepOutcome) {
    let Some(cmd) = step.cmd.take() else {
        fail_step(
            harness,
            &step,
            "Expected a quit command, but update returned None",
        );
    };
    let msg = cmd.execute().unwrap_or_else(|| {
        std::panic::panic_any(format!(
            "Quit cmd produced no message (label={})",
            step.label
        ))
    });
    if !msg.is::<QuitMsg>() {
        fail_step(harness, &step, "Expected quit command to produce QuitMsg");
    }
}

fn type_text(harness: &TestHarness, app: &mut PiApp, text: &str) -> StepOutcome {
    apply_key(
        harness,
        app,
        &format!("type:{text}"),
        KeyMsg::from_runes(text.chars().collect()),
    )
}

fn press_enter(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:Enter", KeyMsg::from_type(KeyType::Enter))
}

fn press_alt_enter(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(
        harness,
        app,
        "key:Alt+Enter",
        KeyMsg::from_type(KeyType::Enter).with_alt(),
    )
}

fn press_esc(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:Esc", KeyMsg::from_type(KeyType::Esc))
}

fn press_ctrlc(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:CtrlC", KeyMsg::from_type(KeyType::CtrlC))
}

fn press_up(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:Up", KeyMsg::from_type(KeyType::Up))
}

fn press_down(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:Down", KeyMsg::from_type(KeyType::Down))
}

fn press_pgup(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:PgUp", KeyMsg::from_type(KeyType::PgUp))
}

fn press_pgdown(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(
        harness,
        app,
        "key:PgDown",
        KeyMsg::from_type(KeyType::PgDown),
    )
}

fn press_left(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:Left", KeyMsg::from_type(KeyType::Left))
}

fn press_tab(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:Tab", KeyMsg::from_type(KeyType::Tab))
}

fn user_msg(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::User,
        content: text.to_string(),
        thinking: None,
    }
}

fn assistant_msg(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::Assistant,
        content: text.to_string(),
        thinking: None,
    }
}

fn parse_scroll_percent(view: &str) -> Option<u32> {
    let marker = view
        .lines()
        .find(|line| line.contains("PgUp/PgDn to scroll"))?;
    let open = marker.find('[')?;
    let close = marker[open + 1..].find('%')?;
    marker[open + 1..open + 1 + close].parse::<u32>().ok()
}

fn sample_usage(input: u64, output: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
        total_tokens: input + output,
        cost: Cost::default(),
    }
}

#[test]
fn tui_state_escape_does_nothing_when_idle_single_line() {
    // Legacy behavior: Escape when idle (no overlay/autocomplete) does nothing
    let harness = TestHarness::new("tui_state_escape_does_nothing_when_idle_single_line");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let step = press_esc(&harness, &mut app);
    // Should NOT quit, just do nothing
    assert!(
        step.cmd.is_none(),
        "Escape when idle should not produce a command"
    );
}

#[test]
fn tui_state_double_escape_opens_tree_by_default() {
    let harness = TestHarness::new("tui_state_double_escape_opens_tree_by_default");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let step = press_esc(&harness, &mut app);
    assert!(
        step.cmd.is_none(),
        "First Escape should not produce a command"
    );
    let step = press_esc(&harness, &mut app);
    assert_after_contains(&harness, &step, "Session Tree");
}

#[test]
fn tui_state_escape_exits_multiline_instead_of_quit() {
    let harness = TestHarness::new("tui_state_escape_exits_multiline_instead_of_quit");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let step = press_alt_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "[multi-line]");
    let step = press_esc(&harness, &mut app);
    assert_after_contains(&harness, &step, "[single-line]");
}

#[test]
fn tui_state_tab_completes_path_when_cursor_in_token() {
    let harness = TestHarness::new("tui_state_tab_completes_path_when_cursor_in_token");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    std::fs::create_dir_all(harness.temp_dir().join("src")).expect("mkdir");
    std::fs::write(harness.temp_dir().join("src/main.rs"), "fn main() {}").expect("write");

    let step = type_text(&harness, &mut app, "src/ma other");
    assert_after_contains(&harness, &step, "src/ma other");

    for _ in 0..6 {
        let _ = press_left(&harness, &mut app);
    }

    let step = press_tab(&harness, &mut app);
    assert_after_contains(&harness, &step, "src/main.rs other");
    assert_after_not_contains(&harness, &step, "Enter/Tab accept");
}

#[test]
fn tui_state_tab_opens_autocomplete_for_ambiguous_paths() {
    let harness = TestHarness::new("tui_state_tab_opens_autocomplete_for_ambiguous_paths");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    std::fs::create_dir_all(harness.temp_dir().join("src")).expect("mkdir");
    std::fs::write(harness.temp_dir().join("src/main.rs"), "fn main() {}").expect("write");
    std::fs::write(harness.temp_dir().join("src/make.rs"), "pub fn make() {}").expect("write");

    let step = type_text(&harness, &mut app, "src/ma");
    assert_after_contains(&harness, &step, "src/ma");
    assert_after_not_contains(&harness, &step, "Enter/Tab accept");

    let step = press_tab(&harness, &mut app);
    assert_after_contains(&harness, &step, "Enter/Tab accept");
    assert_after_contains(&harness, &step, "src/main.rs");
    assert_after_contains(&harness, &step, "src/make.rs");
    assert_after_contains(&harness, &step, "src/ma");
}

#[test]
fn tui_state_ctrlc_clears_input_when_has_text() {
    // Legacy behavior: Ctrl+C with text clears the editor
    let harness = TestHarness::new("tui_state_ctrlc_clears_input_when_has_text");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "hello world");
    let step = press_ctrlc(&harness, &mut app);
    // Should clear input, not quit
    assert!(
        step.cmd.is_none(),
        "Ctrl+C with text should clear, not quit"
    );
    assert_after_contains(&harness, &step, "Input cleared");
}

#[test]
fn tui_state_ctrlc_double_tap_quits_when_idle() {
    // Legacy behavior: Ctrl+C twice in quick succession quits
    let harness = TestHarness::new("tui_state_ctrlc_double_tap_quits_when_idle");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // First Ctrl+C shows hint
    let step = press_ctrlc(&harness, &mut app);
    assert!(step.cmd.is_none(), "First Ctrl+C should not quit");
    assert_after_contains(&harness, &step, "Press Ctrl+C again to quit");

    // Second Ctrl+C quits
    let step = press_ctrlc(&harness, &mut app);
    assert_cmd_is_quit(&harness, step);
}

#[test]
fn tui_state_ctrlc_aborts_when_processing() {
    let harness = TestHarness::new("tui_state_ctrlc_aborts_when_processing");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "hello");
    press_enter(&harness, &mut app);
    let step = press_ctrlc(&harness, &mut app);
    assert_after_contains(&harness, &step, "Aborting request...");
}

#[test]
fn tui_state_enter_submits_in_single_line_mode() {
    let harness = TestHarness::new("tui_state_enter_submits_in_single_line_mode");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "hello world");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Processing...");
}

#[test]
fn tui_state_alt_enter_enables_multiline_mode() {
    let harness = TestHarness::new("tui_state_alt_enter_enables_multiline_mode");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let step = press_alt_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "[multi-line]");
}

#[test]
fn tui_state_alt_enter_submits_when_multiline_mode_and_non_empty() {
    let harness = TestHarness::new("tui_state_alt_enter_submits_when_multiline_mode_and_non_empty");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    press_alt_enter(&harness, &mut app);
    type_text(&harness, &mut app, "multi line submit");
    let step = press_alt_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Processing...");
}

#[test]
fn tui_state_enter_in_multiline_mode_inserts_newline_not_submit() {
    let harness = TestHarness::new("tui_state_enter_in_multiline_mode_inserts_newline_not_submit");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    press_alt_enter(&harness, &mut app);
    type_text(&harness, &mut app, "line1");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "[multi-line]");
    assert_after_not_contains(&harness, &step, "Processing...");
}

#[test]
fn tui_state_history_up_shows_last_submitted_input() {
    let harness = TestHarness::new("tui_state_history_up_shows_last_submitted_input");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "first");
    press_enter(&harness, &mut app);
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::AgentDone(stop)",
        PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        },
    );

    let step = press_up(&harness, &mut app);
    assert_after_contains(&harness, &step, "> first");
}

#[test]
fn tui_state_history_down_clears_input_after_history_up() {
    let harness = TestHarness::new("tui_state_history_down_clears_input_after_history_up");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "first");
    press_enter(&harness, &mut app);
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::AgentDone(stop)",
        PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        },
    );

    press_up(&harness, &mut app);
    let step = press_down(&harness, &mut app);
    assert_after_not_contains(&harness, &step, "> first");
}

#[test]
fn tui_state_pageup_changes_scroll_percent_when_scrollable() {
    let harness = TestHarness::new("tui_state_pageup_changes_scroll_percent_when_scrollable");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let messages = (0..40)
        .map(|idx| user_msg(&format!("line {idx}")))
        .collect::<Vec<_>>();
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ConversationReset(many)",
        PiMsg::ConversationReset {
            messages,
            usage: Usage::default(),
            status: None,
        },
    );

    let baseline_view = normalize_view(&BubbleteaModel::view(&app));
    let baseline_percent = parse_scroll_percent(&baseline_view).expect("no scroll indicator");

    let step = press_pgup(&harness, &mut app);
    let after_percent = parse_scroll_percent(&step.after).expect("no percent");
    assert!(
        after_percent < baseline_percent,
        "Expected PgUp percent < baseline ({after_percent} < {baseline_percent})"
    );
}

#[test]
fn tui_state_pagedown_restores_scroll_percent_when_scrollable() {
    let harness = TestHarness::new("tui_state_pagedown_restores_scroll_percent_when_scrollable");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let messages = (0..40)
        .map(|idx| user_msg(&format!("line {idx}")))
        .collect::<Vec<_>>();
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ConversationReset(many)",
        PiMsg::ConversationReset {
            messages,
            usage: Usage::default(),
            status: None,
        },
    );

    press_pgup(&harness, &mut app);
    let step = press_pgdown(&harness, &mut app);
    let percent = parse_scroll_percent(&step.after).expect("no percent");
    assert_eq!(percent, 100, "Expected PgDn to return to bottom (100%)");
}

#[test]
fn tui_state_agent_start_enters_processing() {
    let harness = TestHarness::new("tui_state_agent_start_enters_processing");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let step = apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    assert_after_contains(&harness, &step, "Processing...");
}

#[test]
fn tui_state_pending_message_queue_shows_steering_preview_while_busy() {
    let harness =
        TestHarness::new("tui_state_pending_message_queue_shows_steering_preview_while_busy");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "queued steering");
    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);

    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Pending:");
    assert_after_contains(&harness, &step, "queued steering");
}

#[test]
fn tui_state_pending_message_queue_shows_follow_up_preview_while_busy() {
    let harness =
        TestHarness::new("tui_state_pending_message_queue_shows_follow_up_preview_while_busy");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "queued follow-up");
    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);

    let step = press_alt_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Pending:");
    assert_after_contains(&harness, &step, "queued follow-up");
}

#[test]
fn tui_state_text_delta_renders_while_processing() {
    let harness = TestHarness::new("tui_state_text_delta_renders_while_processing");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::TextDelta",
        PiMsg::TextDelta("hello".to_string()),
    );
    assert_after_contains(&harness, &step, "Assistant:");
    assert_after_contains(&harness, &step, "hello");
}

#[test]
fn tui_state_thinking_delta_renders_while_processing() {
    let harness = TestHarness::new("tui_state_thinking_delta_renders_while_processing");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ThinkingDelta",
        PiMsg::ThinkingDelta("hmm".to_string()),
    );
    assert_after_contains(&harness, &step, "Thinking:");
    assert_after_contains(&harness, &step, "hmm");
}

#[test]
fn tui_state_tool_start_shows_running_tool_status() {
    let harness = TestHarness::new("tui_state_tool_start_shows_running_tool_status");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(read)",
        PiMsg::ToolStart {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    assert_after_contains(&harness, &step, "Running read");
}

#[test]
fn tui_state_tool_update_does_not_emit_output_until_tool_end() {
    let harness = TestHarness::new("tui_state_tool_update_does_not_emit_output_until_tool_end");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(read)",
        PiMsg::ToolStart {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(read)",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("file contents"))],
            details: None,
        },
    );
    assert_after_not_contains(&harness, &step, "Tool read output:");
}

#[test]
fn tui_state_tool_end_appends_tool_output_message() {
    let harness = TestHarness::new("tui_state_tool_end_appends_tool_output_message");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(read)",
        PiMsg::ToolStart {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(read)",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("file contents"))],
            details: None,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(read)",
        PiMsg::ToolEnd {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );
    assert_after_contains(&harness, &step, "Tool read output:");
    assert_after_contains(&harness, &step, "file contents");
}

#[test]
fn tui_state_agent_done_appends_assistant_message_and_updates_usage() {
    let harness =
        TestHarness::new("tui_state_agent_done_appends_assistant_message_and_updates_usage");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::TextDelta",
        PiMsg::TextDelta("final".to_string()),
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::AgentDone(stop+usage)",
        PiMsg::AgentDone {
            usage: Some(sample_usage(5, 7)),
            stop_reason: StopReason::Stop,
            error_message: None,
        },
    );
    assert_after_contains(&harness, &step, "Assistant:");
    assert_after_contains(&harness, &step, "final");
    assert_after_contains(&harness, &step, "Tokens: 5 in / 7 out");
}

#[test]
fn tui_state_agent_done_aborted_sets_status_message() {
    let harness = TestHarness::new("tui_state_agent_done_aborted_sets_status_message");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::AgentDone(aborted)",
        PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Aborted,
            error_message: None,
        },
    );
    assert_after_contains(&harness, &step, "Request aborted");
}

#[test]
fn tui_state_agent_done_error_without_response_adds_error_message() {
    let harness =
        TestHarness::new("tui_state_agent_done_error_without_response_adds_error_message");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::AgentDone(error,no-response)",
        PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Error,
            error_message: Some("boom".to_string()),
        },
    );
    assert_after_contains(&harness, &step, "Error: boom");
    assert_after_contains(&harness, &step, "boom");
}

#[test]
fn tui_state_agent_done_error_with_response_does_not_duplicate_error_system_message() {
    let harness = TestHarness::new(
        "tui_state_agent_done_error_with_response_does_not_duplicate_error_system_message",
    );
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::TextDelta",
        PiMsg::TextDelta("partial".to_string()),
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::AgentDone(error,with-response)",
        PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Error,
            error_message: Some("boom".to_string()),
        },
    );
    assert_after_contains(&harness, &step, "partial");
    assert_after_not_contains(&harness, &step, "Error: boom");
}

#[test]
fn tui_state_agent_error_adds_system_error_message_and_returns_idle() {
    let harness =
        TestHarness::new("tui_state_agent_error_adds_system_error_message_and_returns_idle");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::AgentError",
        PiMsg::AgentError("boom".to_string()),
    );
    assert_after_contains(&harness, &step, "Error: boom");
    assert_after_contains(&harness, &step, "[single-line]");
}

#[test]
fn tui_state_system_message_adds_system_message() {
    let harness = TestHarness::new("tui_state_system_message_adds_system_message");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::System",
        PiMsg::System("hello".to_string()),
    );
    assert_after_contains(&harness, &step, "hello");
}

#[test]
fn tui_state_conversation_reset_replaces_messages_sets_usage_and_status() {
    let harness =
        TestHarness::new("tui_state_conversation_reset_replaces_messages_sets_usage_and_status");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let messages = vec![user_msg("u1"), assistant_msg("a1")];
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ConversationReset",
        PiMsg::ConversationReset {
            messages,
            usage: sample_usage(11, 22),
            status: Some("reset ok".to_string()),
        },
    );
    assert_after_contains(&harness, &step, "reset ok");
    assert_after_contains(&harness, &step, "Tokens: 11 in / 22 out");
    assert_after_contains(&harness, &step, "You: u1");
    assert_after_contains(&harness, &step, "Assistant:");
    assert_after_contains(&harness, &step, "a1");
}

#[test]
fn tui_state_resources_reloaded_sets_status_message() {
    let harness = TestHarness::new("tui_state_resources_reloaded_sets_status_message");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let resources = ResourceLoader::empty(false);
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ResourcesReloaded",
        PiMsg::ResourcesReloaded {
            resources,
            status: "reloaded".to_string(),
        },
    );
    assert_after_contains(&harness, &step, "reloaded");
}

#[test]
fn tui_state_run_pending_text_submits_next_input() {
    let harness = TestHarness::new("tui_state_run_pending_text_submits_next_input");
    let mut app = build_app(&harness, vec![PendingInput::Text("hello".to_string())]);
    log_initial_state(&harness, &app);

    let step = apply_pi(&harness, &mut app, "PiMsg::RunPending", PiMsg::RunPending);
    assert_after_contains(&harness, &step, "You: hello");
    assert_after_contains(&harness, &step, "Processing...");
}

#[test]
fn tui_state_run_pending_content_submits_next_input() {
    let harness = TestHarness::new("tui_state_run_pending_content_submits_next_input");
    let mut app = build_app(
        &harness,
        vec![PendingInput::Content(vec![ContentBlock::Text(
            TextContent::new("hello"),
        )])],
    );
    log_initial_state(&harness, &app);

    let step = apply_pi(&harness, &mut app, "PiMsg::RunPending", PiMsg::RunPending);
    assert_after_contains(&harness, &step, "You: hello");
    assert_after_contains(&harness, &step, "Processing...");
}

#[test]
fn tui_state_slash_help_adds_help_text() {
    let harness = TestHarness::new("tui_state_slash_help_adds_help_text");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/help");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Available commands:");
    assert_after_contains(&harness, &step, "/help, /h, /?");
}

#[test]
fn tui_state_slash_theme_lists_and_switches() {
    let harness = TestHarness::new("tui_state_slash_theme_lists_and_switches");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/theme");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Available themes:");
    assert_after_contains(&harness, &step, "* dark");
    assert_after_contains(&harness, &step, "light");

    type_text(&harness, &mut app, "/theme light");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched to theme: light");

    let settings_path = harness.temp_path(".pi/settings.json");
    let settings = fs::read_to_string(settings_path).expect("read settings.json");
    assert!(
        settings.contains("\"theme\": \"light\""),
        "expected theme persisted to settings.json"
    );

    type_text(&harness, &mut app, "/theme");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "* light");
}

#[test]
fn tui_state_slash_hotkeys_shows_dynamic_keybindings() {
    let harness = TestHarness::new("tui_state_slash_hotkeys_shows_dynamic_keybindings");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/hotkeys");
    let step = press_enter(&harness, &mut app);
    // Check specific bindings are shown (viewport shows bottom of list)
    // The Selection category should be visible with its bindings
    assert_after_contains(&harness, &step, "enter");
    assert_after_contains(&harness, &step, "escape");
    // Check that action descriptions are shown
    assert_after_contains(&harness, &step, "selection");
}

#[test]
fn tui_state_slash_model_no_args_reports_current_model() {
    let harness = TestHarness::new("tui_state_slash_model_no_args_reports_current_model");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/model");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Current model: dummy/dummy-model");
}

#[test]
fn tui_state_slash_history_shows_previous_inputs() {
    let harness = TestHarness::new("tui_state_slash_history_shows_previous_inputs");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "hello");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "You: hello");

    // Return to idle deterministically (we don't need real provider output for this test).
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::AgentError",
        PiMsg::AgentError("boom".to_string()),
    );

    type_text(&harness, &mut app, "/history");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Input history (most recent first):");
    assert_after_contains(&harness, &step, "1. hello");
}

#[test]
fn tui_state_slash_session_shows_basic_info() {
    let harness = TestHarness::new("tui_state_slash_session_shows_basic_info");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/session");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Session info:");
    assert_after_contains(&harness, &step, "(not saved yet)");
}

#[test]
fn tui_state_slash_settings_shows_summary() {
    let harness = TestHarness::new("tui_state_slash_settings_shows_summary");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/settings");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Settings:");
    assert_after_contains(&harness, &step, "Resources:");
}

#[test]
fn tui_state_slash_export_writes_html_and_reports_path() {
    let harness = TestHarness::new("tui_state_slash_export_writes_html_and_reports_path");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/export");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Exported HTML:");
}

#[test]
fn tui_state_slash_share_writes_temp_html_and_reports_path() {
    let harness = TestHarness::new("tui_state_slash_share_writes_temp_html_and_reports_path");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/share");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Shared HTML:");
}

#[test]
fn tui_state_slash_resume_without_sessions_sets_status() {
    let harness = TestHarness::new("tui_state_slash_resume_without_sessions_sets_status");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/resume");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "No sessions found for this project");
}

#[test]
fn tui_state_slash_copy_reports_clipboard_unavailable_or_success() {
    let harness = TestHarness::new("tui_state_slash_copy_reports_clipboard_unavailable_or_success");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let messages = vec![assistant_msg("hello from assistant")];
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ConversationReset",
        PiMsg::ConversationReset {
            messages,
            usage: Usage::default(),
            status: None,
        },
    );

    type_text(&harness, &mut app, "/copy");
    let step = press_enter(&harness, &mut app);
    if !step.after.contains("Copied to clipboard")
        && !step.after.contains("Clipboard support is disabled")
        && !step.after.contains("Clipboard unavailable")
    {
        fail_step(
            &harness,
            &step,
            "Expected /copy to report clipboard success or unavailable",
        );
    }
}

#[test]
fn tui_state_slash_reload_sets_status_message() {
    let harness = TestHarness::new("tui_state_slash_reload_sets_status_message");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/reload");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Reloading resources...");
}

#[test]
fn tui_state_slash_thinking_sets_level() {
    let harness = TestHarness::new("tui_state_slash_thinking_sets_level");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/thinking high");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Thinking level: high");
}

#[test]
fn tui_state_slash_clear_clears_conversation_and_sets_status() {
    let harness = TestHarness::new("tui_state_slash_clear_clears_conversation_and_sets_status");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ConversationReset",
        PiMsg::ConversationReset {
            messages: vec![user_msg("hello")],
            usage: Usage::default(),
            status: None,
        },
    );
    type_text(&harness, &mut app, "/clear");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Conversation cleared");
    assert_after_contains(&harness, &step, "Welcome to Pi! Type a message to begin");
}

#[test]
fn tui_state_slash_new_resets_conversation_and_sets_status() {
    let harness = TestHarness::new("tui_state_slash_new_resets_conversation_and_sets_status");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ConversationReset",
        PiMsg::ConversationReset {
            messages: vec![user_msg("hello"), assistant_msg("world")],
            usage: sample_usage(12, 34),
            status: Some("old".to_string()),
        },
    );

    type_text(&harness, &mut app, "/new");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Started new session");
    assert_after_not_contains(&harness, &step, "You: hello");
    assert_after_not_contains(&harness, &step, "world");
    assert_after_contains(&harness, &step, "Model set to dummy/dummy-model");
    assert_after_contains(&harness, &step, "Thinking level: off");
}

#[test]
fn tui_state_slash_tree_select_root_user_message_prefills_editor_and_resets_leaf() {
    let harness = TestHarness::new(
        "tui_state_slash_tree_select_root_user_message_prefills_editor_and_resets_leaf",
    );
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Build a simple two-node chain: Root -> Child, so current leaf is Child.
    {
        let session = app.session_handle();
        let mut session_guard = session.try_lock().expect("session try_lock");
        session_guard.append_message(SessionMessage::User {
            content: UserContent::Text("Root".to_string()),
            timestamp: Some(0),
        });
        session_guard.append_message(SessionMessage::User {
            content: UserContent::Text("Child".to_string()),
            timestamp: Some(0),
        });
    }

    type_text(&harness, &mut app, "/tree");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Session Tree");

    // Move selection from Child to Root, then select.
    press_up(&harness, &mut app);
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Branch Summary");

    // Default choice is "No summary".
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched to root");
    assert_after_contains(&harness, &step, "Root");
    // Conversation should be empty after resetting leaf.
    assert_after_not_contains(&harness, &step, "You: Root");
}

#[test]
fn tui_state_extension_ui_notify_adds_system_message() {
    let harness = TestHarness::new("tui_state_extension_ui_notify_adds_system_message");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "req-1",
        "notify",
        json!({ "title": "Heads up", "message": "hello", "level": "info" }),
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(notify)",
        PiMsg::ExtensionUiRequest(request),
    );
    assert_after_contains(&harness, &step, "Extension notify (info): Heads up hello");
}

#[test]
fn tui_state_extension_ui_confirm_prompt_then_yes_sets_extensions_disabled_status() {
    let harness = TestHarness::new(
        "tui_state_extension_ui_confirm_prompt_then_yes_sets_extensions_disabled_status",
    );
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "req-1",
        "confirm",
        json!({ "title": "Confirm", "message": "Ok?" }),
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(confirm)",
        PiMsg::ExtensionUiRequest(request),
    );

    type_text(&harness, &mut app, "yes");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Extensions are disabled");
}

#[test]
fn tui_state_extension_ui_select_invalid_sets_status_and_keeps_prompt() {
    let harness =
        TestHarness::new("tui_state_extension_ui_select_invalid_sets_status_and_keeps_prompt");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "req-1",
        "select",
        json!({
            "title": "Pick one",
            "message": "Choose",
            "options": [
                {"label":"A","value":"a"},
                {"label":"B","value":"b"}
            ]
        }),
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(select)",
        PiMsg::ExtensionUiRequest(request),
    );

    type_text(&harness, &mut app, "99");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(
        &harness,
        &step,
        "Invalid selection. Enter a number, label, or 'cancel'.",
    );
    assert_after_contains(&harness, &step, "Extension select: Pick one");
}

#[test]
fn tui_state_status_message_clears_on_any_keypress() {
    let harness = TestHarness::new("tui_state_status_message_clears_on_any_keypress");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/model");
    press_enter(&harness, &mut app);

    let step = type_text(&harness, &mut app, "x");
    assert_after_not_contains(&harness, &step, "Current model: dummy/dummy-model");
}
