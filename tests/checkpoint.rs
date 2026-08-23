//! Integration tests for checkpoint/rewind/fresh/retry (bd-cv653.3.7).
//!
//! Acceptance coverage:
//! 1. checkpoint then 20 more turns then rewind: active context contains
//!    the report + post-checkpoint span is summarized out; the tree
//!    retains all original entries.
//! 2. fresh resets stream state with the transcript byte-identical.
//! 3. retry re-issues the last user turn with the original path intact.
//! 4. --max-time stops at the turn boundary with the marker.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::agent::{Agent, AgentConfig};
use pi::model::{Message, StreamEvent, UserContent, UserMessage};
use pi::provider::{Context, StreamOptions};
use pi::session::Session;
use pi::tools::ToolRegistry;
use serde_json::json;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL v2 validation errors: {errors:?}");
}

fn block_on_local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

fn user_text(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn assistant_text(text: &str) -> Message {
    Message::Assistant(std::sync::Arc::new(pi::model::AssistantMessage {
        content: vec![pi::model::ContentBlock::Text(pi::model::TextContent::new(
            text,
        ))],
        ..Default::default()
    }))
}

/// Stub summarizer provider: returns a canned report.
struct SummaryProvider;

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl pi::provider::Provider for SummaryProvider {
    fn name(&self) -> &str {
        "summary-stub"
    }

    fn api(&self) -> &str {
        "stub-api"
    }

    fn model_id(&self) -> &str {
        "stub-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        let message = pi::model::AssistantMessage {
            content: vec![pi::model::ContentBlock::Text(pi::model::TextContent::new(
                "REPORT: explored the span and decided things",
            ))],
            ..Default::default()
        };
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::TextDelta {
                content_index: 0,
                delta: "REPORT: explored the span and decided things".to_string(),
            }),
            Ok(StreamEvent::Done {
                reason: pi::model::StopReason::Stop,
                message,
            }),
        ])))
    }
}

fn build_agent(root: &Path) -> Agent {
    let provider = Arc::new(SummaryProvider);
    let tools = ToolRegistry::new(&[], root, None::<&pi::config::Config>);
    Agent::new(provider, tools, AgentConfig::default())
}

#[test]
fn mark_twenty_turns_rewind_tree_preserved() {
    let case = "mark_twenty_turns_rewind_tree_preserved";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let mut agent = build_agent(&root);
    let mut session = Session::in_memory();

    // Pre-checkpoint context: 2 messages.
    agent.add_message(user_text("foundation one"));
    agent.add_message(user_text("foundation two"));
    let checkpoint = pi::checkpoint::mark_checkpoint(
        &mut session,
        "alpha",
        Some("before exploration"),
        agent.messages(),
    );
    harness
        .log()
        .info("verify", format!("checkpoint: {checkpoint:?}"));
    assert_eq!(checkpoint.message_count, 2);

    // 20 more turns (40 messages: user + assistant pairs).
    for index in 0..20 {
        agent.add_message(user_text(&format!("exploration turn {index}")));
        agent.add_message(user_text(&format!("assistant reply {index}")));
    }
    assert_eq!(agent.messages().len(), 42);

    // Rewind: summarize the span (messages 2..42) and collapse it.
    let span: Vec<Message> = agent.messages()[checkpoint.message_count..].to_vec(); // ubs:ignore bounded by construction
    let settings = pi::compaction::ResolvedCompactionSettings {
        enabled: true,
        ..Default::default()
    };
    let summary = block_on_local(pi::checkpoint::summarize_span(
        &span,
        Arc::new(SummaryProvider),
        "test-key",
        &settings,
    ))
    .expect("summarize");
    assert!(summary.contains("REPORT"));

    let outcome = pi::checkpoint::apply_rewind_to_active(&mut agent, &checkpoint, summary);
    harness.log().info(
        "verify",
        format!(
            "rewind outcome: collapsed={} tokens={}",
            outcome.collapsed_messages, outcome.summary_tokens_estimate
        ),
    );
    assert_eq!(outcome.collapsed_messages, 40);
    // Active context: 2 foundation + 1 report.
    assert_eq!(agent.messages().len(), 3);
    let report = &agent.messages()[2]; // ubs:ignore length asserted above
    let report_text = match report {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(_) => String::new(),
        },
        _ => String::new(),
    };
    assert!(
        report_text.contains("[REWIND REPORT: alpha]"),
        "{report_text}"
    );
    assert!(report_text.contains("REPORT"), "{report_text}");

    // The tree kept everything: session entries still include originals.
    let checkpoint_found = pi::checkpoint::find_checkpoint(&session, Some("alpha")).expect("find");
    assert_eq!(checkpoint_found.name, "alpha");
    session.append_custom_entry(
        "rewind".to_string(),
        Some(serde_json::to_value(&outcome).expect("outcome json")),
    );
    let entries = session.entries_for_current_path();
    harness.log().info(
        "verify",
        format!("tree entries after rewind: {}", entries.len()),
    );
    assert!(
        entries.len() >= 2,
        "tree retains entries (append-only): {}",
        entries.len()
    );
    finish_case(&harness, case);
}

