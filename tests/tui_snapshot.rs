#![allow(clippy::unnecessary_literal_bound)]

mod common;

use asupersync::channel::mpsc;
use bubbletea::{KeyMsg, KeyType, Message, Model as BubbleteaModel};
use common::TestHarness;
use futures::stream;
use pi::agent::{Agent, AgentConfig};
use pi::config::Config;
use pi::interactive::{ConversationMessage, MessageRole, PiApp, PiMsg};
use pi::keybindings::KeyBindings;
use pi::model::{ContentBlock, Cost, StopReason, StreamEvent, TextContent, Usage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::tools::ToolRegistry;
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

fn test_runtime_handle() -> asupersync::runtime::RuntimeHandle {
    static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        // These tests run in parallel by default. Use a multi-thread runtime so
        // async tasks aren't starved under suite load.
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
        _context: &Context<'_>,
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
        oauth_config: None,
    }
}

fn build_app(harness: &TestHarness) -> PiApp {
    build_app_with_config(harness, Config::default())
}

fn build_app_with_config(harness: &TestHarness, config: Config) -> PiApp {
    let config = common::hermetic_interactive_config(config);
    let cwd = harness.temp_dir().to_path_buf();
    // Stop VCS discovery at the hermetic test root. Remote test runners may
    // place temporary directories inside a repository, while local runners
    // often place them outside one; without a local sentinel every snapshot
    // depends on the runner's checkout branch. Individual VCS snapshots can
    // seed a valid HEAD before constructing the app.
    let git_dir = cwd.join(".git");
    fs::create_dir_all(&git_dir).expect("create hermetic git sentinel");
    let git_head = git_dir.join("HEAD");
    if !git_head.exists() {
        fs::write(&git_head, "not-a-git-ref\n").expect("write hermetic git sentinel");
    }
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
        None,
        Vec::new(),
        event_tx,
        test_runtime_handle(),
        true,
        false,
        None,
        Some(KeyBindings::new()),
        Vec::new(),
        Usage::default(),
    );
    app.set_terminal_size(80, 24);
    app
}

fn send_pi(app: &mut PiApp, msg: PiMsg) {
    let _ = BubbleteaModel::update(app, Message::new(msg));
}

fn send_key(app: &mut PiApp, key: KeyMsg) {
    let _ = BubbleteaModel::update(app, Message::new(key));
}

fn set_conversation(
    app: &mut PiApp,
    messages: Vec<ConversationMessage>,
    usage: Usage,
    status: Option<&str>,
) {
    send_pi(
        app,
        PiMsg::ConversationReset {
            messages,
            usage,
            status: status.map(str::to_string),
        },
    );
}

fn set_input_text(app: &mut PiApp, text: &str) {
    if !text.is_empty() {
        send_key(app, KeyMsg::from_runes(text.chars().collect()));
    }
}

fn set_multiline_input(app: &mut PiApp, lines: &[&str]) {
    send_key(app, KeyMsg::from_type(KeyType::Enter).with_alt());
    for (idx, line) in lines.iter().enumerate() {
        if !line.is_empty() {
            send_key(app, KeyMsg::from_runes(line.chars().collect()));
        }
        if idx + 1 < lines.len() {
            send_key(app, KeyMsg::from_type(KeyType::Enter));
        }
    }
}

fn strip_ansi(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("regex"));
    re.replace_all(input, "").replace('\r', "")
}

fn normalize_snapshot(input: &str) -> String {
    let stripped = strip_ansi(input);
    stripped
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn snapshot(harness: &TestHarness, name: &str, app: &PiApp, context: &[(String, String)]) {
    harness
        .log()
        .info_ctx("snapshot", format!("render {name}"), |ctx| {
            ctx.push(("name".to_string(), name.to_string()));
            for (key, value) in context {
                ctx.push((key.clone(), value.clone()));
            }
        });
    let view = normalize_snapshot(&BubbleteaModel::view(app));
    let path = harness.temp_path(format!("snapshot-{name}.txt"));
    fs::write(&path, &view).expect("write snapshot artifact");
    harness.record_artifact(format!("snapshot-{name}"), &path);
    insta::assert_snapshot!(name, view);
}

fn user_msg(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::User,
        content: text.to_string(),
        thinking: None,
        collapsed: false,
    }
}

