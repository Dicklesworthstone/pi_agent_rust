#![allow(clippy::too_many_lines)]
#![allow(clippy::similar_names)]

mod common;

use base64::Engine as _;
use clap::Parser;
use common::TestHarness;
use pi::app::{
    apply_piped_stdin, build_initial_content, build_system_prompt, normalize_cli,
    prepare_initial_message, resolve_api_key, resolve_model_scope, select_model_and_thinking,
    validate_rpc_args,
};
use pi::auth::{AuthCredential, AuthStorage};
use pi::cli;
use pi::config::Config;
use pi::model::{ContentBlock, ThinkingLevel};
use pi::models::{ModelEntry, ModelRegistry};
use pi::provider::{InputType, Model, ModelCost};
use pi::session::{EntryBase, ModelChangeEntry, Session, SessionEntry, ThinkingLevelChangeEntry};
use pi::tools::process_file_arguments;
use std::collections::HashMap;

fn make_registry(harness: &TestHarness, creds: &[(&str, &str)]) -> ModelRegistry {
    let auth_path = harness.temp_path("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("load auth storage");
    for (provider, key) in creds {
        auth.set(
            (*provider).to_string(),
            AuthCredential::ApiKey {
                key: (*key).to_string(),
            },
        );
    }
    ModelRegistry::load(&auth, None)
}

fn make_session_with_last_model(provider: &str, model_id: &str) -> Session {
    let mut session = Session::in_memory();
    session
        .entries
        .push(SessionEntry::ModelChange(ModelChangeEntry {
            base: EntryBase {
                id: Some("model".to_string()),
                parent_id: None,
                timestamp: "2026-02-03T00:00:00.000Z".to_string(),
            },
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        }));
    session
}

fn make_session_with_last_thinking(level: &str) -> Session {
    let mut session = Session::in_memory();
    session.entries.push(SessionEntry::ThinkingLevelChange(
        ThinkingLevelChangeEntry {
            base: EntryBase {
                id: Some("thinking".to_string()),
                parent_id: None,
                timestamp: "2026-02-03T00:00:00.000Z".to_string(),
            },
            thinking_level: level.to_string(),
        },
    ));
    session
}

fn custom_model_entry(provider: &str, api_key: Option<&str>) -> ModelEntry {
    ModelEntry {
        model: Model {
            id: "custom-model".to_string(),
            name: "Custom Model".to_string(),
            api: "custom".to_string(),
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
            context_window: 4096,
            max_tokens: 1024,
            headers: HashMap::new(),
        },
        api_key: api_key.map(str::to_string),
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

#[test]
fn select_model_and_thinking_clamps_reasoning_disabled_models_to_off() {
    let harness =
        TestHarness::new("select_model_and_thinking_clamps_reasoning_disabled_models_to_off");
    let registry = make_registry(&harness, &[]);
    let cli = cli::Cli::parse_from([
        "pi",
        "--provider",
        "anthropic",
        "--model",
        "claude-haiku-4-5",
        "--thinking",
        "high",
    ]);

    harness.log().info_ctx("inputs", "CLI args", |ctx| {
        ctx.push(("provider".into(), cli.provider.clone().unwrap_or_default()));
        ctx.push(("model".into(), cli.model.clone().unwrap_or_default()));
        ctx.push((
            "thinking".into(),
            cli.thinking.clone().unwrap_or_else(|| "(none)".to_string()),
        ));
    });

    let selection = select_model_and_thinking(
        &cli,
        &Config::default(),
        &Session::in_memory(),
        &registry,
        &[],
        harness.temp_dir(),
    )
    .expect("select model");

    harness.log().info_ctx("result", "Model selection", |ctx| {
        ctx.push((
            "provider".into(),
            selection.model_entry.model.provider.clone(),
        ));
        ctx.push(("model".into(), selection.model_entry.model.id.clone()));
        ctx.push(("thinking".into(), selection.thinking_level.to_string()));
    });

    assert_eq!(selection.model_entry.model.id, "claude-haiku-4-5");
    assert_eq!(selection.thinking_level, ThinkingLevel::Off);
}

#[test]
fn select_model_and_thinking_clamps_xhigh_when_model_does_not_support_it() {
    let harness =
        TestHarness::new("select_model_and_thinking_clamps_xhigh_when_model_does_not_support_it");
    let registry = make_registry(&harness, &[]);
    let cli = cli::Cli::parse_from([
        "pi",
        "--provider",
        "openai",
        "--model",
        "gpt-4o",
        "--thinking",
        "xhigh",
    ]);

    let selection = select_model_and_thinking(
        &cli,
        &Config::default(),
        &Session::in_memory(),
        &registry,
        &[],
        harness.temp_dir(),
    )
    .expect("select model");

    harness.log().info_ctx("result", "Model selection", |ctx| {
        ctx.push((
            "provider".into(),
            selection.model_entry.model.provider.clone(),
        ));
        ctx.push(("model".into(), selection.model_entry.model.id.clone()));
        ctx.push(("thinking".into(), selection.thinking_level.to_string()));
    });

    assert_eq!(selection.model_entry.model.id, "gpt-4o");
    assert_eq!(selection.thinking_level, ThinkingLevel::High);
}

#[test]
fn select_model_and_thinking_uses_scoped_thinking_level_when_cli_unset() {
    let harness =
        TestHarness::new("select_model_and_thinking_uses_scoped_thinking_level_when_cli_unset");
    let registry = make_registry(&harness, &[]);
    let cli = cli::Cli::parse_from(["pi"]);

    let scoped_models = resolve_model_scope(&["openai/gpt-4o:low".to_string()], &registry, true);

    harness.log().info_ctx("inputs", "Scoped models", |ctx| {
        ctx.push(("count".into(), scoped_models.len().to_string()));
        if let Some(first) = scoped_models.first() {
            ctx.push((
                "first".into(),
                format!("{}/{}", first.model.model.provider, first.model.model.id),
            ));
            ctx.push((
                "thinking".into(),
                first
                    .thinking_level
                    .map_or_else(|| "(none)".to_string(), |t| t.to_string()),
            ));
        }
    });

    let selection = select_model_and_thinking(
        &cli,
        &Config::default(),
        &Session::in_memory(),
        &registry,
        &scoped_models,
        harness.temp_dir(),
    )
    .expect("select model");

    assert_eq!(selection.model_entry.model.provider, "openai");
    assert_eq!(selection.model_entry.model.id, "gpt-4o");
    assert_eq!(selection.thinking_level, ThinkingLevel::Low);
}

#[test]
fn select_model_and_thinking_restores_last_session_model_when_no_cli_selection() {
    let harness = TestHarness::new(
        "select_model_and_thinking_restores_last_session_model_when_no_cli_selection",
    );
    let registry = make_registry(&harness, &[("openai", "test-key")]);
    let cli = cli::Cli::parse_from(["pi"]);
    let session = make_session_with_last_model("openai", "gpt-4o-mini");

    let selection = select_model_and_thinking(
        &cli,
        &Config::default(),
        &session,
        &registry,
        &[],
        harness.temp_dir(),
    )
    .expect("select model");

    harness
        .log()
        .info_ctx("result", "Restored selection", |ctx| {
            ctx.push((
                "provider".into(),
                selection.model_entry.model.provider.clone(),
            ));
            ctx.push(("model".into(), selection.model_entry.model.id.clone()));
        });

    assert_eq!(selection.model_entry.model.provider, "openai");
    assert_eq!(selection.model_entry.model.id, "gpt-4o-mini");
}

#[test]
fn select_model_and_thinking_restores_saved_thinking_on_continue() {
    let harness = TestHarness::new("select_model_and_thinking_restores_saved_thinking_on_continue");
    let registry = make_registry(&harness, &[("anthropic", "test-key")]);
    let cli = cli::Cli::parse_from(["pi", "-c"]);
    let session = make_session_with_last_thinking("minimal");

    let selection = select_model_and_thinking(
        &cli,
        &Config::default(),
        &session,
        &registry,
        &[],
        harness.temp_dir(),
    )
    .expect("select model");

    harness
        .log()
        .info_ctx("result", "Thinking selection", |ctx| {
            ctx.push(("thinking".into(), selection.thinking_level.to_string()));
        });

    assert_eq!(selection.thinking_level, ThinkingLevel::Minimal);
}

#[test]
fn select_model_and_thinking_falls_back_to_available_models_when_no_defaults() {
    let harness = TestHarness::new(
        "select_model_and_thinking_falls_back_to_available_models_when_no_defaults",
    );
    let registry = make_registry(&harness, &[("anthropic", "test-key")]);
    let cli = cli::Cli::parse_from(["pi"]);

    let selection = select_model_and_thinking(
        &cli,
        &Config::default(),
        &Session::in_memory(),
        &registry,
        &[],
        harness.temp_dir(),
    )
    .expect("select model");

    harness
        .log()
        .info_ctx("result", "Fallback selection", |ctx| {
            ctx.push((
                "provider".into(),
                selection.model_entry.model.provider.clone(),
            ));
            ctx.push(("model".into(), selection.model_entry.model.id.clone()));
        });

    assert_eq!(selection.model_entry.model.provider, "anthropic");
}

#[test]
fn build_system_prompt_includes_custom_append_context_and_skills() {
    let harness = TestHarness::new("build_system_prompt_includes_custom_append_context_and_skills");
    let global_dir = harness.create_dir("global");
    harness.create_file("global/AGENTS.md", "GLOBAL\n");

    let project_dir = harness.create_dir("project");
    std::fs::create_dir_all(project_dir.join("sub")).expect("create project/sub");
    std::fs::write(project_dir.join("AGENTS.md"), "ROOT\n").expect("write project AGENTS");
    std::fs::write(project_dir.join("sub").join("AGENTS.md"), "SUB\n").expect("write sub AGENTS");

    let custom_prompt_path = harness.create_file("prompt.txt", "CUSTOM PROMPT");
    let cli = cli::Cli::parse_from([
        "pi",
        "--system-prompt",
        custom_prompt_path.to_string_lossy().as_ref(),
        "--append-system-prompt",
        "APPEND PROMPT",
    ]);

    let skills_prompt = "\n\n# Skills\n- foo\n";
    let enabled_tools = ["read", "bash", "edit"];
    let package_dir = harness.create_dir("package");
    let prompt = build_system_prompt(
        &cli,
        &project_dir.join("sub"),
        &enabled_tools,
        Some(skills_prompt),
        &global_dir,
        &package_dir,
        false,
    );

    harness.log().info_ctx("prompt", "Prompt fragments", |ctx| {
        ctx.push(("len".into(), prompt.len().to_string()));
        ctx.push(("cwd".into(), project_dir.join("sub").display().to_string()));
    });

    assert!(prompt.contains("CUSTOM PROMPT"));
    assert!(prompt.contains("APPEND PROMPT"));
    assert!(prompt.contains("# Project Context"));
    assert!(prompt.contains("GLOBAL"));
    assert!(prompt.contains("ROOT"));
    assert!(prompt.contains("SUB"));
    assert!(prompt.contains("# Skills"));
    assert!(prompt.contains("Current date and time:"));
    assert!(prompt.contains(&format!(
        "Current working directory: {}",
        project_dir.join("sub").display()
    )));

    let global_idx = prompt.find("GLOBAL").expect("GLOBAL in prompt");
    let root_idx = prompt.find("ROOT").expect("ROOT in prompt");
    let sub_idx = prompt.find("SUB").expect("SUB in prompt");
    assert!(global_idx < root_idx && root_idx < sub_idx);
}

#[test]
fn prepare_initial_message_wraps_files_and_appends_first_message() {
    let harness = TestHarness::new("prepare_initial_message_wraps_files_and_appends_first_message");
    let file_path = harness.create_file("a.txt", "hello\nworld\n");
    let mut messages = vec!["please review".to_string()];
    let file_args = vec![file_path.to_string_lossy().to_string()];

    let initial = prepare_initial_message(harness.temp_dir(), &file_args, &mut messages, false)
        .expect("prepare initial")
        .expect("initial message present");

    harness.log().info_ctx("result", "Initial message", |ctx| {
        ctx.push(("text_len".into(), initial.text.len().to_string()));
        ctx.push(("images".into(), initial.images.len().to_string()));
    });

    assert!(messages.is_empty());
    assert!(initial.text.contains("<file name=\""));
    assert!(initial.text.contains("hello"));
    assert!(initial.text.contains("world"));
    assert!(initial.text.contains("please review"));

    let file_idx = initial.text.find("hello").expect("file content in message");
    let msg_idx = initial
        .text
        .find("please review")
        .expect("message content in initial");
    assert!(file_idx < msg_idx);

    let blocks = build_initial_content(&initial);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], ContentBlock::Text(_)));
}

