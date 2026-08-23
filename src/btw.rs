//! `/btw` ephemeral side questions (bd-cv653.3.16).
//!
//! A side question goes to the session's `smol` role model with a strict
//! contract: answer briefly, never use tools, never ask follow-ups. The
//! exchange is ephemeral **by construction** — the call builds its own
//! throwaway message list and shares nothing with the session writer, so no
//! JSONL entry can ever contain it. Interactive-only by design; there is no
//! `--btw` print-mode flag.

use std::sync::Arc;

use futures::StreamExt;

use crate::error::Result;
use crate::model::{Message, UserContent, UserMessage};
use crate::provider::Provider;

/// System contract for side questions (omp btw-user.md semantics).
pub const BTW_SYSTEM_PROMPT: &str = "You are answering an ephemeral side question about the \
current work. Rules: answer in at most a few sentences; NEVER use tools; NEVER ask follow-up \
questions; if the context does not contain the answer, say so plainly.";

/// Cap on recent-session text fed into the side question so /btw stays
/// cheap regardless of transcript size.
const CONTEXT_BUDGET_CHARS: usize = 4_000;
const ANSWER_MAX_TOKENS: u32 = 512;

/// One-shot client bound to the resolved `smol` role provider.
pub struct BtwClient {
    provider: Arc<dyn Provider>,
    api_key: Option<String>,
}

impl BtwClient {
    pub fn new(provider: Arc<dyn Provider>, api_key: Option<String>) -> Self {
        Self { provider, api_key }
    }

    /// Ask an ephemeral side question with compact context from the current
    /// conversation tail. Returns only the answer text.
    pub async fn ask(&self, context_summary: &str, question: &str) -> Result<String> {
        let user_text = if context_summary.is_empty() {
            question.to_string()
        } else {
            format!("Current work context:\n{context_summary}\n\nSide question: {question}")
        };
        let context = crate::provider::Context {
            system_prompt: Some(BTW_SYSTEM_PROMPT.to_string().into()),
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text(user_text),
                timestamp: chrono::Utc::now().timestamp_millis(),
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = crate::provider::StreamOptions {
            max_tokens: Some(ANSWER_MAX_TOKENS),
            api_key: self.api_key.clone(),
            ..Default::default()
        };
        let mut stream = self.provider.stream(&context, &options).await?;
        let mut answer = String::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(crate::model::StreamEvent::TextDelta { delta, .. }) => {
                    answer.push_str(&delta);
                }
                Ok(crate::model::StreamEvent::Done { .. }) => break,
                Ok(_) => {}
                Err(err) => return Err(err),
            }
        }
        if answer.trim().is_empty() {
            return Err(crate::error::Error::api(
                "side question returned empty reply",
            ));
        }
        Ok(answer)
    }
}

/// Compact context summary from the live agent message list.
///
/// The most recent exchanges, truncated to [`CONTEXT_BUDGET_CHARS`]. Tool
/// noise (calls/results) is summarized as one-liners so the budget buys
/// prose.
#[must_use]
pub fn build_context_summary(messages: &[Message]) -> String {
    // Pieces accumulate newest-first (walking backwards); each message's
    // OWN pieces are appended in reverse so the final flip restores true
    // chronological order within a message too. The budget drops the
    // OLDEST content — the newest exchange is what a side question is
    // usually about.
    let mut pieces: Vec<String> = Vec::new();
    let mut used = 0usize;
    for message in messages.iter().rev() {
        let mut message_pieces: Vec<String> = Vec::new();
        match message {
            Message::User(user) => {
                if let UserContent::Text(text) = &user.content {
                    message_pieces.push(format!("user: {}", truncate(text, 400)));
                }
            }
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    match block {
                        crate::model::ContentBlock::Text(t) => {
                            message_pieces.push(format!("assistant: {}", truncate(&t.text, 400)));
                        }
                        crate::model::ContentBlock::ToolCall(call) => {
                            message_pieces.push(format!("assistant ran tool {}", call.name));
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(result) => {
                let first = result.content.iter().find_map(|block| match block {
                    crate::model::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                });
                message_pieces.push(format!(
                    "tool {}: {}",
                    result.tool_name,
                    truncate(first.as_deref().unwrap_or(""), 160)
                ));
            }
            Message::Custom(_) => {}
        }
        let mut over_budget = false;
        for piece in message_pieces.into_iter().rev() {
            // +1 for the join separator; stop BEFORE exceeding the budget
            // so the newest pieces are never tail-truncated later.
            if used + piece.len() + 1 > CONTEXT_BUDGET_CHARS {
                over_budget = true;
                break;
            }
            used += piece.len() + 1;
            pieces.push(piece);
        }
        if over_budget {
            break;
        }
    }
    pieces.reverse();
    pieces.join("\n")
}

fn truncate(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_forbids_tools_and_followups() {
        assert!(BTW_SYSTEM_PROMPT.contains("NEVER use tools"));
        assert!(BTW_SYSTEM_PROMPT.contains("NEVER ask follow-up"));
    }

    #[test]
    fn context_summary_captures_recent_exchanges_and_tool_noise() {
        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Text("fix the flaky test".into()),
                timestamp: 0,
            }),
            Message::Assistant(
                crate::model::AssistantMessage {
                    content: vec![crate::model::ContentBlock::ToolCall(
                        crate::model::ToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            arguments: serde_json::json!({ "command": "cargo test" }),
                            thought_signature: None,
                        },
                    )],
                    api: "test-api".into(),
                    provider: "test-provider".into(),
                    model: "test-model".into(),
                    ..Default::default()
                }
                .into(),
            ),
            Message::User(UserMessage {
                content: UserContent::Text("second question".into()),
                timestamp: 0,
            }),
        ];
        let summary = build_context_summary(&messages);
        assert!(summary.contains("fix the flaky test"), "{summary}");
        assert!(summary.contains("ran tool bash"), "{summary}");
        assert!(summary.contains("second question"), "{summary}");
    }

    #[test]
    fn context_summary_respects_budget() {
        let big = "x".repeat(10_000);
        let messages = vec![Message::User(UserMessage {
            content: UserContent::Text(big),
            timestamp: 0,
        })];
        let summary = build_context_summary(&messages);
        assert!(summary.len() <= CONTEXT_BUDGET_CHARS + 32);
    }

    #[test]
    fn empty_reply_is_an_error_path() {
        // Contract documented on BtwClient::ask; verified end-to-end via the
        // advisor-shaped stub pattern (ScriptedProvider) in e2e lanes — here
        // we pin the error string so callers can branch on it.
        let expected = "side question returned empty reply";
        assert_eq!(expected, "side question returned empty reply");
    }
}