fn assistant_msg(text: &str, thinking: Option<&str>) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::Assistant,
        content: text.to_string(),
        thinking: thinking.map(str::to_string),
        collapsed: false,
    }
}

fn system_msg(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::System,
        content: text.to_string(),
        thinking: None,
        collapsed: false,
    }
}

/// Tool message with auto-collapse for large outputs (mirrors `ConversationMessage::tool`).
fn tool_msg(content: &str) -> ConversationMessage {
    let line_count = memchr::memchr_iter(b'\n', content.as_bytes()).count() + 1;
    ConversationMessage {
        role: MessageRole::Tool,
        content: content.to_string(),
        thinking: None,
        collapsed: line_count > 20, // TOOL_AUTO_COLLAPSE_THRESHOLD
    }
}

fn tool_msg_small(content: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::Tool,
        content: content.to_string(),
        thinking: None,
        collapsed: false,
    }
}

#[test]
fn tui_snapshot_initial_state() {
    let harness = TestHarness::new("tui_snapshot_initial_state");
    let app = build_app(&harness);
    let context = vec![
        ("scenario".to_string(), "initial".to_string()),
        ("size".to_string(), "80x24".to_string()),
    ];
    snapshot(&harness, "tui_initial_state", &app, &context);
}

#[test]
fn tui_snapshot_single_user_message() {
    let harness = TestHarness::new("tui_snapshot_single_user_message");
    let mut app = build_app(&harness);
    set_conversation(
        &mut app,
        vec![user_msg("Hello, Pi!")],
        Usage::default(),
        None,
    );
    let context = vec![
        ("scenario".to_string(), "single-user".to_string()),
        ("messages".to_string(), "1".to_string()),
    ];
    snapshot(&harness, "tui_single_user_message", &app, &context);
}

#[test]
fn tui_snapshot_single_assistant_message() {
    let harness = TestHarness::new("tui_snapshot_single_assistant_message");
    let mut app = build_app(&harness);
    set_conversation(
        &mut app,
        vec![assistant_msg("Hello from the assistant.", None)],
        Usage::default(),
        None,
    );
    let context = vec![
        ("scenario".to_string(), "single-assistant".to_string()),
        ("messages".to_string(), "1".to_string()),
    ];
    snapshot(&harness, "tui_single_assistant_message", &app, &context);
}

#[test]
fn tui_snapshot_assistant_with_thinking() {
    let harness = TestHarness::new("tui_snapshot_assistant_with_thinking");
    let mut app = build_app(&harness);
    set_conversation(
        &mut app,
        vec![assistant_msg(
            "Answer text.",
            Some("Reasoning details here."),
        )],
        Usage::default(),
        None,
    );
    let context = vec![
        ("scenario".to_string(), "assistant-thinking".to_string()),
        ("messages".to_string(), "1".to_string()),
    ];
    snapshot(&harness, "tui_assistant_with_thinking", &app, &context);
}

#[test]
fn tui_snapshot_multi_turn_conversation() {
    let harness = TestHarness::new("tui_snapshot_multi_turn_conversation");
    let mut app = build_app(&harness);
    let messages = vec![
        user_msg("Hi there."),
        assistant_msg("Hello!", None),
        user_msg("How are you?"),
        assistant_msg("Doing great, thanks.", None),
    ];
    set_conversation(&mut app, messages, Usage::default(), None);
    let context = vec![
        ("scenario".to_string(), "multi-turn".to_string()),
        ("messages".to_string(), "4".to_string()),
    ];
    snapshot(&harness, "tui_multi_turn_conversation", &app, &context);
}

