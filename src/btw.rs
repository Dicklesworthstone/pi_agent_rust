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
use crate::model::{Message, ThinkingLevel, UserContent, UserMessage};
use crate::provider::{Provider, StreamOptions};
use crate::text_completion::{
    AuxiliaryPrivacy, MAX_INPUT_BYTES, MAX_TEXT_BYTES, OMITTED_INPUT, RequestStop, collect_text,
    redact_inputs, with_timeout,
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
    options: StreamOptions,
    credential_binding: Option<ModelCredentialBinding>,
    privacy: Arc<AuxiliaryPrivacy>,
}

/// Retain credential provenance separately from the last resolved key. The
/// supplied auth snapshot remains authoritative, including unsaved SDK
/// credentials; each request rechecks its live environment and pool cooldown.
/// Native role rebinding can supply a refreshed store without silently
/// replacing an embedder's credential scope with a disk read.
struct ModelCredentialBinding {
    entry: crate::models::ModelEntry,
    auth: crate::auth::AuthStorage,
    override_key: Option<String>,
}

/// A bounded, screened side request ready for background execution. Fields
/// remain private so callers cannot bypass preparation with unchecked text.
pub struct PreparedBtwRequest {
    system_prompt: String,
    context_summary: String,
    question: String,
    privacy: Arc<AuxiliaryPrivacy>,
    cost_budget: Option<crate::cost::CostBudget>,
}

impl BtwClient {
    pub fn new(provider: Arc<dyn Provider>, api_key: Option<String>) -> Self {
        Self {
            provider,
            options: StreamOptions {
                api_key,
                max_tokens: Some(ANSWER_MAX_TOKENS),
                ..Default::default()
            },
            credential_binding: None,
            privacy: Arc::new(AuxiliaryPrivacy::default()),
        }
    }

    #[must_use]
    pub fn with_secrets_settings(
        mut self,
        settings: Option<&crate::secrets::SecretsSettings>,
    ) -> Self {
        self.privacy = Arc::new(AuxiliaryPrivacy::from_settings(settings));
        self
    }

    /// Resolve the side model's provider, options and credentials. The
    /// foreground `--api-key` belongs to its original provider and endpoint;
    /// another destination resolves its own stored or configured credential.
    /// Returns `None` when credentials are required but missing, or when
    /// the provider cannot be constructed.
    pub fn for_model_entry(
        entry: &crate::models::ModelEntry,
        primary_entry: &crate::models::ModelEntry,
        cli_api_key: Option<&str>,
        auth: &crate::auth::AuthStorage,
        secrets: Option<&crate::secrets::SecretsSettings>,
    ) -> Option<std::sync::Arc<Self>> {
        // Match the native title client's destination identity. Provider
        // aliases share credentials only when the configured endpoint also
        // matches; rebinding the smol role cannot retarget a foreground key.
        let same_destination = crate::provider_metadata::provider_ids_match(
            &entry.model.provider,
            &primary_entry.model.provider,
        ) && entry.model.base_url == primary_entry.model.base_url;
        let override_key = cli_api_key.filter(|_| same_destination);
        let key = crate::models::resolve_model_key(override_key, auth, entry).ok()?;
        let credentialed =
            !crate::models::model_requires_configured_credential(entry) || key.is_some();
        if !credentialed {
            return None;
        }
        crate::providers::create_provider_with_auth(entry, None, Some(auth))
            .ok()
            .map(|provider| {
                let mut client = Self::new(provider, key).with_secrets_settings(secrets);
                client.options.headers.clone_from(&entry.headers);
                client.options.max_tokens = Some(entry.model.max_tokens.min(ANSWER_MAX_TOKENS));
                client.options.thinking_level =
                    Some(entry.clamp_thinking_level(ThinkingLevel::Minimal));
                client.credential_binding = Some(ModelCredentialBinding {
                    entry: entry.clone(),
                    auth: auth.clone(),
                    override_key: override_key.map(str::to_string),
                });
                std::sync::Arc::new(client)
            })
    }

    /// Ask an ephemeral side question with compact context from the current
    /// conversation tail. Returns only a clean, complete answer, never a
    /// preview left behind by a failed or disconnected provider.
    ///
    /// Configured patterns apply even when a library caller supplies its own
    /// context summary, and block mode refuses before provider admission.
    /// This never exports reversible IDs from a disposable secret vault.
    /// Owner cancellation and missing timer authority are reported separately
    /// from timeout, and neither admits a new provider request.
    pub async fn ask(&self, context_summary: &str, question: &str) -> Result<String> {
        let [system_prompt, context_summary, question]: [String; 3] = redact_inputs(
            &[BTW_SYSTEM_PROMPT, context_summary, question],
            &self.privacy,
        )?
        .try_into()
        .map_err(|_| Error::validation("side-question privacy projection changed shape"))?;
        self.ask_prepared(PreparedBtwRequest {
            system_prompt,
            context_summary,
            question,
            privacy: Arc::clone(&self.privacy),
            cost_budget: self.provider.cost_budget().cloned(),
        })
        .await
    }