#[test]
fn prepare_initial_message_leaves_remaining_messages() {
    let harness = TestHarness::new("prepare_initial_message_leaves_remaining_messages");
    let file_path = harness.create_file("a.txt", "hello\n");
    let mut messages = vec!["first".to_string(), "second".to_string()];
    let file_args = vec![file_path.to_string_lossy().to_string()];

    let initial = prepare_initial_message(harness.temp_dir(), &file_args, &mut messages, false)
        .expect("prepare initial")
        .expect("initial message present");

    assert_eq!(messages, vec!["second".to_string()]);
    assert!(initial.text.contains("first"));
}

#[test]
fn prepare_initial_message_attaches_images_and_builds_content_blocks() {
    let harness =
        TestHarness::new("prepare_initial_message_attaches_images_and_builds_content_blocks");
    let png_base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMBAA7x2FoAAAAASUVORK5CYII=";
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_base64)
        .expect("decode png");

    let image_path = harness.create_file("image.png", &bytes);
    let mut messages = Vec::new();
    let file_args = vec![image_path.to_string_lossy().to_string()];

    let initial = prepare_initial_message(harness.temp_dir(), &file_args, &mut messages, false)
        .expect("prepare initial")
        .expect("initial message present");

    harness
        .log()
        .info_ctx("result", "Image initial message", |ctx| {
            ctx.push(("text_len".into(), initial.text.len().to_string()));
            ctx.push(("images".into(), initial.images.len().to_string()));
            ctx.push(("path".into(), image_path.display().to_string()));
        });

    assert!(
        initial
            .text
            .contains(&format!("<file name=\"{}\"></file>", image_path.display()))
    );
    assert_eq!(initial.images.len(), 1);
    assert_eq!(initial.images[0].mime_type, "image/png");
    assert!(!initial.images[0].data.is_empty());

    let blocks = build_initial_content(&initial);
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], ContentBlock::Text(_)));
    assert!(matches!(&blocks[1], ContentBlock::Image(_)));
}

