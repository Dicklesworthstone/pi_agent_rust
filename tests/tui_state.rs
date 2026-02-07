#![allow(clippy::unnecessary_literal_bound)]

mod common;

use asupersync::channel::mpsc;
use asupersync::sync::Mutex;
use bubbletea::{Cmd, KeyMsg, KeyType, Message, Model as BubbleteaModel, QuitMsg};
use common::TestHarness;
use futures::stream;
use pi::agent::{Agent, AgentConfig};
use pi::config::{Config, TerminalSettings};
use pi::extensions::{
    ExtensionManager, ExtensionUiRequest, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::interactive::{ConversationMessage, MessageRole, PendingInput, PiApp, PiMsg};
use pi::keybindings::KeyBindings;
use pi::model::{
    AssistantMessage, ContentBlock, Cost, ImageContent, StopReason, StreamEvent, TextContent,
    Usage, UserContent,
};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::{Session, SessionEntry, SessionMessage, encode_cwd};
use pi::tools::ToolRegistry;
use regex::Regex;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("set permissions");
}

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
        oauth_config: None,
    }
}

fn make_model_entry(provider: &str, id: &str, base_url: &str) -> ModelEntry {
    let mut entry = dummy_model_entry();
    entry.model.provider = provider.to_string();
    entry.model.id = id.to_string();
    entry.model.base_url = base_url.to_string();
    entry
}

fn build_app_with_session(
    harness: &TestHarness,
    pending_inputs: Vec<PendingInput>,
    session: Session,
) -> PiApp {
    build_app_with_session_and_config(harness, pending_inputs, session, Config::default())
}

fn build_app_with_session_and_config(
    harness: &TestHarness,
    pending_inputs: Vec<PendingInput>,
    session: Session,
    config: Config,
) -> PiApp {
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
    let (messages, usage) = conversation_from_session(&session);
    let session = Arc::new(Mutex::new(session));

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
        messages,
        usage,
    );
    app.set_terminal_size(80, 24);
    app
}

fn build_app_with_session_and_events_and_extension(
    harness: &TestHarness,
    pending_inputs: Vec<PendingInput>,
    session: Session,
    config: Config,
    extension_source: &str,
) -> (PiApp, mpsc::Receiver<PiMsg>) {
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
    let (event_tx, event_rx) = mpsc::channel(1024);
    let (messages, usage) = conversation_from_session(&session);
    let session = Arc::new(Mutex::new(session));

    let manager = ExtensionManager::new();
    let ext_entry_path = harness.create_file("extensions/ext.mjs", extension_source.as_bytes());

    let tools_for_ext = Arc::new(ToolRegistry::new(&[], &cwd, Some(&config)));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };
    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools_for_ext);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec");
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

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
        Some(manager),
        Some(KeyBindings::new()),
        messages,
        usage,
    );
    app.set_terminal_size(80, 24);
    (app, event_rx)
}

fn build_app_with_models(
    harness: &TestHarness,
    session: Session,
    config: Config,
    model_entry: ModelEntry,
    model_scope: Vec<ModelEntry>,
    available_models: Vec<ModelEntry>,
    keybindings: KeyBindings,
) -> PiApp {
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
    let (event_tx, _event_rx) = mpsc::channel(1024);
    let (messages, usage) = conversation_from_session(&session);
    let session = Arc::new(Mutex::new(session));

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
        false,
        None,
        Some(keybindings),
        messages,
        usage,
    );
    app.set_terminal_size(80, 24);
    app
}

fn read_project_settings_json(harness: &TestHarness) -> serde_json::Value {
    let path = harness.temp_dir().join(".pi/settings.json");
    let content = std::fs::read_to_string(&path).expect("read settings.json");
    serde_json::from_str(&content).expect("parse settings.json")
}

#[allow(dead_code)]
fn build_app_with_session_and_events(
    harness: &TestHarness,
    pending_inputs: Vec<PendingInput>,
    session: Session,
) -> (PiApp, mpsc::Receiver<PiMsg>) {
    build_app_with_session_and_events_and_config(
        harness,
        pending_inputs,
        session,
        Config::default(),
    )
}

#[allow(dead_code)]
fn build_app_with_session_and_events_and_config(
    harness: &TestHarness,
    pending_inputs: Vec<PendingInput>,
    session: Session,
    config: Config,
) -> (PiApp, mpsc::Receiver<PiMsg>) {
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
    let (event_tx, event_rx) = mpsc::channel(1024);
    let (messages, usage) = conversation_from_session(&session);
    let session = Arc::new(Mutex::new(session));

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
        messages,
        usage,
    );
    app.set_terminal_size(80, 24);
    (app, event_rx)
}

fn build_app(harness: &TestHarness, pending_inputs: Vec<PendingInput>) -> PiApp {
    build_app_with_session(harness, pending_inputs, Session::in_memory())
}

fn conversation_from_session(session: &Session) -> (Vec<ConversationMessage>, Usage) {
    let mut messages = Vec::new();
    let mut usage = Usage::default();

    for entry in session.entries_for_current_path() {
        let SessionEntry::Message(message_entry) = entry else {
            continue;
        };

        match &message_entry.message {
            SessionMessage::User { content, .. } => {
                messages.push(ConversationMessage {
                    role: MessageRole::User,
                    content: user_content_to_text(content),
                    thinking: None,
                    collapsed: false,
                });
            }
            SessionMessage::Assistant { message } => {
                let (text, thinking) = assistant_content_to_text(&message.content);
                add_usage(&mut usage, &message.usage);
                messages.push(ConversationMessage {
                    role: MessageRole::Assistant,
                    content: text,
                    thinking,
                    collapsed: false,
                });
            }
            SessionMessage::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } => {
                let prefix = if *is_error {
                    "Tool error"
                } else {
                    "Tool result"
                };
                let text = content_blocks_to_text(content);
                messages.push(ConversationMessage {
                    role: MessageRole::Tool,
                    content: format!("{prefix} ({tool_name}): {text}"),
                    thinking: None,
                    collapsed: false,
                });
            }
            SessionMessage::Custom {
                content, display, ..
            } => {
                if *display {
                    messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: content.clone(),
                        thinking: None,
                        collapsed: false,
                    });
                }
            }
            _ => {}
        }
    }

    (messages, usage)
}

fn user_content_to_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_blocks_to_text(blocks),
    }
}

fn assistant_content_to_text(content: &[ContentBlock]) -> (String, Option<String>) {
    let mut text = String::new();
    let mut thinking = String::new();

    for block in content {
        match block {
            ContentBlock::Text(t) => text.push_str(&t.text),
            ContentBlock::Thinking(t) => thinking.push_str(&t.thinking),
            _ => {}
        }
    }

    let thinking = if thinking.trim().is_empty() {
        None
    } else {
        Some(thinking)
    };

    (text, thinking)
}

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut output = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut output, &text_block.text),
            ContentBlock::Image(image) => {
                push_line(&mut output, &format!("[image: {}]", image.mime_type));
            }
            ContentBlock::Thinking(thinking_block) => {
                push_line(&mut output, &thinking_block.thinking);
            }
            ContentBlock::ToolCall(call) => {
                push_line(&mut output, &format!("[tool call: {}]", call.name));
            }
        }
    }
    output
}

fn push_line(out: &mut String, line: &str) {
    if line.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

fn add_usage(total: &mut Usage, delta: &Usage) {
    total.input = total.input.saturating_add(delta.input);
    total.output = total.output.saturating_add(delta.output);
    total.cache_read = total.cache_read.saturating_add(delta.cache_read);
    total.cache_write = total.cache_write.saturating_add(delta.cache_write);
    total.total_tokens = total.total_tokens.saturating_add(delta.total_tokens);
    total.cost.input += delta.cost.input;
    total.cost.output += delta.cost.output;
    total.cost.cache_read += delta.cost.cache_read;
    total.cost.cache_write += delta.cost.cache_write;
    total.cost.total += delta.cost.total;
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

fn assert_all_newlines_are_crlf(input: &str) {
    let bytes = input.as_bytes();
    for idx in 0..bytes.len() {
        if bytes[idx] == b'\n' {
            assert!(idx > 0, "Found leading LF without preceding CR");
            assert_eq!(
                bytes[idx - 1],
                b'\r',
                "Found LF at byte {idx} not preceded by CR"
            );
        }
    }
}

#[allow(dead_code)]
fn create_session_on_disk(
    base_dir: &std::path::Path,
    cwd: &std::path::Path,
    name: &str,
    user_text: &str,
) -> std::path::PathBuf {
    let project_dir = base_dir.join(encode_cwd(cwd));
    std::fs::create_dir_all(&project_dir).expect("create sessions dir");

    let mut session = Session::create_with_dir(Some(base_dir.to_path_buf()));
    session.header.cwd = cwd.display().to_string();
    session.set_name(name);
    session.append_message(SessionMessage::User {
        content: UserContent::Text(user_text.to_string()),
        timestamp: Some(0),
    });
    let path = project_dir.join(format!("{name}.jsonl"));
    session.path = Some(path.clone());
    common::run_async(async move {
        session.save().await.expect("save session");
    });
    path
}

#[allow(dead_code)]
fn create_session_on_disk_with_id(
    base_dir: &std::path::Path,
    cwd: &std::path::Path,
    name: &str,
    session_id: &str,
    user_text: &str,
) -> std::path::PathBuf {
    let project_dir = base_dir.join(encode_cwd(cwd));
    std::fs::create_dir_all(&project_dir).expect("create sessions dir");

    let mut session = Session::create_with_dir(Some(base_dir.to_path_buf()));
    session.header.cwd = cwd.display().to_string();
    session.header.id = session_id.to_string();
    session.set_name(name);
    session.append_message(SessionMessage::User {
        content: UserContent::Text(user_text.to_string()),
        timestamp: Some(0),
    });
    let path = project_dir.join(format!("{name}.jsonl"));
    session.path = Some(path.clone());
    common::run_async(async move {
        session.save().await.expect("save session");
    });
    path
}

#[allow(dead_code)]
fn wait_for_pi_msgs(
    event_rx: &mpsc::Receiver<PiMsg>,
    timeout: Duration,
    predicate: impl Fn(&[PiMsg]) -> bool,
) -> Vec<PiMsg> {
    let start = Instant::now();
    let mut events = Vec::new();
    loop {
        match event_rx.try_recv() {
            Ok(msg) => {
                events.push(msg);
                if predicate(&events) {
                    break;
                }
            }
            Err(mpsc::RecvError::Empty) => {
                if start.elapsed() >= timeout {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    events
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

const SINGLE_LINE_HINT: &str = "Enter: send  Shift+Enter: newline  Alt+Enter: multi-line";
const MULTI_LINE_HINT: &str = "Alt+Enter: send  Enter: newline  Esc: single-line";

fn log_initial_state(harness: &TestHarness, app: &PiApp) {
    let view = normalize_view(&BubbleteaModel::view(app));
    let mode = if view.contains(MULTI_LINE_HINT) {
        "multi"
    } else if view.contains(SINGLE_LINE_HINT) {
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
    // Dump full after view for debugging
    let dump_dir = "/tmp/claude-1000/-data-projects-pi-agent-rust/1fdfcc26-21eb-4e93-9a47-a30d0beec819/scratchpad";
    let _ = std::fs::create_dir_all(dump_dir);
    let slug = step
        .label
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    let _ = std::fs::write(format!("{dump_dir}/fail_after_{slug}.txt"), &step.after);
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

fn press_shift_enter(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(
        harness,
        app,
        "key:Shift+Enter",
        KeyMsg::from_type(KeyType::ShiftEnter),
    )
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

fn press_ctrld(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:CtrlD", KeyMsg::from_type(KeyType::CtrlD))
}

fn press_ctrlt(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:CtrlT", KeyMsg::from_type(KeyType::CtrlT))
}

fn press_ctrlp(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:CtrlP", KeyMsg::from_type(KeyType::CtrlP))
}

fn press_ctrlo(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:CtrlO", KeyMsg::from_type(KeyType::CtrlO))
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
        collapsed: false,
    }
}

fn assistant_msg(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::Assistant,
        content: text.to_string(),
        thinking: None,
        collapsed: false,
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

fn numbered_lines(count: usize) -> String {
    (1..=count)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
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
    assert_after_contains(&harness, &step, MULTI_LINE_HINT);
    let step = press_esc(&harness, &mut app);
    assert_after_contains(&harness, &step, SINGLE_LINE_HINT);
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
fn tui_state_tab_accepts_autocomplete_selection() {
    let harness = TestHarness::new("tui_state_tab_accepts_autocomplete_selection");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    std::fs::create_dir_all(harness.temp_dir().join("src")).expect("mkdir");
    std::fs::write(harness.temp_dir().join("src/main.rs"), "fn main() {}").expect("write");
    std::fs::write(harness.temp_dir().join("src/make.rs"), "pub fn make() {}").expect("write");

    let step = type_text(&harness, &mut app, "src/ma");
    assert_after_contains(&harness, &step, "src/ma");

    let step = press_tab(&harness, &mut app);
    assert_after_contains(&harness, &step, "Enter/Tab accept");

    let step = press_tab(&harness, &mut app);
    assert_after_contains(&harness, &step, "src/main.rs");
    assert_after_not_contains(&harness, &step, "Enter/Tab accept");
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
fn tui_state_shift_enter_inserts_newline_and_enters_multiline_mode() {
    let harness =
        TestHarness::new("tui_state_shift_enter_inserts_newline_and_enters_multiline_mode");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "line1");
    let step = press_shift_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, MULTI_LINE_HINT);
    assert_after_not_contains(&harness, &step, "Processing...");
}

#[test]
fn tui_view_normalizes_newlines_to_crlf_after_multiline_and_resize() {
    let harness =
        TestHarness::new("tui_view_normalizes_newlines_to_crlf_after_multiline_and_resize");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "line1");
    press_shift_enter(&harness, &mut app);
    type_text(&harness, &mut app, "line2");

    let view = BubbleteaModel::view(&app);
    assert_all_newlines_are_crlf(&view);

    app.set_terminal_size(100, 40);
    let view = BubbleteaModel::view(&app);
    assert_all_newlines_are_crlf(&view);
}

#[test]
fn tui_state_alt_enter_enables_multiline_mode() {
    let harness = TestHarness::new("tui_state_alt_enter_enables_multiline_mode");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let step = press_alt_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, MULTI_LINE_HINT);
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
    assert_after_contains(&harness, &step, MULTI_LINE_HINT);
    assert_after_not_contains(&harness, &step, "Processing...");
}

#[test]
fn tui_state_history_navigation_with_no_history_preserves_input() {
    let harness = TestHarness::new("tui_state_history_navigation_with_no_history_preserves_input");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "draft");
    let step = press_up(&harness, &mut app);
    assert_after_contains(&harness, &step, "> draft");

    let step = press_down(&harness, &mut app);
    assert_after_contains(&harness, &step, "> draft");
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
fn tui_state_hide_thinking_block_hides_thinking_until_toggled() {
    let harness = TestHarness::new("tui_state_hide_thinking_block_hides_thinking_until_toggled");
    let config = Config {
        hide_thinking_block: Some(true),
        ..Config::default()
    };
    let mut app =
        build_app_with_session_and_config(&harness, Vec::new(), Session::in_memory(), config);
    log_initial_state(&harness, &app);

    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ThinkingDelta(hidden)",
        PiMsg::ThinkingDelta("hmm".to_string()),
    );
    assert_after_not_contains(&harness, &step, "Thinking:");
    assert_after_not_contains(&harness, &step, "hmm");

    let step = press_ctrlt(&harness, &mut app);
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
fn tui_state_tool_update_with_diff_details_appends_diff_block() {
    let harness = TestHarness::new("tui_state_tool_update_with_diff_details_appends_diff_block");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+diff)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Successfully replaced text in foo.txt.",
            ))],
            details: Some(json!({
                "diff": "+1 added line\n-1 removed line\n 1 context",
            })),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit)",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "Tool edit output:");
    assert_after_contains(&harness, &step, "Successfully replaced text in foo.txt.");
    assert_after_contains(&harness, &step, "@@ foo.txt @@");
    assert_after_contains(&harness, &step, "+1 added line");
    assert_after_contains(&harness, &step, "-1 removed line");
}

#[test]
fn tui_state_tool_update_with_large_diff_shows_truncation_indicator() {
    let harness =
        TestHarness::new("tui_state_tool_update_with_large_diff_shows_truncation_indicator");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let mut diff_lines = Vec::new();
    for i in 0..35 {
        diff_lines.push(format!("- {i} old value {i}"));
        diff_lines.push(format!("+ {i} new value {i}"));
    }
    let diff = diff_lines.join("\n");

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+large-diff)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Successfully replaced text in foo.txt.",
            ))],
            details: Some(json!({ "diff": diff })),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit)",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );
    assert_after_contains(&harness, &step, "collapsed");

    // The large diff auto-collapses; toggle global collapse twice to re-expand.
    press_ctrlo(&harness, &mut app);
    let mut step = press_ctrlo(&harness, &mut app);

    assert_after_contains(&harness, &step, "@@ foo.txt @@");
    if !step.after.contains("diff truncated") {
        for _ in 0..10 {
            let next = press_pgdown(&harness, &mut app);
            if next.after.contains("diff truncated") {
                step = next;
                break;
            }
            if next.after == step.after {
                break;
            }
            step = next;
        }
    }
    assert_after_contains(&harness, &step, "diff truncated");
}