    /// Screen complete selected transcript fields and the side question in
    /// one projection before clipping context. A credential discovered after
    /// the display cutoff still protects earlier echoes or refuses the call.
    pub async fn ask_with_messages(&self, messages: &[Message], question: &str) -> Result<String> {
        self.ask_prepared(self.prepare_with_messages(messages, question)?)
            .await
    }

    /// Prepare only bounded text while the host borrows the live transcript.
    /// Attachments and the full conversation never move to the background task.
    pub fn prepare_with_messages(
        &self,
        messages: &[Message],
        question: &str,
    ) -> Result<PreparedBtwRequest> {
        self.prepare_projected(messages, question, |inputs| {
            redact_inputs(inputs, &self.privacy)
        })
    }

    /// Preserve the live agent's knowledge of bare credential values, even
    /// when their original assignments are outside the selected context.
    pub fn prepare_with_agent(
        &self,
        agent: &crate::agent::Agent,
        question: &str,
    ) -> Result<PreparedBtwRequest> {
        let mut request = self.prepare_projected(agent.messages(), question, |inputs| {
            agent.project_auxiliary_inputs(inputs, &self.privacy)
        })?;
        request.cost_budget = agent.cost_budget().cloned();
        Ok(request)
    }

    fn prepare_projected(
        &self,
        messages: &[Message],
        question: &str,
        screen: impl FnOnce(&[&str]) -> Result<Vec<String>>,
    ) -> Result<PreparedBtwRequest> {
        let reserved_bytes = BTW_SYSTEM_PROMPT
            .len()
            .checked_add(question.len())
            .filter(|bytes| *bytes <= MAX_INPUT_BYTES)
            .ok_or_else(|| {
                Error::validation(
                    "PI_AUXILIARY_INPUT_LIMIT: input exceeds the auxiliary privacy scan budget",
                )
            })?;
        let pieces = collect_context_pieces(messages, reserved_bytes);
        let inputs: Vec<&str> = std::iter::once(BTW_SYSTEM_PROMPT)
            .chain(pieces.iter().flat_map(ContextPiece::inputs))
            .chain(std::iter::once(question))
            .collect();
        let protected = screen(&inputs)?;
        if protected.len() != inputs.len() {
            return Err(Error::validation(
                "side-question privacy projection changed shape",
            ));
        }
        let mut protected = protected.into_iter();
        let system_prompt = protected.next().ok_or_else(|| {
            Error::validation("side-question privacy projection lost system text")
        })?;
        let question = protected
            .next_back()
            .ok_or_else(|| Error::validation("side-question privacy projection lost question"))?;
        Ok(PreparedBtwRequest {
            system_prompt,
            context_summary: render_context_summary(pieces, protected),
            question,
            privacy: Arc::clone(&self.privacy),
            cost_budget: self.provider.cost_budget().cloned(),
        })
    }

    /// Execute a previously screened request without scanning its redaction
    /// markers again. Preparation is bound to this client's privacy policy.
    pub async fn ask_prepared(&self, request: PreparedBtwRequest) -> Result<String> {
        if !Arc::ptr_eq(&self.privacy, &request.privacy) {
            return Err(Error::validation(
                "PI_AUXILIARY_POLICY_MISMATCH: side question was prepared by another client",
            ));
        }
        let PreparedBtwRequest {
            system_prompt,
            context_summary,
            question,
            cost_budget,
            ..
        } = request;
        let provider = cost_budget.map_or_else(
            || {
                self.provider
                    .unguarded_provider()
                    .unwrap_or_else(|| Arc::clone(&self.provider))
            },
            |budget| crate::provider::guard_with_cost_budget(Arc::clone(&self.provider), budget),
        );
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
        with_timeout(ANSWER_TIMEOUT, async {
            let mut options = self.options.clone();
            if let Some(binding) = &self.credential_binding {
                options.api_key = crate::models::resolve_model_key(
                    binding.override_key.as_deref(),
                    &binding.auth,
                    &binding.entry,
                )?;
            }
            let stream = provider.stream(&context, &options).await?;
            collect_text(stream, MAX_TEXT_BYTES).await
        })
        .await
        .map_err(|stop| match stop {
            RequestStop::TimedOut => Error::api("side question timed out"),
            RequestStop::Cancelled => {
                Error::api("PI_AUXILIARY_CANCELLED: side question cancelled by its request owner")
            }
            RequestStop::TimeUnavailable => {
                Error::config("PI_AUXILIARY_TIME_DENIED: side question requires timer authority")
            }
        })?
    }
}

