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
use crate::text_completion::{
    MAX_INPUT_BYTES, MAX_TEXT_BYTES, OMITTED_INPUT, RequestStop, collect_text, redact_inputs,
    with_timeout,
};

/// System contract for side questions (omp btw-user.md semantics).
pub const BTW_SYSTEM_PROMPT: &str = "You are answering an ephemeral side question about the \
current work. Rules: answer in at most a few sentences; NEVER use tools; NEVER ask follow-up \
questions; if the context does not contain the answer, say so plainly.";

/// Cap on recent-session text fed into the side question so /btw stays
/// cheap regardless of transcript size. The final projection is byte-bounded.
const CONTEXT_BUDGET_CHARS: usize = 4_000;
const MAX_CONTEXT_PIECES: usize = 64;
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
    ///
    /// This tool-free projection always redacts built-in credential shapes,
    /// including when a library caller supplies its own context summary. It
    /// never exports reversible IDs from a disposable secret vault.
    /// Owner cancellation and missing timer authority are reported separately
    /// from timeout, and neither admits a new provider request.
    pub async fn ask(&self, context_summary: &str, question: &str) -> Result<String> {
        let [system_prompt, context_summary, question]: [String; 3] =
            redact_inputs(&[BTW_SYSTEM_PROMPT, context_summary, question])?
                .try_into()
                .map_err(|_| Error::validation("side-question privacy projection changed shape"))?;
        let user_text = if context_summary.is_empty() {
            question
        } else {
            format!("Current work context:\n{context_summary}\n\nSide question: {question}")
        };
        let context = crate::provider::Context {
            system_prompt: Some(system_prompt.into()),
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
        .map_err(|stop| match stop {
            RequestStop::TimedOut => Error::api("side question timed out"),
            RequestStop::Cancelled => Error::api(
                "PI_AUXILIARY_CANCELLED: side question cancelled by its request owner",
            ),
            RequestStop::TimeUnavailable => Error::config(
                "PI_AUXILIARY_TIME_DENIED: side question requires timer authority",
            ),
        })?
    }
}

/// Retain a complete candidate before screening, not a raw prefix that may
/// cut a credential below its detector's minimum length. Stop at a bounded
/// raw scan budget and report an omission instead of sending uninspected data.
fn push_context_piece(
    pieces: &mut Vec<(String, usize)>,
    raw_bytes: &mut usize,
    prefix: &[&str],
    text: &str,
    limit: usize,
) -> bool {
    if pieces.len() >= MAX_CONTEXT_PIECES {
        return false;
    }
    let bytes = prefix
        .iter()
        .try_fold(text.len(), |total, part| total.checked_add(part.len()));
    let next = bytes.and_then(|bytes| raw_bytes.checked_add(bytes));
    let Some(next) = next.filter(|next| *next <= MAX_INPUT_BYTES) else {
        if OMITTED_INPUT.len() <= MAX_INPUT_BYTES.saturating_sub(*raw_bytes) {
            pieces.push((OMITTED_INPUT.to_string(), OMITTED_INPUT.len()));
        }
        return false;
    };
    *raw_bytes = next;
    let mut piece = prefix.concat();
    let prefix_chars = piece.chars().count();
    piece.push_str(text);
    pieces.push((piece, limit.saturating_add(prefix_chars)));
    true
}