#[test]
fn tui_state_tool_update_with_diff_without_replace_message_uses_generic_header() {
    let harness = TestHarness::new(
        "tui_state_tool_update_with_diff_without_replace_message_uses_generic_header",
    );
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+generic-diff)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("Edit completed."))],
            details: Some(json!({
                "diff": "- 1 old text\n+ 1 new text"
            })),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit)",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "Tool edit output:");
    assert_after_contains(&harness, &step, "Diff:");
    assert_after_contains(&harness, &step, "+ 1 new text");
    assert_after_not_contains(&harness, &step, "@@");
}

#[test]
fn tui_state_tool_update_with_details_and_no_content_renders_pretty_json() {
    let harness =
        TestHarness::new("tui_state_tool_update_with_details_and_no_content_renders_pretty_json");
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
        "PiMsg::ToolUpdate(read+details-only)",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: Vec::new(),
            details: Some(json!({
                "matches": 3,
                "path": "src/main.rs"
            })),
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
    assert_after_contains(&harness, &step, "\"matches\": 3");
    assert_after_contains(&harness, &step, "\"path\": \"src/main.rs\"");
}

#[test]
fn tui_state_tool_output_over_threshold_auto_collapses_with_preview() {
    let harness =
        TestHarness::new("tui_state_tool_output_over_threshold_auto_collapses_with_preview");
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
        "PiMsg::ToolUpdate(read) large-output",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(numbered_lines(30)))],
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
    assert_after_contains(&harness, &step, "collapsed");
    assert_after_contains(&harness, &step, "line 1");
    assert_after_contains(&harness, &step, "line 5");
    assert_after_not_contains(&harness, &step, "line 6");
    assert_after_contains(&harness, &step, "25 more lines");
}

#[test]
fn tui_state_tool_output_at_threshold_stays_expanded() {
    let harness = TestHarness::new("tui_state_tool_output_at_threshold_stays_expanded");
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
        "PiMsg::ToolUpdate(read) threshold-output",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(numbered_lines(19)))],
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

    assert_after_not_contains(&harness, &step, "collapsed");
    assert_after_contains(&harness, &step, "line 19");
}

#[test]
fn tui_state_expand_tools_reexpands_auto_collapsed_blocks() {
    let harness = TestHarness::new("tui_state_expand_tools_reexpands_auto_collapsed_blocks");
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
        "PiMsg::ToolUpdate(read) large-output",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(numbered_lines(30)))],
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
    assert_after_contains(&harness, &step, "collapsed");

    let step = press_ctrlo(&harness, &mut app);
    assert_after_contains(&harness, &step, "Tool read output:");
    assert_after_contains(&harness, &step, "collapsed");
    assert_after_not_contains(&harness, &step, "line 1");

    let step = press_ctrlo(&harness, &mut app);
    assert_after_contains(&harness, &step, "Tool read output:");
    assert_after_not_contains(&harness, &step, "collapsed");
    assert_after_contains(&harness, &step, "line 1");
    if !step.after.contains("line 30") {
        let mut current = step;
        for _ in 0..10 {
            let next = press_pgdown(&harness, &mut app);
            if next.after.contains("line 30") {
                current = next;
                break;
            }
            if next.after == current.after {
                break;
            }
            current = next;
        }
        assert_after_contains(&harness, &current, "line 30");
    }
}

#[test]
fn tui_state_expand_tools_toggles_tool_output_visibility() {
    let harness = TestHarness::new("tui_state_expand_tools_toggles_tool_output_visibility");
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
    assert_after_not_contains(&harness, &step, "collapsed");

    let step = press_ctrlo(&harness, &mut app);
    assert_after_contains(&harness, &step, "Tool read output:");
    assert_after_contains(&harness, &step, "collapsed");
    assert_after_not_contains(&harness, &step, "file contents");

    let step = press_ctrlo(&harness, &mut app);
    assert_after_contains(&harness, &step, "Tool read output:");
    assert_after_contains(&harness, &step, "file contents");
    assert_after_not_contains(&harness, &step, "collapsed");
}

#[test]
fn tui_state_terminal_show_images_false_hides_images_in_tool_output() {
    let harness =
        TestHarness::new("tui_state_terminal_show_images_false_hides_images_in_tool_output");
    let config = Config {
        terminal: Some(TerminalSettings {
            show_images: Some(false),
            clear_on_shrink: None,
        }),
        ..Config::default()
    };
    let mut app =
        build_app_with_session_and_config(&harness, Vec::new(), Session::in_memory(), config);

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
            content: vec![
                ContentBlock::Text(TextContent::new("file contents")),
                ContentBlock::Image(ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                }),
            ],
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
    assert_after_contains(&harness, &step, "1 image(s) hidden");
    assert_after_not_contains(&harness, &step, "[image:");
}

#[test]
fn tui_state_terminal_show_images_true_shows_image_placeholders_in_tool_output() {
    let harness = TestHarness::new(
        "tui_state_terminal_show_images_true_shows_image_placeholders_in_tool_output",
    );
    let config = Config {
        terminal: Some(TerminalSettings {
            show_images: Some(true),
            clear_on_shrink: None,
        }),
        ..Config::default()
    };
    let mut app =
        build_app_with_session_and_config(&harness, Vec::new(), Session::in_memory(), config);

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
            content: vec![
                ContentBlock::Text(TextContent::new("file contents")),
                ContentBlock::Image(ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                }),
            ],
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
    let has_placeholder = step.after.contains("[image: image/png]");
    let has_kitty = step.after.contains("\u{1b}_G");
    let has_iterm2 = step.after.contains("\u{1b}]1337;File=");
    if !(has_placeholder || has_kitty || has_iterm2) {
        fail_step(
            &harness,
            &step,
            "Expected an inline image rendering in view",
        );
    }
    assert_after_not_contains(&harness, &step, "image(s) hidden");
}

#[test]
fn tui_state_terminal_show_images_false_reports_multiple_hidden_images() {
    let harness =
        TestHarness::new("tui_state_terminal_show_images_false_reports_multiple_hidden_images");
    let config = Config {
        terminal: Some(TerminalSettings {
            show_images: Some(false),
            clear_on_shrink: None,
        }),
        ..Config::default()
    };
    let mut app =
        build_app_with_session_and_config(&harness, Vec::new(), Session::in_memory(), config);

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
        "PiMsg::ToolUpdate(read) two-images",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![
                ContentBlock::Text(TextContent::new("file contents")),
                ContentBlock::Image(ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                }),
                ContentBlock::Image(ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/jpeg".to_string(),
                }),
            ],
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
    assert_after_contains(&harness, &step, "2 image(s) hidden");
    assert_after_not_contains(&harness, &step, "[image:");
}

