//! GitLab Duo provider implementation.
//!
//! GitLab Duo uses the `/api/v4/chat/completions` endpoint with a proprietary
//! request/response format (NOT OpenAI-compatible).
//!
//! Authentication is via a GitLab Personal Access Token (PAT) or OAuth token
//! passed as `Authorization: Bearer <token>`.
//!
//! Self-hosted GitLab instances are supported via a configurable base URL
//! (defaults to `https://gitlab.com`).
//!
//! bd-3uqg.3.5

use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, Usage,
    UserContent,
};
use crate::models::CompatConfig;
use crate::provider::{Context, Provider, StreamOptions};
use async_trait::async_trait;
use futures::Stream;
use futures::stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

// ── Constants ────────────────────────────────────────────────────

/// Default GitLab instance base URL.
const DEFAULT_GITLAB_BASE: &str = "https://gitlab.com";

/// Chat completions API path.
const CHAT_API_PATH: &str = "/api/v4/chat/completions";

// ── Request types ────────────────────────────────────────────────

/// GitLab Duo Chat request body.
#[derive(Debug, Serialize)]
pub struct GitLabChatRequest {
    /// The user's question/prompt.
    content: String,
    /// Additional context items (files, MRs, issues).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    additional_context: Vec<GitLabContextItem>,
}

/// A context item attached to a GitLab Chat request.
#[derive(Debug, Serialize)]
struct GitLabContextItem {
    /// Category: "file", "merge_request", "issue", "snippet".
    category: String,
    /// Identifier for the context item.
    id: String,
    /// Content of the context item.
    content: String,
}

/// GitLab Chat response (plain text or JSON wrapper).
#[derive(Debug, Deserialize)]
struct GitLabChatResponse {
    /// The generated response text.
    #[serde(default)]
    response: String,
    /// Alternative: some GitLab versions return content directly.
    #[serde(default)]
    content: String,
}

// ── Provider ─────────────────────────────────────────────────────

/// GitLab Duo provider.
pub struct GitLabProvider {
    /// HTTP client.
    client: Client,
    /// Model identifier.
    model: String,
    /// GitLab instance base URL (e.g., `https://gitlab.com` or `https://gitlab.example.com`).
    base_url: String,
    /// Provider name for event attribution.
    provider_name: String,
    /// Compatibility overrides (unused for GitLab but kept for interface consistency).
    #[allow(dead_code)]
    compat: Option<CompatConfig>,
}