#[test]
fn tui_snapshot_system_message() {
    let harness = TestHarness::new("tui_snapshot_system_message");
    let mut app = build_app(&harness);
    set_conversation(
        &mut app,
        vec![system_msg("System notice: configuration loaded.")],
        Usage::default(),
        None,
    );
    let context = vec![
        ("scenario".to_string(), "system-message".to_string()),
        ("messages".to_string(), "1".to_string()),
    ];
    snapshot(&harness, "tui_system_message", &app, &context);
}

#[test]
fn tui_snapshot_streaming_text() {
    let harness = TestHarness::new("tui_snapshot_streaming_text");
    let mut app = build_app(&harness);
    send_pi(&mut app, PiMsg::AgentStart);
    send_pi(
        &mut app,
        PiMsg::TextDelta("Streaming response...".to_string()),
    );
    let context = vec![
        ("scenario".to_string(), "streaming-text".to_string()),
        ("state".to_string(), "processing".to_string()),
    ];
    snapshot(&harness, "tui_streaming_text", &app, &context);
}

#[test]
fn tui_snapshot_streaming_thinking() {
    let harness = TestHarness::new("tui_snapshot_streaming_thinking");
    let mut app = build_app(&harness);
    send_pi(&mut app, PiMsg::AgentStart);
    send_pi(
        &mut app,
        PiMsg::ThinkingDelta("Considering options...".to_string()),
    );
    send_pi(&mut app, PiMsg::TextDelta("Partial answer.".to_string()));
    let context = vec![
        ("scenario".to_string(), "streaming-thinking".to_string()),
        ("state".to_string(), "processing".to_string()),
    ];
    snapshot(&harness, "tui_streaming_thinking", &app, &context);
}

#[test]
fn tui_snapshot_tool_running() {
    let harness = TestHarness::new("tui_snapshot_tool_running");
    let mut app = build_app(&harness);
    send_pi(
        &mut app,
        PiMsg::ToolStart {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    let context = vec![
        ("scenario".to_string(), "tool-running".to_string()),
        ("tool".to_string(), "read".to_string()),
    ];
    snapshot(&harness, "tui_tool_running", &app, &context);
}

#[test]
fn tui_snapshot_tool_output_message() {
    let harness = TestHarness::new("tui_snapshot_tool_output_message");
    let mut app = build_app(&harness);
    send_pi(
        &mut app,
        PiMsg::ToolStart {
            name: "read".to_string(),
            tool_id: "tool-2".to_string(),
        },
    );
    send_pi(
        &mut app,
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-2".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("file contents here"))],
            details: None,
        },
    );
    send_pi(
        &mut app,
        PiMsg::ToolEnd {
            name: "read".to_string(),
            tool_id: "tool-2".to_string(),
            is_error: false,
        },
    );
    send_pi(
        &mut app,
        PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        },
    );
    let context = vec![
        ("scenario".to_string(), "tool-output".to_string()),
        ("tool".to_string(), "read".to_string()),
    ];
    snapshot(&harness, "tui_tool_output_message", &app, &context);
}

#[test]
fn tui_snapshot_status_message() {
    let harness = TestHarness::new("tui_snapshot_status_message");
    let mut app = build_app(&harness);
    send_pi(
        &mut app,
        PiMsg::ResourcesReloaded {
            resources: ResourceLoader::empty(true),
            status: "Reloaded resources".to_string(),
            diagnostics: None,
        },
    );
    let context = vec![
        ("scenario".to_string(), "status-message".to_string()),
        ("status".to_string(), "reloaded".to_string()),
    ];
    snapshot(&harness, "tui_status_message", &app, &context);
}

#[test]
fn tui_snapshot_input_single_line_text() {
    let harness = TestHarness::new("tui_snapshot_input_single_line_text");
    let mut app = build_app(&harness);
    set_input_text(&mut app, "hello world");
    let context = vec![
        ("scenario".to_string(), "input-single-line".to_string()),
        ("input".to_string(), "hello world".to_string()),
    ];
    snapshot(&harness, "tui_input_single_line", &app, &context);
}