#[test]
fn tui_state_terminal_show_images_false_still_renders_tool_output_when_only_images() {
    let harness = TestHarness::new(
        "tui_state_terminal_show_images_false_still_renders_tool_output_when_only_images",
    );
    let config = Config {
        terminal: Some(TerminalSettings {
            show_images: Some(false),
            clear_on_shrink: None,
        }),
        ..Config::default()
    };
    let mut app =
        build_app_with_session_and_config(&harness, Vec::new(), Session::in_memory(), config);

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
        "PiMsg::ToolUpdate(read) image-only",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Image(ImageContent {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
            })],
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
    assert_after_contains(&harness, &step, "1 image(s) hidden");
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
    assert_after_contains(&harness, &step, SINGLE_LINE_HINT);
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
            diagnostics: None,
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
fn tui_state_slash_scoped_models_set_persists_and_scopes_ctrlp() {
    let harness = TestHarness::new("tui_state_slash_scoped_models_set_persists_and_scopes_ctrlp");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");
    let google = make_model_entry(
        "google",
        "gemini-a",
        "https://generativeai.googleapis.com/v1beta/models",
    );

    let model_scope = Vec::new();
    let available_models = vec![anthropic.clone(), openai, google];

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    type_text(&harness, &mut app, "/scoped-models openai/*");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Scoped models updated: 1 matched");

    let settings = read_project_settings_json(&harness);
    assert_eq!(
        settings
            .get("enabled_models")
            .and_then(|value| value.as_array())
            .map(|array| { array.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>() }),
        Some(vec!["openai/*"])
    );

    let step = press_ctrlp(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched model: openai/gpt-a");
}

#[test]
fn tui_state_slash_scoped_models_clear_persists_and_restores_all_models() {
    let harness =
        TestHarness::new("tui_state_slash_scoped_models_clear_persists_and_restores_all_models");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");
    let google = make_model_entry(
        "google",
        "gemini-a",
        "https://generativeai.googleapis.com/v1beta/models",
    );

    let model_scope = Vec::new();
    let available_models = vec![anthropic.clone(), openai, google];

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    type_text(&harness, &mut app, "/scoped-models openai/*");
    press_enter(&harness, &mut app);

    type_text(&harness, &mut app, "/scoped-models clear");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Scoped models cleared");

    let settings = read_project_settings_json(&harness);
    assert_eq!(
        settings
            .get("enabled_models")
            .and_then(|value| value.as_array())
            .map(std::vec::Vec::len),
        Some(0)
    );

    let step = press_ctrlp(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched model: google/gemini-a");
}

#[test]
fn tui_state_ctrlp_cycles_models_with_scope_and_updates_session_header() {
    let harness =
        TestHarness::new("tui_state_ctrlp_cycles_models_with_scope_and_updates_session_header");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");
    let google = make_model_entry(
        "google",
        "gemini-a",
        "https://generativeai.googleapis.com/v1beta/models",
    );

    // Deliberately scrambled ordering: cycling should use a stable ordering.
    let model_scope = vec![openai.clone(), anthropic.clone()];
    let available_models = vec![google, openai, anthropic.clone()];

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    let step = press_ctrlp(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched model: openai/gpt-a");

    let session_handle = app.session_handle();
    let session_guard = session_handle.try_lock().expect("session lock");
    assert_eq!(session_guard.header.provider.as_deref(), Some("openai"));
    assert_eq!(session_guard.header.model_id.as_deref(), Some("gpt-a"));
    drop(session_guard);

    press_ctrlp(&harness, &mut app);
    let session_guard = session_handle.try_lock().expect("session lock");
    assert_eq!(session_guard.header.provider.as_deref(), Some("anthropic"));
    assert_eq!(session_guard.header.model_id.as_deref(), Some("claude-a"));
}

#[test]
fn tui_state_ctrlp_cycles_models_without_scope_uses_available_models() {
    let harness =
        TestHarness::new("tui_state_ctrlp_cycles_models_without_scope_uses_available_models");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");
    let google = make_model_entry(
        "google",
        "gemini-a",
        "https://generativeai.googleapis.com/v1beta/models",
    );

    let available_models = vec![openai, google, anthropic.clone()];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    press_ctrlp(&harness, &mut app);
    let session_handle = app.session_handle();
    let session_guard = session_handle.try_lock().expect("session lock");
    assert_eq!(session_guard.header.provider.as_deref(), Some("google"));
    assert_eq!(session_guard.header.model_id.as_deref(), Some("gemini-a"));
}

#[test]
fn tui_state_cycle_model_backward_can_be_bound_and_updates_session_header() {
    let harness =
        TestHarness::new("tui_state_cycle_model_backward_can_be_bound_and_updates_session_header");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    let temp = harness.temp_dir().join("keybindings.json");
    std::fs::write(
        &temp,
        r#"{
  "cycleModelBackward": ["ctrl+o"]
}"#,
    )
    .expect("write keybindings");
    let keybindings = KeyBindings::load(&temp).expect("load keybindings");

    let model_entry = openai.clone();
    let model_scope = vec![openai.clone(), anthropic.clone()];
    let available_models = vec![anthropic, openai];

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        model_entry,
        model_scope,
        available_models,
        keybindings,
    );

    press_ctrlo(&harness, &mut app);
    let session_handle = app.session_handle();
    let session_guard = session_handle.try_lock().expect("session lock");
    assert_eq!(session_guard.header.provider.as_deref(), Some("anthropic"));
    assert_eq!(session_guard.header.model_id.as_deref(), Some("claude-a"));
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
fn tui_state_slash_settings_opens_selector_and_restores_editor() {
    let harness = TestHarness::new("tui_state_slash_settings_opens_selector_and_restores_editor");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/settings");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Settings");
    assert_after_contains(&harness, &step, "Summary");
    assert_after_not_contains(&harness, &step, SINGLE_LINE_HINT);

    // Navigate to Theme and open the picker.
    press_down(&harness, &mut app);
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Select Theme");
    assert_after_contains(&harness, &step, "dark (built-in)");
    assert_after_contains(&harness, &step, "light (built-in)");
    assert_after_not_contains(&harness, &step, SINGLE_LINE_HINT);

    // Switch to `light` and ensure it persists to .pi/settings.json.
    press_down(&harness, &mut app);
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched to theme: light");
    assert_after_contains(&harness, &step, SINGLE_LINE_HINT);

    let settings_path = harness.temp_dir().join(".pi/settings.json");
    let content = std::fs::read_to_string(&settings_path).expect("read settings.json");
    let value: serde_json::Value = serde_json::from_str(&content).expect("parse settings.json");
    assert_eq!(value["theme"], "light");

    // Reopen and toggle a delivery mode (should persist to .pi/settings.json).
    type_text(&harness, &mut app, "/settings");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "steeringMode:");
    press_down(&harness, &mut app);
    press_down(&harness, &mut app);
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Updated steeringMode: all");

    let settings_path = harness.temp_dir().join(".pi/settings.json");
    let content = std::fs::read_to_string(&settings_path).expect("read settings.json");
    let value: serde_json::Value = serde_json::from_str(&content).expect("parse settings.json");
    assert_eq!(value["steeringMode"], "all");

    // Reopen and cancel to ensure editor is restored.
    type_text(&harness, &mut app, "/settings");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "↑/↓/j/k: navigate");
    let step = press_esc(&harness, &mut app);
    assert_after_contains(&harness, &step, SINGLE_LINE_HINT);
    assert_after_not_contains(&harness, &step, "↑/↓/j/k: navigate");
}

#[test]
fn tui_state_slash_settings_quiet_startup_persists_and_overrides_global() {
    let harness =
        TestHarness::new("tui_state_slash_settings_quiet_startup_persists_and_overrides_global");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/settings");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Settings");
    assert_after_contains(&harness, &step, "quietStartup:");

    // Navigate to quietStartup entry:
    // Summary(0), Theme(1), SteeringMode(2), FollowUpMode(3), QuietStartup(4)
    press_down(&harness, &mut app);
    press_down(&harness, &mut app);
    press_down(&harness, &mut app);
    press_down(&harness, &mut app);
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Updated quietStartup: on");

    let settings_path = harness.temp_dir().join(".pi/settings.json");
    let content = std::fs::read_to_string(&settings_path).expect("read settings.json");
    let value: serde_json::Value = serde_json::from_str(&content).expect("parse settings.json");
    assert_eq!(value["quiet_startup"], json!(true));

    // Ensure project settings override global on load (legacy keys accepted via serde aliases).
    let global_dir = harness.create_dir("global");
    std::fs::write(
        global_dir.join("settings.json"),
        r#"{ "quietStartup": false }"#,
    )
    .expect("write global settings");
    let loaded = Config::load_with_roots(None, &global_dir, harness.temp_dir()).expect("load");
    assert_eq!(loaded.quiet_startup, Some(true));
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
fn tui_state_slash_share_reports_error_when_gh_missing() {
    let harness = TestHarness::new("tui_state_slash_share_reports_error_when_gh_missing");
    let missing = harness.temp_path("missing-gh");
    let config = Config {
        gh_path: Some(missing.display().to_string()),
        ..Default::default()
    };
    let (mut app, event_rx) = build_app_with_session_and_events_and_config(
        &harness,
        Vec::new(),
        Session::in_memory(),
        config,
    );
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/share");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Sharing session...");

    // Under load, async command execution plus shell startup for fake `gh`
    // can exceed 1s before AgentError is emitted.
    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(3), |msgs| {
        msgs.iter().any(|msg| matches!(msg, PiMsg::AgentError(_)))
    });
    let error = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::AgentError(_)))
        .expect("expected AgentError for missing gh");
    let step = apply_pi(&harness, &mut app, "PiMsg::AgentError", error);
    assert_after_contains(&harness, &step, "GitHub CLI `gh` not found");
}

#[test]
#[cfg(unix)]
fn tui_state_slash_share_reports_error_when_gh_not_authenticated() {
    let harness = TestHarness::new("tui_state_slash_share_reports_error_when_gh_not_authenticated");

    let gh_path = harness.temp_path("gh");
    let script = "#!/bin/sh\nset -e\n\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"status\" ]; then\n  echo \"You are not logged into any GitHub hosts\" >&2\n  exit 1\nfi\n\necho \"unexpected gh args: $@\" >&2\nexit 2\n";
    fs::write(&gh_path, script).expect("write fake gh");
    make_executable(&gh_path);

    let config = Config {
        gh_path: Some(gh_path.display().to_string()),
        ..Default::default()
    };
    let (mut app, event_rx) = build_app_with_session_and_events_and_config(
        &harness,
        Vec::new(),
        Session::in_memory(),
        config,
    );
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/share");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Sharing session...");

    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(1), |msgs| {
        msgs.iter().any(|msg| matches!(msg, PiMsg::AgentError(_)))
    });
    let error = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::AgentError(_)))
        .expect("expected AgentError for unauthenticated gh");
    let step = apply_pi(&harness, &mut app, "PiMsg::AgentError", error);
    assert_after_contains(&harness, &step, "`gh` is not authenticated.");
    assert_after_contains(&harness, &step, "Run `gh auth login` and retry.");
}

#[test]
#[cfg(unix)]
fn tui_state_slash_share_reports_parse_error_and_cleans_temp_file() {
    let harness =
        TestHarness::new("tui_state_slash_share_reports_parse_error_and_cleans_temp_file");

    let record_path = harness.temp_path("gh_record_path.txt");
    let gh_path = harness.temp_path("gh");
    let script = format!(
        "#!/bin/sh\nset -e\n\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"status\" ]; then\n  exit 0\nfi\n\nif [ \"$1\" = \"gist\" ] && [ \"$2\" = \"create\" ]; then\n  file=\"\"\n  for arg in \"$@\"; do\n    file=\"$arg\"\n  done\n  printf '%s' \"$file\" > \"{record_path}\"\n  echo \"created gist but no url\"\n  exit 0\nfi\n\necho \"unexpected gh args: $@\" >&2\nexit 2\n",
        record_path = record_path.display(),
    );
    fs::write(&gh_path, script).expect("write fake gh");
    make_executable(&gh_path);

    let config = Config {
        gh_path: Some(gh_path.display().to_string()),
        ..Default::default()
    };
    let (mut app, event_rx) = build_app_with_session_and_events_and_config(
        &harness,
        Vec::new(),
        Session::in_memory(),
        config,
    );
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/share");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Sharing session...");

    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(1), |msgs| {
        msgs.iter().any(|msg| matches!(msg, PiMsg::AgentError(_)))
    });
    let error = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::AgentError(_)))
        .expect("expected AgentError for gist parse failure");
    let step = apply_pi(&harness, &mut app, "PiMsg::AgentError", error);
    assert_after_contains(
        &harness,
        &step,
        "Failed to parse gist URL from `gh gist create` output.",
    );

    let recorded = fs::read_to_string(&record_path).expect("read record path");
    let shared_path = std::path::Path::new(recorded.trim());
    let start = Instant::now();
    while shared_path.exists() && start.elapsed() < Duration::from_millis(500) {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !shared_path.exists(),
        "expected temp HTML file to be cleaned up on parse failure (still exists at {})",
        shared_path.display()
    );
}

#[test]
#[cfg(unix)]
fn tui_state_slash_share_creates_gist_and_reports_urls_and_cleans_temp_file() {
    let harness = TestHarness::new(
        "tui_state_slash_share_creates_gist_and_reports_urls_and_cleans_temp_file",
    );

    let record_path = harness.temp_path("gh_record_path.txt");
    let gh_path = harness.temp_path("gh");
    let script = format!(
        "#!/bin/sh\nset -e\n\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"status\" ]; then\n  exit 0\nfi\n\nif [ \"$1\" = \"gist\" ] && [ \"$2\" = \"create\" ]; then\n  file=\"\"\n  for arg in \"$@\"; do\n    file=\"$arg\"\n  done\n  printf '%s' \"$file\" > \"{record_path}\"\n  echo \"https://gist.github.com/testuser/abcdef1234567890\"\n  exit 0\nfi\n\necho \"unexpected gh args: $@\" >&2\nexit 2\n",
        record_path = record_path.display(),
    );
    fs::write(&gh_path, script).expect("write fake gh");
    make_executable(&gh_path);

    let config = Config {
        gh_path: Some(gh_path.display().to_string()),
        ..Default::default()
    };
    let (mut app, event_rx) = build_app_with_session_and_events_and_config(
        &harness,
        Vec::new(),
        Session::in_memory(),
        config,
    );
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/share");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Sharing session...");

    // Full-suite parallel load can delay command completion past 1s.
    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(3), |msgs| {
        msgs.iter()
            .any(|msg| matches!(msg, PiMsg::System(_)) || matches!(msg, PiMsg::AgentError(_)))
    });
    let msg = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::System(_)) || matches!(msg, PiMsg::AgentError(_)))
        .expect("expected share result");
    let step = apply_pi(&harness, &mut app, "PiMsg share result", msg);
    assert_after_contains(&harness, &step, "Share URL:");
    assert_after_contains(
        &harness,
        &step,
        "https://buildwithpi.ai/session/#abcdef1234567890",
    );
    assert_after_contains(&harness, &step, "Gist:");
    assert_after_contains(
        &harness,
        &step,
        "https://gist.github.com/testuser/abcdef1234567890",
    );

    let recorded = fs::read_to_string(&record_path).expect("read record path");
    let shared_path = std::path::Path::new(recorded.trim());
    let start = Instant::now();
    while shared_path.exists() && start.elapsed() < Duration::from_millis(500) {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !shared_path.exists(),
        "expected temp HTML file to be cleaned up (still exists at {})",
        shared_path.display()
    );
}