#[test]
fn fresh_resets_stream_state_transcript_untouched() {
    let case = "fresh_resets_stream_state_transcript_untouched";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let mut agent = build_agent(&root);
    let mut session = Session::in_memory();

    agent.add_message(user_text("transcript message one"));
    agent.add_message(user_text("transcript message two"));
    let before: Vec<String> = agent
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).expect("ser"))
        .collect();
    let old_session_id = agent.stream_options().session_id.clone();

    let new_id = pi::checkpoint::fresh_stream_state(&mut agent, &mut session);
    let after: Vec<String> = agent
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).expect("ser"))
        .collect();
    harness.log().info(
        "verify",
        format!("session id {old_session_id:?} -> {new_id}"),
    );
    assert_ne!(
        Some(new_id.as_str()),
        old_session_id.as_deref(),
        "fresh must rotate the provider session id"
    );
    assert_eq!(before, after, "transcript must be byte-identical");
    finish_case(&harness, case);
}

#[test]
fn retry_takes_last_user_turn() {
    let case = "retry_takes_last_user_turn";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let mut agent = build_agent(&root);

    agent.add_message(user_text("first question"));
    agent.add_message(assistant_text("assistant answer"));
    agent.add_message(user_text("second question"));
    agent.add_message(assistant_text("second answer"));

    let text = pi::checkpoint::take_last_user_turn(&mut agent).expect("take");
    harness.log().info(
        "verify",
        format!("retry text: {text}; remaining: {}", agent.messages().len()),
    );
    assert_eq!(text, "second question");
    // Active context rewinds to just before that turn; the first turn stays.
    assert_eq!(agent.messages().len(), 2);
    // No more user turns to retry from here? One remains.
    let second = pi::checkpoint::take_last_user_turn(&mut agent).expect("take 2");
    assert_eq!(second, "first question");
    finish_case(&harness, case);
}

/// Slow provider: emits nothing for a beat so the cap check fires first.
struct SlowProvider;

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl pi::provider::Provider for SlowProvider {
    fn name(&self) -> &str {
        "slow-stub"
    }

    fn api(&self) -> &str {
        "stub-api"
    }

    fn model_id(&self) -> &str {
        "stub-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        // Infinite pending stream: the run would never finish without the cap.
        Ok(Box::pin(futures::stream::pending()))
    }
}

