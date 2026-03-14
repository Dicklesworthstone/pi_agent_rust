use super::share::{parse_gist_url_and_id, parse_share_is_public, share_gist_description};
use super::*;
use crate::agent::AgentConfig;
use crate::auth::ApiKeyValue;
use crate::extensions::{ExtensionManager, PROTOCOL_VERSION, RegisterPayload};
use crate::model::StreamEvent;
use crate::provider::{Context, Provider, StreamOptions};
use crate::resources::{ResourceCliOptions, ResourceLoader};
use crate::session::Session;
use crate::tools::ToolRegistry;
use asupersync::channel::mpsc;
use asupersync::runtime::RuntimeBuilder;
use futures::stream;
use serde_json::json;
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

fn test_runtime_handle() -> asupersync::runtime::RuntimeHandle {
    static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        RuntimeBuilder::current_thread()
            .build()
            .expect("build asupersync runtime")
    })
    .handle()
}

fn build_test_app_with_config(config: Config) -> PiApp {
    build_test_app_with_config_and_extensions(config, None)
}

fn build_test_app_with_config_and_extensions(
    config: Config,
    extensions: Option<crate::extensions::ExtensionManager>,
) -> PiApp {
    let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&[], Path::new("."), Some(&config)),
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
    let model_entry = test_model_entry("dummy", "dummy-model");
    let model_scope = vec![model_entry.clone()];
    let available_models = vec![model_entry.clone()];
    let (event_tx, _event_rx) = mpsc::channel(64);

    PiApp::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
        config,
        resources,
        resource_cli,
        Path::new(".").to_path_buf(),
        model_entry,
        model_scope,
        available_models,
        Vec::new(),
        event_tx,
        test_runtime_handle(),
        false,
        extensions,
        Some(KeyBindings::new()),
        Vec::new(),
        Usage::default(),
    )
}

fn test_extension_manager_with_registered_extension() -> ExtensionManager {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "test-extension".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: vec![json!({
            "name": "test-command",
            "description": "test command",
        })],
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });
    manager
}

fn custom_test_message(content: &str) -> ModelMessage {
    ModelMessage::Custom(CustomMessage {
        content: content.to_string(),
        custom_type: "test".to_string(),
        display: false,
        details: None,
        timestamp: 0,
    })
}

#[test]
fn format_count_suffixes() {
    assert_eq!(format_count(0), "0");
    assert_eq!(format_count(999), "999");
    assert_eq!(format_count(1_000), "1.0K");
    assert_eq!(format_count(1_500), "1.5K");
    assert_eq!(format_count(42_000), "42.0K");
    assert_eq!(format_count(1_000_000), "1.0M");
    assert_eq!(format_count(2_500_000), "2.5M");
}

#[test]
fn tool_progress_format_display() {
    let mut p = ToolProgress::new();
    p.elapsed_ms = 5_000;
    p.line_count = 42;
    let display = p.format_display("bash");
    assert!(display.contains("Running bash"));
    assert!(display.contains("5s"));
    assert!(display.contains("42 lines"));

    // With byte count instead of lines
    p.line_count = 0;
    p.byte_count = 1_500;
    let display = p.format_display("grep");
    assert!(display.contains("Running grep"));
    assert!(display.contains("1.5K bytes"));
    assert!(!display.contains("lines"));

    // With timeout
    p.timeout_ms = Some(120_000);
    let display = p.format_display("bash");
    assert!(display.contains("timeout 120s"));
}

#[test]
fn tool_progress_update_from_details() {
    let mut p = ToolProgress::new();
    let details = json!({
        "progress": {
            "elapsedMs": 3000,
            "lineCount": 100,
            "byteCount": 5000,
            "timeoutMs": 60000
        }
    });
    p.update_from_details(Some(&details));
    assert_eq!(p.elapsed_ms, 3000);
    assert_eq!(p.line_count, 100);
    assert_eq!(p.byte_count, 5000);
    assert_eq!(p.timeout_ms, Some(60000));
}

#[test]
fn tool_progress_update_from_no_details() {
    let mut p = ToolProgress::new();
    // Sleep a tiny bit so elapsed > 0
    std::thread::sleep(std::time::Duration::from_millis(5));
    p.update_from_details(None);
    assert!(p.elapsed_ms >= 5);
    assert_eq!(p.line_count, 0);
}

#[test]
fn initial_window_size_cmd_emits_window_size_message() {
    let msg = PiApp::initial_window_size_cmd()
        .execute()
        .expect("window size message");
    let size = msg
        .downcast::<WindowSizeMsg>()
        .expect("window size message type");

    assert!(size.width > 0);
    assert!(size.height > 0);
}

#[test]
fn startup_init_cmd_sequences_window_size_before_pending() {
    let msg = PiApp::startup_init_cmd(None, Some(Cmd::new(|| Message::new(PiMsg::RunPending))))
        .expect("startup init command")
        .execute()
        .expect("startup init message");
    let sequence = msg
        .downcast::<bubbletea::message::SequenceMsg>()
        .expect("startup sequence message");

    let mut cmds = sequence.0.into_iter();
    let first = cmds
        .next()
        .expect("window size cmd")
        .execute()
        .expect("window size message");
    assert!(
        first.downcast_ref::<WindowSizeMsg>().is_some(),
        "first startup command should refresh window size"
    );

    let second = cmds
        .next()
        .expect("pending cmd")
        .execute()
        .expect("pending message");
    assert!(
        second
            .downcast_ref::<PiMsg>()
            .is_some_and(|msg| matches!(msg, PiMsg::RunPending)),
        "second startup command should run pending work"
    );
}

#[test]
fn enqueue_ui_shutdown_waits_for_capacity_in_full_channel() {
    asupersync::test_utils::run_test(|| async {
        let (event_tx, event_rx) = mpsc::channel(1);
        event_tx
            .try_send(PiMsg::System("busy".to_string()))
            .expect("fill bounded event channel");

        let send_cx = Cx::for_request();
        let recv_cx = Cx::for_request();
        let send_shutdown = enqueue_ui_shutdown(&event_tx, &send_cx);
        let recv_messages = async {
            let first = event_rx.recv(&recv_cx).await.expect("first queued message");
            let second = event_rx.recv(&recv_cx).await.expect("shutdown message");
            (first, second)
        };

        let ((), (first, second)) = futures::join!(send_shutdown, recv_messages);

        assert!(matches!(first, PiMsg::System(text) if text == "busy"));
        assert!(matches!(second, PiMsg::UiShutdown));
    });
}

#[test]
fn enqueue_pi_event_preserves_extension_ui_requests_under_backpressure() {
    asupersync::test_utils::run_test(|| async {
        let (event_tx, event_rx) = mpsc::channel(1);
        event_tx
            .try_send(PiMsg::System("busy".to_string()))
            .expect("fill bounded event channel");

        let request = ExtensionUiRequest::new(
            "req-confirm",
            "confirm",
            json!({ "title": "Need approval" }),
        );
        let send_cx = Cx::for_request();
        let recv_cx = Cx::for_request();
        let send_request = enqueue_pi_event(
            &event_tx,
            &send_cx,
            PiMsg::ExtensionUiRequest(request.clone()),
        );
        let recv_messages = async {
            let first = event_rx.recv(&recv_cx).await.expect("first queued message");
            let second = event_rx.recv(&recv_cx).await.expect("extension ui request");
            (first, second)
        };

        let (enqueued, (first, second)) = futures::join!(send_request, recv_messages);

        assert!(
            enqueued,
            "extension UI request should enqueue once capacity opens"
        );
        assert!(matches!(first, PiMsg::System(text) if text == "busy"));
        match second {
            PiMsg::ExtensionUiRequest(actual) => {
                assert_eq!(actual.id, request.id);
                assert_eq!(actual.method, request.method);
                assert_eq!(actual.payload, request.payload);
            }
            other => panic!("expected extension UI request, got {other:?}"),
        }
    });
}