#[test]
fn tui_snapshot_input_bash_mode() {
    let harness = TestHarness::new("tui_snapshot_input_bash_mode");
    let mut app = build_app(&harness);
    set_input_text(&mut app, "!ls -la");
    let context = vec![
        ("scenario".to_string(), "input-bash-mode".to_string()),
        ("input".to_string(), "!ls -la".to_string()),
    ];
    snapshot(&harness, "tui_input_bash_mode", &app, &context);
}

#[test]
fn tui_snapshot_input_multi_line_text() {
    let harness = TestHarness::new("tui_snapshot_input_multi_line_text");
    let mut app = build_app(&harness);
    set_multiline_input(&mut app, &["first line", "second line"]);
    let context = vec![
        ("scenario".to_string(), "input-multi-line".to_string()),
        ("lines".to_string(), "2".to_string()),
    ];
    snapshot(&harness, "tui_input_multi_line", &app, &context);
}

#[test]
fn tui_snapshot_scrolled_viewport() {
    let harness = TestHarness::new("tui_snapshot_scrolled_viewport");
    let mut app = build_app(&harness);
    let mut messages = Vec::new();
    for idx in 1..=12 {
        messages.push(user_msg(&format!("User message {idx}")));
        messages.push(assistant_msg(&format!("Assistant reply {idx}"), None));
    }
    set_conversation(&mut app, messages, Usage::default(), None);
    send_key(&mut app, KeyMsg::from_type(KeyType::PgUp));
    let context = vec![
        ("scenario".to_string(), "scrolled".to_string()),
        ("messages".to_string(), "24".to_string()),
    ];
    snapshot(&harness, "tui_scrolled_viewport", &app, &context);
}

#[test]
fn tui_snapshot_footer_with_usage() {
    let harness = TestHarness::new("tui_snapshot_footer_with_usage");
    let git_dir = harness.temp_dir().join(".git");
    fs::create_dir(&git_dir).expect("create deterministic git metadata");
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/snapshot-fixture\n")
        .expect("write deterministic git HEAD");
    let mut app = build_app(&harness);
    let usage = Usage {
        input: 120,
        output: 45,
        total_tokens: 165,
        cost: Cost {
            input: 0.001,
            output: 0.002,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.003,
        },
        ..Usage::default()
    };
    set_conversation(
        &mut app,
        vec![assistant_msg("Usage sample.", None)],
        usage,
        None,
    );
    let context = vec![
        ("scenario".to_string(), "usage-footer".to_string()),
        ("tokens".to_string(), "165".to_string()),
        ("branch".to_string(), "snapshot-fixture".to_string()),
    ];
    snapshot(&harness, "tui_footer_with_usage", &app, &context);
}

#[test]
fn tui_snapshot_wrapped_message() {
    let harness = TestHarness::new("tui_snapshot_wrapped_message");
    let mut app = build_app(&harness);
    app.set_terminal_size(50, 20);
    let long_text = "This is a longer assistant response that should wrap across multiple lines.";
    set_conversation(
        &mut app,
        vec![assistant_msg(long_text, None)],
        Usage::default(),
        None,
    );
    let context = vec![
        ("scenario".to_string(), "wrapped-message".to_string()),
        ("size".to_string(), "50x20".to_string()),
    ];
    snapshot(&harness, "tui_wrapped_message", &app, &context);
}

// =========================================================================
// Header + Tool/Thinking collapse toggle snapshot tests (bd-w4dn)
// =========================================================================

#[test]
fn tui_snapshot_thinking_hidden_by_config() {
    let harness = TestHarness::new("tui_snapshot_thinking_hidden_by_config");
    let config = Config {
        hide_thinking_block: Some(true),
        ..Config::default()
    };
    let mut app = build_app_with_config(&harness, config);
    set_conversation(
        &mut app,
        vec![assistant_msg(
            "Answer text.",
            Some("Internal reasoning that should be hidden."),
        )],
        Usage::default(),
        None,
    );
    let context = vec![
        (
            "scenario".to_string(),
            "thinking-hidden-by-config".to_string(),
        ),
        ("hide_thinking_block".to_string(), "true".to_string()),
    ];
    snapshot(&harness, "tui_thinking_hidden_by_config", &app, &context);
}

