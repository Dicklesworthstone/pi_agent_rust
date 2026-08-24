//! Snapcompact vision-model replay tests (bd-cv653.7.6, AC 2 + AC 4).
//!
mod common;

use std::sync::{Arc, Mutex};

use pi::agent::{Agent, AgentConfig};
use pi::compaction_snap::{
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, SnapFrame, SnapPayload, attach_frames,
    strip_snapcompact_images,
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
    let payload = SnapPayload::new(vec![
        SnapFrame {
            png: "QUFB".to_string(),
            width: FRAME_W,
            height: FRAME_H,
        },
        SnapFrame {
            png: "QkJC".to_string(),
            width: FRAME_W,
            height: FRAME_H,
        },
    ]);
    attach_frames(base, Some(&payload))
}

const FRAME_W: u32 = 10;
const FRAME_H: u32 = 10;

fn image_block_count(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => match &u.content {
                UserContent::Blocks(blocks) => Some(blocks),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .filter(|b| matches!(b, ContentBlock::Image(_)))
        .count()
}

fn run_capture(config: AgentConfig) -> Vec<Message> {
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
        agent.add_message(summary_with_two_frames());
        // Empty stream ends the loop; the captured context is what matters.
        let _ = agent.run("ping", |_| {}).await;
        let guard = captured.lock().expect("context mutex");
        guard.clone().unwrap_or_default()
    })
}

#[test]
fn vision_capable_model_receives_compaction_frames_as_image_blocks() {
    let messages = run_capture(AgentConfig {
        model_accepts_images: true,
        ..AgentConfig::default()
    });
    assert!(
        messages.iter().any(|m| message_has_summary_text(m)),
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
        messages.iter().any(|m| message_has_summary_text(m)),
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