#[test]
fn enqueue_pi_event_preserves_conversation_reset_under_backpressure() {
    asupersync::test_utils::run_test(|| async {
        let (event_tx, event_rx) = mpsc::channel(1);
        event_tx
            .try_send(PiMsg::System("busy".to_string()))
            .expect("fill bounded event channel");

        let send_cx = Cx::for_request();
        let recv_cx = Cx::for_request();
        let send_reset = enqueue_pi_event(
            &event_tx,
            &send_cx,
            PiMsg::ConversationReset {
                messages: Vec::new(),
                usage: Usage::default(),
                status: Some("Session resumed".to_string()),
            },
        );
        let recv_messages = async {
            let first = event_rx.recv(&recv_cx).await.expect("first queued message");
            let second = event_rx
                .recv(&recv_cx)
                .await
                .expect("conversation reset message");
            (first, second)
        };

        let (enqueued, (first, second)) = futures::join!(send_reset, recv_messages);

        assert!(
            enqueued,
            "conversation reset should enqueue once capacity opens"
        );
        assert!(matches!(first, PiMsg::System(text) if text == "busy"));
        match second {
            PiMsg::ConversationReset {
                messages,
                usage,
                status,
            } => {
                assert!(messages.is_empty());
                assert_eq!(usage.input, 0);
                assert_eq!(usage.output, 0);
                assert_eq!(usage.cache_read, 0);
                assert_eq!(usage.cache_write, 0);
                assert_eq!(usage.total_tokens, 0);
                assert!(usage.cost.input.abs() <= f64::EPSILON);
                assert!(usage.cost.output.abs() <= f64::EPSILON);
                assert!(usage.cost.cache_read.abs() <= f64::EPSILON);
                assert!(usage.cost.cache_write.abs() <= f64::EPSILON);
                assert!(usage.cost.total.abs() <= f64::EPSILON);
                assert_eq!(status.as_deref(), Some("Session resumed"));
            }
            other => panic!("expected conversation reset, got {other:?}"),
        }
    });
}

#[test]
fn enqueue_pi_event_current_uses_ambient_context_under_backpressure() {
    asupersync::test_utils::run_test(|| async {
        let (event_tx, event_rx) = mpsc::channel(1);
        event_tx
            .try_send(PiMsg::System("busy".to_string()))
            .expect("fill bounded event channel");

        let current_cx = Cx::for_testing();
        let _guard = Cx::set_current(Some(current_cx));
        let recv_cx = Cx::for_request();
        let send_system = enqueue_pi_event_current(&event_tx, PiMsg::System("queued".to_string()));
        let recv_messages = async {
            let first = event_rx.recv(&recv_cx).await.expect("first queued message");
            let second = event_rx
                .recv(&recv_cx)
                .await
                .expect("second queued message");
            (first, second)
        };

        let (enqueued, (first, second)) = futures::join!(send_system, recv_messages);

        assert!(enqueued, "ambient context send should survive backpressure");
        assert!(matches!(first, PiMsg::System(text) if text == "busy"));
        assert!(matches!(second, PiMsg::System(text) if text == "queued"));
    });
}

#[test]
fn enqueue_pi_event_current_respects_ambient_context_cancellation() {
    asupersync::test_utils::run_test(|| async {
        use asupersync::channel::mpsc::RecvError;
        use asupersync::types::CancelKind;

        let (event_tx, event_rx) = mpsc::channel(1);
        event_tx
            .try_send(PiMsg::System("busy".to_string()))
            .expect("fill bounded event channel");

        let current_cx = Cx::for_testing();
        current_cx.cancel_with(CancelKind::User, Some("cancel stale UI send"));
        let _guard = Cx::set_current(Some(current_cx));

        let enqueued =
            enqueue_pi_event_current(&event_tx, PiMsg::System("stale".to_string())).await;
        assert!(
            !enqueued,
            "cancelled ambient context must reject stale UI sends"
        );

        let recv_cx = Cx::for_request();
        let first = event_rx.recv(&recv_cx).await.expect("first queued message");
        assert!(matches!(first, PiMsg::System(text) if text == "busy"));
        assert!(
            matches!(event_rx.try_recv(), Err(RecvError::Empty)),
            "cancelled send should not enqueue a follow-on message"
        );
    });
}

#[test]
fn tmux_wheel_guard_extracts_saved_binding_command() {
    let line = r##"bind-key -T root WheelUpPane            if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -e }"##;
    assert_eq!(
        TmuxWheelGuard::binding_command(line, "WheelUpPane").as_deref(),
        Some(
            r##"if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -e }"##
        )
    );
}

#[test]
fn tmux_wheel_guard_extracts_repeatable_binding_command() {
    let line = r"bind-key -r -T root WheelUpPane send-keys -M";
    assert_eq!(
        TmuxWheelGuard::binding_command(line, "WheelUpPane").as_deref(),
        Some("send-keys -M")
    );
}