/// Compact, credential-redacted context from the live agent message list.
///
/// The newest exchanges are selected first. Their complete selected text is
/// screened together before clipping to the display budget, including text
/// blocks accompanying attachments. Caller-owned transcript data is untouched;
/// oversized source fields are omitted rather than partially disclosed.
#[must_use]
pub fn build_context_summary(messages: &[Message]) -> String {
    let mut pieces = Vec::new();
    let mut raw_bytes = 0usize;
    'messages: for message in messages.iter().rev() {
        match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    if !push_context_piece(&mut pieces, &mut raw_bytes, &["user: "], text, 400) {
                        break;
                    }
                }
                UserContent::Blocks(blocks) => {
                    for block in blocks.iter().rev() {
                        if let crate::model::ContentBlock::Text(text) = block
                            && !push_context_piece(
                                &mut pieces, &mut raw_bytes, &["user: "], &text.text, 400,
                            )
                        {
                            break 'messages;
                        }
                    }
                }
            },
            Message::Assistant(assistant) => {
                for block in assistant.content.iter().rev() {
                    let retained = match block {
                        crate::model::ContentBlock::Text(text) => push_context_piece(
                            &mut pieces, &mut raw_bytes, &["assistant: "], &text.text, 400,
                        ),
                        crate::model::ContentBlock::ToolCall(call) => push_context_piece(
                            &mut pieces, &mut raw_bytes, &["assistant ran tool "], &call.name, 400,
                        ),
                        _ => true,
                    };
                    if !retained {
                        break 'messages;
                    }
                }
            }
            Message::ToolResult(result) => {
                let first = result.content.iter().find_map(|block| match block {
                    crate::model::ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                });
                if !push_context_piece(
                    &mut pieces,
                    &mut raw_bytes,
                    &["tool ", &result.tool_name, ": "],
                    first.unwrap_or(""),
                    160,
                ) {
                    break;
                }
            }
            Message::Custom(_) => {}
        }
    }
    let inputs: Vec<&str> = pieces.iter().map(|(text, _)| text.as_str()).collect();
    let Ok(protected) = redact_inputs(&inputs) else {
        return OMITTED_INPUT.to_string();
    };
    let mut rendered = Vec::new();
    let mut used = 0usize;
    for (piece, (_, limit)) in protected.into_iter().zip(pieces) {
        let piece = truncate(&piece, limit);
        if used + piece.len() + 1 > CONTEXT_BUDGET_CHARS {
            break;
        }
        used += piece.len() + 1;
        rendered.push(piece.to_string());
    }
    rendered.reverse();
    rendered.join("\n")
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

    struct ScriptedProvider(Vec<crate::model::StreamEvent>, Option<String>);

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
            if let Some(expected) = &self.1 {
                let Message::User(user) = &context.messages[0] else {
                    panic!("expected a user prompt");
                };
                let UserContent::Text(text) = &user.content else {
                    panic!("expected a text prompt");
                };
                assert_eq!(text, expected);
            }
            Ok(Box::pin(futures::stream::iter(
                self.0.clone().into_iter().map(Ok),
            )))
        }
    }

    fn scripted_client(events: Vec<crate::model::StreamEvent>) -> BtwClient {
        BtwClient::new(Arc::new(ScriptedProvider(events, None)), Some("test-key".to_string()))
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

    fn user(text: impl Into<String>) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: 0,
        })
    }

    #[test]
    fn summary_screens_whole_fields_before_clipping_and_cross_message_discovery() {
        let secret = "r4nd0mCredentialValue123456";
        let assignment = format!("{} API_KEY={secret}", "x".repeat(500));
        let messages = vec![user(assignment), user(format!("echo {secret}"))];
        let summary = build_context_summary(&messages);
        assert!(!summary.contains(secret));
        assert!(summary.contains("echo <pi-secret:redacted>"));
        let clipped = build_context_summary(&[user(format!(
            "{}sk-abcdefghijklmnopqrstuvwxyz012345", "x".repeat(385),
        ))]);
        assert!(!clipped.contains("sk-"), "raw key prefix must not survive clipping");
        assert!(clipped.len() <= CONTEXT_BUDGET_CHARS);
    }

    #[test]
    fn summary_includes_attachment_text_blocks_in_chronological_order() {
        let messages = vec![
            user("older"),
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    crate::model::ContentBlock::Text(crate::model::TextContent::new("first caption")),
                    crate::model::ContentBlock::Text(crate::model::TextContent::new("second caption")),
                ]),
                timestamp: 0,
            }),
        ];
        assert_eq!(
            build_context_summary(&messages),
            "user: older\nuser: first caption\nuser: second caption",
        );
    }

    #[test]
    fn oversized_context_is_omitted_without_discarding_newer_safe_context() {
        let huge = format!("PRIVATE-PREFIX{}", "x".repeat(MAX_INPUT_BYTES));
        let summary = build_context_summary(&[user(huge), user("newest context")]);
        assert!(!summary.contains("PRIVATE-PREFIX"));
        assert!(summary.contains(OMITTED_INPUT));
        assert!(summary.ends_with("user: newest context"));
        assert!(summary.len() <= CONTEXT_BUDGET_CHARS);
        let unicode = vec![user("🦀".repeat(1000)); 100];
        assert!(build_context_summary(&unicode).len() <= CONTEXT_BUDGET_CHARS);
    }

    #[test]
    fn ask_screens_direct_context_and_question_but_preserves_authentication() {
        asupersync::test_utils::run_test(|| async {
            let secret = "r4nd0mCredentialValue123456";
            let question = format!("Does API_KEY={secret} match?");
            let expected = "Current work context:\necho <pi-secret:redacted>\n\nSide question: Does API_KEY=<pi-secret:redacted> match?";
            let client = BtwClient::new(
                Arc::new(ScriptedProvider(
                    vec![crate::model::StreamEvent::Done {
                        reason: crate::model::StopReason::Stop,
                        message: crate::model::AssistantMessage {
                            content: vec![crate::model::ContentBlock::Text(
                                crate::model::TextContent::new("values omitted"),
                            )],
                            ..Default::default()
                        },
                    }],
                    Some(expected.to_string()),
                )),
                Some("test-key".to_string()),
            );
            assert_eq!(client.ask(&format!("echo {secret}"), &question).await.unwrap(), "values omitted");
        });
    }

    #[test]
    fn oversized_direct_question_is_refused_before_provider_admission() {
        asupersync::test_utils::run_test(|| async {
            let client = BtwClient::new(
                Arc::new(ScriptedProvider(Vec::new(), Some("must not be invoked".to_string()))),
                Some("test-key".to_string()),
            );
            let error = client.ask("", &"x".repeat(MAX_INPUT_BYTES + 1)).await.unwrap_err();
            assert!(error.to_string().contains("PI_AUXILIARY_INPUT_LIMIT"));
        });
    }

    #[test]
    fn cancelled_side_question_is_not_reported_as_timeout_or_sent_to_provider() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = crate::agent_cx::AgentCx::for_request();
        owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel side question"));
        let client = BtwClient::new(
            Arc::new(ScriptedProvider(Vec::new(), Some("must not be invoked".to_string()))),
            Some("test-key".to_string()),
        );
        let error = runtime
            .block_on(owner.with_current(client.ask("", "why?")))
            .unwrap_err();
        assert!(error.to_string().contains("PI_AUXILIARY_CANCELLED"));
        assert!(!error.to_string().contains("timed out"));
    }

    #[test]
    fn timerless_side_question_is_refused_before_provider_admission() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = {
            let _guard = asupersync::Cx::for_request()
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            crate::agent_cx::AgentCx::for_current_or_request()
        };
        let client = BtwClient::new(
            Arc::new(ScriptedProvider(Vec::new(), Some("must not be invoked".to_string()))),
            Some("test-key".to_string()),
        );
        let error = runtime
            .block_on(owner.with_current(client.ask("", "why?")))
            .unwrap_err();
        assert!(error.to_string().contains("PI_AUXILIARY_TIME_DENIED"));
    }
}