#[test]
#[cfg(unix)]
fn tui_state_slash_share_is_cancellable_and_cleans_temp_file() {
    let harness = TestHarness::new("tui_state_slash_share_is_cancellable_and_cleans_temp_file");

    let record_path = harness.temp_path("gh_record_path.txt");
    let gh_path = harness.temp_path("gh");
    let script = format!(
        "#!/bin/sh\nset -e\n\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"status\" ]; then\n  exit 0\nfi\n\nif [ \"$1\" = \"gist\" ] && [ \"$2\" = \"create\" ]; then\n  file=\"\"\n  for arg in \"$@\"; do\n    file=\"$arg\"\n  done\n  printf '%s' \"$file\" > \"{record_path}\"\n  sleep 1\n  echo \"https://gist.github.com/testuser/abcdef1234567890\"\n  exit 0\nfi\n\necho \"unexpected gh args: $@\" >&2\nexit 2\n",
        record_path = record_path.display(),
    );
    fs::write(&gh_path, script).expect("write fake gh");
    make_executable(&gh_path);

    let config = Config {
        gh_path: Some(gh_path.display().to_string()),
        ..Default::default()
    };
    let (mut app, event_rx) = build_app_with_session_and_events_and_config(
        &harness,
        Vec::new(),
        Session::in_memory(),
        config,
    );
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/share");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Sharing session...");

    let start = Instant::now();
    while !record_path.exists() && start.elapsed() < Duration::from_millis(500) {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(record_path.exists(), "expected fake gh to record temp path");

    let step = press_esc(&harness, &mut app);
    assert_after_contains(&harness, &step, "Aborting request...");

    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(1), |msgs| {
        msgs.iter()
            .any(|msg| matches!(msg, PiMsg::System(message) if message.contains("Share cancelled")))
    });
    let msg = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::System(message) if message.contains("Share cancelled")))
        .expect("expected Share cancelled message");
    let step = apply_pi(&harness, &mut app, "PiMsg::System", msg);
    assert_after_contains(&harness, &step, "Share cancelled");

    let recorded = fs::read_to_string(&record_path).expect("read record path");
    let shared_path = std::path::Path::new(recorded.trim());
    let start = Instant::now();
    while shared_path.exists() && start.elapsed() < Duration::from_millis(500) {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !shared_path.exists(),
        "expected temp HTML file to be cleaned up (still exists at {})",
        shared_path.display()
    );
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
fn tui_state_slash_resume_selects_latest_session_and_loads_messages() {
    let harness =
        TestHarness::new("tui_state_slash_resume_selects_latest_session_and_loads_messages");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_dir().to_path_buf();

    create_session_on_disk(&base_dir, &cwd, "older", "Older session message");
    thread::sleep(Duration::from_millis(10));
    create_session_on_disk(&base_dir, &cwd, "newer", "Newer session message");

    let mut session = Session::create_with_dir(Some(base_dir));
    session.header.cwd = cwd.display().to_string();
    let (mut app, event_rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/resume");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Select a session to resume");
    assert_after_contains(&harness, &step, "newer");
    assert_after_contains(&harness, &step, "older");

    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Loading session...");

    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(2), |msgs| {
        msgs.iter()
            .any(|msg| matches!(msg, PiMsg::ConversationReset { .. }))
    });
    let reset = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::ConversationReset { .. }))
        .expect("expected ConversationReset after resume");
    let step = apply_pi(&harness, &mut app, "PiMsg::ConversationReset", reset);
    assert_after_contains(&harness, &step, "Session resumed");
    assert_after_contains(&harness, &step, "Newer session message");
}

#[test]
fn tui_state_slash_resume_handles_non_ascii_session_id() {
    let harness = TestHarness::new("tui_state_slash_resume_handles_non_ascii_session_id");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_dir().to_path_buf();

    create_session_on_disk_with_id(&base_dir, &cwd, "unicode", "αβγδεζηθι", "Unicode session");

    let mut session = Session::create_with_dir(Some(base_dir));
    session.header.cwd = cwd.display().to_string();
    let (mut app, _event_rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/resume");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Select a session to resume");
    assert_after_contains(&harness, &step, "αβγδεζηθ");
}

#[test]
fn tui_state_slash_resume_can_be_cancelled_by_extension() {
    let harness = TestHarness::new("tui_state_slash_resume_can_be_cancelled_by_extension");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_dir().to_path_buf();

    create_session_on_disk(&base_dir, &cwd, "older", "Older session message");
    thread::sleep(Duration::from_millis(10));
    create_session_on_disk(&base_dir, &cwd, "newer", "Newer session message");

    let mut session = Session::create_with_dir(Some(base_dir));
    session.header.cwd = cwd.display().to_string();

    let extension_source = r#"
export default function init(pi) {
  pi.on("session_before_switch", async () => false);
}
"#;
    let (mut app, event_rx) = build_app_with_session_and_events_and_extension(
        &harness,
        Vec::new(),
        session,
        Config::default(),
        extension_source,
    );
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/resume");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Select a session to resume");

    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Loading session...");

    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(1), |msgs| {
        msgs.iter().any(|msg| matches!(msg, PiMsg::System(_)))
    });
    let system = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::System(_)))
        .expect("expected System message after cancelled resume");
    let step = apply_pi(&harness, &mut app, "PiMsg::System", system);
    assert_after_contains(&harness, &step, "Session switch cancelled by extension");
    assert_after_not_contains(&harness, &step, "Session resumed");
    assert_after_not_contains(&harness, &step, "Newer session message");
}

#[test]
fn tui_state_slash_resume_fail_open_when_extension_errors() {
    let harness = TestHarness::new("tui_state_slash_resume_fail_open_when_extension_errors");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_dir().to_path_buf();

    create_session_on_disk(&base_dir, &cwd, "older", "Older session message");
    thread::sleep(Duration::from_millis(10));
    create_session_on_disk(&base_dir, &cwd, "newer", "Newer session message");

    let mut session = Session::create_with_dir(Some(base_dir));
    session.header.cwd = cwd.display().to_string();

    let extension_source = r#"
export default function init(pi) {
  pi.on("session_before_switch", async () => { throw new Error("boom"); });
}
"#;
    let (mut app, event_rx) = build_app_with_session_and_events_and_extension(
        &harness,
        Vec::new(),
        session,
        Config::default(),
        extension_source,
    );
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/resume");
    press_enter(&harness, &mut app);
    press_enter(&harness, &mut app);

    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(1), |msgs| {
        msgs.iter()
            .any(|msg| matches!(msg, PiMsg::ConversationReset { .. }))
    });
    let reset = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::ConversationReset { .. }))
        .expect("expected ConversationReset after resume");
    let step = apply_pi(&harness, &mut app, "PiMsg::ConversationReset", reset);
    assert_after_contains(&harness, &step, "Session resumed");
    assert_after_contains(&harness, &step, "Newer session message");
}

#[test]
fn tui_state_session_picker_ctrl_d_prompts_for_delete() {
    let harness = TestHarness::new("tui_state_session_picker_ctrl_d_prompts_for_delete");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_dir().to_path_buf();

    create_session_on_disk(&base_dir, &cwd, "session-a", "Message A");
    let mut session = Session::create_with_dir(Some(base_dir));
    session.header.cwd = cwd.display().to_string();
    let (mut app, _event_rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/resume");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Select a session to resume");

    let step = press_ctrld(&harness, &mut app);
    assert_after_contains(&harness, &step, "Delete session? Press y/n to confirm.");
    assert_after_contains(&harness, &step, "Select a session to resume");

    let step = apply_key(&harness, &mut app, "key:n", KeyMsg::from_runes(vec!['n']));
    assert_after_contains(&harness, &step, "Select a session to resume");
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
        && !step
            .after
            .contains("Clipboard support not available in this build")
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
fn tui_state_slash_new_can_be_cancelled_by_extension() {
    let harness = TestHarness::new("tui_state_slash_new_can_be_cancelled_by_extension");
    let session = Session::in_memory();

    let extension_source = r#"
export default function init(pi) {
  pi.on("session_before_switch", async () => false);
}
"#;
    let (mut app, event_rx) = build_app_with_session_and_events_and_extension(
        &harness,
        Vec::new(),
        session,
        Config::default(),
        extension_source,
    );
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ConversationReset",
        PiMsg::ConversationReset {
            messages: vec![user_msg("hello"), assistant_msg("world")],
            usage: sample_usage(12, 34),
            status: None,
        },
    );

    type_text(&harness, &mut app, "/new");
    press_enter(&harness, &mut app);

    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(1), |msgs| {
        msgs.iter().any(|msg| matches!(msg, PiMsg::System(_)))
    });
    let system = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::System(_)))
        .expect("expected System message after cancelled new");
    let step = apply_pi(&harness, &mut app, "PiMsg::System", system);
    assert_after_contains(&harness, &step, "Session switch cancelled by extension");
    assert_after_contains(&harness, &step, "You: hello");
    assert_after_contains(&harness, &step, "world");
    assert_after_not_contains(&harness, &step, "Started new session");
}

#[test]
fn tui_state_slash_new_fail_open_when_extension_errors() {
    let harness = TestHarness::new("tui_state_slash_new_fail_open_when_extension_errors");
    let session = Session::in_memory();

    let extension_source = r#"
export default function init(pi) {
  pi.on("session_before_switch", async () => { throw new Error("boom"); });
}
"#;
    let (mut app, event_rx) = build_app_with_session_and_events_and_extension(
        &harness,
        Vec::new(),
        session,
        Config::default(),
        extension_source,
    );
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ConversationReset",
        PiMsg::ConversationReset {
            messages: vec![user_msg("hello"), assistant_msg("world")],
            usage: sample_usage(12, 34),
            status: None,
        },
    );

    type_text(&harness, &mut app, "/new");
    press_enter(&harness, &mut app);

    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(1), |msgs| {
        msgs.iter()
            .any(|msg| matches!(msg, PiMsg::ConversationReset { .. }))
    });
    let reset = events
        .into_iter()
        .find(|msg| matches!(msg, PiMsg::ConversationReset { .. }))
        .expect("expected ConversationReset after new session");
    let step = apply_pi(&harness, &mut app, "PiMsg::ConversationReset", reset);
    assert_after_contains(&harness, &step, "Started new session");
    assert_after_not_contains(&harness, &step, "You: hello");
    assert_after_not_contains(&harness, &step, "world");
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
fn tui_state_slash_fork_creates_session_and_prefills_editor() {
    let harness = TestHarness::new("tui_state_slash_fork_creates_session_and_prefills_editor");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_dir().to_path_buf();

    let mut session = Session::create_with_dir(Some(base_dir.clone()));
    session.header.cwd = cwd.display().to_string();
    session.append_message(SessionMessage::User {
        content: UserContent::Text("Root message".to_string()),
        timestamp: Some(0),
    });
    session.append_message(SessionMessage::User {
        content: UserContent::Text("Child message".to_string()),
        timestamp: Some(0),
    });

    let (mut app, event_rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/fork");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Forking session...");

    // /fork first runs a cancellable extension hook (timeout is 5s), so a
    // sub-second wait is flaky and can miss ConversationReset on busy hosts.
    let events = wait_for_pi_msgs(&event_rx, Duration::from_secs(6), |msgs| {
        let has_reset = msgs
            .iter()
            .any(|msg| matches!(msg, PiMsg::ConversationReset { .. }));
        let has_editor = msgs
            .iter()
            .any(|msg| matches!(msg, PiMsg::SetEditorText(_)));
        has_reset && has_editor
    });

    let mut reset_msg = None;
    let mut editor_msg = None;
    let mut fork_err = None;
    for msg in events {
        match msg {
            PiMsg::ConversationReset { .. } => reset_msg = Some(msg),
            PiMsg::SetEditorText(_) => editor_msg = Some(msg),
            PiMsg::AgentError(err) => {
                fork_err = Some(err);
            }
            _ => {}
        }
    }
    assert!(fork_err.is_none(), "Unexpected fork error: {fork_err:?}");

    let reset = reset_msg.expect("expected ConversationReset after fork");
    let step = apply_pi(&harness, &mut app, "PiMsg::ConversationReset", reset);
    assert_after_contains(&harness, &step, "Forked new session from Child message");

    let editor = editor_msg.expect("expected SetEditorText after fork");
    let step = apply_pi(&harness, &mut app, "PiMsg::SetEditorText", editor);
    assert_after_contains(&harness, &step, "Child message");

    let repo_cwd = std::env::current_dir().expect("cwd");
    let fork_dir = base_dir.join(encode_cwd(&repo_cwd));
    let mut has_jsonl = false;
    if let Ok(entries) = std::fs::read_dir(&fork_dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
                has_jsonl = true;
                break;
            }
        }
    }
    assert!(has_jsonl, "expected fork to create a session file");
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
    assert_after_contains(&harness, &step, "[unknown] select: Pick one");
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