#[test]
fn tmux_wheel_guard_extracts_command_after_quoted_option_value() {
    let line = r#"bind-key -N "wheel note" -T root WheelUpPane display-message "foo bar""#;
    assert_eq!(
        TmuxWheelGuard::binding_command(line, "WheelUpPane").as_deref(),
        Some(r#"display-message "foo bar""#)
    );
}

#[test]
fn tmux_wheel_guard_builds_pane_scoped_binding_args() {
    let fallback =
        r##"if-shell -F "#{||:#{pane_in_mode},#{mouse_any_flag}}" { send-keys -M } { copy-mode -e }"##
            .to_string();
    let args = TmuxWheelGuard::pane_scoped_binding_args("%3", "WheelUpPane", fallback.clone());

    assert_eq!(args[0], "bind-key");
    assert_eq!(args[1], "-T");
    assert_eq!(args[2], "root");
    assert_eq!(args[3], "WheelUpPane");
    assert_eq!(args[4], "if-shell");
    assert_eq!(args[5], "-F");
    assert_eq!(args[6], "#{==:#{pane_id},%3}");
    assert_eq!(args[7], "send-keys -M");
    assert_eq!(args[8], fallback);
}

#[test]
fn tmux_wheel_guard_binding_command_preserves_quoted_segments() {
    let line = r#"bind-key -T root WheelUpPane display-message "foo bar""#;
    assert_eq!(
        TmuxWheelGuard::binding_command(line, "WheelUpPane").as_deref(),
        Some(r#"display-message "foo bar""#)
    );
}

#[test]
fn tool_message_auto_collapse_threshold() {
    // Small output: not collapsed.
    let small = ConversationMessage::tool("Tool bash:\nline1\nline2".to_string());
    assert!(!small.collapsed);
    assert_eq!(small.role, MessageRole::Tool);

    // Exactly at threshold: not collapsed (20 lines = threshold).
    let lines: String = (1..=TOOL_AUTO_COLLAPSE_THRESHOLD)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let at_threshold = ConversationMessage::tool(lines);
    assert!(!at_threshold.collapsed);

    // Over threshold: auto-collapsed.
    let lines: String = (1..=TOOL_AUTO_COLLAPSE_THRESHOLD + 1)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let over_threshold = ConversationMessage::tool(lines);
    assert!(over_threshold.collapsed);
}

#[test]
fn non_tool_message_never_collapsed() {
    let msg =
        ConversationMessage::new(MessageRole::User, "a very long message\n".repeat(100), None);
    assert!(!msg.collapsed);
}

#[test]
fn extension_ui_select_accepts_string_options() {
    let request = ExtensionUiRequest::new(
        "req-1",
        "select",
        json!({
            "title": "Pick a color",
            "options": ["red", "green", "blue"],
        }),
    );

    let prompt = format_extension_ui_prompt(&request);
    assert!(prompt.contains("1) red"));
    assert!(prompt.contains("2) green"));
    assert!(prompt.contains("3) blue"));

    let response = parse_extension_ui_response(&request, "2").expect("parse selection");
    assert_eq!(response.value, Some(json!("green")));

    let response = parse_extension_ui_response(&request, "red").expect("parse selection");
    assert_eq!(response.value, Some(json!("red")));
}

#[test]
fn extension_ui_select_accepts_object_options() {
    let request = ExtensionUiRequest::new(
        "req-1",
        "select",
        json!({
            "title": "Pick",
            "options": [
                { "label": "A", "value": "alpha" },
                { "label": "B" },
            ],
        }),
    );

    let response = parse_extension_ui_response(&request, "1").expect("parse selection");
    assert_eq!(response.value, Some(json!("alpha")));

    let response = parse_extension_ui_response(&request, "B").expect("parse selection");
    assert_eq!(response.value, Some(json!("B")));
}

#[cfg(all(feature = "clipboard", feature = "image-resize"))]
#[test]
fn paste_image_from_clipboard_writes_temp_png() {
    use arboard::ImageData;
    use std::borrow::Cow;

    let Ok(mut clipboard) = ArboardClipboard::new() else {
        return;
    };

    let image = ImageData {
        width: 1,
        height: 1,
        bytes: Cow::Owned(vec![255, 0, 0, 255]),
    };

    if clipboard.set_image(image).is_err() {
        return;
    }

    let Some(path) = PiApp::paste_image_from_clipboard() else {
        return;
    };

    assert!(path.exists());
    assert_eq!(path.extension().and_then(|s| s.to_str()), Some("png"));
}

// --- extension_commands_for_catalog tests ---

#[test]
fn ext_commands_catalog_builds_entries() {
    let manager = crate::extensions::ExtensionManager::new();
    manager.register(crate::extensions::RegisterPayload {
        name: "test-ext".to_string(),
        version: "1.0.0".to_string(),
        api_version: crate::extensions::PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: vec![
            json!({"name": "deploy", "description": "Deploy the app"}),
            json!({"name": "rollback"}),
        ],
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });

    let entries = extension_commands_for_catalog(&manager);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "deploy");
    assert_eq!(entries[0].description.as_deref(), Some("Deploy the app"));
    assert_eq!(entries[1].name, "rollback");
    assert!(entries[1].description.is_none());
}

#[test]
fn ext_commands_catalog_empty_manager() {
    let manager = crate::extensions::ExtensionManager::new();
    let entries = extension_commands_for_catalog(&manager);
    assert!(entries.is_empty());
}

// --- truncate tests ---

#[test]
fn truncate_short_string() {
    assert_eq!(truncate("hi", 10), "hi");
}

#[test]
fn truncate_exact_fit() {
    assert_eq!(truncate("hello", 5), "hello");
}

#[test]
fn truncate_adds_ellipsis() {
    assert_eq!(truncate("hello world!", 8), "hello...");
}

#[test]
fn truncate_zero() {
    assert_eq!(truncate("anything", 0), "");
}

#[test]
fn truncate_very_small_max() {
    assert_eq!(truncate("hello", 1), ".");
    assert_eq!(truncate("hello", 2), "..");
    assert_eq!(truncate("hello", 3), "...");
}

// --- strip_thinking_level_suffix tests ---

#[test]
fn strip_thinking_suffix_present() {
    assert_eq!(
        strip_thinking_level_suffix("claude-opus:high"),
        "claude-opus"
    );
    assert_eq!(strip_thinking_level_suffix("model:off"), "model");
    assert_eq!(strip_thinking_level_suffix("m:xhigh"), "m");
}

#[test]
fn strip_thinking_suffix_absent() {
    assert_eq!(strip_thinking_level_suffix("claude-opus"), "claude-opus");
}

#[test]
fn strip_thinking_suffix_unknown_level() {
    assert_eq!(strip_thinking_level_suffix("claude:turbo"), "claude:turbo");
}

// --- parse_scoped_model_patterns tests ---

#[test]
fn parse_model_patterns_comma_separated() {
    assert_eq!(
        parse_scoped_model_patterns("gpt-4*,claude*"),
        vec!["gpt-4*", "claude*"]
    );
}

#[test]
fn parse_model_patterns_space_separated() {
    assert_eq!(
        parse_scoped_model_patterns("gpt-4o claude-opus"),
        vec!["gpt-4o", "claude-opus"]
    );
}

#[test]
fn parse_model_patterns_mixed() {
    assert_eq!(parse_scoped_model_patterns("a, b c"), vec!["a", "b", "c"]);
}

#[test]
fn parse_model_patterns_empty() {
    assert!(parse_scoped_model_patterns("").is_empty());
    assert!(parse_scoped_model_patterns("  ").is_empty());
}

// --- queued_message_preview tests ---

#[test]
fn queued_preview_short() {
    assert_eq!(queued_message_preview("hello", 10), "hello");
}

#[test]
fn queued_preview_truncated() {
    assert_eq!(queued_message_preview("hello world!", 8), "hello...");
}

#[test]
fn queued_preview_multiline() {
    assert_eq!(queued_message_preview("\n\nhello\nworld", 20), "hello");
}

#[test]
fn queued_preview_empty() {
    assert_eq!(queued_message_preview("", 10), "(empty)");
    assert_eq!(queued_message_preview("  \n  \n  ", 10), "(empty)");
}

// --- parse_gist_url_and_id tests ---

#[test]
fn parse_gist_url_valid() {
    let output = "Created gist https://gist.github.com/user/abc123def456";
    let result = parse_gist_url_and_id(output);
    assert_eq!(
        result,
        Some((
            "https://gist.github.com/user/abc123def456".to_string(),
            "abc123def456".to_string()
        ))
    );
}

#[test]
fn parse_gist_url_no_gist() {
    assert!(parse_gist_url_and_id("no url here").is_none());
}

#[test]
fn parse_gist_url_wrong_host() {
    assert!(parse_gist_url_and_id("https://github.com/user/repo").is_none());
}

#[test]
fn parse_gist_url_with_quotes_and_trailing_punctuation() {
    let output = "Created gist: 'https://gist.github.com/testuser/abc123def456', done.";
    let result = parse_gist_url_and_id(output);
    assert_eq!(
        result,
        Some((
            "https://gist.github.com/testuser/abc123def456".to_string(),
            "abc123def456".to_string()
        ))
    );
}

// --- share command helpers tests ---

#[test]
fn share_parse_public_flag() {
    assert!(parse_share_is_public("public"));
    assert!(parse_share_is_public("PUBLIC"));
    assert!(parse_share_is_public("  Public  "));
    assert!(!parse_share_is_public(""));
    assert!(!parse_share_is_public("private"));
    assert!(!parse_share_is_public("something else"));
}

#[test]
fn share_gist_description_with_session_name() {
    let desc = share_gist_description(Some("my-project-debug"));
    assert_eq!(desc, "Pi session: my-project-debug");
}

#[test]
fn share_gist_description_without_session_name() {
    let desc = share_gist_description(None);
    assert!(desc.starts_with("Pi session 20"));
    assert!(desc.contains('T'));
    assert!(desc.ends_with('Z'));
}

// --- parse_queue_mode tests ---

#[test]
fn parse_queue_mode_all() {
    assert!(matches!(
        parse_queue_mode_or_default(Some("all")),
        QueueMode::All
    ));
}

#[test]
fn parse_queue_mode_default() {
    assert!(matches!(
        parse_queue_mode_or_default(None),
        QueueMode::OneAtATime
    ));
    assert!(matches!(
        parse_queue_mode_or_default(Some("anything")),
        QueueMode::OneAtATime
    ));
}

#[test]
fn apply_queue_modes_updates_injected_queue_delivery_policy() {
    let app = build_test_app_with_config(Config {
        steering_mode: Some("one-at-a-time".to_string()),
        follow_up_mode: Some("one-at-a-time".to_string()),
        ..Config::default()
    });
    app.apply_queue_modes(QueueMode::All, QueueMode::All);

    let queued_follow_ups = {
        let mut queue = app.injected_queue.lock().expect("lock injected queue");
        queue.push_follow_up(custom_test_message("first"));
        queue.push_follow_up(custom_test_message("second"));
        queue.pop_follow_up().len()
    };

    assert_eq!(
        queued_follow_ups, 2,
        "updated queue modes should apply to extension-injected messages too"
    );
}

#[test]
fn default_permissive_status_mentions_restart_when_extensions_are_active() {
    let app = build_test_app_with_config_and_extensions(
        Config::default(),
        Some(test_extension_manager_with_registered_extension()),
    );

    let status = app.default_permissive_update_status(false);
    assert!(status.contains("restart active extensions/session to apply"));
}

#[test]
fn settings_summary_notes_restart_requirement_for_future_policy_changes() {
    let app = build_test_app_with_config_and_extensions(
        Config::default(),
        Some(test_extension_manager_with_registered_extension()),
    );

    let summary = app.format_settings_summary();
    assert!(summary.contains(
        "extensionPolicy.defaultPermissive: on (future changes apply after extension restart)"
    ));
}

#[test]
fn default_permissive_status_omits_restart_when_manager_has_no_loaded_extensions() {
    let app =
        build_test_app_with_config_and_extensions(Config::default(), Some(ExtensionManager::new()));

    let status = app.default_permissive_update_status(false);
    assert!(!status.contains("restart active extensions/session to apply"));
}

#[test]
fn settings_summary_omits_restart_requirement_when_manager_has_no_loaded_extensions() {
    let app =
        build_test_app_with_config_and_extensions(Config::default(), Some(ExtensionManager::new()));

    let summary = app.format_settings_summary();
    assert!(summary.contains("extensionPolicy.defaultPermissive: on"));
    assert!(!summary.contains("future changes apply after extension restart"));
}

#[test]
fn settings_overlay_default_permissive_notes_restart_only_for_loaded_extensions() {
    let app = build_test_app_with_config_and_extensions(
        Config::default(),
        Some(test_extension_manager_with_registered_extension()),
    );
    let rendered = app.render_settings_ui(&SettingsUiState::new());
    assert!(rendered.contains("extensionPolicy.defaultPermissive: on (restart required)"));

    let app_without_loaded_extensions =
        build_test_app_with_config_and_extensions(Config::default(), Some(ExtensionManager::new()));
    let rendered_without_loaded_extensions =
        app_without_loaded_extensions.render_settings_ui(&SettingsUiState::new());
    assert!(rendered_without_loaded_extensions.contains("extensionPolicy.defaultPermissive: on"));
    assert!(!rendered_without_loaded_extensions.contains("restart required"));
}

// --- push_line tests ---

#[test]
fn push_line_to_empty() {
    let mut s = String::new();
    push_line(&mut s, "hello");
    assert_eq!(s, "hello");
}

#[test]
fn push_line_appends_with_newline() {
    let mut s = "hello".to_string();
    push_line(&mut s, "world");
    assert_eq!(s, "hello\nworld");
}

#[test]
fn push_line_skips_empty() {
    let mut s = "hello".to_string();
    push_line(&mut s, "");
    assert_eq!(s, "hello");
}

// --- parse_bash_command additional edge cases ---

// --- pretty_json tests ---

#[test]
fn pretty_json_formats_object() {
    let val = json!({"a": 1});
    let out = pretty_json(&val);
    assert!(out.contains("\"a\": 1"));
    assert!(out.contains('\n'));
}

#[test]
fn pretty_json_formats_null() {
    assert_eq!(pretty_json(&json!(null)), "null");
}

// --- SlashCommand::parse tests ---

#[test]
fn slash_command_parse_known_commands() {
    assert!(matches!(
        SlashCommand::parse("/help"),
        Some((SlashCommand::Help, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/h"),
        Some((SlashCommand::Help, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/?"),
        Some((SlashCommand::Help, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/exit"),
        Some((SlashCommand::Exit, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/quit"),
        Some((SlashCommand::Exit, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/q"),
        Some((SlashCommand::Exit, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/clear"),
        Some((SlashCommand::Clear, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/cls"),
        Some((SlashCommand::Clear, ""))
    ));
}

#[test]
fn slash_command_parse_with_args() {
    let (cmd, args) = SlashCommand::parse("/model claude-opus").unwrap();
    assert!(matches!(cmd, SlashCommand::Model));
    assert_eq!(args, "claude-opus");

    let (cmd, args) = SlashCommand::parse("/name my session").unwrap();
    assert!(matches!(cmd, SlashCommand::Name));
    assert_eq!(args, "my session");
}

#[test]
fn slash_command_parse_case_insensitive() {
    assert!(SlashCommand::parse("/HELP").is_some());
    assert!(SlashCommand::parse("/Model").is_some());
    assert!(SlashCommand::parse("/EXIT").is_some());
}

#[test]
fn slash_command_parse_unknown() {
    assert!(SlashCommand::parse("/deploy").is_none());
    assert!(SlashCommand::parse("/unknown").is_none());
}

#[test]
fn slash_command_parse_no_slash() {
    assert!(SlashCommand::parse("help").is_none());
    assert!(SlashCommand::parse("model gpt-4").is_none());
}

#[test]
fn slash_command_parse_aliases() {
    assert!(matches!(
        SlashCommand::parse("/m"),
        Some((SlashCommand::Model, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/t"),
        Some((SlashCommand::Thinking, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/think"),
        Some((SlashCommand::Thinking, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/r"),
        Some((SlashCommand::Resume, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/cp"),
        Some((SlashCommand::Copy, ""))
    ));
    assert!(matches!(
        SlashCommand::parse("/info"),
        Some((SlashCommand::Session, ""))
    ));
}

// --- format_tool_output tests ---

#[test]
fn format_tool_output_text_only() {
    let blocks = vec![ContentBlock::Text(TextContent::new("tool result"))];
    let result = format_tool_output(&blocks, None, false);
    assert_eq!(result.as_deref(), Some("tool result"));
}

#[test]
fn format_tool_output_with_diff_details() {
    let blocks = vec![ContentBlock::Text(TextContent::new(
        "Successfully replaced text in foo.rs.",
    ))];
    let details = json!({ "diff": "- 1 old\n+ 1 new" });
    let result = format_tool_output(&blocks, Some(&details), false).unwrap();
    assert!(result.contains("Diff:"));
    assert!(result.contains("- 1 old"));
    assert!(result.contains("+ 1 new"));
}

#[test]
fn format_tool_output_empty_returns_none() {
    let blocks: Vec<ContentBlock> = vec![];
    assert!(format_tool_output(&blocks, None, false).is_none());
}

#[test]
fn format_tool_output_empty_text_with_details_shows_json() {
    let blocks: Vec<ContentBlock> = vec![];
    let details = json!({"key": "value"});
    let result = format_tool_output(&blocks, Some(&details), false).unwrap();
    assert!(result.contains("key"));
    assert!(result.contains("value"));
}

#[test]
fn format_tool_output_empty_diff_in_details() {
    let blocks = vec![ContentBlock::Text(TextContent::new("Success"))];
    let details = json!({ "diff": "  " }); // whitespace-only diff
    let result = format_tool_output(&blocks, Some(&details), false).unwrap();
    // Should NOT contain Diff: header since diff is effectively empty
    assert!(!result.contains("Diff:"));
    assert!(result.contains("Success"));
}

// --- assistant_content_to_text tests ---

#[test]
fn assistant_text_only() {
    let blocks = vec![ContentBlock::Text(TextContent::new("Hello"))];
    let (text, thinking) = assistant_content_to_text(&blocks);
    assert_eq!(text, "Hello");
    assert!(thinking.is_none());
}

#[test]
fn assistant_text_with_thinking() {
    let blocks = vec![
        ContentBlock::Thinking(crate::model::ThinkingContent {
            thinking: "Let me reason...".to_string(),
            thinking_signature: None,
        }),
        ContentBlock::Text(TextContent::new("response")),
    ];
    let (text, thinking) = assistant_content_to_text(&blocks);
    assert_eq!(text, "response");
    assert_eq!(thinking.as_deref(), Some("Let me reason..."));
}

#[test]
fn assistant_empty_thinking_is_none() {
    let blocks = vec![
        ContentBlock::Thinking(crate::model::ThinkingContent {
            thinking: "  ".to_string(),
            thinking_signature: None,
        }),
        ContentBlock::Text(TextContent::new("response")),
    ];
    let (_, thinking) = assistant_content_to_text(&blocks);
    assert!(
        thinking.is_none(),
        "whitespace-only thinking should be None"
    );
}

// --- ConversationMessage tests ---

#[test]
fn conversation_message_tool_role() {
    let msg = ConversationMessage::tool("Tool read:\nfile contents".to_string());
    assert_eq!(msg.role, MessageRole::Tool);
    assert!(msg.content.contains("file contents"));
}

#[test]
fn conversation_message_new_user_not_collapsed() {
    let msg = ConversationMessage::new(MessageRole::User, "question".to_string(), None);
    assert_eq!(msg.role, MessageRole::User);
    assert!(!msg.collapsed);
}

#[test]
fn conversation_message_with_thinking() {
    let msg = ConversationMessage::new(
        MessageRole::Assistant,
        "response".to_string(),
        Some("I'm thinking...".to_string()),
    );
    assert_eq!(msg.thinking.as_deref(), Some("I'm thinking..."));
}

// --- extension UI prompt/response ---

#[test]
fn extension_ui_confirm_prompt_format() {
    let request = ExtensionUiRequest::new("req-1", "confirm", json!({ "title": "Proceed?" }));
    let prompt = format_extension_ui_prompt(&request);
    assert!(prompt.contains("Proceed?"));
}

#[test]
fn extension_ui_confirm_yes() {
    let request = ExtensionUiRequest::new("req-1", "confirm", json!({ "title": "Proceed?" }));
    let response = parse_extension_ui_response(&request, "yes").unwrap();
    assert_eq!(response.value, Some(json!(true)));
}

#[test]
fn extension_ui_confirm_no() {
    let request = ExtensionUiRequest::new("req-1", "confirm", json!({ "title": "Proceed?" }));
    let response = parse_extension_ui_response(&request, "no").unwrap();
    assert_eq!(response.value, Some(json!(false)));
}

#[test]
fn extension_ui_input_response() {
    let request = ExtensionUiRequest::new("req-1", "input", json!({ "title": "Enter name:" }));
    let response = parse_extension_ui_response(&request, "Alice").unwrap();
    assert_eq!(response.value, Some(json!("Alice")));
}

#[test]
fn extension_ui_select_by_label_text() {
    let request = ExtensionUiRequest::new(
        "req-1",
        "select",
        json!({
            "title": "Pick",
            "options": ["alpha", "beta", "gamma"],
        }),
    );
    let response = parse_extension_ui_response(&request, "beta").unwrap();
    assert_eq!(response.value, Some(json!("beta")));
}

// --- tool_content_blocks_to_text tests ---

#[test]
fn tool_content_blocks_text_only() {
    let blocks = vec![ContentBlock::Text(TextContent::new("hello world"))];
    let result = tool_content_blocks_to_text(&blocks, false);
    assert_eq!(result, "hello world");
}

#[test]
fn tool_content_blocks_multiple_text() {
    let blocks = vec![
        ContentBlock::Text(TextContent::new("line 1")),
        ContentBlock::Text(TextContent::new("line 2")),
    ];
    let result = tool_content_blocks_to_text(&blocks, false);
    assert!(result.contains("line 1"));
    assert!(result.contains("line 2"));
}

#[test]
fn tool_content_blocks_images_hidden() {
    let blocks = vec![
        ContentBlock::Text(TextContent::new("text")),
        ContentBlock::Image(crate::model::ImageContent {
            data: String::new(),
            mime_type: "image/png".to_string(),
        }),
        ContentBlock::Image(crate::model::ImageContent {
            data: String::new(),
            mime_type: "image/png".to_string(),
        }),
    ];
    let result = tool_content_blocks_to_text(&blocks, false);
    assert!(result.contains("text"));
    assert!(result.contains("[2 image(s) hidden]"));
}

#[test]
fn tool_content_blocks_thinking() {
    let blocks = vec![ContentBlock::Thinking(crate::model::ThinkingContent {
        thinking: "reasoning here".to_string(),
        thinking_signature: None,
    })];
    let result = tool_content_blocks_to_text(&blocks, false);
    assert_eq!(result, "reasoning here");
}

#[test]
fn tool_content_blocks_tool_call() {
    let blocks = vec![ContentBlock::ToolCall(crate::model::ToolCall {
        id: "tc-1".to_string(),
        name: "bash".to_string(),
        arguments: json!({"command": "ls"}),
        thought_signature: None,
    })];
    let result = tool_content_blocks_to_text(&blocks, false);
    assert!(result.contains("[tool call: bash]"));
}

#[test]
fn tool_content_blocks_empty() {
    let result = tool_content_blocks_to_text(&[], false);
    assert!(result.is_empty());
}

// --- format_resource_diagnostics tests ---

#[test]
fn format_resource_diagnostics_single_warning() {
    let diags = vec![crate::resources::ResourceDiagnostic {
        kind: crate::resources::DiagnosticKind::Warning,
        message: "File too large".to_string(),
        path: PathBuf::from("/tmp/skills/big.md"),
        collision: None,
    }];
    let (text, count) = format_resource_diagnostics("Skills", &diags);
    assert_eq!(count, 1);
    assert!(text.contains("Skills:"));
    assert!(text.contains("warning: File too large"));
    assert!(text.contains("/tmp/skills/big.md"));
}

#[test]
fn format_resource_diagnostics_collision() {
    let diags = vec![crate::resources::ResourceDiagnostic {
        kind: crate::resources::DiagnosticKind::Collision,
        message: "Duplicate skill name".to_string(),
        path: PathBuf::from("/a/skill.md"),
        collision: Some(crate::resources::CollisionInfo {
            resource_type: "skill".to_string(),
            name: "deploy".to_string(),
            winner_path: PathBuf::from("/a/skill.md"),
            loser_path: PathBuf::from("/b/skill.md"),
        }),
    }];
    let (text, count) = format_resource_diagnostics("Skills", &diags);
    assert_eq!(count, 1);
    assert!(text.contains("collision:"));
    assert!(text.contains("[winner: /a/skill.md loser: /b/skill.md]"));
}

#[test]
fn format_resource_diagnostics_sorts_by_path_then_kind() {
    let diags = vec![
        crate::resources::ResourceDiagnostic {
            kind: crate::resources::DiagnosticKind::Collision,
            message: "z-message".to_string(),
            path: PathBuf::from("/a"),
            collision: None,
        },
        crate::resources::ResourceDiagnostic {
            kind: crate::resources::DiagnosticKind::Warning,
            message: "a-message".to_string(),
            path: PathBuf::from("/a"),
            collision: None,
        },
        crate::resources::ResourceDiagnostic {
            kind: crate::resources::DiagnosticKind::Warning,
            message: "b-message".to_string(),
            path: PathBuf::from("/b"),
            collision: None,
        },
    ];
    let (text, count) = format_resource_diagnostics("Test", &diags);
    assert_eq!(count, 3);
    // Within /a: warning (rank 0) comes before collision (rank 1)
    let warn_pos = text.find("a-message").unwrap();
    let coll_pos = text.find("z-message").unwrap();
    assert!(
        warn_pos < coll_pos,
        "Warning should appear before collision for same path"
    );
}

#[test]
fn format_resource_diagnostics_empty() {
    let (text, count) = format_resource_diagnostics("Skills", &[]);
    assert_eq!(count, 0);
    assert!(text.contains("Skills:"));
}

// --- kind_rank tests ---

#[test]
fn kind_rank_ordering() {
    assert!(
        kind_rank(&crate::resources::DiagnosticKind::Warning)
            < kind_rank(&crate::resources::DiagnosticKind::Collision)
    );
}

// --- user_content_to_text tests ---

#[test]
fn user_content_text_variant() {
    let content = UserContent::Text("hello".to_string());
    assert_eq!(user_content_to_text(&content), "hello");
}

#[test]
fn user_content_blocks_variant() {
    let content = UserContent::Blocks(vec![
        ContentBlock::Text(TextContent::new("first")),
        ContentBlock::Text(TextContent::new("second")),
    ]);
    let result = user_content_to_text(&content);
    assert!(result.contains("first"));
    assert!(result.contains("second"));
}

// --- content_blocks_to_text tests ---

#[test]
fn content_blocks_to_text_mixed() {
    let blocks = vec![
        ContentBlock::Text(TextContent::new("text")),
        ContentBlock::Thinking(crate::model::ThinkingContent {
            thinking: "think".to_string(),
            thinking_signature: None,
        }),
        ContentBlock::ToolCall(crate::model::ToolCall {
            id: "tc-1".to_string(),
            name: "read".to_string(),
            arguments: json!({}),
            thought_signature: None,
        }),
    ];
    let result = content_blocks_to_text(&blocks);
    assert!(result.contains("text"));
    assert!(result.contains("think"));
    assert!(result.contains("[tool call: read]"));
}

// --- split_content_blocks_for_input tests ---

#[test]
fn split_content_blocks_text_and_images() {
    let blocks = vec![
        ContentBlock::Text(TextContent::new("hello")),
        ContentBlock::Image(crate::model::ImageContent {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        }),
        ContentBlock::Thinking(crate::model::ThinkingContent {
            thinking: "ignored".to_string(),
            thinking_signature: None,
        }),
    ];
    let (text, images) = split_content_blocks_for_input(&blocks);
    assert_eq!(text, "hello");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].data, "base64data");
}

#[test]
fn split_content_blocks_empty() {
    let (text, images) = split_content_blocks_for_input(&[]);
    assert!(text.is_empty());
    assert!(images.is_empty());
}

// --- build_content_blocks_for_input tests ---

#[test]
fn build_content_blocks_text_and_images() {
    let img = crate::model::ImageContent {
        data: "d".to_string(),
        mime_type: "image/png".to_string(),
    };
    let blocks = build_content_blocks_for_input("hello", &[img]);
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], ContentBlock::Text(t) if t.text == "hello"));
    assert!(matches!(&blocks[1], ContentBlock::Image(_)));
}

#[test]
fn build_content_blocks_empty_text_skipped() {
    let blocks = build_content_blocks_for_input("  ", &[]);
    assert!(blocks.is_empty());
}

#[test]
fn normalize_api_key_input_trims_outer_whitespace() {
    let parsed = normalize_api_key_input("  sk-test-123  ").expect("should parse");
    assert_eq!(parsed, "sk-test-123");
}

#[test]
fn normalize_api_key_input_rejects_empty() {
    let err = normalize_api_key_input("   ").expect_err("should fail");
    assert!(err.contains("cannot be empty"));
}

#[test]
fn normalize_api_key_input_rejects_internal_whitespace() {
    let err = normalize_api_key_input("sk test").expect_err("should fail");
    assert!(err.contains("must not contain whitespace"));
}

#[test]
fn normalize_auth_provider_input_maps_gemini_alias() {
    assert_eq!(normalize_auth_provider_input("gemini"), "google");
    assert_eq!(normalize_auth_provider_input(" GOOGLE "), "google");
}

#[test]
fn api_key_login_prompt_supports_openai_and_google() {
    let openai_prompt = api_key_login_prompt("openai").expect("openai prompt");
    assert!(openai_prompt.contains("platform.openai.com/api-keys"));
    let google_prompt = api_key_login_prompt("google").expect("google prompt");
    assert!(google_prompt.contains("google/gemini"));
}

#[test]
fn slash_help_mentions_generic_login_flow() {
    let help = SlashCommand::help_text();
    assert!(help.contains(
        "/login [provider]  - Login/setup credentials; without provider shows status table"
    ));
    assert!(help.contains("/logout [provider] - Remove stored credentials"));
}

#[test]
fn format_login_provider_listing_includes_builtin_and_extension_status() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let auth_path = dir.path().join("auth.json");
    let mut auth = crate::auth::AuthStorage::load(auth_path).expect("load auth");

    auth.set(
        "anthropic",
        crate::auth::AuthCredential::OAuth {
            access_token: "anthropic-access".to_string(),
            refresh_token: "anthropic-refresh".to_string(),
            expires: chrono::Utc::now().timestamp_millis() + 3_600_000,
            token_url: None,
            client_id: None,
        },
    );
    auth.set(
        "google",
        crate::auth::AuthCredential::ApiKey {
            key: ApiKeyValue::Raw("google-api-key".to_string()),
        },
    );
    auth.set(
        "my-ext",
        crate::auth::AuthCredential::OAuth {
            access_token: "ext-access".to_string(),
            refresh_token: "ext-refresh".to_string(),
            expires: chrono::Utc::now().timestamp_millis() - 60_000,
            token_url: None,
            client_id: None,
        },
    );

    let mut ext_entry = test_model_entry("my-ext", "model-1");
    ext_entry.oauth_config = Some(crate::models::OAuthConfig {
        auth_url: "https://auth.example.invalid/oauth/authorize".to_string(),
        token_url: "https://auth.example.invalid/oauth/token".to_string(),
        client_id: "ext-client".to_string(),
        scopes: vec!["scope.read".to_string()],
        redirect_uri: None,
    });
    let available_models = vec![test_model_entry("openai", "gpt-4o"), ext_entry];

    let listing = format_login_provider_listing(&auth, &available_models);
    assert!(listing.contains("Available login providers:"));
    assert!(listing.contains("Built-in:"));
    assert!(listing.contains("anthropic"));
    assert!(listing.contains("openai"));
    assert!(listing.contains("google"));
    assert!(listing.contains("Extension providers:"));
    assert!(listing.contains("my-ext"));
    assert!(listing.contains("Authenticated (expires in"));
    assert!(listing.contains("Authenticated (expired"));
    assert!(listing.contains("Usage: /login <provider>"));
}

#[test]
fn save_provider_credential_persists_google_under_canonical_key() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let auth_path = dir.path().join("auth.json");
    let mut auth = crate::auth::AuthStorage::load(auth_path.clone()).expect("load auth");

    save_provider_credential(
        &mut auth,
        "gemini",
        crate::auth::AuthCredential::ApiKey {
            key: ApiKeyValue::Raw("gemini-test-key".to_string()),
        },
    );
    auth.save().expect("save credential");

    let loaded = crate::auth::AuthStorage::load(auth_path).expect("reload auth");
    assert_eq!(loaded.api_key("google").as_deref(), Some("gemini-test-key"));
    assert!(loaded.get("gemini").is_none());
}

#[test]
fn remove_provider_credentials_clears_google_and_gemini_aliases() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let auth_path = dir.path().join("auth.json");
    let mut auth = crate::auth::AuthStorage::load(auth_path.clone()).expect("load auth");
    auth.set(
        "google",
        crate::auth::AuthCredential::ApiKey {
            key: ApiKeyValue::Raw("google-key".to_string()),
        },
    );
    auth.set(
        "gemini",
        crate::auth::AuthCredential::ApiKey {
            key: ApiKeyValue::Raw("legacy-gemini-key".to_string()),
        },
    );
    auth.save().expect("seed auth");

    let mut auth = crate::auth::AuthStorage::load(auth_path.clone()).expect("reload auth");
    assert!(remove_provider_credentials(&mut auth, "gemini"));
    auth.save().expect("persist removals");

    let loaded = crate::auth::AuthStorage::load(auth_path).expect("reload post-remove");
    assert!(loaded.get("google").is_none());
    assert!(loaded.get("gemini").is_none());
}

// --- SlashCommand::parse additional coverage ---

#[test]
fn slash_command_all_variants_parse() {
    // Verify all main slash commands parse correctly
    let cases = vec![
        ("/login", SlashCommand::Login),
        ("/logout", SlashCommand::Logout),
        ("/settings", SlashCommand::Settings),
        ("/history", SlashCommand::History),
        ("/export", SlashCommand::Export),
        ("/session", SlashCommand::Session),
        ("/theme", SlashCommand::Theme),
        ("/resume", SlashCommand::Resume),
        ("/new", SlashCommand::New),
        ("/copy", SlashCommand::Copy),
        ("/name", SlashCommand::Name),
        ("/hotkeys", SlashCommand::Hotkeys),
        ("/changelog", SlashCommand::Changelog),
        ("/tree", SlashCommand::Tree),
        ("/fork", SlashCommand::Fork),
        ("/compact", SlashCommand::Compact),
        ("/reload", SlashCommand::Reload),
        ("/share", SlashCommand::Share),
    ];
    for (input, expected) in cases {
        let result = SlashCommand::parse(input);
        assert!(
            result.is_some(),
            "Expected {input} to parse as a SlashCommand"
        );
        let (cmd, _) = result.unwrap();
        assert_eq!(
            std::mem::discriminant(&cmd),
            std::mem::discriminant(&expected),
            "Mismatch for input {input}"
        );
    }
}

#[test]
fn slash_command_empty_and_whitespace() {
    assert!(SlashCommand::parse("").is_none());
    assert!(SlashCommand::parse("  ").is_none());
    assert!(SlashCommand::parse("/").is_none());
}

// --- ConversationMessage collapse boundary ---

#[test]
fn tool_collapse_single_line() {
    let msg = ConversationMessage::tool("one line".to_string());
    assert!(!msg.collapsed);
}

#[test]
fn tool_collapse_exactly_threshold_plus_one() {
    // TOOL_AUTO_COLLAPSE_THRESHOLD + 1 lines
    let content = (1..=TOOL_AUTO_COLLAPSE_THRESHOLD + 1)
        .map(|i| format!("L{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let msg = ConversationMessage::tool(content);
    assert!(msg.collapsed);
}

// --- resolve_scoped_model_entries tests ---

fn test_model_entry(provider: &str, id: &str) -> ModelEntry {
    ModelEntry {
        model: crate::provider::Model {
            id: id.to_string(),
            name: id.to_string(),
            api: "test".to_string(),
            provider: provider.to_string(),
            base_url: "https://example.invalid".to_string(),
            reasoning: false,
            input: vec![crate::provider::InputType::Text],
            cost: crate::provider::ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 4096,
            max_tokens: 1024,
            headers: std::collections::HashMap::new(),
        },
        api_key: None,
        headers: std::collections::HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn resolved_ids(entries: &[ModelEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| format!("{}/{}", e.model.provider, e.model.id))
        .collect()
}

fn make_test_models() -> Vec<ModelEntry> {
    vec![
        test_model_entry("openai", "gpt-4o"),
        test_model_entry("openai", "gpt-4o-mini"),
        test_model_entry("openai", "o1"),
        test_model_entry("anthropic", "claude-sonnet-4"),
        test_model_entry("google", "gemini-pro"),
    ]
}

#[test]
fn resolve_scoped_exact_match_by_id() {
    let models = vec![
        test_model_entry("anthropic", "claude-sonnet-4"),
        test_model_entry("openai", "gpt-4o"),
    ];
    let patterns = vec!["gpt-4o".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(resolved_ids(&result), vec!["openai/gpt-4o"]);
}

#[test]
fn resolve_scoped_exact_match_by_full_id() {
    let models = vec![
        test_model_entry("anthropic", "claude-sonnet-4"),
        test_model_entry("openai", "gpt-4o"),
    ];
    let patterns = vec!["anthropic/claude-sonnet-4".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(resolved_ids(&result), vec!["anthropic/claude-sonnet-4"]);
}

#[test]
fn resolve_scoped_glob_wildcard() {
    let models = vec![
        test_model_entry("openai", "gpt-4o"),
        test_model_entry("openai", "gpt-4o-mini"),
        test_model_entry("anthropic", "claude-sonnet-4"),
    ];
    let patterns = vec!["gpt-4*".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(
        resolved_ids(&result),
        vec!["openai/gpt-4o", "openai/gpt-4o-mini"]
    );
}

#[test]
fn resolve_scoped_glob_provider_slash() {
    let models = vec![
        test_model_entry("openai", "gpt-4o"),
        test_model_entry("openai", "o1"),
        test_model_entry("anthropic", "claude-sonnet-4"),
    ];
    let patterns = vec!["openai/*".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(resolved_ids(&result), vec!["openai/gpt-4o", "openai/o1"]);
}

#[test]
fn resolve_scoped_case_insensitive() {
    let models = vec![test_model_entry("OpenAI", "GPT-4o")];
    let patterns = vec!["gpt-4o".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].model.id, "GPT-4o");
}

#[test]
fn resolve_scoped_deduplicates() {
    let models = vec![
        test_model_entry("openai", "gpt-4o"),
        test_model_entry("anthropic", "claude-sonnet-4"),
    ];
    // Both patterns match gpt-4o, but it should appear only once.
    let patterns = vec!["gpt-4o".to_string(), "openai/*".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(resolved_ids(&result), vec!["openai/gpt-4o"]);
}

#[test]
fn resolve_scoped_output_sorted() {
    let models = vec![
        test_model_entry("openai", "gpt-4o"),
        test_model_entry("anthropic", "claude-sonnet-4"),
        test_model_entry("google", "gemini-pro"),
    ];
    let patterns = vec!["*".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    let ids = resolved_ids(&result);
    assert_eq!(
        ids,
        vec![
            "anthropic/claude-sonnet-4",
            "google/gemini-pro",
            "openai/gpt-4o"
        ]
    );
}

#[test]
fn resolve_scoped_invalid_glob_returns_error() {
    let models = vec![test_model_entry("openai", "gpt-4o")];
    let patterns = vec!["[invalid".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Invalid model pattern"));
}

#[test]
fn resolve_scoped_no_match_returns_empty() {
    let models = vec![test_model_entry("openai", "gpt-4o")];
    let patterns = vec!["nonexistent-model".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert!(result.is_empty());
}

#[test]
fn resolve_scoped_thinking_suffix_stripped() {
    let models = vec![
        test_model_entry("anthropic", "claude-sonnet-4"),
        test_model_entry("openai", "gpt-4o"),
    ];
    let patterns = vec!["claude-sonnet-4:high".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(resolved_ids(&result), vec!["anthropic/claude-sonnet-4"]);
}

#[test]
fn resolve_scoped_question_mark_glob() {
    let models = vec![
        test_model_entry("openai", "o1"),
        test_model_entry("openai", "o3"),
        test_model_entry("openai", "gpt-4o"),
    ];
    let patterns = vec!["o?".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert_eq!(resolved_ids(&result), vec!["openai/o1", "openai/o3"]);
}

#[test]
fn resolve_scoped_empty_available_returns_empty() {
    let models: Vec<ModelEntry> = Vec::new();
    let patterns = vec!["*".to_string()];
    let result = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert!(result.is_empty());
}

// ========================================================================
// Scoped-models UI polish tests (TUI-2)
// ========================================================================

#[test]
fn scoped_models_invalid_glob_error_includes_pattern() {
    let models = vec![
        test_model_entry("openai", "gpt-4o"),
        test_model_entry("openai", "gpt-4o-mini"),
        test_model_entry("anthropic", "claude-sonnet-4"),
    ];
    let patterns = vec!["[invalid".to_string()];
    let err = resolve_scoped_model_entries(&patterns, &models).unwrap_err();
    assert!(
        err.contains("[invalid"),
        "Error should include the bad pattern: {err}"
    );
    assert!(
        err.contains("Invalid"),
        "Error should describe the issue: {err}"
    );
}

#[test]
fn scoped_models_glob_preview_matches_expected() {
    let models = vec![
        test_model_entry("openai", "gpt-4o"),
        test_model_entry("openai", "gpt-4o-mini"),
        test_model_entry("anthropic", "claude-sonnet-4"),
    ];
    let patterns = vec!["gpt-4*".to_string()];
    let resolved = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert!(
        !resolved.is_empty(),
        "Should match at least one gpt-4 model"
    );
    // Verify all matched models contain "gpt-4" in the id
    for entry in &resolved {
        let id_lower = entry.model.id.to_lowercase();
        assert!(
            id_lower.starts_with("gpt-4"),
            "Matched model {id_lower} should start with gpt-4"
        );
    }
}

#[test]
fn scoped_models_dedup_overlapping_patterns() {
    let models = vec![
        test_model_entry("openai", "gpt-4o"),
        test_model_entry("openai", "gpt-4o-mini"),
        test_model_entry("anthropic", "claude-sonnet-4"),
    ];
    // Two patterns that can match the same models
    let patterns = vec!["gpt-4*".to_string(), "openai/*".to_string()];
    let resolved = resolve_scoped_model_entries(&patterns, &models).unwrap();
    // Count how many times each model appears
    let mut seen = std::collections::HashSet::new();
    for entry in &resolved {
        let key = format!(
            "{}/{}",
            entry.model.provider.to_lowercase(),
            entry.model.id.to_lowercase()
        );
        assert!(
            seen.insert(key.clone()),
            "Duplicate model in resolved list: {key}"
        );
    }
}

#[test]
fn scoped_models_no_match_returns_empty() {
    let models = vec![
        test_model_entry("openai", "gpt-4o"),
        test_model_entry("openai", "gpt-4o-mini"),
        test_model_entry("anthropic", "claude-sonnet-4"),
    ];
    let patterns = vec!["nonexistent-provider-xyz*".to_string()];
    let resolved = resolve_scoped_model_entries(&patterns, &models).unwrap();
    assert!(resolved.is_empty(), "Should return empty for no matches");
}

#[test]
fn scoped_models_clear_message_format() {
    let previous_patterns = ["gpt-4*".to_string(), "claude*".to_string()];
    let cleared_msg = format!(
        "Cleared {} pattern(s) (was: {})",
        previous_patterns.len(),
        previous_patterns.join(", ")
    );
    assert!(cleared_msg.contains("gpt-4*"));
    assert!(cleared_msg.contains("claude*"));
    assert!(cleared_msg.contains("2 pattern(s)"));
}

mod render_tool_message_tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn colors_diff_only_after_header() {
        let styles = Theme::dark().tui_styles();
        let input = "+notdiff\nDiff:\n+added\n-removed\n 1 ctx";
        let rendered = render_tool_message(input, &styles);

        assert!(rendered.contains(&styles.muted.render("+notdiff")));
        assert!(rendered.contains(&styles.muted_bold.render("Diff:")));
        assert!(rendered.contains(&styles.success_bold.render("+added")));
        assert!(rendered.contains(&styles.error_bold.render("-removed")));
        assert!(rendered.contains(&styles.muted.render(" 1 ctx")));
    }

    #[test]
    fn file_path_header_extracted() {
        let styles = Theme::dark().tui_styles();
        let input = "Successfully replaced text in src/main.rs.\nDiff:\n+ 1 new line";
        let rendered = render_tool_message(input, &styles);
        assert!(
            rendered.contains(&styles.muted_bold.render("@@ src/main.rs @@")),
            "Expected @@ src/main.rs @@ header, got: {rendered}"
        );
        assert!(!rendered.contains(&styles.muted_bold.render("Diff:")));
    }

    #[test]
    fn fallback_diff_header_when_no_path() {
        let styles = Theme::dark().tui_styles();
        let input = "Some other tool output.\nDiff:\n+ 1 added";
        let rendered = render_tool_message(input, &styles);
        assert!(
            rendered.contains(&styles.muted_bold.render("Diff:")),
            "Expected fallback Diff: header, got: {rendered}"
        );
    }

    #[test]
    fn word_level_diff_for_paired_lines() {
        let styles = Theme::dark().tui_styles();
        let input =
            "Successfully replaced text in foo.rs.\nDiff:\n- 1 let x = old;\n+ 1 let x = new;";
        let rendered = render_tool_message(input, &styles);
        let underline_old = styles.error_bold.underline();
        let underline_new = styles.success_bold.underline();
        assert!(
            rendered.contains(&underline_old.render("old;")),
            "Expected underlined 'old;' in removed line, got: {rendered}"
        );
        assert!(
            rendered.contains(&underline_new.render("new;")),
            "Expected underlined 'new;' in added line, got: {rendered}"
        );
    }

    #[test]
    fn split_diff_prefix_basic() {
        assert_eq!(
            split_diff_prefix("-  3 let x = 1;"),
            ("-  3 ", "let x = 1;")
        );
        assert_eq!(split_diff_prefix("+ 12 new text"), ("+ 12 ", "new text"));
    }

    #[test]
    fn split_diff_prefix_edge_cases() {
        assert_eq!(split_diff_prefix("-"), ("-", ""));
        assert_eq!(split_diff_prefix("+  1 "), ("+  1 ", ""));
        assert_eq!(split_diff_prefix(""), ("", ""));
    }

    #[test]
    fn large_diff_truncation() {
        let styles = Theme::dark().tui_styles();
        let mut lines = vec!["Successfully replaced text in big.rs.".to_string()];
        lines.push("Diff:".to_string());
        for i in 1..=60 {
            lines.push(format!("- {i} old line {i}"));
            lines.push(format!("+ {i} new line {i}"));
        }
        let input = lines.join("\n");
        let rendered = render_tool_message(&input, &styles);
        assert!(
            rendered.contains("diff truncated"),
            "Expected truncation marker, got: {rendered}"
        );
    }

    #[test]
    fn no_diff_renders_only_muted_text() {
        let styles = Theme::dark().tui_styles();
        let input = "Tool read:\nfile contents here";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.muted.render("Tool read:")));
        assert!(rendered.contains(&styles.muted.render("file contents here")));
        assert!(!rendered.contains("Diff:"));
        assert!(!rendered.contains("@@"));
    }

    #[test]
    fn empty_input_returns_empty() {
        let styles = Theme::dark().tui_styles();
        let rendered = render_tool_message("", &styles);
        assert!(rendered.is_empty() || rendered == styles.muted.render(""));
    }

    #[test]
    fn unpaired_minus_line_no_word_diff() {
        let styles = Theme::dark().tui_styles();
        // Single - line with no following + should render in error_bold without word diff
        let input = "output\nDiff:\n- 1 removed line\n 2 context";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.error_bold.render("- 1 removed line")));
        assert!(rendered.contains(&styles.muted.render(" 2 context")));
    }

    #[test]
    fn unpaired_plus_line_renders_success() {
        let styles = Theme::dark().tui_styles();
        // Standalone + line (no preceding -) should render in success_bold
        let input = "output\nDiff:\n+ 1 added line";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.success_bold.render("+ 1 added line")));
    }

    #[test]
    fn context_only_diff_no_color() {
        let styles = Theme::dark().tui_styles();
        let input = "output\nDiff:\n 1 unchanged line\n 2 also unchanged";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.muted.render(" 1 unchanged line")));
        assert!(rendered.contains(&styles.muted.render(" 2 also unchanged")));
    }

    #[test]
    fn word_diff_fallback_when_content_empty() {
        let styles = Theme::dark().tui_styles();
        // Prefix-only lines: split_diff_prefix returns ("- 1 ", "") for "- 1 "
        // render_word_diff_pair should fall back to simple coloring
        let input = "output\nDiff:\n-\n+";
        let rendered = render_tool_message(input, &styles);
        assert!(rendered.contains(&styles.error_bold.render("-")));
        assert!(rendered.contains(&styles.success_bold.render("+")));
    }
}

// ── Git branch reading tests ──────────────────────────────────────────

#[test]
fn git_branch_normal_ref() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(super::read_git_branch(dir.path()), Some("main".to_string()));
}

#[test]
fn git_branch_feature_branch() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/add-auth\n").unwrap();
    assert_eq!(
        super::read_git_branch(dir.path()),
        Some("feature/add-auth".to_string())
    );
}

#[test]
fn git_branch_detached_head() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(
        git_dir.join("HEAD"),
        "abc1234def5678901234567890abcdef12345678\n",
    )
    .unwrap();
    assert_eq!(
        super::read_git_branch(dir.path()),
        Some("abc1234".to_string())
    );
}

#[test]
fn git_branch_not_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    // No .git directory
    assert_eq!(super::read_git_branch(dir.path()), None);
}

#[test]
fn git_branch_malformed_head() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "garbage content\n").unwrap();
    assert_eq!(super::read_git_branch(dir.path()), None);
}

#[test]
fn git_branch_empty_head() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "").unwrap();
    assert_eq!(super::read_git_branch(dir.path()), None);
}

#[test]
fn git_branch_found_from_nested_directory() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join(".git");
    let nested = dir.path().join("src/ui");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(super::read_git_branch(&nested), Some("main".to_string()));
}

#[test]
fn git_branch_worktree_gitdir_file() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("worktree/src");
    let worktree_root = dir.path().join("worktree");
    let gitdir = dir.path().join("actual-git-dir");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::write(worktree_root.join(".git"), "gitdir: ../actual-git-dir\n").unwrap();
    std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature/worktree\n").unwrap();
    assert_eq!(
        super::read_git_branch(&nested),
        Some("feature/worktree".to_string())
    );
}