#[test]
fn process_file_arguments_missing_file_reports_error() {
    let harness = TestHarness::new("process_file_arguments_missing_file_reports_error");
    let args = vec!["missing.txt".to_string()];
    let err = process_file_arguments(&args, harness.temp_dir(), false)
        .expect_err("missing file should error");
    assert!(err.to_string().contains("Cannot access file"));
}

#[test]
fn process_file_arguments_small_image_respects_auto_resize_flag() {
    let harness = TestHarness::new("process_file_arguments_small_image_respects_auto_resize_flag");
    let png_base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMBAA7x2FoAAAAASUVORK5CYII=";
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_base64)
        .expect("decode png");
    let image_path = harness.create_file("image.png", &bytes);
    let args = vec![image_path.to_string_lossy().to_string()];

    let processed =
        process_file_arguments(&args, harness.temp_dir(), true).expect("process file arguments");
    assert_eq!(processed.images.len(), 1);
    assert!(
        processed
            .text
            .contains(&format!("<file name=\"{}\"></file>", image_path.display()))
    );
}

#[test]
fn apply_piped_stdin_inserts_message_and_sets_print() {
    let mut cli = cli::Cli::parse_from(["pi", "hello", "world"]);
    apply_piped_stdin(&mut cli, Some("stdin".to_string()));

    assert!(cli.print);
    let messages = cli.message_args();
    assert_eq!(messages, vec!["stdin", "hello", "world"]);
}