/// Retain a complete candidate before screening, not a raw prefix that may
/// cut a credential below its detector's minimum length. Stop at a bounded
/// raw scan budget and report an omission instead of sending uninspected data.
struct ContextPiece {
    prefix: &'static str,
    tool_name: Option<String>,
    text: String,
    limit: ContextLimit,
}

enum ContextLimit {
    Chars(usize),
    Remaining,
}

impl ContextPiece {
    fn inputs(&self) -> impl Iterator<Item = &str> {
        self.tool_name
            .as_deref()
            .into_iter()
            .chain(std::iter::once(self.text.as_str()))
    }
}

fn push_context_piece(
    pieces: &mut Vec<ContextPiece>,
    raw_bytes: &mut usize,
    prefix: &'static str,
    tool_name: Option<&str>,
    text: &str,
    limit: ContextLimit,
) -> bool {
    if pieces.len() >= MAX_CONTEXT_PIECES {
        return false;
    }
    let bytes = text
        .len()
        .checked_add(prefix.len())
        .and_then(|bytes| bytes.checked_add(tool_name.map_or(0, str::len)))
        .and_then(|bytes| bytes.checked_add(if tool_name.is_some() { 2 } else { 0 }));
    let next = bytes.and_then(|bytes| raw_bytes.checked_add(bytes));
    let Some(next) = next.filter(|next| *next <= MAX_INPUT_BYTES) else {
        if OMITTED_INPUT.len() <= MAX_INPUT_BYTES.saturating_sub(*raw_bytes) {
            pieces.push(ContextPiece {
                prefix: "",
                tool_name: None,
                text: OMITTED_INPUT.to_string(),
                limit: ContextLimit::Chars(OMITTED_INPUT.len()),
            });
        }
        return false;
    };
    *raw_bytes = next;
    pieces.push(ContextPiece {
        prefix,
        tool_name: tool_name.map(str::to_string),
        text: text.to_string(),
        limit,
    });
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
    let pieces = collect_context_pieces(messages, 0);
    let inputs: Vec<&str> = pieces.iter().flat_map(ContextPiece::inputs).collect();
    let Ok(protected) = redact_inputs(&inputs, &AuxiliaryPrivacy::default()) else {
        return OMITTED_INPUT.to_string();
    };
    render_context_summary(pieces, protected)
}

fn collect_context_pieces(messages: &[Message], reserved_bytes: usize) -> Vec<ContextPiece> {
    let mut pieces = Vec::new();
    let mut raw_bytes = reserved_bytes;
    'messages: for message in messages.iter().rev() {
        match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    if !push_context_piece(
                        &mut pieces,
                        &mut raw_bytes,
                        "user: ",
                        None,
                        text,
                        ContextLimit::Chars(400),
                    ) {
                        break;
                    }
                }
                UserContent::Blocks(blocks) => {
                    for block in blocks.iter().rev() {
                        if let crate::model::ContentBlock::Text(text) = block
                            && !push_context_piece(
                                &mut pieces,
                                &mut raw_bytes,
                                "user: ",
                                None,
                                &text.text,
                                ContextLimit::Chars(400),
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
                            &mut pieces,
                            &mut raw_bytes,
                            "assistant: ",
                            None,
                            &text.text,
                            ContextLimit::Chars(400),
                        ),
                        crate::model::ContentBlock::ToolCall(call) => push_context_piece(
                            &mut pieces,
                            &mut raw_bytes,
                            "assistant ran tool ",
                            None,
                            &call.name,
                            ContextLimit::Chars(400),
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
                    "tool ",
                    Some(&result.tool_name),
                    first.unwrap_or(""),
                    ContextLimit::Chars(160),
                ) {
                    break;
                }
            }
            Message::Custom(custom) if crate::compaction_snap::is_archive_message(message) => {
                // Side requests are text-only regardless of the main model.
                // Screen the complete retained source before this client's
                // normal context clipping; never copy image bytes to it.
                let text = crate::compaction_snap::text_for_custom(custom);
                if !push_context_piece(
                    &mut pieces,
                    &mut raw_bytes,
                    "compacted history: ",
                    None,
                    &text,
                    ContextLimit::Remaining,
                ) {
                    break;
                }
            }
            Message::Custom(_) => {}
        }
    }
    pieces
}