#[test]
fn tui_state_tool_update_with_progress_shows_elapsed_and_lines() {
    let harness = TestHarness::new("tui_state_tool_update_with_progress_shows_elapsed_and_lines");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Start a tool.
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );

    // Send a ToolUpdate with progress metrics (elapsed >= 1s triggers display).
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) with progress",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("some output"))],
            details: Some(json!({
                "progress": {
                    "elapsedMs": 5000,
                    "lineCount": 42,
                    "byteCount": 1024
                }
            })),
        },
    );

    // The spinner should show "Running bash" with progress "(5s * 42 lines)".
    assert_after_contains(&harness, &step, "Running bash");
    assert_after_contains(&harness, &step, "5s");
    assert_after_contains(&harness, &step, "42 lines");
}

#[test]
fn tui_state_tool_progress_hidden_under_one_second() {
    let harness = TestHarness::new("tui_state_tool_progress_hidden_under_one_second");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );

    // Send progress with < 1s elapsed — should NOT show the elapsed time.
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) sub-second",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("quick"))],
            details: Some(json!({
                "progress": {
                    "elapsedMs": 500,
                    "lineCount": 3,
                    "byteCount": 100
                }
            })),
        },
    );

    assert_after_contains(&harness, &step, "Running bash");
    // Sub-second progress should not appear.
    assert_after_not_contains(&harness, &step, "0s");
    assert_after_not_contains(&harness, &step, "3 lines");
}

#[test]
fn tui_state_tool_update_without_progress_keeps_spinner_without_metrics() {
    let harness =
        TestHarness::new("tui_state_tool_update_without_progress_keeps_spinner_without_metrics");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );

    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) no-progress",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("still running"))],
            details: Some(json!({
                "note": "no progress payload here"
            })),
        },
    );

    assert_after_contains(&harness, &step, "Running bash");
    assert_after_not_contains(&harness, &step, "lines");
    assert_after_not_contains(&harness, &step, "timeout");
}

#[test]
fn tui_state_tool_progress_reset_on_new_tool_start() {
    let harness = TestHarness::new("tui_state_tool_progress_reset_on_new_tool_start");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // First tool with progress.
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) progress",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("out"))],
            details: Some(json!({
                "progress": {
                    "elapsedMs": 10000,
                    "lineCount": 999,
                    "byteCount": 50000
                }
            })),
        },
    );

    // End first tool, start second with no progress yet.
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(bash)",
        PiMsg::ToolEnd {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(read)",
        PiMsg::ToolStart {
            name: "read".to_string(),
            tool_id: "tool-2".to_string(),
        },
    );

    // The old progress from bash should NOT leak into the read tool spinner.
    assert_after_contains(&harness, &step, "Running read");
    assert_after_not_contains(&harness, &step, "999 lines");
    assert_after_not_contains(&harness, &step, "10s");
}

#[test]
fn tui_state_tool_update_with_progress_shows_bytes_when_lines_missing() {
    let harness =
        TestHarness::new("tui_state_tool_update_with_progress_shows_bytes_when_lines_missing");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );

    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) byte-only progress",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("byte-only progress"))],
            details: Some(json!({
                "progress": {
                    "elapsedMs": 5000,
                    "byteCount": 4096
                }
            })),
        },
    );

    assert_after_contains(&harness, &step, "Running bash");
    assert_after_contains(&harness, &step, "5s");
    assert_after_contains(&harness, &step, "bytes");
    assert_after_not_contains(&harness, &step, "lines");
}

#[test]
fn tui_state_tool_update_with_progress_shows_timeout_suffix() {
    let harness = TestHarness::new("tui_state_tool_update_with_progress_shows_timeout_suffix");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );

    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) timeout progress",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("timeout progress"))],
            details: Some(json!({
                "progress": {
                    "elapsedMs": 5000,
                    "lineCount": 12,
                    "timeoutMs": 120_000
                }
            })),
        },
    );

    assert_after_contains(&harness, &step, "Running bash");
    assert_after_contains(&harness, &step, "5s");
    assert_after_contains(&harness, &step, "12 lines");
    assert_after_contains(&harness, &step, "timeout 120s");
}

// ============================================================================
// Capability prompt overlay tests
// ============================================================================

#[test]
fn tui_state_capability_prompt_shows_overlay() {
    let harness = TestHarness::new("tui_state_capability_prompt_shows_overlay");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "cap-1",
        "confirm",
        json!({
            "title": "Allow extension capability: exec",
            "message": "Extension my-ext requests capability 'exec'. Allow?",
            "extension_id": "my-ext",
            "capability": "exec",
        }),
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(capability)",
        PiMsg::ExtensionUiRequest(request),
    );

    // Modal should render with key elements.
    assert_after_contains(&harness, &step, "Extension Permission Request");
    assert_after_contains(&harness, &step, "my-ext");
    assert_after_contains(&harness, &step, "exec");
    assert_after_contains(&harness, &step, "Allow Once");
    assert_after_contains(&harness, &step, "Deny");
}

#[test]
fn tui_state_capability_prompt_navigate_buttons() {
    let harness = TestHarness::new("tui_state_capability_prompt_navigate_buttons");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "cap-2",
        "confirm",
        json!({
            "extension_id": "test-ext",
            "capability": "http",
            "message": "test prompt",
        }),
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(capability)",
        PiMsg::ExtensionUiRequest(request),
    );

    // Default focus on first button (Allow Once).  Press Right to move to Allow Always.
    let step = apply_key(
        &harness,
        &mut app,
        "key:Right",
        KeyMsg::from_type(KeyType::Right),
    );
    assert_after_contains(&harness, &step, "Allow Always");

    // Press Right again to move to Deny.
    let step = apply_key(
        &harness,
        &mut app,
        "key:Right",
        KeyMsg::from_type(KeyType::Right),
    );
    assert_after_contains(&harness, &step, "Deny");
}

#[test]
fn tui_state_capability_prompt_escape_denies() {
    let harness = TestHarness::new("tui_state_capability_prompt_escape_denies");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "cap-3",
        "confirm",
        json!({
            "extension_id": "test-ext",
            "capability": "exec",
            "message": "test",
        }),
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(capability)",
        PiMsg::ExtensionUiRequest(request),
    );

    // Press Escape to deny.
    let step = apply_key(
        &harness,
        &mut app,
        "key:Esc",
        KeyMsg::from_type(KeyType::Esc),
    );

    // Overlay should be dismissed (no more "Extension Permission Request").
    assert_after_not_contains(&harness, &step, "Extension Permission Request");
}

#[test]
fn tui_state_capability_prompt_enter_confirms() {
    let harness = TestHarness::new("tui_state_capability_prompt_enter_confirms");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "cap-4",
        "confirm",
        json!({
            "extension_id": "test-ext",
            "capability": "exec",
            "message": "test",
        }),
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(capability)",
        PiMsg::ExtensionUiRequest(request),
    );

    // Press Enter to confirm the default (Allow Once).
    let step = apply_key(
        &harness,
        &mut app,
        "key:Enter",
        KeyMsg::from_type(KeyType::Enter),
    );

    // Overlay should be dismissed.
    assert_after_not_contains(&harness, &step, "Extension Permission Request");
}

#[test]
fn tui_state_generic_confirm_not_intercepted_as_capability() {
    let harness = TestHarness::new("tui_state_generic_confirm_not_intercepted_as_capability");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // A generic confirm (no extension_id/capability fields) should NOT trigger the overlay.
    let request = ExtensionUiRequest::new(
        "gen-1",
        "confirm",
        json!({
            "title": "Are you sure?",
            "message": "Please confirm.",
        }),
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(generic confirm)",
        PiMsg::ExtensionUiRequest(request),
    );

    // Should NOT show the capability overlay — should fall through to the text-based flow.
    assert_after_not_contains(&harness, &step, "Extension Permission Request");
}

#[test]
fn tui_state_capability_prompt_blocks_regular_input() {
    let harness = TestHarness::new("tui_state_capability_prompt_blocks_regular_input");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Show capability prompt.
    let request = ExtensionUiRequest::new(
        "cap-block",
        "confirm",
        json!({
            "extension_id": "test-ext",
            "capability": "exec",
            "message": "test",
        }),
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(capability)",
        PiMsg::ExtensionUiRequest(request),
    );

    // Try typing regular text — should NOT appear in input area because modal is active.
    let step = apply_key(&harness, &mut app, "key:a", KeyMsg::from_runes(vec!['a']));
    // The prompt should still be visible, and normal text input should not be processed.
    assert_after_contains(&harness, &step, "Extension Permission Request");
    // Input area should NOT be shown while prompt is open.
    assert_after_not_contains(&harness, &step, "> ");
}

#[test]
fn tui_state_capability_prompt_tab_cycles_buttons() {
    let harness = TestHarness::new("tui_state_capability_prompt_tab_cycles_buttons");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "cap-tab",
        "confirm",
        json!({
            "extension_id": "test-ext",
            "capability": "http",
            "message": "test",
        }),
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(capability)",
        PiMsg::ExtensionUiRequest(request),
    );

    // Tab cycles forward through buttons.
    apply_key(
        &harness,
        &mut app,
        "key:Tab",
        KeyMsg::from_type(KeyType::Tab),
    );
    apply_key(
        &harness,
        &mut app,
        "key:Tab",
        KeyMsg::from_type(KeyType::Tab),
    );
    apply_key(
        &harness,
        &mut app,
        "key:Tab",
        KeyMsg::from_type(KeyType::Tab),
    );

    // After 3 tabs we should be on "Deny Always" (index 3).
    // Press Enter to confirm.
    let step = apply_key(
        &harness,
        &mut app,
        "key:Enter",
        KeyMsg::from_type(KeyType::Enter),
    );

    // Overlay should be dismissed.
    assert_after_not_contains(&harness, &step, "Extension Permission Request");
}

#[test]
fn tui_state_capability_prompt_shows_auto_deny_timer() {
    let harness = TestHarness::new("tui_state_capability_prompt_shows_auto_deny_timer");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "cap-timer",
        "confirm",
        json!({
            "extension_id": "timer-ext",
            "capability": "fs",
            "message": "File system access",
        }),
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(capability)",
        PiMsg::ExtensionUiRequest(request),
    );

    // Auto-deny timer should be visible (default 30s).
    assert_after_contains(&harness, &step, "Auto-deny in 30s");
}

#[test]
fn tui_state_capability_prompt_shows_description() {
    let harness = TestHarness::new("tui_state_capability_prompt_shows_description");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let request = ExtensionUiRequest::new(
        "cap-desc",
        "confirm",
        json!({
            "extension_id": "fancy-ext",
            "capability": "env",
            "message": "Access environment variables HOME, PATH",
        }),
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ExtensionUiRequest(capability)",
        PiMsg::ExtensionUiRequest(request),
    );

    assert_after_contains(&harness, &step, "fancy-ext");
    assert_after_contains(&harness, &step, "env");
    assert_after_contains(&harness, &step, "Access environment variables HOME, PATH");
}

// --- Branch Picker Tests ---
// These tests call branch picker methods directly to avoid keybinding
// conflicts (Ctrl+B is also CursorLeft in Emacs mode, etc.).

#[test]
fn tui_grad_branch_picker_no_branches_shows_message() {
    let harness = TestHarness::new("tui_grad_branch_picker_no_branches_shows_message");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Directly open branch picker.
    app.open_branch_picker();

    // Single-branch session should show status message.
    assert_eq!(
        app.status_message(),
        Some("No branches to pick (use /fork to create one)")
    );
    assert!(!app.has_branch_picker());
}

#[test]
fn tui_grad_branch_picker_blocked_during_processing() {
    let harness = TestHarness::new("tui_grad_branch_picker_blocked_during_processing");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Start a tool (puts agent in ToolRunning state).
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );

    // Try to open branch picker while tool is running.
    app.open_branch_picker();

    assert_eq!(
        app.status_message(),
        Some("Cannot switch branches while processing")
    );
    assert!(!app.has_branch_picker());
}

#[test]
fn tui_grad_branch_indicator_hidden_for_single_branch() {
    let harness = TestHarness::new("tui_grad_branch_indicator_hidden_for_single_branch");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    let step = apply_key(
        &harness,
        &mut app,
        "key:noop",
        KeyMsg::from_runes(vec![' ']),
    );

    // No branch indicator should appear for a single-branch session.
    assert_after_not_contains(&harness, &step, "[branch");
}

#[test]
fn tui_grad_cycle_sibling_no_branches_shows_message() {
    let harness = TestHarness::new("tui_grad_cycle_sibling_no_branches_shows_message");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Directly cycle sibling branch.
    app.cycle_sibling_branch(true);

    assert_eq!(
        app.status_message(),
        Some("No sibling branches (use /fork to create one)")
    );
}

#[test]
fn tui_grad_cycle_sibling_blocked_during_processing() {
    let harness = TestHarness::new("tui_grad_cycle_sibling_blocked_during_processing");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Start a tool.
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );

    // Try to cycle sibling while processing.
    app.cycle_sibling_branch(false);

    assert_eq!(
        app.status_message(),
        Some("Cannot switch branches while processing")
    );
}