#[test]
fn apply_piped_stdin_none_keeps_args() {
    let mut cli = cli::Cli::parse_from(["pi", "hello"]);
    apply_piped_stdin(&mut cli, None);
    assert!(!cli.print);
    assert_eq!(cli.message_args(), vec!["hello"]);
}

#[test]
fn normalize_cli_sets_no_session_for_print_mode() {
    let mut cli = cli::Cli::parse_from(["pi", "-p", "hello"]);
    assert!(!cli.no_session);
    normalize_cli(&mut cli);
    assert!(cli.no_session);
}

#[test]
fn validate_rpc_args_rejects_file_args() {
    let cli = cli::Cli::parse_from(["pi", "--mode", "rpc", "@file.txt"]);
    let err = validate_rpc_args(&cli).expect_err("rpc should reject file args");
    assert!(
        err.to_string()
            .contains("@file arguments are not supported")
    );
}

#[test]
fn session_no_session_flag_creates_in_memory_session() {
    asupersync::test_utils::run_test(|| async {
        let mut cli = cli::Cli::parse_from(["pi", "-p", "hello"]);
        normalize_cli(&mut cli);
        let session = Box::pin(Session::new(&cli, &Config::default()))
            .await
            .expect("session");
        assert!(session.path.is_none());
    });
}