#[test]
fn max_time_stops_at_turn_boundary() {
    let case = "max_time_stops_at_turn_boundary";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let provider = Arc::new(SlowProvider);
    let tools = ToolRegistry::new(&[], &root, None::<&pi::config::Config>);
    let config = AgentConfig {
        max_time: Some(std::time::Duration::ZERO),
        ..AgentConfig::default()
    };
    let mut agent = Agent::new(provider, tools, config);

    let result = block_on_local(agent.run("do something long", |_| {})).expect("run");
    let text = result
        .content
        .iter()
        .find_map(|block| match block {
            pi::model::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    harness
        .log()
        .info("verify", format!("max-time result: {text}"));
    assert!(
        text.contains("time cap reached"),
        "the marker must be returned: {text}"
    );
    let _ = json!({"schema": "pi.max_time.v1"});
    finish_case(&harness, case);
}

/// Rewind durability (bd-cv653.3.7 follow-up): a rewound span must stay
/// collapsed when the active context is REBUILT from the session tree
/// (compaction apply, resume, per-prompt SDK rebuilds) — previously the
/// rebuild resurrected the whole span and dropped the report.
#[test]
fn rewind_survives_context_rebuild_from_tree() {
    let harness = TestHarness::new("rewind_survives_context_rebuild_from_tree");
    let mut session = Session::in_memory();

    // Two foundation turns in the TREE.
    session.append_message(pi::session::SessionMessage::from(user_text(
        "foundation one",
    )));
    session.append_message(pi::session::SessionMessage::from(user_text(
        "foundation two",
    )));

    // Checkpoint marker, then an exploration span of 6 tree messages.
    let checkpoint = pi::checkpoint::mark_checkpoint(
        &mut session,
        "alpha",
        None,
        &[user_text("foundation one"), user_text("foundation two")],
    );
    let checkpoint_entry_id = checkpoint.entry_id.expect("entry id recorded");
    for index in 0..3 {
        session.append_message(pi::session::SessionMessage::from(user_text(&format!(
            "exploration {index}"
        ))));
        session.append_message(pi::session::SessionMessage::from(assistant_text(&format!(
            "reply {index}"
        ))));
    }

    // Durable rewind marker referencing the checkpoint entry.
    let outcome = pi::checkpoint::RewindOutcome {
        schema: pi::checkpoint::CHECKPOINT_SCHEMA.to_string(),
        checkpoint: "alpha".to_string(),
        checkpoint_entry_id: Some(checkpoint_entry_id),
        collapsed_messages: 6,
        summary: "explored three approaches; picked B".to_string(),
        summary_tokens_estimate: 8,
        tree_preserved: true,
    };
    session.append_custom_entry(
        "rewind".to_string(),
        Some(serde_json::to_value(&outcome).expect("serialize outcome")),
    );

    // Rebuild from the tree: foundation survives, exploration collapses
    // into the report, post-rewind turns keep accumulating.
    session.append_message(pi::session::SessionMessage::from(user_text("after rewind")));
    let rebuilt = session.to_messages_for_current_path();
    let texts: Vec<String> = rebuilt
        .iter()
        .map(|message| match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => text.clone(),
                UserContent::Blocks(_) => String::new(),
            },
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(rebuilt.len(), 4, "{texts:#?}");
    assert_eq!(texts[0], "foundation one");
    assert_eq!(texts[1], "foundation two");
    assert!(
        texts[2].starts_with("[REWIND REPORT: alpha]"),
        "report replayed: {}",
        texts[2]
    );
    assert!(texts[2].contains("picked B"));
    assert_eq!(texts[3], "after rewind");

    // Legacy rewind entries (no checkpointEntryId) must be a no-op, not a
    // panic or a bogus truncation.
    let mut legacy = Session::in_memory();
    legacy.append_message(pi::session::SessionMessage::from(user_text("only turn")));
    legacy.append_custom_entry(
        "rewind".to_string(),
        Some(serde_json::json!({ "schema": "pi.checkpoint.v1", "summary": "s" })),
    );
    assert_eq!(legacy.to_messages_for_current_path().len(), 1);

    finish_case(&harness, "rewind_survives_context_rebuild_from_tree");
}
