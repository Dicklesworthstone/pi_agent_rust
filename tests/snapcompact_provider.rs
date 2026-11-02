// Integration tests are separate crates, so the crate-level `recursion_limit`
// raised in src/lib.rs does not reach here. Proving `Send` for the replay
// future now exceeds the default 128 — asupersync 0.5.0 nests its runtime
// future types more deeply than 0.4.4 did — which the compiler reports as an
// overflow error rather than a warning in this target.
#![recursion_limit = "256"]

//! Snapcompact vision-model replay tests (bd-cv653.7.6, AC 2 + AC 4).
//!
mod common;

use std::sync::{Arc, Mutex};

use pi::agent::{Agent, AgentConfig};
use pi::compaction_snap::{
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, SnapPayload, attach_frames,
    render_frames, strip_snapcompact_images,
};
use pi::model::{ContentBlock, Message, UserContent, UserMessage};
use pi::provider::{Context, Provider, StreamOptions};
use pi::tools::ToolRegistry;

struct CapturingProvider {
    context: Arc<Mutex<Option<Vec<Message>>>>,
    name: String,
    api: String,
    model_id: String,
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn api(&self) -> &str {
        &self.api
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = pi::error::Result<pi::model::StreamEvent>> + Send>,
        >,
    > {
        let mut guard = self.context.lock().expect("context mutex");
        *guard = Some(context.messages.to_vec());
        drop(guard);
        Ok(Box::pin(futures::stream::empty()))
    }
}

fn summary_with_two_frames() -> Message {
    let summary_text =
        format!("{COMPACTION_SUMMARY_PREFIX}compact body{COMPACTION_SUMMARY_SUFFIX}");
    let base = Message::User(UserMessage {
        content: UserContent::Text(summary_text),
        timestamp: 42,
    });
    // Real rendered frames: attachment validates PNG integrity and geometry,
    // so placeholder bytes would leave the summary text-only.
    let payload = SnapPayload::new(vec![
        render_frames("[User]: first frame").remove(0),
        render_frames("[User]: second frame").remove(0),
    ]);
    attach_frames(base, Some(&payload))
}

fn image_block_count(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => match &u.content {
                UserContent::Blocks(blocks) => Some(blocks),
                UserContent::Text(_) => None,
            },
            _ => None,
        })
        .flatten()
        .filter(|b| matches!(b, ContentBlock::Image(_)))
        .count()
}

fn run_capture(config: AgentConfig) -> Vec<Message> {
    run_capture_message(config, summary_with_two_frames())
}

fn run_capture_message(config: AgentConfig, message: Message) -> Vec<Message> {
    run_capture_with_settings(config, message, None)
}

fn run_capture_with_settings(
    config: AgentConfig,
    message: Message,
    settings: Option<pi::compaction::ResolvedCompactionSettings>,
) -> Vec<Message> {
    common::run_async(async {
        let captured = Arc::new(Mutex::new(None));
        let provider = Arc::new(CapturingProvider {
            context: Arc::clone(&captured),
            name: "capture".to_string(),
            api: "capture".to_string(),
            model_id: "capture-model".to_string(),
        });
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(&[], std::path::Path::new("."), None),
            config,
        );
        agent.add_message(message);
        // Empty stream ends the loop; the captured context is what matters.
        if let Some(settings) = settings {
            let mut session = pi::agent::AgentSession::new(
                agent,
                Arc::new(asupersync::sync::Mutex::new(pi::session::Session::create())),
                false,
                settings,
            );
            let _ = session.agent.run("ping", |_| {}).await;
        } else {
            let _ = agent.run("ping", |_| {}).await;
        }
        let guard = captured.lock().expect("context mutex");
        guard.clone().unwrap_or_default()
    })
}

fn source_archive(source: &str) -> Message {
    attach_frames(
        Message::User(UserMessage {
            content: UserContent::Text(format!(
                "{COMPACTION_SUMMARY_PREFIX}short checkpoint{COMPACTION_SUMMARY_SUFFIX}"
            )),
            timestamp: 42,
        }),
        Some(&SnapPayload::new(render_frames(source)).with_source_text(source.to_string())),
    )
}

#[test]
fn native_provider_receives_one_reading_and_never_archive_metadata() {
    let source = "[User]: Preserve the release key BLUE-WREN and the exact path caf\u{e9}.rs.";
    let original = source_archive(source);
    let original_json = serde_json::to_value(&original).unwrap();
    for accepts_images in [true, false] {
        let messages = run_capture_message(
            AgentConfig {
                model_accepts_images: accepts_images,
                ..AgentConfig::default()
            },
            original.clone(),
        );
        assert_eq!(image_block_count(&messages), usize::from(accepts_images));
        let wire = serde_json::to_string(&messages).unwrap();
        assert_eq!(wire.contains("BLUE-WREN"), !accepts_images);
        assert!(!wire.contains("sourceText"));
        assert!(!wire.contains("pi.snapcompact"));
        assert_eq!(serde_json::to_value(&original).unwrap(), original_json);
    }
}