#[test]
fn resolve_api_key_precedence_and_error_paths() {
    let harness = TestHarness::new("resolve_api_key_precedence_and_error_paths");
    let auth_path = harness.temp_path("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("load auth storage");
    auth.set(
        "custom".to_string(),
        AuthCredential::ApiKey {
            key: "auth-key".to_string(),
        },
    );

    let entry = custom_model_entry("custom", Some("entry-key"));

    let cli_override = cli::Cli::parse_from(["pi", "--api-key", "cli-key"]);
    let resolved = resolve_api_key(&auth, &cli_override, &entry).expect("resolve api key");
    assert_eq!(resolved, "cli-key");

    let cli_no_override = cli::Cli::parse_from(["pi"]);
    let resolved = resolve_api_key(&auth, &cli_no_override, &entry).expect("resolve api key");
    assert_eq!(resolved, "auth-key");

    let auth_empty =
        AuthStorage::load(harness.temp_path("empty-auth.json")).expect("load empty auth storage");
    let resolved = resolve_api_key(&auth_empty, &cli_no_override, &entry).expect("resolve api key");
    assert_eq!(resolved, "entry-key");

    let entry_missing = custom_model_entry("custom", None);
    let err = resolve_api_key(&auth_empty, &cli_no_override, &entry_missing)
        .expect_err("missing key should error");
    assert!(
        err.to_string()
            .contains("No API key found for provider custom")
    );
}

// ────────────────────────────────────────────────────────────────
// CLI input plumbing: additional coverage (bd-2q2v)
// ────────────────────────────────────────────────────────────────

#[test]
fn prepare_initial_message_no_files_returns_none() {
    let harness = TestHarness::new("prepare_initial_message_no_files_returns_none");
    let mut messages = vec!["hello".to_string()];
    let result =
        prepare_initial_message(harness.temp_dir(), &[], &mut messages, false).expect("ok");
    assert!(result.is_none());
    // messages are untouched when no file args
    assert_eq!(messages, vec!["hello".to_string()]);
}

#[test]
fn prepare_initial_message_files_only_no_messages() {
    let harness = TestHarness::new("prepare_initial_message_files_only_no_messages");
    let file_path = harness.create_file("data.txt", "payload\n");
    let mut messages: Vec<String> = Vec::new();
    let file_args = vec![file_path.to_string_lossy().to_string()];

    let initial = prepare_initial_message(harness.temp_dir(), &file_args, &mut messages, false)
        .expect("prepare initial")
        .expect("initial message present");

    assert!(initial.text.contains("payload"));
    assert!(initial.text.contains("<file name=\""));
    assert!(messages.is_empty());
}

#[test]
fn prepare_initial_message_empty_file_returns_none() {
    let harness = TestHarness::new("prepare_initial_message_empty_file_returns_none");
    let file_path = harness.create_file("empty.txt", "");
    let mut messages: Vec<String> = Vec::new();
    let file_args = vec![file_path.to_string_lossy().to_string()];

    let result =
        prepare_initial_message(harness.temp_dir(), &file_args, &mut messages, false).expect("ok");
    // Empty file produces no text and no images → returns None
    assert!(result.is_none());
}

#[test]
fn process_file_arguments_multiple_text_files() {
    let harness = TestHarness::new("process_file_arguments_multiple_text_files");
    let a = harness.create_file("a.txt", "alpha\n");
    let b = harness.create_file("b.txt", "bravo\n");
    let c = harness.create_file("c.txt", "charlie\n");
    let args: Vec<String> = [&a, &b, &c]
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let processed = process_file_arguments(&args, harness.temp_dir(), false).expect("ok");

    assert!(processed.text.contains("alpha"));
    assert!(processed.text.contains("bravo"));
    assert!(processed.text.contains("charlie"));
    assert_eq!(processed.images.len(), 0);

    // Each file gets its own <file> tag
    let file_tag_count = processed.text.matches("<file name=\"").count();
    assert_eq!(file_tag_count, 3);
}

#[test]
fn process_file_arguments_empty_file_skipped() {
    let harness = TestHarness::new("process_file_arguments_empty_file_skipped");
    let empty = harness.create_file("empty.txt", "");
    let nonempty = harness.create_file("data.txt", "content\n");
    let args: Vec<String> = [&empty, &nonempty]
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let processed = process_file_arguments(&args, harness.temp_dir(), false).expect("ok");

    assert!(processed.text.contains("content"));
    // Only non-empty file gets a <file> tag
    let file_tag_count = processed.text.matches("<file name=\"").count();
    assert_eq!(file_tag_count, 1);
}

#[test]
fn process_file_arguments_mixed_text_and_image() {
    let harness = TestHarness::new("process_file_arguments_mixed_text_and_image");
    let png_base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMBAA7x2FoAAAAASUVORK5CYII=";
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png_base64)
        .expect("decode png");

    let text_file = harness.create_file("notes.txt", "some notes\n");
    let image_file = harness.create_file("img.png", &bytes);
    let args: Vec<String> = [&text_file, &image_file]
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let processed = process_file_arguments(&args, harness.temp_dir(), false).expect("ok");

    assert!(processed.text.contains("some notes"));
    assert_eq!(processed.images.len(), 1);
    assert_eq!(processed.images[0].mime_type, "image/png");
    // Both files produce <file> tags
    let file_tag_count = processed.text.matches("<file name=\"").count();
    assert_eq!(file_tag_count, 2);
}