fn render_context_summary(
    pieces: Vec<ContextPiece>,
    protected: impl IntoIterator<Item = String>,
) -> String {
    let mut protected = protected.into_iter();
    let mut rendered = Vec::new();
    let mut used = 0usize;
    for piece in pieces {
        let mut text = piece.prefix.to_string();
        if piece.tool_name.is_some() {
            let Some(tool_name) = protected.next() else {
                return OMITTED_INPUT.to_string();
            };
            text.push_str(&tool_name);
            text.push_str(": ");
        }
        let prefix_chars = text.chars().count();
        let Some(content) = protected.next() else {
            return OMITTED_INPUT.to_string();
        };
        text.push_str(&content);
        let piece = match piece.limit {
            ContextLimit::Chars(limit) => {
                truncate(&text, limit.saturating_add(prefix_chars)).to_string()
            }
            ContextLimit::Remaining => {
                let available = CONTEXT_BUDGET_CHARS.saturating_sub(used + 1);
                if text.len() <= available {
                    text
                } else {
                    const OMITTED: &str = " [Remaining archived text omitted.]";
                    if available <= prefix_chars + OMITTED.len() {
                        break;
                    }
                    let mut end = available.saturating_sub(OMITTED.len());
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}{OMITTED}", &text[..end])
                }
            }
        };
        if used + piece.len() + 1 > CONTEXT_BUDGET_CHARS {
            break;
        }
        used += piece.len() + 1;
        rendered.push(piece);
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

    #[derive(Default)]
    struct RecordingProvider {
        calls: std::sync::atomic::AtomicUsize,
        prompts: std::sync::Mutex<Vec<String>>,
    }

    #[allow(clippy::unnecessary_literal_bound)]
    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        fn name(&self) -> &str {
            "btw-privacy-test"
        }

        fn api(&self) -> &str {
            "btw-privacy-test"
        }

        fn model_id(&self) -> &str {
            "btw-privacy-model"
        }

        async fn stream(
            &self,
            context: &crate::provider::Context<'_>,
            options: &crate::provider::StreamOptions,
        ) -> Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = Result<crate::model::StreamEvent>> + Send>,
            >,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(options.api_key.as_deref(), Some("test-key"));
            assert!(context.tools.is_empty());
            let Message::User(user) = &context.messages[0] else {
                panic!("expected side-question user input");
            };
            let UserContent::Text(text) = &user.content else {
                panic!("expected bounded side-question text");
            };
            self.prompts.lock().unwrap().push(text.clone());
            Ok(Box::pin(futures::stream::iter([Ok(
                crate::model::StreamEvent::Done {
                    reason: crate::model::StopReason::Stop,
                    message: crate::model::AssistantMessage {
                        content: vec![crate::model::ContentBlock::Text(
                            crate::model::TextContent::new("safe answer"),
                        )],
                        ..Default::default()
                    },
                },
            )])))
        }
    }

    fn privacy_client(mode: &str) -> (BtwClient, Arc<RecordingProvider>) {
        let provider = Arc::new(RecordingProvider::default());
        let client = BtwClient::new(provider.clone(), Some("test-key".to_string()))
            .with_secrets_settings(Some(&crate::secrets::SecretsSettings {
                mode: Some(mode.to_string()),
                extra_patterns: Some(vec![r"ACME-\d{6}".to_string()]),
            }));
        (client, provider)
    }

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
            std::pin::Pin<
                Box<dyn futures::Stream<Item = Result<crate::model::StreamEvent>> + Send>,
            >,
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
        BtwClient::new(
            Arc::new(ScriptedProvider(events, None)),
            Some("test-key".to_string()),
        )
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
            assert_eq!(
                client.ask("working context", "why?").await.unwrap(),
                "the answer"
            );
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
            assert!(
                client
                    .ask("", "why?")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("provider unavailable")
            );
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
        let client = BtwClient::for_model_entry(&entry, &entry, None, &auth, None)
            .expect("local provider builds");
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
        assert!(BtwClient::for_model_entry(&entry, &entry, None, &auth, None).is_none());
    }

    fn role_entry(provider: &str, base_url: &str, key: Option<&str>) -> crate::models::ModelEntry {
        crate::models::ModelEntry {
            model: crate::provider::Model {
                id: "side-fixture".to_string(),
                name: "Side fixture".to_string(),
                api: "openai-completions".to_string(),
                provider: provider.to_string(),
                base_url: base_url.to_string(),
                reasoning: false,
                input: vec![crate::provider::InputType::Text],
                cost: crate::provider::ModelCost::default(),
                context_window: 8_192,
                max_tokens: 2_048,
                headers: std::collections::HashMap::new(),
            },
            api_key: key.map(str::to_string),
            headers: std::collections::HashMap::from([(
                "x-side-route".to_string(),
                "smol".to_string(),
            )]),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    }

    type CapturedSideRequest = (std::collections::HashMap<String, String>, serde_json::Value);

    /// Exercise the real model factory, request serialization and SSE reader;
    /// every socket and accept is bounded even if admission unexpectedly fails.
    fn side_peer() -> (String, std::thread::JoinHandle<CapturedSideRequest>) {
        use std::io::{BufRead, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("side peer");
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            let mut socket = loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "side request deadline"
                );
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("side peer accept: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(&mut socket);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "POST /v1/chat/completions HTTP/1.1\r\n");
            let mut bytes = line.len();
            let mut headers = std::collections::HashMap::new();
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                bytes += line.len();
                assert!(bytes <= 64 * 1024, "side request headers are bounded");
                if line == "\r\n" {
                    break;
                }
                let (name, value) = line.split_once(':').expect("header");
                headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
            }
            let length: usize = headers["content-length"].parse().unwrap();
            assert!(length <= 64 * 1024, "side request body is bounded");
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            drop(reader);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let start = serde_json::json!({
                "id": "side-response", "object": "chat.completion.chunk",
                "created": 0, "model": "side-fixture",
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Side answer"}, "finish_reason": null}],
            });
            let end = serde_json::json!({
                "id": "side-response", "object": "chat.completion.chunk",
                "created": 0, "model": "side-fixture",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 8, "completion_tokens": 3, "total_tokens": 11},
            });
            let response = format!("data: {start}\n\ndata: {end}\n\ndata: [DONE]\n\n");
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            socket.flush().unwrap();
            (headers, body)
        });
        (base_url, worker)
    }

    #[test]
    fn side_model_wire_preserves_destination_credentials_headers_and_supported_options() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("side wire runtime");
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut auth = crate::auth::AuthStorage::empty_at(directory.path().join("auth.json"));
            auth.set(
                "btw-secondary",
                crate::auth::AuthCredential::ApiKey {
                    key: "side-stored-key".to_string(),
                },
            );
            for case in [
                "same",
                "other-provider",
                "other-endpoint",
                "canonical-alias",
            ] {
                let (base_url, peer) = side_peer();
                let mut primary = role_entry("btw-primary", &base_url, None);
                let mut role = role_entry("btw-primary", &base_url, Some("side-inline-key"));
                let expected_key = match case {
                    "other-provider" => {
                        role.model.provider = "btw-secondary".to_string();
                        "side-stored-key"
                    }
                    "other-endpoint" => {
                        primary.model.base_url.push_str("/foreground");
                        "side-inline-key"
                    }
                    "canonical-alias" => {
                        primary.model.provider = "openrouter".to_string();
                        role.model.provider = "open-router".to_string();
                        "foreground-key"
                    }
                    _ => "foreground-key",
                };
                if case == "other-endpoint" {
                    role.model.reasoning = true;
                    role.model.max_tokens = 128;
                    role.compat = Some(crate::models::CompatConfig {
                        supports_reasoning_effort: Some(true),
                        reasoning_required: Some(true),
                        thinking_level_map: Some(std::collections::HashMap::from([
                            ("off".to_string(), None),
                            ("minimal".to_string(), None),
                            ("low".to_string(), Some("low".to_string())),
                        ])),
                        ..Default::default()
                    });
                }
                let client = BtwClient::for_model_entry(
                    &role,
                    &primary,
                    Some(" foreground-key "),
                    &auth,
                    None,
                )
                .expect("authorized side destination");
                assert_eq!(
                    client.ask("current work", "why?").await.unwrap(),
                    "Side answer"
                );
                let (headers, body) = peer.join().expect("completed side request");
                assert_eq!(headers["authorization"], format!("Bearer {expected_key}"));
                assert_eq!(headers["x-side-route"], "smol");
                assert_eq!(body["model"], "side-fixture");
                assert_eq!(
                    body["max_tokens"],
                    role.model.max_tokens.min(ANSWER_MAX_TOKENS)
                );
                if case == "other-endpoint" {
                    assert_eq!(body["reasoning_effort"], "low");
                } else {
                    assert!(body.get("reasoning_effort").is_none());
                }
                let serialized = body.to_string();
                for key in ["foreground-key", "side-stored-key", "side-inline-key"] {
                    assert!(
                        !serialized.contains(key),
                        "credentials stay out of model context"
                    );
                }
            }
        });
    }

    #[test]
    fn unrelated_side_roles_require_their_own_credentials_but_local_roles_still_work() {
        let directory = tempfile::tempdir().unwrap();
        let auth = crate::auth::AuthStorage::empty_at(directory.path().join("auth.json"));
        let primary = role_entry("btw-primary", "https://primary.invalid/v1", None);
        for (provider, url) in [
            ("btw-secondary", "https://primary.invalid/v1"),
            ("btw-primary", "https://secondary.invalid/v1"),
        ] {
            let role = role_entry(provider, url, None);
            assert!(
                BtwClient::for_model_entry(&role, &primary, Some("foreground-key"), &auth, None)
                    .is_none()
            );
            let mut local = role;
            local.auth_header = false;
            let client =
                BtwClient::for_model_entry(&local, &primary, Some("foreground-key"), &auth, None)
                    .expect("credential-free side role");
            assert!(client.options.api_key.is_none());
        }
    }

    #[test]
    fn retained_model_client_rechecks_exhaustion_without_overriding_explicit_keys() {
        const CHILD: &str = "PI_BTW_CREDENTIAL_ADMISSION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "btw::tests::retained_model_client_rechecks_exhaustion_without_overriding_explicit_keys",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("OPENAI_API_KEYS", "pool-key-one,pool-key-two")
                .env("OPENAI_API_KEY", "singular-bypass")
                .output()
                .expect("run isolated side-question credential fixture");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }

        crate::auth::reset_credential_rings_for_tests();
        asupersync::test_utils::run_test(|| async {
            let directory = tempfile::tempdir().unwrap();
            let mut auth = crate::auth::AuthStorage::empty_at(directory.path().join("auth.json"));
            auth.set(
                "openai",
                crate::auth::AuthCredential::ApiKey {
                    key: "stored-bypass".to_string(),
                },
            );
            let mut entry = crate::models::ad_hoc_model_entry("openai", "gpt-4o-mini")
                .expect("known OpenAI model");
            entry.api_key = Some("catalog-bypass".to_string());
            let provider = Arc::new(RecordingProvider::default());
            let mut client = BtwClient::for_model_entry(&entry, &entry, None, &auth, None)
                .expect("pool is initially available");
            Arc::get_mut(&mut client).unwrap().provider = provider.clone();
            let request = client.prepare_with_messages(&[], "why?").unwrap();

            // A foreground request can exhaust the pool after the client and
            // even its screened request were prepared.
            crate::auth::report_provider_rate_limit("openai", "pool-key-one");
            crate::auth::report_provider_rate_limit("openai", "pool-key-two");
            let error = client.ask_prepared(request).await.unwrap_err();
            assert!(crate::failover::marks_credential_pool_exhausted(
                &error.to_string()
            ));
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);

            let direct = BtwClient::new(provider.clone(), Some("test-key".to_string()));
            assert_eq!(direct.ask("", "why?").await.unwrap(), "safe answer");
            let mut pinned =
                BtwClient::for_model_entry(&entry, &entry, Some("test-key"), &auth, None)
                    .expect("explicit override remains available");
            Arc::get_mut(&mut pinned).unwrap().provider = provider.clone();
            assert_eq!(pinned.ask("", "why?").await.unwrap(), "safe answer");
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        });
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
        // The key starts at the same offset as before; the padding ends in a
        // space because ruleset v5 does not match `sk-` directly after a letter.
        let clipped = build_context_summary(&[user(format!(
            "{} sk-abcdefghijklmnopqrstuvwxyz012345",
            "x".repeat(384),
        ))]);
        assert!(
            !clipped.contains("sk-"),
            "raw key prefix must not survive clipping"
        );
        assert!(clipped.len() <= CONTEXT_BUDGET_CHARS);
    }

    #[test]
    fn side_context_reads_archives_within_remaining_budget_after_full_source_screening() {
        let secret = "r4nd0mCredentialValue123456";
        let source = format!(
            "ARCHIVED-DECISION echo {secret}\n{}\nAPI_KEY={secret}",
            "caf\u{e9} \u{65e5}\u{672c}\n".repeat(600),
        );
        let archive = crate::compaction_snap::attach_frames(
            user("short checkpoint"),
            Some(
                &crate::compaction_snap::SnapPayload::new(crate::compaction_snap::render_frames(
                    &source,
                ))
                .with_source_text(source),
            ),
        );
        let canonical = serde_json::to_value(&archive).unwrap();
        let summary = build_context_summary(&[archive.clone(), user("newest context")]);
        assert!(summary.contains("ARCHIVED-DECISION echo <pi-secret:redacted>"));
        assert!(!summary.contains(secret));
        assert!(summary.contains("Remaining archived text omitted"));
        assert!(summary.ends_with("user: newest context"));
        assert!(summary.len() <= CONTEXT_BUDGET_CHARS);
        assert!(!summary.contains("sourceText") && !summary.contains("image/png"));
        assert_eq!(serde_json::to_value(&archive).unwrap(), canonical);
    }

    #[test]
    fn summary_includes_attachment_text_blocks_in_chronological_order() {
        let messages = vec![
            user("older"),
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    crate::model::ContentBlock::Text(crate::model::TextContent::new(
                        "first caption",
                    )),
                    crate::model::ContentBlock::Text(crate::model::TextContent::new(
                        "second caption",
                    )),
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
            assert_eq!(
                client
                    .ask(&format!("echo {secret}"), &question)
                    .await
                    .unwrap(),
                "values omitted"
            );
        });
    }

    #[test]
    fn configured_side_questions_screen_complete_fields_before_clipping() {
        asupersync::test_utils::run_test(|| async {
            for mode in ["obfuscate", "off"] {
                let (client, provider) = privacy_client(mode);
                let echo = "r4nd0mCredentialValue123456";
                let messages = vec![
                    user(format!("{}ACME-654321", "x".repeat(391))),
                    user(format!("echo {echo}")),
                ];
                let original = serde_json::to_value(&messages).unwrap();
                let question = format!("Does API_KEY={echo} match ACME-123456?");
                assert_eq!(
                    client
                        .ask_with_messages(&messages, &question)
                        .await
                        .unwrap(),
                    "safe answer",
                );
                assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
                let prompts = provider.prompts.lock().unwrap();
                assert!(
                    !prompts[0].contains("ACME-"),
                    "no clipped credential prefix"
                );
                assert!(
                    !prompts[0].contains(echo),
                    "discovery spans question and history"
                );
                assert!(prompts[0].contains("echo <pi-secret:redacted>"));
                assert!(prompts[0].contains("match <pi-secret:redacted>?"));
                drop(prompts);
                assert_eq!(serde_json::to_value(&messages).unwrap(), original);
            }
        });
    }

    #[test]
    fn block_policy_refuses_direct_and_history_requests_before_provider_admission() {
        asupersync::test_utils::run_test(|| async {
            let (client, provider) = privacy_client("block");
            for (context, question) in [
                ("", "What is ACME-123456?"),
                ("ACME-123456", "explain this context"),
                ("", "API_KEY=r4nd0mCredentialValue123456"),
            ] {
                let error = client.ask(context, question).await.unwrap_err();
                assert!(error.to_string().contains("PI_SECRET_BLOCK"));
                assert!(!error.to_string().contains("ACME-123456"));
                assert!(!error.to_string().contains("r4nd0mCredentialValue123456"));
            }
            for secret in ["ACME-123456", "API_KEY=r4nd0mCredentialValue123456"] {
                let messages = vec![user(format!("{} {secret}", "x".repeat(500)))];
                let error = client
                    .ask_with_messages(&messages, "explain the earlier work")
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("PI_SECRET_BLOCK"));
            }
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(provider.prompts.lock().unwrap().is_empty());
            assert_eq!(
                client.ask("clean context", "why?").await.unwrap(),
                "safe answer"
            );
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        });
    }

    #[test]
    #[allow(clippy::significant_drop_tightening)]
    fn anchored_patterns_apply_to_source_text_before_display_labels() {
        asupersync::test_utils::run_test(|| async {
            for mode in ["obfuscate", "block"] {
                let provider = Arc::new(RecordingProvider::default());
                let client = BtwClient::new(provider.clone(), Some("test-key".to_string()))
                    .with_secrets_settings(Some(&crate::secrets::SecretsSettings {
                        mode: Some(mode.to_string()),
                        extra_patterns: Some(vec![r"^ACME-\d{6}$".to_string()]),
                    }));
                let messages = vec![user("ACME-123456")];
                let answer = client.ask_with_messages(&messages, "explain").await;
                if mode == "block" {
                    assert!(answer.unwrap_err().to_string().contains("PI_SECRET_BLOCK"));
                    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
                } else {
                    assert_eq!(answer.unwrap(), "safe answer");
                    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
                    let prompts = provider.prompts.lock().unwrap();
                    assert!(prompts[0].contains("user: <pi-secret:redacted>"));
                    assert!(!prompts[0].contains("ACME-"));
                }
            }
        });
    }

    #[test]
    fn live_preparation_keeps_prior_vault_discoveries_and_block_mode() {
        asupersync::test_utils::run_test(|| async {
            let (client, provider) = privacy_client("obfuscate");
            let mut agent = crate::agent::Agent::new(
                provider.clone(),
                crate::tools::ToolRegistry::from_tools(Vec::new()),
                crate::agent::AgentConfig::default(),
            );
            let known = "rememberedCredentialValue123456";
            agent
                .secrets_transform_outbound_text(&format!("API_KEY={known}"))
                .unwrap();
            // The defining assignment has already fallen out of the history.
            agent.replace_messages(vec![user(format!("echo {known}"))]);
            let request = client.prepare_with_agent(&agent, "why?").unwrap();
            assert_eq!(client.ask_prepared(request).await.unwrap(), "safe answer");
            let prompts = provider.prompts.lock().unwrap();
            assert!(!prompts[0].contains(known));
            assert!(prompts[0].contains("echo <pi-secret:redacted>"));
            assert!(!prompts[0].contains("<pi-secret:000001>"));
            drop(prompts);
            let (block_client, block_provider) = privacy_client("block");
            let error = block_client
                .prepare_with_agent(&agent, "why?")
                .err()
                .expect("known bare credential must block before clipping");
            assert!(error.to_string().contains("PI_SECRET_BLOCK"));
            assert_eq!(
                block_provider
                    .calls
                    .load(std::sync::atomic::Ordering::SeqCst),
                0
            );
            assert_eq!(agent.mask_secrets_text(known), "<pi-secret:000001>");
        });
    }

    #[test]
    #[allow(clippy::float_cmp)] // Exact fixture amounts are copied, not calculated.
    fn prepared_side_request_keeps_its_original_session_budget() {
        asupersync::test_utils::run_test(|| async {
            let (client, provider) = privacy_client("obfuscate");
            let original = crate::cost::CostBudget::new(None, Some(1.0), 0.0).unwrap();
            let mut agent = crate::agent::Agent::new(
                provider.clone(),
                crate::tools::ToolRegistry::from_tools(Vec::new()),
                crate::agent::AgentConfig::default(),
            );
            agent.set_cost_budget(Some(original.clone()));
            let prepared = client.prepare_with_agent(&agent, "old question").unwrap();
            original.record(1.0).unwrap();
            let replacement = crate::cost::CostBudget::new(None, Some(1.0), 0.0).unwrap();
            agent.set_cost_budget(Some(replacement.clone()));
            assert!(matches!(
                client.ask_prepared(prepared).await,
                Err(Error::CostCap { spent, cap }) if spent == 1.0 && cap == 1.0
            ));
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            let prepared = client.prepare_with_agent(&agent, "new question").unwrap();
            assert_eq!(client.ask_prepared(prepared).await.unwrap(), "safe answer");
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(original.spent(), 1.0);
            assert_eq!(replacement.spent(), 0.0);
        });
    }

    #[test]
    fn prepared_request_cannot_be_sent_through_another_clients_privacy_policy() {
        asupersync::test_utils::run_test(|| async {
            let (source, _) = privacy_client("obfuscate");
            let (destination, provider) = privacy_client("block");
            let request = source
                .prepare_with_messages(&[], "What is ACME-123456?")
                .unwrap();
            let error = destination.ask_prepared(request).await.unwrap_err();
            assert!(error.to_string().contains("PI_AUXILIARY_POLICY_MISMATCH"));
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn oversized_direct_question_is_refused_before_provider_admission() {
        asupersync::test_utils::run_test(|| async {
            let client = BtwClient::new(
                Arc::new(ScriptedProvider(
                    Vec::new(),
                    Some("must not be invoked".to_string()),
                )),
                Some("test-key".to_string()),
            );
            let error = client
                .ask("", &"x".repeat(MAX_INPUT_BYTES + 1))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("PI_AUXILIARY_INPUT_LIMIT"));
        });
    }

    #[test]
    fn cancelled_side_question_is_not_reported_as_timeout_or_sent_to_provider() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = crate::agent_cx::AgentCx::for_request();
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel side question"),
        );
        let client = BtwClient::new(
            Arc::new(ScriptedProvider(
                Vec::new(),
                Some("must not be invoked".to_string()),
            )),
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
            Arc::new(ScriptedProvider(
                Vec::new(),
                Some("must not be invoked".to_string()),
            )),
            Some("test-key".to_string()),
        );
        let error = runtime
            .block_on(owner.with_current(client.ask("", "why?")))
            .unwrap_err();
        assert!(error.to_string().contains("PI_AUXILIARY_TIME_DENIED"));
    }
}
