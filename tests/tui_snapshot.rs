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

fn build_app(harness: &TestHarness) -> PiApp {
    let config = Config::default();
    let cwd = harness.temp_dir().to_path_buf();
    let tools = ToolRegistry::new(&[], &cwd, Some(&config));
    let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
    let agent = Agent::new(provider, tools, AgentConfig::default());
    let session = Session::in_memory();
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
        Vec::new(),
        event_tx,
        test_runtime_handle(),
        true,
        None,
        Some(KeyBindings::new()),
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
    }
}

fn assistant_msg(text: &str, thinking: Option<&str>) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::Assistant,
        content: text.to_string(),
        thinking: thinking.map(str::to_string),
    }
}

fn system_msg(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::System,
        content: text.to_string(),
        thinking: None,
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