#[test]
fn process_file_arguments_unicode_content_preserved() {
    let harness = TestHarness::new("process_file_arguments_unicode_content_preserved");
    let file_path = harness.create_file("unicode.txt", "hello 世界\nこんにちは\n");
    let args = vec![file_path.to_string_lossy().to_string()];

    let processed = process_file_arguments(&args, harness.temp_dir(), false).expect("ok");

    assert!(processed.text.contains("hello 世界"));
    assert!(processed.text.contains("こんにちは"));
}

#[test]
fn cli_file_args_and_message_args_separation() {
    let cli = cli::Cli::parse_from(["pi", "@a.txt", "hello", "@b.md", "world", "@c.rs"]);
    assert_eq!(cli.file_args(), vec!["a.txt", "b.md", "c.rs"]);
    assert_eq!(cli.message_args(), vec!["hello", "world"]);
}

#[test]
fn cli_file_args_empty_when_no_at_prefix() {
    let cli = cli::Cli::parse_from(["pi", "hello", "world"]);
    assert!(cli.file_args().is_empty());
    assert_eq!(cli.message_args(), vec!["hello", "world"]);
}

#[test]
fn cli_message_args_empty_when_only_files() {
    let cli = cli::Cli::parse_from(["pi", "@a.txt", "@b.txt"]);
    assert_eq!(cli.file_args(), vec!["a.txt", "b.txt"]);
    assert!(cli.message_args().is_empty());
}