impl GitLabProvider {
    /// Create a new GitLab Duo provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model: model.into(),
            base_url: DEFAULT_GITLAB_BASE.to_string(),
            provider_name: "gitlab".to_string(),
            compat: None,
        }
    }

    /// Set the GitLab instance base URL (for self-hosted).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            self.base_url = trimmed.to_string();
        }
        self
    }

    /// Set the provider name for event attribution.
    #[must_use]
    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = name.into();
        self
    }

    /// Attach compatibility overrides.
    #[must_use]
    pub fn with_compat(mut self, compat: Option<CompatConfig>) -> Self {
        self.compat = compat;
        self
    }

    /// Inject a custom HTTP client (for testing / VCR).
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Build the chat completions URL.
    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}{CHAT_API_PATH}")
    }

    /// Build a GitLab Chat request from the agent context.
    pub fn build_request(context: &Context<'_>) -> GitLabChatRequest {
        // Extract the last user message as the primary content.
        let mut content = String::new();
        let mut additional_context = Vec::new();

        // Walk messages to build context. The last user message becomes `content`,
        // earlier messages become additional context for continuity.
        for (i, msg) in context.messages.iter().enumerate().rev() {
            match msg {
                Message::User(user_msg) => {
                    if content.is_empty() {
                        // Last user message → primary content.
                        match &user_msg.content {
                            UserContent::Text(text) => content.clone_from(text),
                            UserContent::Blocks(blocks) => {
                                // Concatenate text parts.
                                let texts: Vec<&str> = blocks
                                    .iter()
                                    .filter_map(|p| {
                                        if let ContentBlock::Text(t) = p {
                                            Some(t.text.as_str())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                                content = texts.join("\n");
                            }
                        }
                    } else {
                        // Earlier user message → context.
                        let text = match &user_msg.content {
                            UserContent::Text(t) => t.clone(),
                            UserContent::Blocks(blocks) => blocks
                                .iter()
                                .filter_map(|p| {
                                    if let ContentBlock::Text(t) = p {
                                        Some(t.text.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        };
                        additional_context.push(GitLabContextItem {
                            category: "file".to_string(),
                            id: format!("message-{i}"),
                            content: format!("[User]: {text}"),
                        });
                    }
                }
                Message::Assistant(asst_msg) => {
                    // Include prior assistant responses as context.
                    let text: String = asst_msg
                        .content
                        .iter()
                        .filter_map(|c| {
                            if let ContentBlock::Text(t) = c {
                                Some(t.text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        additional_context.push(GitLabContextItem {
                            category: "file".to_string(),
                            id: format!("message-{i}"),
                            content: format!("[Assistant]: {text}"),
                        });
                    }
                }
                _ => {}
            }
        }

        // Include system prompt as context if present.
        if let Some(system) = &context.system_prompt {
            additional_context.push(GitLabContextItem {
                category: "file".to_string(),
                id: "system-prompt".to_string(),
                content: format!("[System]: {system}"),
            });
        }

        // Reverse additional_context to chronological order.
        additional_context.reverse();

        // Fallback if no user message found.
        if content.is_empty() {
            content = "Hello".to_string();
        }

        GitLabChatRequest {
            content,
            additional_context,
        }
    }
}

#[async_trait]
impl Provider for GitLabProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn api(&self) -> &'static str {
        "gitlab-chat"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let request_body = Self::build_request(context);
        let url = self.chat_url();

        let api_key  = options.api_key.as_deref().ok_or_else(|| {
            Error::auth(
                "GitLab API token is required. Set GITLAB_TOKEN or GITLAB_API_KEY environment variable.",
            )
        })?;

        let body_bytes = serde_json::to_vec(&request_body)
            .map_err(|e| Error::provider("gitlab", format!("Failed to serialize request: {e}")))?;

        let mut request = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");

        // Add any custom headers from options.
        for (key, value) in &options.headers {
            request = request.try_header(key, value)?;
        }

        let response = Box::pin(request.body(body_bytes).send())
            .await
            .map_err(|e| Error::provider("gitlab", format!("Request failed: {e}")))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read body>".to_string());

        if !(200..300).contains(&status) {
            return Err(Error::provider(
                "gitlab",
                format!("GitLab API error (HTTP {status}): {text}"),
            ));
        }

        // Parse the response — try JSON first, fall back to plain text.
        let response_text = if let Ok(parsed) = serde_json::from_str::<GitLabChatResponse>(&text) {
            if !parsed.response.is_empty() {
                parsed.response
            } else if !parsed.content.is_empty() {
                parsed.content
            } else {
                text
            }
        } else {
            // Plain text response.
            text
        };

        // Build the final assistant message.
        let message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent {
                text: response_text.clone(),
                text_signature: None,
            })],
            api: "gitlab-chat".to_string(),
            provider: self.provider_name.clone(),
            model: self.model.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };

        // GitLab Chat API is non-streaming, so we emit the full event sequence.
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::Start {
                partial: message.clone(),
            }),
            Ok(StreamEvent::TextStart { content_index: 0 }),
            Ok(StreamEvent::TextDelta {
                content_index: 0,
                delta: response_text.clone(),
            }),
            Ok(StreamEvent::TextEnd {
                content_index: 0,
                content: response_text,
            }),
            Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            }),
        ];

        Ok(Box::pin(stream::iter(events)))
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UserMessage;
    use crate::provider::ToolDef;

    #[test]
    fn test_gitlab_provider_defaults() {
        let p = GitLabProvider::new("gitlab-duo-chat");
        assert_eq!(p.name(), "gitlab");
        assert_eq!(p.api(), "gitlab-chat");
        assert_eq!(p.model_id(), "gitlab-duo-chat");
        assert_eq!(p.base_url, DEFAULT_GITLAB_BASE);
    }

    #[test]
    fn test_gitlab_provider_builder() {
        let p = GitLabProvider::new("gitlab-duo-chat")
            .with_provider_name("gitlab-duo")
            .with_base_url("https://gitlab.example.com");

        assert_eq!(p.name(), "gitlab-duo");
        assert_eq!(p.base_url, "https://gitlab.example.com");
    }

    #[test]
    fn test_gitlab_chat_url_construction() {
        let p = GitLabProvider::new("model");
        assert_eq!(p.chat_url(), "https://gitlab.com/api/v4/chat/completions");

        let p = GitLabProvider::new("model").with_base_url("https://gitlab.example.com/");
        assert_eq!(
            p.chat_url(),
            "https://gitlab.example.com/api/v4/chat/completions"
        );
    }

    #[test]
    fn test_build_request_simple() {
        let context = Context::owned(
            Some("Be helpful.".to_string()),
            vec![Message::User(UserMessage {
                content: UserContent::Text("How do I define a class?".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );

        let req = GitLabProvider::build_request(&context);
        assert_eq!(req.content, "How do I define a class?");
        assert_eq!(req.additional_context.len(), 1); // system prompt
        assert_eq!(req.additional_context[0].id, "system-prompt");
    }

    #[test]
    fn test_build_request_multi_turn() {
        let context = Context::owned(
            None,
            vec![
                Message::User(UserMessage {
                    content: UserContent::Text("What is Rust?".to_string()),
                    timestamp: 0,
                }),
                Message::assistant(AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent {
                        text: "Rust is a systems language.".to_string(),
                        text_signature: None,
                    })],
                    api: String::new(),
                    provider: String::new(),
                    model: String::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::default(),
                    stop_details: None,
                    error_message: None,
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("Tell me more.".to_string()),
                    timestamp: 0,
                }),
            ],
            Vec::new(),
        );

        let req = GitLabProvider::build_request(&context);
        assert_eq!(req.content, "Tell me more.");
        // Should have 2 context items: first user msg + assistant response.
        assert_eq!(req.additional_context.len(), 2);
    }

    #[test]
    fn test_build_request_empty_messages_fallback() {
        let context = Context::owned(None, Vec::new(), Vec::new());

        let req = GitLabProvider::build_request(&context);
        assert_eq!(req.content, "Hello"); // fallback
    }

    #[test]
    fn test_build_request_omits_standard_tool_call_schema() {
        let context = Context::owned(
            Some("Use tools when available.".to_string()),
            vec![Message::User(UserMessage {
                content: UserContent::Text("Call echo with hello.".to_string()),
                timestamp: 0,
            })],
            vec![ToolDef {
                name: "echo".to_string(),
                description: "Echo text back to the caller.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"]
                }),
            }],
        );

        let req = GitLabProvider::build_request(&context);
        let encoded = serde_json::to_value(&req).expect("serialize gitlab request");

        assert_eq!(req.content, "Call echo with hello.");
        assert_eq!(context.tools.len(), 1);
        assert!(encoded.get("tools").is_none());
        assert!(encoded.get("tool_choice").is_none());
        assert!(encoded.get("functions").is_none());
    }

    #[test]
    fn test_gitlab_response_deserialization() {
        let json = r#"{"response": "Here is how you define a class in Ruby..."}"#;
        let resp: GitLabChatResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.response, "Here is how you define a class in Ruby...");
    }

    #[test]
    fn test_gitlab_response_content_field() {
        let json = r#"{"content": "Alternative response format"}"#;
        let resp: GitLabChatResponse = serde_json::from_str(json).expect("parse");
        assert!(resp.response.is_empty());
        assert_eq!(resp.content, "Alternative response format");
    }

    #[test]
    fn test_gitlab_empty_base_url_uses_default() {
        let p = GitLabProvider::new("model").with_base_url("");
        assert_eq!(p.base_url, DEFAULT_GITLAB_BASE);
    }

    #[test]
    fn test_gitlab_whitespace_base_url_uses_default() {
        let p = GitLabProvider::new("model").with_base_url("   \n\t  ");
        assert_eq!(p.base_url, DEFAULT_GITLAB_BASE);
    }

    #[test]
    fn test_gitlab_base_url_is_trimmed() {
        let p = GitLabProvider::new("model").with_base_url(" https://gitlab.example.com/ ");
        assert_eq!(p.base_url, "https://gitlab.example.com/");
        assert_eq!(
            p.chat_url(),
            "https://gitlab.example.com/api/v4/chat/completions"
        );
    }
}