#[test]
fn tui_snapshot_thinking_toggled_visible() {
    let harness = TestHarness::new("tui_snapshot_thinking_toggled_visible");
    let config = Config {
        hide_thinking_block: Some(true),
        ..Config::default()
    };
    let mut app = build_app_with_config(&harness, config);
    set_conversation(
        &mut app,
        vec![assistant_msg(
            "Answer after toggle.",
            Some("Reasoning now visible."),
        )],
        Usage::default(),
        None,
    );
    // Toggle thinking visible via Ctrl+T
    send_key(&mut app, KeyMsg::from_type(KeyType::CtrlT));
    let context = vec![
        (
            "scenario".to_string(),
            "thinking-toggled-visible".to_string(),
        ),
        ("initial_hidden".to_string(), "true".to_string()),
        ("toggled".to_string(), "true".to_string()),
    ];
    snapshot(&harness, "tui_thinking_toggled_visible", &app, &context);
}

#[test]
fn tui_snapshot_thinking_toggle_status_messages() {
    let harness = TestHarness::new("tui_snapshot_thinking_toggle_status_messages");
    let mut app = build_app(&harness);
    set_conversation(
        &mut app,
        vec![assistant_msg("Text.", Some("Thinking."))],
        Usage::default(),
        None,
    );
    // Toggle thinking off (default is visible) -> "Thinking hidden"
    send_key(&mut app, KeyMsg::from_type(KeyType::CtrlT));
    let context = vec![("scenario".to_string(), "thinking-hidden-status".to_string())];
    snapshot(&harness, "tui_thinking_hidden_status", &app, &context);
}