#[test]
fn apply_piped_stdin_empty_content_no_op() {
    let mut cli = cli::Cli::parse_from(["pi", "hello"]);
    apply_piped_stdin(&mut cli, Some(String::new()));
    assert!(!cli.print);
    assert_eq!(cli.message_args(), vec!["hello"]);
}

#[test]
fn apply_piped_stdin_whitespace_only_no_op() {
    let mut cli = cli::Cli::parse_from(["pi", "hello"]);
    apply_piped_stdin(&mut cli, Some("\n\n\r\n".to_string()));
    assert!(!cli.print);
    assert_eq!(cli.message_args(), vec!["hello"]);
}

#[test]
fn apply_piped_stdin_trims_trailing_newlines() {
    let mut cli = cli::Cli::parse_from(["pi"]);
    apply_piped_stdin(&mut cli, Some("some input\n\n".to_string()));
    assert!(cli.print);
    assert_eq!(cli.message_args(), vec!["some input"]);
}

#[test]
fn normalize_cli_no_session_flag_directly() {
    let mut cli = cli::Cli::parse_from(["pi", "--no-session", "hello"]);
    assert!(cli.no_session);
    assert!(!cli.print);
    // normalize_cli doesn't change no_session when print is false
    normalize_cli(&mut cli);
    assert!(cli.no_session);
    assert!(!cli.print);
}

#[test]
fn validate_rpc_args_accepts_plain_messages() {
    let cli = cli::Cli::parse_from(["pi", "--mode", "rpc", "hello"]);
    validate_rpc_args(&cli).expect("plain messages in rpc should be ok");
}

#[test]
fn validate_rpc_args_accepts_no_args() {
    let cli = cli::Cli::parse_from(["pi", "--mode", "rpc"]);
    validate_rpc_args(&cli).expect("no args in rpc should be ok");
}

#[test]
fn prepare_initial_message_multiple_files_with_message() {
    let harness = TestHarness::new("prepare_initial_message_multiple_files_with_message");
    let a = harness.create_file("a.txt", "alpha\n");
    let b = harness.create_file("b.txt", "bravo\n");
    let mut messages = vec!["summarize these".to_string(), "extra".to_string()];
    let file_args: Vec<String> = [&a, &b]
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let initial = prepare_initial_message(harness.temp_dir(), &file_args, &mut messages, false)
        .expect("prepare initial")
        .expect("initial message present");

    // Both file contents present
    assert!(initial.text.contains("alpha"));
    assert!(initial.text.contains("bravo"));
    // First message appended, second remains
    assert!(initial.text.contains("summarize these"));
    assert_eq!(messages, vec!["extra".to_string()]);

    let blocks = build_initial_content(&initial);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], ContentBlock::Text(_)));
}

#[test]
fn cli_enabled_tools_default() {
    let cli = cli::Cli::parse_from(["pi"]);
    let tools = cli.enabled_tools();
    assert!(tools.contains(&"read"));
    assert!(tools.contains(&"bash"));
    assert!(tools.contains(&"edit"));
    assert!(tools.contains(&"write"));
}

#[test]
fn cli_no_tools_returns_empty() {
    let cli = cli::Cli::parse_from(["pi", "--no-tools"]);
    assert!(cli.enabled_tools().is_empty());
}