#[test]
fn model_switches_reselect_archive_reading_without_mutating_history() {
    let captured = Arc::new(Mutex::new(None));
    let mut agent = Agent::new(
        Arc::new(CapturingProvider {
            context: captured,
            name: "capture".to_string(),
            api: "capture".to_string(),
            model_id: "capture-model".to_string(),
        }),
        ToolRegistry::new(&[], std::path::Path::new("."), None),
        AgentConfig::default(),
    );
    agent.add_message(source_archive("[User]: exact retained fact SWIFT-413"));
    let canonical = serde_json::to_value(agent.messages()).unwrap();
    for vision in [true, false, true, false] {
        agent.set_model_accepts_images(vision);
        let request = agent.request_context_json();
        let messages: Vec<Message> = serde_json::from_value(request["messages"].clone()).unwrap();
        assert_eq!(image_block_count(&messages), usize::from(vision));
        assert_eq!(request.to_string().contains("SWIFT-413"), !vision);
        assert_eq!(serde_json::to_value(agent.messages()).unwrap(), canonical);
    }
}

#[test]
fn image_blocking_keeps_the_readable_source_instead_of_image_placeholders() {
    let messages = run_capture_message(
        AgentConfig {
            model_accepts_images: true,
            block_images: true,
            ..AgentConfig::default()
        },
        source_archive("[User]: important ARCHIVE-461 detail"),
    );
    let wire = serde_json::to_string(&messages).unwrap();
    assert_eq!(image_block_count(&messages), 0);
    assert!(wire.contains("ARCHIVE-461"));
    assert!(!wire.contains("Image reading is disabled"));
}

#[test]
fn retained_source_is_screened_and_vision_frames_are_regenerated_after_policy_change() {
    let secret = "CUSTOM-ARCHIVE-SECRET-8432";
    let source = format!("[User]: do not disclose {secret}");
    let original = source_archive(&source);
    let original_frames = render_frames(&source);
    for vision in [false, true] {
        let messages = run_capture_message(
            AgentConfig {
                model_accepts_images: vision,
                secrets: Some(pi::secrets::SecretsSettings {
                    mode: Some("obfuscate".to_string()),
                    extra_patterns: Some(vec![secret.to_string()]),
                }),
                ..AgentConfig::default()
            },
            original.clone(),
        );
        let wire = serde_json::to_string(&messages).unwrap();
        assert!(!wire.contains(secret));
        if vision {
            assert_eq!(image_block_count(&messages), 1);
            assert!(!wire.contains(&original_frames[0].png));
        }
    }
    assert!(serde_json::to_string(&original).unwrap().contains(secret));
}

#[test]
fn vision_reading_is_derived_from_source_even_when_stored_pngs_are_valid() {
    let source = "[User]: harmless screened archive";
    let stale_frames = render_frames("[User]: API_KEY=r4nd0mCredentialValue123456");
    let archive = attach_frames(
        Message::User(UserMessage {
            content: UserContent::Text(format!(
                "{COMPACTION_SUMMARY_PREFIX}checkpoint{COMPACTION_SUMMARY_SUFFIX}"
            )),
            timestamp: 42,
        }),
        Some(&SnapPayload::new(stale_frames.clone()).with_source_text(source.to_string())),
    );
    let messages = run_capture_message(
        AgentConfig {
            model_accepts_images: true,
            secrets: Some(pi::secrets::SecretsSettings {
                mode: Some("obfuscate".to_string()),
                extra_patterns: None,
            }),
            ..AgentConfig::default()
        },
        archive,
    );
    let wire = serde_json::to_string(&messages).unwrap();
    assert_eq!(image_block_count(&messages), 1);
    assert!(wire.contains(&render_frames(source)[0].png));
    assert!(!wire.contains(&stale_frames[0].png));
}

#[test]
fn smaller_model_window_clips_only_outbound_archive_and_reports_the_omission() {
    let source = format!(
        "[User]: HEAD-FACT\n{}\nMIDDLE-FACT\n{}\nTAIL-FACT",
        "archive detail ".repeat(300),
        "archive detail ".repeat(300),
    );
    let captured = Arc::new(Mutex::new(None));
    let mut agent = Agent::new(
        Arc::new(CapturingProvider {
            context: captured,
            name: "capture".to_string(),
            api: "capture".to_string(),
            model_id: "capture-model".to_string(),
        }),
        ToolRegistry::new(&[], std::path::Path::new("."), None),
        AgentConfig::default(),
    );
    agent.add_message(source_archive(&source));
    let mut session = pi::agent::AgentSession::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(pi::session::Session::create())),
        false,
        pi::compaction::ResolvedCompactionSettings {
            context_window_tokens: 1_024,
            reserve_tokens: 512,
            ..pi::compaction::ResolvedCompactionSettings::default()
        },
    );
    let canonical = serde_json::to_value(session.agent.messages()).unwrap();
    let request = session.agent.request_context_json();
    let text = request["messages"][0]["content"].as_str().unwrap();
    assert!(text.contains("HEAD-FACT") && text.contains("TAIL-FACT"));
    assert!(!text.contains("MIDDLE-FACT"));
    assert!(text.contains("Middle of compacted history omitted"));
    assert_eq!(
        serde_json::to_value(session.agent.messages()).unwrap(),
        canonical
    );

    // Enlarging the window restores the source without reloading the session.
    session.set_compaction_context_window(64_000);
    let restored = session.agent.request_context_json();
    assert!(restored.to_string().contains("MIDDLE-FACT"));
    assert!(
        !restored
            .to_string()
            .contains("Middle of compacted history omitted")
    );
}