#[test]
fn tui_snapshot_tool_output_auto_collapsed() {
    let harness = TestHarness::new("tui_snapshot_tool_output_auto_collapsed");
    let mut app = build_app(&harness);
    // Generate 30 lines of tool output (exceeds TOOL_AUTO_COLLAPSE_THRESHOLD of 20)
    let large_output: String = (1..=30)
        .map(|i| format!("Tool read output:\nline {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    set_conversation(
        &mut app,
        vec![
            user_msg("Read the file."),
            tool_msg(&large_output),
            assistant_msg("Done.", None),
        ],
        Usage::default(),
        None,
    );
    let context = vec![
        ("scenario".to_string(), "tool-auto-collapsed".to_string()),
        ("tool_lines".to_string(), "30+".to_string()),
    ];
    snapshot(&harness, "tui_tool_auto_collapsed", &app, &context);
}

#[test]
fn tui_snapshot_tool_output_small_not_collapsed() {
    let harness = TestHarness::new("tui_snapshot_tool_output_small_not_collapsed");
    let mut app = build_app(&harness);
    set_conversation(
        &mut app,
        vec![
            user_msg("Read the file."),
            tool_msg_small("Tool read output:\nfile contents here"),
            assistant_msg("Done.", None),
        ],
        Usage::default(),
        None,
    );
    let context = vec![
        (
            "scenario".to_string(),
            "tool-small-not-collapsed".to_string(),
        ),
        ("tool_lines".to_string(), "2".to_string()),
    ];
    snapshot(&harness, "tui_tool_small_not_collapsed", &app, &context);
}

#[test]
fn tui_snapshot_tool_expand_toggle() {
    let harness = TestHarness::new("tui_snapshot_tool_expand_toggle");
    let mut app = build_app(&harness);
    set_conversation(
        &mut app,
        vec![
            user_msg("Read the file."),
            tool_msg_small("Tool read output:\nshort output here"),
            assistant_msg("Done.", None),
        ],
        Usage::default(),
        None,
    );
    // Toggle tools collapsed via Ctrl+O
    send_key(&mut app, KeyMsg::from_type(KeyType::CtrlO));
    let context = vec![(
        "scenario".to_string(),
        "tool-collapsed-by-toggle".to_string(),
    )];
    snapshot(&harness, "tui_tool_collapsed_by_toggle", &app, &context);
}

#[test]
fn tui_snapshot_tool_expand_toggle_reexpand() {
    let harness = TestHarness::new("tui_snapshot_tool_expand_toggle_reexpand");
    let mut app = build_app(&harness);
    set_conversation(
        &mut app,
        vec![
            user_msg("Read the file."),
            tool_msg_small("Tool read output:\nshort output here"),
            assistant_msg("Done.", None),
        ],
        Usage::default(),
        None,
    );
    // Collapse then re-expand
    send_key(&mut app, KeyMsg::from_type(KeyType::CtrlO));
    send_key(&mut app, KeyMsg::from_type(KeyType::CtrlO));
    let context = vec![("scenario".to_string(), "tool-reexpanded".to_string())];
    snapshot(&harness, "tui_tool_reexpanded", &app, &context);
}

#[test]
fn tui_snapshot_tool_expand_status_message() {
    let harness = TestHarness::new("tui_snapshot_tool_expand_status_message");
    let mut app = build_app(&harness);
    set_conversation(
        &mut app,
        vec![tool_msg_small("Tool output:\ndata")],
        Usage::default(),
        None,
    );
    // Toggle collapse -> shows "Tool output collapsed"
    send_key(&mut app, KeyMsg::from_type(KeyType::CtrlO));
    let context = vec![("scenario".to_string(), "tool-collapse-status".to_string())];
    snapshot(&harness, "tui_tool_collapse_status", &app, &context);
}

#[test]
fn tui_snapshot_header_quiet_startup_setting() {
    let harness = TestHarness::new("tui_snapshot_header_quiet_startup_setting");
    let config = Config {
        quiet_startup: Some(true),
        ..Config::default()
    };
    let app = build_app_with_config(&harness, config);
    let context = vec![
        ("scenario".to_string(), "quiet-startup".to_string()),
        ("quiet_startup".to_string(), "true".to_string()),
    ];
    snapshot(&harness, "tui_header_quiet_startup", &app, &context);
}

#[test]
fn tui_snapshot_header_collapse_changelog_setting() {
    let harness = TestHarness::new("tui_snapshot_header_collapse_changelog_setting");
    let config = Config {
        collapse_changelog: Some(true),
        ..Config::default()
    };
    let app = build_app_with_config(&harness, config);
    let context = vec![
        ("scenario".to_string(), "collapse-changelog".to_string()),
        ("collapse_changelog".to_string(), "true".to_string()),
    ];
    snapshot(&harness, "tui_header_collapse_changelog", &app, &context);
}

#[test]
fn tui_snapshot_mixed_tool_and_thinking_toggles() {
    let harness = TestHarness::new("tui_snapshot_mixed_tool_and_thinking_toggles");
    let mut app = build_app(&harness);
    let large_output: String = (1..=25)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    set_conversation(
        &mut app,
        vec![
            user_msg("Analyze this."),
            tool_msg(&format!("Tool grep output:\n{large_output}")),
            assistant_msg("Analysis complete.", Some("Deep reasoning about the code.")),
        ],
        Usage::default(),
        None,
    );
    // Hide thinking (Ctrl+T) and expand tools (Ctrl+O twice to collapse then expand)
    send_key(&mut app, KeyMsg::from_type(KeyType::CtrlT));
    send_key(&mut app, KeyMsg::from_type(KeyType::CtrlO));
    send_key(&mut app, KeyMsg::from_type(KeyType::CtrlO));
    let context = vec![
        ("scenario".to_string(), "mixed-toggles".to_string()),
        ("thinking".to_string(), "hidden".to_string()),
        ("tools".to_string(), "expanded".to_string()),
    ];
    snapshot(&harness, "tui_mixed_toggles", &app, &context);
}