/// Helper: creates a session with two branches forking from the root message.
/// Returns (`session`, `root_id`, `branch1_leaf_id`, `branch2_leaf_id`).
fn create_two_branch_session() -> (Session, String, String, String) {
    let mut session = Session::in_memory();
    // Root message
    let root_id = session.append_message(SessionMessage::User {
        content: UserContent::Text("Root question".to_string()),
        timestamp: Some(0),
    });
    // Branch 1: assistant reply + user follow-up
    let _asst1_id = session.append_message(SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("Answer A"))],
            api: "anthropic".to_string(),
            provider: "dummy".to_string(),
            model: "dummy-model".to_string(),
            usage: Usage {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                total_tokens: 15,
                cost: Cost::default(),
            },
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 1,
        },
    });
    let branch1_leaf = session.append_message(SessionMessage::User {
        content: UserContent::Text("Follow-up on branch 1".to_string()),
        timestamp: Some(2),
    });

    // Navigate back to root to create fork point
    session.navigate_to(&root_id);

    // Branch 2: different assistant reply + user follow-up
    let _asst2_id = session.append_message(SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("Answer B"))],
            api: "anthropic".to_string(),
            provider: "dummy".to_string(),
            model: "dummy-model".to_string(),
            usage: Usage {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                total_tokens: 15,
                cost: Cost::default(),
            },
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 3,
        },
    });
    let branch2_leaf = session.append_message(SessionMessage::User {
        content: UserContent::Text("Follow-up on branch 2".to_string()),
        timestamp: Some(4),
    });

    (session, root_id, branch1_leaf, branch2_leaf)
}

/// Helper: creates a session with many sibling branches that all fork from root.
fn create_many_branch_session(branch_count: usize) -> Session {
    assert!(branch_count >= 2, "branch_count must be at least 2");
    let mut session = Session::in_memory();
    let root_id = session.append_message(SessionMessage::User {
        content: UserContent::Text("Root question".to_string()),
        timestamp: Some(0),
    });

    for i in 0..branch_count {
        let i_i64 = i64::try_from(i).expect("branch index fits in i64");
        session.navigate_to(&root_id);
        let _assistant_id = session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "Answer branch {i}"
                )))],
                api: "anthropic".to_string(),
                provider: "dummy".to_string(),
                model: "dummy-model".to_string(),
                usage: Usage {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                    total_tokens: 15,
                    cost: Cost::default(),
                },
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: i_i64 + 1,
            },
        });
        let _leaf_id = session.append_message(SessionMessage::User {
            content: UserContent::Text(format!("Follow-up on branch {i}")),
            timestamp: Some(i_i64 + 10),
        });
    }

    session
}

#[test]
fn tui_grad_branch_indicator_shows_for_multi_branch_session() {
    let harness = TestHarness::new("tui_grad_branch_indicator_shows_for_multi_branch_session");
    let (session, _, _, _) = create_two_branch_session();
    let (app, _rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    // Render view and check for branch indicator in header.
    let view = normalize_view(&BubbleteaModel::view(&app));
    assert!(
        view.to_lowercase().contains("branch"),
        "Expected branch indicator in header for multi-branch session, got:\n{view}"
    );
}

#[test]
fn tui_grad_branch_picker_opens_with_branches() {
    let harness = TestHarness::new("tui_grad_branch_picker_opens_with_branches");
    let (session, _, _, _) = create_two_branch_session();
    let (mut app, _rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    // Open branch picker directly.
    app.open_branch_picker();

    // Picker should be open (not a "No branches" message).
    assert!(app.has_branch_picker(), "Branch picker should be open");
    assert!(
        app.status_message().is_none(),
        "No error status expected when picker opens"
    );
}

#[test]
fn tui_grad_branch_picker_escape_closes() {
    let harness = TestHarness::new("tui_grad_branch_picker_escape_closes");
    let (session, _, _, _) = create_two_branch_session();
    let (mut app, _rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    // Open picker.
    app.open_branch_picker();
    assert!(app.has_branch_picker());

    // Close with Escape via handle_branch_picker_key.
    app.handle_branch_picker_key(&KeyMsg::from_type(KeyType::Esc));

    assert!(
        !app.has_branch_picker(),
        "Branch picker should be closed after Escape"
    );
}

#[test]
fn tui_grad_branch_picker_navigation_up_down() {
    let harness = TestHarness::new("tui_grad_branch_picker_navigation_up_down");
    let (session, _, _, _) = create_two_branch_session();
    let (mut app, _rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    // Open picker.
    app.open_branch_picker();
    assert!(app.has_branch_picker());

    // Navigate down - picker should still be open.
    app.handle_branch_picker_key(&KeyMsg::from_type(KeyType::Down));
    assert!(
        app.has_branch_picker(),
        "Picker should remain open after Down"
    );

    // Navigate up - picker should still be open.
    app.handle_branch_picker_key(&KeyMsg::from_type(KeyType::Up));
    assert!(
        app.has_branch_picker(),
        "Picker should remain open after Up"
    );
}

#[test]
fn tui_grad_branch_picker_enter_switches_branch() {
    let harness = TestHarness::new("tui_grad_branch_picker_enter_switches_branch");
    let (session, _, _, _) = create_two_branch_session();
    let (mut app, _rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    // Open picker.
    app.open_branch_picker();
    assert!(app.has_branch_picker());

    // Navigate to a different branch and press Enter.
    app.handle_branch_picker_key(&KeyMsg::from_type(KeyType::Down));
    app.handle_branch_picker_key(&KeyMsg::from_type(KeyType::Enter));

    // Picker should close after branch switch.
    assert!(!app.has_branch_picker(), "Picker should close after Enter");
}

#[test]
fn tui_grad_cycle_sibling_forward_with_branches() {
    let harness = TestHarness::new("tui_grad_cycle_sibling_forward_with_branches");
    let (session, _, _, _) = create_two_branch_session();
    let (mut app, _rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    // Cycle forward - should succeed (no error message).
    app.cycle_sibling_branch(true);

    // Should NOT show "No sibling branches" since we have 2 branches.
    let msg = app.status_message().unwrap_or("");
    assert!(
        !msg.contains("No sibling branches"),
        "Expected successful branch cycle, got status: {msg}"
    );
}

#[test]
fn tui_grad_cycle_sibling_backward_with_branches() {
    let harness = TestHarness::new("tui_grad_cycle_sibling_backward_with_branches");
    let (session, _, _, _) = create_two_branch_session();
    let (mut app, _rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    // Cycle backward - should succeed (no error message).
    app.cycle_sibling_branch(false);

    // Should NOT show "No sibling branches" since we have 2 branches.
    let msg = app.status_message().unwrap_or("");
    assert!(
        !msg.contains("No sibling branches"),
        "Expected successful branch cycle, got status: {msg}"
    );
}

#[test]
fn tui_grad_branch_picker_handles_fifty_plus_branches() {
    let harness = TestHarness::new("tui_grad_branch_picker_handles_fifty_plus_branches");
    let session = create_many_branch_session(60);
    let (mut app, _rx) = build_app_with_session_and_events(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    let start = Instant::now();
    app.open_branch_picker();
    assert!(app.has_branch_picker(), "Branch picker should open");

    // Exercise scrolling/wrapping behavior over many rows.
    for _ in 0..75 {
        app.handle_branch_picker_key(&KeyMsg::from_type(KeyType::Down));
        assert!(app.has_branch_picker(), "Branch picker should remain open");
    }
    for _ in 0..30 {
        app.handle_branch_picker_key(&KeyMsg::from_type(KeyType::Up));
        assert!(app.has_branch_picker(), "Branch picker should remain open");
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "Expected branch picker navigation to stay responsive, took {elapsed:?}"
    );

    let view = normalize_view(&BubbleteaModel::view(&app));
    assert!(
        view.contains("Select a branch"),
        "Expected branch picker overlay in view"
    );

    app.handle_branch_picker_key(&KeyMsg::from_type(KeyType::Enter));
    assert!(
        !app.has_branch_picker(),
        "Branch picker should close after selecting a branch"
    );
}

// ─── TUI Graduation: Inline Diff Rendering ─────────────────────────────────

#[test]
fn tui_grad_diff_pure_addition_renders_only_plus_lines() {
    let harness = TestHarness::new("tui_grad_diff_pure_addition_renders_only_plus_lines");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+add-only-diff)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Successfully replaced text in new_feature.rs.",
            ))],
            details: Some(json!({
                "diff": "+use std::io;\n+use std::fs;\n+\n+fn new_feature() {\n+    println!(\"hello\");\n+}"
            })),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit)",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "@@ new_feature.rs @@");
    assert_after_contains(&harness, &step, "+use std::io;");
    assert_after_contains(&harness, &step, "+fn new_feature()");
}

#[test]
fn tui_grad_diff_pure_removal_renders_only_minus_lines() {
    let harness = TestHarness::new("tui_grad_diff_pure_removal_renders_only_minus_lines");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+remove-only-diff)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Successfully replaced text in legacy.rs.",
            ))],
            details: Some(json!({
                "diff": "-fn deprecated_fn() {\n-    // old code\n-    todo!()\n-}"
            })),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit)",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "@@ legacy.rs @@");
    assert_after_contains(&harness, &step, "-fn deprecated_fn()");
    assert_after_contains(&harness, &step, "-    todo!()");
}

#[test]
fn tui_grad_diff_multiline_replacement_preserves_context() {
    let harness = TestHarness::new("tui_grad_diff_multiline_replacement_preserves_context");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+context-diff)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Successfully replaced text in config.rs.",
            ))],
            details: Some(json!({
                "diff": " fn configure() {\n-    let old_val = 42;\n+    let new_val = 99;\n     ok()\n }"
            })),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit)",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "@@ config.rs @@");
    assert_after_contains(&harness, &step, "-    let old_val = 42;");
    assert_after_contains(&harness, &step, "+    let new_val = 99;");
    assert_after_contains(&harness, &step, "fn configure()");
}

#[test]
fn tui_grad_diff_tool_error_omits_diff() {
    let harness = TestHarness::new("tui_grad_diff_tool_error_omits_diff");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+error)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Error: old_string not found in file.",
            ))],
            details: None,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit) error",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: true,
        },
    );

    assert_after_contains(&harness, &step, "Tool edit");
    assert_after_contains(&harness, &step, "old_string not found");
    assert_after_not_contains(&harness, &step, "@@");
    assert_after_not_contains(&harness, &step, "Diff:");
}

#[test]
fn tui_grad_diff_no_diff_key_shows_plain_output() {
    let harness = TestHarness::new("tui_grad_diff_no_diff_key_shows_plain_output");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) plain",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "total 24\ndrwxr-xr-x 3 user user 4096 Jan 1 00:00 src",
            ))],
            details: None,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(bash)",
        PiMsg::ToolEnd {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "Tool bash output:");
    assert_after_contains(&harness, &step, "total 24");
    assert_after_not_contains(&harness, &step, "@@");
    assert_after_not_contains(&harness, &step, "Diff:");
}

// ─── TUI Graduation: Progress Indicators ────────────────────────────────────

#[test]
fn tui_grad_progress_shows_metrics_when_elapsed_over_one_second() {
    let harness = TestHarness::new("tui_grad_progress_shows_metrics_when_elapsed_over_one_second");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(grep)",
        PiMsg::ToolStart {
            name: "grep".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(grep) with-progress",
        PiMsg::ToolUpdate {
            name: "grep".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("searching..."))],
            details: Some(json!({
                "progress": {
                    "elapsedMs": 5000,
                    "lineCount": 150,
                    "byteCount": 8192
                }
            })),
        },
    );

    assert_after_contains(&harness, &step, "Running grep");
    assert_after_contains(&harness, &step, "5s");
    assert_after_contains(&harness, &step, "150 lines");
}

#[test]
fn tui_grad_progress_shows_timeout_when_present() {
    let harness = TestHarness::new("tui_grad_progress_shows_timeout_when_present");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) with-timeout",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("running long command"))],
            details: Some(json!({
                "progress": {
                    "elapsedMs": 3000,
                    "lineCount": 10,
                    "byteCount": 500,
                    "timeoutMs": 120_000
                }
            })),
        },
    );

    assert_after_contains(&harness, &step, "Running bash");
    assert_after_contains(&harness, &step, "3s");
    assert_after_contains(&harness, &step, "timeout 120s");
}

// ─── TUI Graduation: Collapsible Block Interactions ─────────────────────────

#[test]
fn tui_grad_collapse_multiple_tools_mixed_sizes() {
    let harness = TestHarness::new("tui_grad_collapse_multiple_tools_mixed_sizes");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // First tool: small output (should NOT auto-collapse).
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
        "PiMsg::ToolUpdate(read) small",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("short output"))],
            details: None,
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(read)",
        PiMsg::ToolEnd {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    // Second tool: large output (should auto-collapse).
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-2".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) large",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-2".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(numbered_lines(30)))],
            details: None,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(bash)",
        PiMsg::ToolEnd {
            name: "bash".to_string(),
            tool_id: "tool-2".to_string(),
            is_error: false,
        },
    );

    // Small output should be visible, large output should be collapsed.
    assert_after_contains(&harness, &step, "short output");
    assert_after_contains(&harness, &step, "collapsed");
}