#[test]
fn omitted_middle_is_still_screened_before_text_reading_is_budgeted() {
    let secret = "r4nd0mCredentialValue123456";
    let source = format!(
        "[User]: echo {secret}\n{}\nAPI_KEY={secret}\n{}\nTAIL-FACT",
        "ordinary archive detail ".repeat(300),
        "ordinary archive detail ".repeat(300),
    );
    let messages = run_capture_with_settings(
        AgentConfig {
            model_accepts_images: false,
            secrets: Some(pi::secrets::SecretsSettings {
                mode: Some("obfuscate".to_string()),
                extra_patterns: None,
            }),
            ..AgentConfig::default()
        },
        source_archive(&source),
        Some(pi::compaction::ResolvedCompactionSettings {
            context_window_tokens: 1_024,
            reserve_tokens: 512,
            ..pi::compaction::ResolvedCompactionSettings::default()
        }),
    );
    let wire = serde_json::to_string(&messages).unwrap();
    assert!(!wire.contains(secret));
    assert!(wire.contains("echo <pi-secret:"));
    assert!(!wire.contains("API_KEY="));
    assert!(wire.contains("Middle of compacted history omitted"));
    assert!(wire.contains("TAIL-FACT"));
}

#[test]
fn native_session_save_and_resume_preserves_source_for_text_and_vision_readers() {
    common::run_async(async {
        let directory = tempfile::tempdir().expect("session directory");
        let source = "[User]: Durable archive fact PLOVER-782, caf\u{e9}.";
        let payload = SnapPayload::new(render_frames(source)).with_source_text(source.to_string());
        let mut session =
            pi::session::Session::create_with_dir(Some(directory.path().to_path_buf()));
        let kept = session.append_message(pi::session::SessionMessage::User {
            content: UserContent::Text("retained recent turn".to_string()),
            timestamp: Some(43),
        });
        session.append_compaction(
            "short checkpoint".to_string(),
            kept,
            4_000,
            Some(pi::compaction_snap::payload_to_details(None, &payload)),
            None,
        );
        session.save().await.expect("save archive");
        let path = session.path.as_ref().expect("saved path");
        let resumed = pi::session::Session::open(path.to_str().expect("UTF-8 test path"))
            .await
            .expect("resume archive");
        let canonical = resumed.to_messages_for_current_path();
        for vision in [false, true, false] {
            let mut projected = canonical.clone();
            strip_snapcompact_images(&mut projected, vision);
            let wire = serde_json::to_string(&projected).unwrap();
            assert_eq!(wire.contains("PLOVER-782"), !vision);
            assert_eq!(image_block_count(&projected), usize::from(vision));
            assert!(wire.contains("retained recent turn"));
        }
        let restored = serde_json::to_string(&resumed.to_messages_for_current_path()).unwrap();
        assert!(restored.contains("PLOVER-782"));
        assert!(restored.contains("sourceText"));
    });
}

#[test]
fn vision_capable_model_receives_compaction_frames_as_image_blocks() {
    let messages = run_capture(AgentConfig {
        model_accepts_images: true,
        ..AgentConfig::default()
    });
    assert!(
        messages.iter().any(message_has_summary_text),
        "summary text must reach the provider"
    );
    assert_eq!(
        image_block_count(&messages),
        2,
        "vision-capable models receive both rasterized frames as image blocks"
    );
}

#[test]
fn text_only_model_receives_no_snapcompact_frames() {
    let messages = run_capture(AgentConfig {
        model_accepts_images: false,
        ..AgentConfig::default()
    });
    assert!(
        messages.iter().any(message_has_summary_text),
        "summary TEXT must still reach text-only providers"
    );
    assert_eq!(
        image_block_count(&messages),
        0,
        "text-only models must never receive snapcompact frames"
    );
}

fn message_has_summary_text(m: &Message) -> bool {
    matches!(m,
    Message::User(u) if match &u.content {
        UserContent::Text(t) => t.contains("<summary>"),
        UserContent::Blocks(b) => b.first().is_some_and(|blk| matches!(blk,
            ContentBlock::Text(t) if t.text.starts_with(COMPACTION_SUMMARY_PREFIX))),
    })
}

#[test]
fn strip_helper_reports_degradation_stats_for_logging() {
    let mut msgs = vec![summary_with_two_frames()];
    let stats = strip_snapcompact_images(&mut msgs, false);
    assert_eq!(stats.removed_frames, 2);
    assert_eq!(stats.affected_messages, 1);
}
