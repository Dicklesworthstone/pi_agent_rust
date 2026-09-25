//! `/btw` ephemeral side questions (bd-cv653.3.16).
//!
//! A side question goes to the session's `smol` role model with a strict
//! contract: answer briefly, never use tools, never ask follow-ups. The
//! exchange is ephemeral **by construction** — the call builds its own
//! throwaway message list and shares nothing with the session writer, so no
//! JSONL entry can ever contain it. Interactive-only by design; there is no
//! `--btw` print-mode flag.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::model::{Message, UserContent, UserMessage};
use crate::provider::Provider;
use crate::text_completion::{MAX_TEXT_BYTES, collect_text, with_timeout};

/// System contract for side questions (omp btw-user.md semantics).
pub const BTW_SYSTEM_PROMPT: &str = "You are answering an ephemeral side question about the \
current work. Rules: answer in at most a few sentences; NEVER use tools; NEVER ask follow-up \
questions; if the context does not contain the answer, say so plainly.";

/// Cap on recent-session text fed into the side question so /btw stays
/// cheap regardless of transcript size.
const CONTEXT_BUDGET_CHARS: usize = 4_000;
const ANSWER_MAX_TOKENS: u32 = 512;
/// One deadline covers connection setup and the complete streamed response.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// Builds `/btw` clients for resolved model entries (bd-9jgrt). Captured by
/// the interactive app so `/model smol <spec>` can rebind mid-session.
pub type BtwClientFactory = std::sync::Arc<
    dyn Fn(&crate::models::ModelEntry) -> Option<std::sync::Arc<BtwClient>> + Send + Sync,
>;

/// One-shot client bound to the resolved `smol` role provider.
pub struct BtwClient {
    provider: Arc<dyn Provider>,
    api_key: Option<String>,
}

impl BtwClient {
    pub fn new(provider: Arc<dyn Provider>, api_key: Option<String>) -> Self {
        Self { provider, api_key }
    }

    /// Resolve provider + credentials for `entry` and build a client using
    /// the startup precedence (`--api-key` > stored auth > inline key).
    /// Returns `None` when credentials are required but missing, or when
    /// the provider cannot be constructed.
    pub fn for_model_entry(
        entry: &crate::models::ModelEntry,
        cli_api_key: Option<&str>,
        auth: &crate::auth::AuthStorage,
    ) -> Option<std::sync::Arc<Self>> {
        let key = crate::models::resolve_model_key(cli_api_key, auth, entry);
        let credentialed =
            !crate::models::model_requires_configured_credential(entry) || key.is_some();
        if !credentialed {
            return None;
        }
        crate::providers::create_provider(entry, None)
            .ok()
            .map(|provider| std::sync::Arc::new(Self::new(provider, key)))
    }

    /// Ask an ephemeral side question with compact context from the current
    /// conversation tail. Returns only a clean, complete answer, never a
    /// preview left behind by a failed or disconnected provider.
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
        with_timeout(ANSWER_TIMEOUT, async {
            let stream = self.provider.stream(&context, &options).await?;
            collect_text(stream, MAX_TEXT_BYTES).await
        })
        .await
        .ok_or_else(|| Error::api("side question timed out"))?
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

    struct ScriptedProvider(Vec<crate::model::StreamEvent>);

    #[allow(clippy::unnecessary_literal_bound)]
    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "btw-test"
        }

        fn api(&self) -> &str {
            "btw-test"
        }

        fn model_id(&self) -> &str {
            "btw-test-model"
        }

        async fn stream(
            &self,
            context: &crate::provider::Context<'_>,
            options: &crate::provider::StreamOptions,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = Result<crate::model::StreamEvent>> + Send>>,
        > {
            assert!(context.tools.is_empty());
            assert_eq!(context.system_prompt.as_deref(), Some(BTW_SYSTEM_PROMPT));
            assert_eq!(context.messages.len(), 1);
            assert_eq!(options.max_tokens, Some(ANSWER_MAX_TOKENS));
            assert_eq!(options.api_key.as_deref(), Some("test-key"));
            Ok(Box::pin(futures::stream::iter(
                self.0.clone().into_iter().map(Ok),
            )))
        }
    }

    fn scripted_client(events: Vec<crate::model::StreamEvent>) -> BtwClient {
        BtwClient::new(Arc::new(ScriptedProvider(events)), Some("test-key".to_string()))
    }

    #[test]
    fn ask_accepts_terminal_only_response() {
        asupersync::test_utils::run_test(|| async {
            let client = scripted_client(vec![crate::model::StreamEvent::Done {
                reason: crate::model::StopReason::Stop,
                message: crate::model::AssistantMessage {
                    content: vec![crate::model::ContentBlock::Text(
                        crate::model::TextContent::new("the answer"),
                    )],
                    ..Default::default()
                },
            }]);
            assert_eq!(client.ask("working context", "why?").await.unwrap(), "the answer");
        });
    }

    #[test]
    fn ask_rejects_disconnected_partial_response() {
        asupersync::test_utils::run_test(|| async {
            let client = scripted_client(vec![crate::model::StreamEvent::TextDelta {
                content_index: 0,
                delta: "unfinished answer".to_string(),
            }]);
            assert!(client.ask("", "why?").await.is_err());
        });
    }

    #[test]
    fn ask_rejects_provider_error_after_partial_response() {
        asupersync::test_utils::run_test(|| async {
            let client = scripted_client(vec![
                crate::model::StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "unfinished answer".to_string(),
                },
                crate::model::StreamEvent::Error {
                    reason: crate::model::StopReason::Error,
                    error: crate::model::AssistantMessage {
                        stop_reason: crate::model::StopReason::Error,
                        error_message: Some("provider unavailable".to_string()),
                        ..Default::default()
                    },
                },
            ]);
            assert!(client
                .ask("", "why?")
                .await
                .unwrap_err()
                .to_string()
                .contains("provider unavailable"));
        });
    }

    #[test]
    fn for_model_entry_builds_client_for_credential_free_provider() {
        let entry = crate::models::ad_hoc_model_entry("ollama", "llama3")
            .expect("ollama ad-hoc entry resolves");
        let auth = crate::auth::AuthStorage::load(
            std::env::temp_dir().join(format!("pi-btw-test-auth-{}.json", std::process::id())),
        )
        .expect("empty auth storage loads");
        let client =
            BtwClient::for_model_entry(&entry, None, &auth).expect("local provider builds");
        // The Arc is the contract callers hold; a deref proves construction.
        let _arc: std::sync::Arc<BtwClient> = client;
    }

    #[test]
    fn for_model_entry_rejects_credentialed_provider_without_key() {
        let mut entry = crate::models::ad_hoc_model_entry("anthropic", "claude-sonnet-4-5")
            .expect("anthropic ad-hoc entry resolves");
        // `ad_hoc_model_entry` loads `Config::auth_path()` itself and stamps
        // whatever that resolves onto the entry. For the global path that
        // includes external-credential auto-detection, so on any machine where
        // Claude Code is logged in the entry arrives already credentialed and
        // the empty `AuthStorage` below never gets a say — the assertion then
        // measured the developer's login rather than the code. Clearing the
        // field is what puts the "without key" back into the test name.
        entry.api_key = None; // ubs:ignore no clone and no loop on this line
        assert!(crate::models::model_requires_configured_credential(&entry));
        let auth = crate::auth::AuthStorage::load(std::env::temp_dir().join(format!(
            "pi-btw-test-auth-empty-{}.json",
            std::process::id()
        )))
        .expect("empty auth storage loads");
        assert!(BtwClient::for_model_entry(&entry, None, &auth).is_none());
    }
}