#[test]
fn tui_grad_collapse_global_toggle_affects_all_tool_blocks() {
    let harness = TestHarness::new("tui_grad_collapse_global_toggle_affects_all_tool_blocks");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Create two small tool outputs (not auto-collapsed).
    for i in 1..=2 {
        apply_pi(
            &harness,
            &mut app,
            &format!("PiMsg::ToolStart(read) tool-{i}"),
            PiMsg::ToolStart {
                name: "read".to_string(),
                tool_id: format!("tool-{i}"),
            },
        );
        apply_pi(
            &harness,
            &mut app,
            &format!("PiMsg::ToolUpdate(read) tool-{i}"),
            PiMsg::ToolUpdate {
                name: "read".to_string(),
                tool_id: format!("tool-{i}"),
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "output from tool {i}"
                )))],
                details: None,
            },
        );
        apply_pi(
            &harness,
            &mut app,
            &format!("PiMsg::ToolEnd(read) tool-{i}"),
            PiMsg::ToolEnd {
                name: "read".to_string(),
                tool_id: format!("tool-{i}"),
                is_error: false,
            },
        );
    }

    // Both should be visible.
    let view = normalize_view(&BubbleteaModel::view(&app));
    assert!(
        view.contains("output from tool 1"),
        "tool 1 should be visible"
    );
    assert!(
        view.contains("output from tool 2"),
        "tool 2 should be visible"
    );

    // Toggle collapse (Ctrl+O).
    let step = press_ctrlo(&harness, &mut app);
    assert_after_contains(&harness, &step, "collapsed");
    assert_after_not_contains(&harness, &step, "output from tool 1");
    assert_after_not_contains(&harness, &step, "output from tool 2");

    // Toggle back.
    let step = press_ctrlo(&harness, &mut app);
    assert_after_contains(&harness, &step, "output from tool 1");
    assert_after_contains(&harness, &step, "output from tool 2");
}

#[test]
fn tui_grad_collapse_auto_collapsed_shows_preview_line_count() {
    let harness = TestHarness::new("tui_grad_collapse_auto_collapsed_shows_preview_line_count");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) 25-lines",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(numbered_lines(25)))],
            details: None,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(bash)",
        PiMsg::ToolEnd {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "collapsed");
    assert_after_contains(&harness, &step, "line 1");
    assert_after_contains(&harness, &step, "line 5");
    assert_after_not_contains(&harness, &step, "line 6");
    assert_after_contains(&harness, &step, "more lines");
}

// ─── TUI Graduation: Image Rendering ────────────────────────────────────────

#[test]
fn tui_grad_image_default_config_shows_images() {
    let harness = TestHarness::new("tui_grad_image_default_config_shows_images");
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
        "PiMsg::ToolUpdate(read) with-image",
        PiMsg::ToolUpdate {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![
                ContentBlock::Text(TextContent::new("screenshot captured")),
                ContentBlock::Image(ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                }),
            ],
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

    assert_after_contains(&harness, &step, "screenshot captured");
    assert_after_not_contains(&harness, &step, "image(s) hidden");
}

#[test]
fn tui_grad_image_mixed_content_with_show_images_false_preserves_text() {
    let harness =
        TestHarness::new("tui_grad_image_mixed_content_with_show_images_false_preserves_text");
    let config = Config {
        terminal: Some(TerminalSettings {
            show_images: Some(false),
            clear_on_shrink: None,
        }),
        ..Config::default()
    };
    let mut app =
        build_app_with_session_and_config(&harness, Vec::new(), Session::in_memory(), config);
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) text+images",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![
                ContentBlock::Text(TextContent::new("command output line 1")),
                ContentBlock::Image(ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                }),
                ContentBlock::Text(TextContent::new("command output line 2")),
                ContentBlock::Image(ImageContent {
                    data: "d29ybGQ=".to_string(),
                    mime_type: "image/jpeg".to_string(),
                }),
            ],
            details: None,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(bash)",
        PiMsg::ToolEnd {
            name: "bash".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "command output line 1");
    assert_after_contains(&harness, &step, "command output line 2");
    assert_after_contains(&harness, &step, "2 image(s) hidden");
    assert_after_not_contains(&harness, &step, "[image:");
}

// ─── TUI Graduation: Integration (cross-feature) ───────────────────────────

#[test]
fn tui_grad_integration_multiple_tools_in_sequence() {
    let harness = TestHarness::new("tui_grad_integration_multiple_tools_in_sequence");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Tool 1: read with small output.
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
            content: vec![ContentBlock::Text(TextContent::new("fn main() {}"))],
            details: None,
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(read)",
        PiMsg::ToolEnd {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    // Tool 2: edit with diff.
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-2".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+diff)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-2".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Successfully replaced text in main.rs.",
            ))],
            details: Some(json!({
                "diff": "-fn main() {}\n+fn main() {\n+    println!(\"hello\");\n+}"
            })),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit)",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-2".to_string(),
            is_error: false,
        },
    );

    // Tool 3: bash with large output (auto-collapses).
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-3".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) large",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-3".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(numbered_lines(30)))],
            details: None,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(bash)",
        PiMsg::ToolEnd {
            name: "bash".to_string(),
            tool_id: "tool-3".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "fn main() {}");
    assert_after_contains(&harness, &step, "@@ main.rs @@");
    assert_after_contains(&harness, &step, "collapsed");
}

#[test]
fn tui_grad_integration_branching_with_tool_diffs() {
    let harness = TestHarness::new("tui_grad_integration_branching_with_tool_diffs");
    let (session, _root_id, _branch_a_id, _branch_b_id) = create_two_branch_session();

    let mut app = build_app_with_session(&harness, Vec::new(), session);
    log_initial_state(&harness, &app);

    // Session was navigated to branch 2 last, so we see branch 2 content.
    let view = normalize_view(&BubbleteaModel::view(&app));
    assert!(
        view.contains("Answer B"),
        "Should show branch B content (last navigated branch)"
    );

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+diff)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Successfully replaced text in branch_a.rs.",
            ))],
            details: Some(json!({
                "diff": "-old_branch_a_code\n+new_branch_a_code"
            })),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit)",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "@@ branch_a.rs @@");
    assert_after_contains(&harness, &step, "+new_branch_a_code");
}

#[test]
fn tui_grad_integration_tool_error_then_success_sequence() {
    let harness = TestHarness::new("tui_grad_integration_tool_error_then_success_sequence");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // First tool call: error.
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit) error-content",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Error: old_string not found in file.",
            ))],
            details: None,
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit) error",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: true,
        },
    );

    // Second tool call: success with diff.
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-2".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+diff) retry",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-2".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Successfully replaced text in retry.rs.",
            ))],
            details: Some(json!({
                "diff": "-old_value\n+new_value"
            })),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit) success",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-2".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "old_string not found");
    assert_after_contains(&harness, &step, "@@ retry.rs @@");
    assert_after_contains(&harness, &step, "+new_value");
}

#[test]
fn tui_grad_integration_diff_with_collapse_toggle() {
    let harness = TestHarness::new("tui_grad_integration_diff_with_collapse_toggle");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(edit)",
        PiMsg::ToolStart {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
        },
    );

    let mut diff_lines = Vec::new();
    for i in 0..20 {
        diff_lines.push(format!("-old line {i}"));
        diff_lines.push(format!("+new line {i}"));
    }
    let diff = diff_lines.join("\n");

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(edit+large-diff)",
        PiMsg::ToolUpdate {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "Successfully replaced text in large_file.rs.",
            ))],
            details: Some(json!({ "diff": diff })),
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(edit)",
        PiMsg::ToolEnd {
            name: "edit".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    assert_after_contains(&harness, &step, "collapsed");

    // Toggle expand (Ctrl+O twice: first collapse globally, second expand all).
    press_ctrlo(&harness, &mut app);
    let step = press_ctrlo(&harness, &mut app);

    assert_after_contains(&harness, &step, "@@ large_file.rs @@");
}

// ===========================================================================
// Model selector overlay tests
// ===========================================================================

fn press_ctrll(harness: &TestHarness, app: &mut PiApp) -> StepOutcome {
    apply_key(harness, app, "key:CtrlL", KeyMsg::from_type(KeyType::CtrlL))
}

#[test]
fn tui_state_model_selector_opens_on_ctrll() {
    let harness = TestHarness::new("tui_state_model_selector_opens_on_ctrll");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    let available_models = vec![anthropic.clone(), openai];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    let step = press_ctrll(&harness, &mut app);
    assert_after_contains(&harness, &step, "Select a model");
    assert_after_contains(&harness, &step, "(type to filter)");
    assert_after_contains(&harness, &step, "anthropic/claude-a");
    assert_after_contains(&harness, &step, "openai/gpt-a");
}

#[test]
fn tui_state_model_selector_cancel_on_esc() {
    let harness = TestHarness::new("tui_state_model_selector_cancel_on_esc");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    let available_models = vec![anthropic.clone(), openai];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    press_ctrll(&harness, &mut app);
    let step = press_esc(&harness, &mut app);
    assert_after_not_contains(&harness, &step, "Select a model");
    assert_after_contains(&harness, &step, "Model selector cancelled");
}

#[test]
fn tui_state_model_selector_filters_on_typing() {
    let harness = TestHarness::new("tui_state_model_selector_filters_on_typing");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");
    let google = make_model_entry(
        "google",
        "gemini-a",
        "https://generativeai.googleapis.com/v1beta/models",
    );

    let available_models = vec![anthropic.clone(), openai, google];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    press_ctrll(&harness, &mut app);
    // Type "gpt" to filter
    let step = type_text(&harness, &mut app, "gpt");
    assert_after_contains(&harness, &step, "Select a model");
    assert_after_contains(&harness, &step, "openai/gpt-a");
    // Other model rows should be filtered out. The current model id can still
    // appear outside the selector list (e.g. header/status), so assert on row
    // formatting rather than raw id substring.
    assert_after_not_contains(&harness, &step, "> anthropic/claude-a *");
    assert_after_not_contains(&harness, &step, "google/gemini-a");
}

#[test]
fn tui_state_model_selector_navigates_with_arrows() {
    let harness = TestHarness::new("tui_state_model_selector_navigates_with_arrows");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    let available_models = vec![anthropic.clone(), openai];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    press_ctrll(&harness, &mut app);
    // Initially first item is selected (anthropic/claude-a)
    let step = press_down(&harness, &mut app);
    // After pressing down, second item should be selected
    // The ">" prefix marks the selected item
    assert_after_contains(&harness, &step, "Select a model");
}

#[test]
fn tui_state_model_selector_select_switches_model() {
    let harness = TestHarness::new("tui_state_model_selector_select_switches_model");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    let available_models = vec![anthropic.clone(), openai];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    press_ctrll(&harness, &mut app);
    // Navigate to second model (openai/gpt-a)
    press_down(&harness, &mut app);
    // Select it
    let step = press_enter(&harness, &mut app);
    // Overlay should close and model should switch
    assert_after_not_contains(&harness, &step, "Select a model");
    assert_after_contains(&harness, &step, "Switched model: openai/gpt-a");

    // Verify session header updated
    let session_handle = app.session_handle();
    let session_guard = session_handle.try_lock().expect("session lock");
    assert_eq!(session_guard.header.provider.as_deref(), Some("openai"));
    assert_eq!(session_guard.header.model_id.as_deref(), Some("gpt-a"));
}

#[test]
fn tui_state_model_selector_select_current_shows_already_using() {
    let harness = TestHarness::new("tui_state_model_selector_select_current_shows_already_using");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    let available_models = vec![anthropic.clone(), openai];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    press_ctrll(&harness, &mut app);
    // Select first model (same as current)
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Already using");
}

#[test]
fn tui_state_model_selector_no_models_shows_message() {
    let harness = TestHarness::new("tui_state_model_selector_no_models_shows_message");

    let model_entry = dummy_model_entry();
    let available_models: Vec<ModelEntry> = Vec::new();
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        model_entry,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    let step = press_ctrll(&harness, &mut app);
    assert_after_contains(&harness, &step, "No models available");
    assert_after_not_contains(&harness, &step, "Select a model");
}

#[test]
fn tui_state_ctrlp_single_model_shows_only_one_message() {
    let harness = TestHarness::new("tui_state_ctrlp_single_model_shows_only_one_message");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );

    let available_models = vec![anthropic.clone()];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    let step = press_ctrlp(&harness, &mut app);
    assert_after_contains(&harness, &step, "Only one model available");
}

#[test]
fn tui_state_ctrlp_single_model_in_scope_shows_scope_message() {
    let harness = TestHarness::new("tui_state_ctrlp_single_model_in_scope_shows_scope_message");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    // Scope contains only one model but available has two
    let model_scope = vec![anthropic.clone()];
    let available_models = vec![anthropic.clone(), openai];

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    let step = press_ctrlp(&harness, &mut app);
    assert_after_contains(&harness, &step, "Only one model in scope");
}

#[test]
fn tui_state_ctrlp_cycling_wraps_forward_through_all_models() {
    let harness = TestHarness::new("tui_state_ctrlp_cycling_wraps_forward_through_all_models");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");
    let google = make_model_entry(
        "google",
        "gemini-a",
        "https://generativeai.googleapis.com/v1beta/models",
    );

    let available_models = vec![openai, google, anthropic.clone()];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    // Sorted order: anthropic/claude-a, google/gemini-a, openai/gpt-a
    // Current: anthropic/claude-a → next: google/gemini-a
    let step = press_ctrlp(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched model: google/gemini-a");

    // google/gemini-a → openai/gpt-a
    let step = press_ctrlp(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched model: openai/gpt-a");

    // openai/gpt-a → wraps to anthropic/claude-a
    let step = press_ctrlp(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched model: anthropic/claude-a");
}

#[test]
fn tui_state_model_selector_backspace_removes_filter() {
    let harness = TestHarness::new("tui_state_model_selector_backspace_removes_filter");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    let available_models = vec![anthropic.clone(), openai];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );

    press_ctrll(&harness, &mut app);
    // Filter to show only openai
    type_text(&harness, &mut app, "gpt");
    // Backspace 3 times to clear filter
    apply_key(
        &harness,
        &mut app,
        "key:Backspace",
        KeyMsg::from_type(KeyType::Backspace),
    );
    apply_key(
        &harness,
        &mut app,
        "key:Backspace",
        KeyMsg::from_type(KeyType::Backspace),
    );
    let step = apply_key(
        &harness,
        &mut app,
        "key:Backspace",
        KeyMsg::from_type(KeyType::Backspace),
    );
    // Both models should be visible again
    assert_after_contains(&harness, &step, "anthropic/claude-a");
    assert_after_contains(&harness, &step, "openai/gpt-a");
}

// ---------------------------------------------------------------------------
// bd-w4dn: Header + tool/thinking collapse toggle tests
// ---------------------------------------------------------------------------

#[test]
fn tui_state_header_shows_pi_and_model_name() {
    let harness = TestHarness::new("tui_state_header_shows_pi_and_model_name");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Trigger a view render via a no-op key press.
    let step = apply_key(
        &harness,
        &mut app,
        "key:noop",
        KeyMsg::from_runes(vec![' ']),
    );
    assert_after_contains(&harness, &step, "Pi");
    assert_after_contains(&harness, &step, "dummy-model");
}

#[test]
fn tui_state_collapse_changelog_settings_toggle_persists() {
    let harness = TestHarness::new("tui_state_collapse_changelog_settings_toggle_persists");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/settings");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Settings");
    assert_after_contains(&harness, &step, "collapseChangelog:");

    // Navigate to CollapseChangelog entry:
    // Summary(0), Theme(1), SteeringMode(2), FollowUpMode(3), QuietStartup(4), CollapseChangelog(5)
    press_down(&harness, &mut app);
    press_down(&harness, &mut app);
    press_down(&harness, &mut app);
    press_down(&harness, &mut app);
    press_down(&harness, &mut app);
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Updated collapseChangelog: on");

    let settings = read_project_settings_json(&harness);
    assert_eq!(settings["collapse_changelog"], json!(true));
}

#[test]
fn tui_state_hide_thinking_block_settings_toggle_persists() {
    let harness = TestHarness::new("tui_state_hide_thinking_block_settings_toggle_persists");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    type_text(&harness, &mut app, "/settings");
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Settings");
    assert_after_contains(&harness, &step, "hideThinkingBlock:");

    // Navigate to HideThinkingBlock entry:
    // Summary(0), Theme(1), SteeringMode(2), FollowUpMode(3), QuietStartup(4),
    // CollapseChangelog(5), HideThinkingBlock(6)
    for _ in 0..6 {
        press_down(&harness, &mut app);
    }
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Updated hideThinkingBlock: on");

    let settings = read_project_settings_json(&harness);
    assert_eq!(settings["hide_thinking_block"], json!(true));
}

#[test]
fn tui_state_ctrlt_thinking_visible_hidden_visible_cycle() {
    let harness = TestHarness::new("tui_state_ctrlt_thinking_visible_hidden_visible_cycle");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Generate thinking content while processing.
    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ThinkingDelta(visible)",
        PiMsg::ThinkingDelta("deep thought".to_string()),
    );
    // Thinking is visible by default.
    assert_after_contains(&harness, &step, "Thinking:");
    assert_after_contains(&harness, &step, "deep thought");

    // Ctrl+T hides thinking.
    let step = press_ctrlt(&harness, &mut app);
    assert_after_not_contains(&harness, &step, "Thinking:");
    assert_after_not_contains(&harness, &step, "deep thought");

    // Ctrl+T shows thinking again.
    let step = press_ctrlt(&harness, &mut app);
    assert_after_contains(&harness, &step, "Thinking:");
    assert_after_contains(&harness, &step, "deep thought");
}

#[test]
fn tui_state_tool_error_output_collapse_toggle() {
    let harness = TestHarness::new("tui_state_tool_error_output_collapse_toggle");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(bash)",
        PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "tool-err-1".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(bash) error-output",
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "tool-err-1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "command not found: foobar",
            ))],
            details: None,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(bash) is_error",
        PiMsg::ToolEnd {
            name: "bash".to_string(),
            tool_id: "tool-err-1".to_string(),
            is_error: true,
        },
    );
    // Error tool output is shown.
    assert_after_contains(&harness, &step, "command not found: foobar");

    // Ctrl+O collapses.
    let step = press_ctrlo(&harness, &mut app);
    assert_after_contains(&harness, &step, "collapsed");
    assert_after_not_contains(&harness, &step, "command not found: foobar");

    // Ctrl+O re-expands.
    let step = press_ctrlo(&harness, &mut app);
    assert_after_not_contains(&harness, &step, "collapsed");
    assert_after_contains(&harness, &step, "command not found: foobar");
}

#[test]
fn tui_state_multiple_tool_blocks_collapse_together() {
    let harness = TestHarness::new("tui_state_multiple_tool_blocks_collapse_together");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // First tool: read
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
            content: vec![ContentBlock::Text(TextContent::new("contents of file A"))],
            details: None,
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(read)",
        PiMsg::ToolEnd {
            name: "read".to_string(),
            tool_id: "tool-1".to_string(),
            is_error: false,
        },
    );

    // Second tool: grep
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolStart(grep)",
        PiMsg::ToolStart {
            name: "grep".to_string(),
            tool_id: "tool-2".to_string(),
        },
    );
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolUpdate(grep)",
        PiMsg::ToolUpdate {
            name: "grep".to_string(),
            tool_id: "tool-2".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "match found at line 42",
            ))],
            details: None,
        },
    );
    let step = apply_pi(
        &harness,
        &mut app,
        "PiMsg::ToolEnd(grep)",
        PiMsg::ToolEnd {
            name: "grep".to_string(),
            tool_id: "tool-2".to_string(),
            is_error: false,
        },
    );

    // Both tool outputs should be visible.
    assert_after_contains(&harness, &step, "contents of file A");
    assert_after_contains(&harness, &step, "match found at line 42");

    // Ctrl+O collapses all tool outputs.
    let step = press_ctrlo(&harness, &mut app);
    assert_after_not_contains(&harness, &step, "contents of file A");
    assert_after_not_contains(&harness, &step, "match found at line 42");

    // Ctrl+O re-expands all.
    let step = press_ctrlo(&harness, &mut app);
    assert_after_contains(&harness, &step, "contents of file A");
    assert_after_contains(&harness, &step, "match found at line 42");
}

#[test]
fn tui_state_thinking_and_tool_toggles_independent() {
    let harness = TestHarness::new("tui_state_thinking_and_tool_toggles_independent");
    let mut app = build_app(&harness, Vec::new());
    log_initial_state(&harness, &app);

    // Generate thinking content.
    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);
    apply_pi(
        &harness,
        &mut app,
        "PiMsg::ThinkingDelta",
        PiMsg::ThinkingDelta("reasoning step".to_string()),
    );

    // Tool output.
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
            content: vec![ContentBlock::Text(TextContent::new("file data"))],
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

    // Both visible initially.
    assert_after_contains(&harness, &step, "reasoning step");
    assert_after_contains(&harness, &step, "file data");

    // Hide thinking only (Ctrl+T).
    let step = press_ctrlt(&harness, &mut app);
    assert_after_not_contains(&harness, &step, "reasoning step");
    assert_after_contains(&harness, &step, "file data");

    // Collapse tools only (Ctrl+O).
    let step = press_ctrlo(&harness, &mut app);
    assert_after_not_contains(&harness, &step, "reasoning step");
    assert_after_not_contains(&harness, &step, "file data");

    // Show thinking back (Ctrl+T) — tools stay collapsed.
    let step = press_ctrlt(&harness, &mut app);
    assert_after_contains(&harness, &step, "reasoning step");
    assert_after_not_contains(&harness, &step, "file data");

    // Expand tools (Ctrl+O) — both now visible.
    let step = press_ctrlo(&harness, &mut app);
    assert_after_contains(&harness, &step, "reasoning step");
    assert_after_contains(&harness, &step, "file data");
}

#[test]
fn tui_state_collapse_changelog_toggle_off_then_on() {
    let harness = TestHarness::new("tui_state_collapse_changelog_toggle_off_then_on");
    let config = Config {
        collapse_changelog: Some(true),
        ..Config::default()
    };
    let mut app =
        build_app_with_session_and_config(&harness, Vec::new(), Session::in_memory(), config);
    log_initial_state(&harness, &app);

    // Open settings and navigate to CollapseChangelog (entry 5).
    type_text(&harness, &mut app, "/settings");
    press_enter(&harness, &mut app);
    for _ in 0..5 {
        press_down(&harness, &mut app);
    }
    // Toggle off.
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Updated collapseChangelog: off");

    let settings = read_project_settings_json(&harness);
    assert_eq!(settings["collapse_changelog"], json!(false));

    // Reopen and toggle back on.
    type_text(&harness, &mut app, "/settings");
    press_enter(&harness, &mut app);
    for _ in 0..5 {
        press_down(&harness, &mut app);
    }
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Updated collapseChangelog: on");

    let settings = read_project_settings_json(&harness);
    assert_eq!(settings["collapse_changelog"], json!(true));
}

// ===========================================================================
// bd-4ma1: Model selector + scoped cycling additional tests
// ===========================================================================

#[test]
fn tui_state_model_selector_blocked_during_processing() {
    let harness = TestHarness::new("tui_state_model_selector_blocked_during_processing");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    let available_models = vec![anthropic.clone(), openai];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );
    log_initial_state(&harness, &app);

    // Simulate agent processing state.
    apply_pi(&harness, &mut app, "PiMsg::AgentStart", PiMsg::AgentStart);

    // Ctrl+L while processing should be blocked.
    let step = press_ctrll(&harness, &mut app);
    assert_after_contains(&harness, &step, "Cannot switch models while processing");
    assert_after_not_contains(&harness, &step, "Select a model");
}

#[test]
fn tui_state_model_selector_jk_navigation() {
    let harness = TestHarness::new("tui_state_model_selector_jk_navigation");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");
    let google = make_model_entry(
        "google",
        "gemini-a",
        "https://generativeai.googleapis.com/v1beta/models",
    );

    let available_models = vec![anthropic.clone(), openai, google];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );
    log_initial_state(&harness, &app);

    press_ctrll(&harness, &mut app);
    // Navigate down with j twice → should land on third model (index 2)
    type_text(&harness, &mut app, "j");
    type_text(&harness, &mut app, "j");
    // Select it with Enter
    let step = press_enter(&harness, &mut app);
    // Third model in sorted order: anthropic/claude-a, google/gemini-a, openai/gpt-a
    // Start at idx 0 → j → idx 1 → j → idx 2 → Enter selects openai/gpt-a
    assert_after_contains(&harness, &step, "Switched model: openai/gpt-a");
}

#[test]
fn tui_state_model_selector_filter_then_select() {
    let harness = TestHarness::new("tui_state_model_selector_filter_then_select");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");
    let google = make_model_entry(
        "google",
        "gemini-a",
        "https://generativeai.googleapis.com/v1beta/models",
    );

    let available_models = vec![anthropic.clone(), openai, google];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );
    log_initial_state(&harness, &app);

    press_ctrll(&harness, &mut app);
    // Type filter text that matches only gemini
    type_text(&harness, &mut app, "gemini");
    // First filtered result should be auto-selected, press Enter to confirm
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "Switched model: google/gemini-a");
    assert_after_not_contains(&harness, &step, "Select a model");
}

#[test]
fn tui_state_model_selector_no_match_enter_shows_no_model() {
    let harness = TestHarness::new("tui_state_model_selector_no_match_enter_shows_no_model");

    let anthropic = make_model_entry(
        "anthropic",
        "claude-a",
        "https://api.anthropic.com/v1/messages",
    );
    let openai = make_model_entry("openai", "gpt-a", "https://api.openai.com/v1");

    let available_models = vec![anthropic.clone(), openai];
    let model_scope = Vec::new();

    let mut app = build_app_with_models(
        &harness,
        Session::in_memory(),
        Config::default(),
        anthropic,
        model_scope,
        available_models,
        KeyBindings::new(),
    );
    log_initial_state(&harness, &app);

    press_ctrll(&harness, &mut app);
    // Type filter text that matches nothing
    type_text(&harness, &mut app, "zzzzz");
    // Press Enter with no matches → should show "No model selected"
    let step = press_enter(&harness, &mut app);
    assert_after_contains(&harness, &step, "No model selected");
    assert_after_not_contains(&harness, &step, "Select a model");
}
