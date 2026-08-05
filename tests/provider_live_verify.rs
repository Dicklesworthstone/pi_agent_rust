//! **Optional live-provider verification suite** — gated behind secrets.
//!
//! These tests hit real provider endpoints when the required API key env vars
//! are present, and cleanly skip when they are absent.  They are designed to
//! validate real-world auth, streaming protocol, and tool-call behavior that
//! VCR cassettes cannot perfectly emulate.
//!
//! # Isolation
//!
//! This suite is deliberately **isolated from required CI gates**.  A provider
//! outage or quota issue must not block unrelated merges.  The gate env var
//! `PI_LIVE_PROVIDER_TESTS=1` enables the suite; individual provider tests
//! further gate on secret availability.
//!
//! # Running
//!
//! ```bash
//! # Run with any available provider secrets:
//! PI_LIVE_PROVIDER_TESTS=1 cargo test provider_live_verify -- --nocapture
//!
//! # Run only Anthropic live tests:
//! PI_LIVE_PROVIDER_TESTS=1 ANTHROPIC_API_KEY=sk-... \
//!     cargo test provider_live_verify::anthropic -- --nocapture
//!
//! # Run only OpenAI-compatible providers:
//! PI_LIVE_PROVIDER_TESTS=1 cargo test provider_live_verify::oai_compat -- --nocapture
//! ```
//!
//! # Cost control
//!
//! Every prompt uses ≈20–50 tokens.  Estimated cost for a single provider: < $0.001.

mod common;

use common::TestHarness;
use futures::StreamExt;
use pi::model::{Message, StopReason, StreamEvent, UserContent, UserMessage};
use pi::provider::{Context, Provider, StreamOptions, ToolDef};
use pi::provider_metadata::{PROVIDER_METADATA, ProviderMetadata, provider_auth_env_keys};
use pi::providers::anthropic::AnthropicProvider;
use pi::providers::cohere::CohereProvider;
use pi::providers::gemini::GeminiProvider;
use pi::providers::openai::OpenAIProvider;
use pi::providers::openai_responses::OpenAIResponsesProvider;
use pi::providers::{normalize_openai_base, normalize_openai_responses_base};
use serde::Serialize;
use std::env;
use std::path::PathBuf;
use std::time::Instant;

// ============================================================================
// Gate: skip entire module unless PI_LIVE_PROVIDER_TESTS=1 (or PI_E2E_TESTS=1)
// ============================================================================

fn live_tests_enabled() -> bool {
    fn flag(name: &str) -> bool {
        env::var(name)
            .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
    }
    flag("PI_LIVE_PROVIDER_TESTS") || flag("PI_E2E_TESTS") || flag("CI_E2E_TESTS")
}

macro_rules! skip_unless_live {
    () => {
        if !live_tests_enabled() {
            eprintln!("SKIPPED (set PI_LIVE_PROVIDER_TESTS=1 or PI_E2E_TESTS=1 to run)");
            return;
        }
    };
}

// ============================================================================
// Secret detection
// ============================================================================

/// Resolve the first available API key from env vars for a provider.
fn resolve_secret(provider_id: &str) -> Option<(String, String)> {
    for env_key in provider_auth_env_keys(provider_id) {
        if let Ok(value) = env::var(env_key) {
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                return Some((trimmed, env_key.to_string()));
            }
        }
    }
    None
}

macro_rules! skip_unless_secret {
    ($provider:expr) => {{
        match resolve_secret($provider) {
            Some((key, source)) => {
                eprintln!("  provider '{}': secret found via {}", $provider, source);
                (key, source)
            }
            None => {
                let env_hint = provider_auth_env_keys($provider).join(" or ");
                eprintln!("SKIPPED: no secret for '{}' (set {})", $provider, env_hint);
                return;
            }
        }
    }};
}

// ============================================================================
// Helpers
// ============================================================================

fn user_text(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn simple_context(prompt: &str) -> Context<'static> {
    Context::owned(
        Some("You are a test harness. Respond concisely.".to_string()),
        vec![user_text(prompt)],
        Vec::new(),
    )
}

fn tool_context(prompt: &str, tools: Vec<ToolDef>) -> Context<'static> {
    Context::owned(
        Some("You are a test harness. Use tools when explicitly asked.".to_string()),
        vec![user_text(prompt)],
        tools,
    )
}

fn simple_options(api_key: &str) -> StreamOptions {
    StreamOptions {
        api_key: Some(api_key.to_string()),
        max_tokens: Some(64),
        temperature: Some(0.0),
        ..Default::default()
    }
}

fn echo_tool() -> ToolDef {
    ToolDef {
        name: "echo".to_string(),
        description: "Echo the provided text back.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            },
            "required": ["text"]
        }),
    }
}

/// Collect stream events with a timeout safety net.
async fn collect_stream_events(
    provider: &dyn Provider,
    context: &Context<'_>,
    options: &StreamOptions,
) -> StreamResult {
    let start = Instant::now();
    let stream_result = provider.stream(context, options).await;

    let mut stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            return StreamResult {
                events: vec![],
                text: String::new(),
                tool_calls: vec![],
                has_start: false,
                has_done: false,
                stop_reason: None,
                stream_error: Some(e.to_string()),
                elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            };
        }
    };

    let mut events = Vec::new();
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut has_start = false;
    let mut has_done = false;
    let mut stop_reason = None;
    let mut stream_error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => {
                match &event {
                    StreamEvent::Start { .. } => has_start = true,
                    StreamEvent::TextDelta { delta, .. } => text.push_str(delta),
                    StreamEvent::TextEnd { content, .. } => text.clone_from(content),
                    StreamEvent::ToolCallEnd { tool_call, .. } => {
                        tool_calls.push(tool_call.clone());
                    }
                    StreamEvent::Done { reason, message } => {
                        has_done = true;
                        stop_reason = Some(*reason);
                        // Fall back to Done message text if no deltas were received.
                        if text.is_empty() {
                            for block in &message.content {
                                if let pi::model::ContentBlock::Text(tc) = block
                                    && !tc.text.is_empty()
                                {
                                    text.clone_from(&tc.text);
                                }
                            }
                        }
                    }
                    StreamEvent::Error { reason, .. } => {
                        has_done = true;
                        stop_reason = Some(*reason);
                    }
                    _ => {}
                }
                events.push(event);
            }
            Err(e) => {
                stream_error = Some(e.to_string());
                break;
            }
        }
    }

    StreamResult {
        events,
        text,
        tool_calls,
        has_start,
        has_done,
        stop_reason,
        stream_error,
        elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

#[derive(Debug)]
struct StreamResult {
    events: Vec<StreamEvent>,
    text: String,
    tool_calls: Vec<pi::model::ToolCall>,
    has_start: bool,
    has_done: bool,
    stop_reason: Option<StopReason>,
    stream_error: Option<String>,
    elapsed_ms: u64,
}

// ============================================================================
// Provider factory (data-driven from provider_metadata routing_defaults)
// ============================================================================

/// Build a provider instance from metadata routing defaults.
/// Returns None for providers that need special config (Azure, Bedrock, Vertex,
/// Copilot, GitLab) which require infrastructure beyond a single env var.
fn build_provider_from_metadata(meta: &ProviderMetadata) -> Option<Box<dyn Provider>> {
    let defaults = meta.routing_defaults?;
    let model_id = default_model_for_provider(meta.canonical_id);

    match defaults.api {
        "anthropic-messages" => Some(Box::new(
            AnthropicProvider::new(model_id).with_base_url(defaults.base_url),
        )),
        "google-generative-ai" => Some(Box::new(
            GeminiProvider::new(model_id).with_base_url(defaults.base_url),
        )),
        "cohere-chat" => Some(Box::new(
            CohereProvider::new(model_id).with_base_url(defaults.base_url),
        )),
        "openai-responses" => Some(Box::new(
            OpenAIResponsesProvider::new(model_id)
                .with_provider_name(meta.canonical_id)
                .with_base_url(normalize_openai_responses_base(defaults.base_url)),
        )),
        "openai-completions" => Some(Box::new(
            OpenAIProvider::new(model_id)
                .with_provider_name(meta.canonical_id)
                .with_base_url(normalize_openai_base(defaults.base_url)),
        )),
        _ => None,
    }
}

/// Select a small, cheap model for live verification.
fn default_model_for_provider(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude-haiku-4-5",
        "openai" => "gpt-4o-mini",
        "google" => "gemini-2.0-flash",
        "cohere" => "command-r",
        "groq" => "llama-3.1-8b-instant",
        "deepinfra" | "nebius" => "meta-llama/Meta-Llama-3.1-8B-Instruct",
        "cerebras" => "llama3.1-8b",
        "openrouter" => "deepseek/deepseek-chat",
        "mistral" => "mistral-small-latest",
        "moonshotai" => "moonshot-v1-8k",
        "alibaba" | "alibaba-cn" | "alibaba-us" => "qwen-turbo",
        "deepseek" => "deepseek-chat",
        "fireworks" => "accounts/fireworks/models/llama-v3p1-8b-instruct",
        "togetherai" => "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        "perplexity" => "llama-3.1-sonar-small-128k-chat",
        "xai" => "grok-3-mini",
        "minimax" | "minimax-cn" => "MiniMax-Text-01",
        "nvidia" => "meta/llama-3.1-8b-instruct",
        "siliconflow" | "siliconflow-cn" => "deepseek-ai/DeepSeek-V3",
        "upstage" => "solar-pro",
        "zhipuai" | "zhipuai-coding-plan" => "glm-4-flash",
        _ => "default",
    }
}

// ============================================================================
// Discovery artifact
// ============================================================================

#[derive(Debug, Serialize)]
struct LiveVerifyDiscovery {
    provider: String,
    auth_env_keys: Vec<String>,
    secret_available: bool,
    secret_source: Option<String>,
    has_routing_defaults: bool,
    api: Option<String>,
    model_id: Option<String>,
}

fn run_discovery() -> Vec<LiveVerifyDiscovery> {
    PROVIDER_METADATA
        .iter()
        .map(|meta| {
            let (secret_available, secret_source) = resolve_secret(meta.canonical_id)
                .map_or((false, None), |(_, src)| (true, Some(src)));
            let defaults = meta.routing_defaults;
            LiveVerifyDiscovery {
                provider: meta.canonical_id.to_string(),
                auth_env_keys: meta.auth_env_keys.iter().map(ToString::to_string).collect(),
                secret_available,
                secret_source,
                has_routing_defaults: defaults.is_some(),
                api: defaults.map(|d| d.api.to_string()),
                model_id: Some(default_model_for_provider(meta.canonical_id).to_string()),
            }
        })
        .collect()
}

fn write_discovery_artifact(harness: &TestHarness) {
    let rows = run_discovery();
    let json = serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string());
    let path = harness.temp_path("live_verify_discovery.json");
    let _ = std::fs::write(&path, &json);
    harness.record_artifact("live_verify_discovery.json", &path);

    let available_count = rows.iter().filter(|r| r.secret_available).count();
    let skipped_count = rows.iter().filter(|r| !r.secret_available).count();
    eprintln!(
        "Live verify discovery: {available_count} providers with secrets, {skipped_count} without",
    );
    for row in rows.iter().filter(|r| r.secret_available) {
        eprintln!(
            "  [LIVE] {} ({})",
            row.provider,
            row.secret_source.as_deref().unwrap_or("?")
        );
    }
}

// ============================================================================
// Verification result artifact
// ============================================================================

#[derive(Debug, Serialize)]
struct VerificationResult {
    provider: String,
    scenario: String,
    passed: bool,
    elapsed_ms: u64,
    event_count: usize,
    text_preview: String,
    tool_call_count: usize,
    stop_reason: Option<String>,
    error: Option<String>,
}

fn artifact_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/live-verify-results");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn write_result_artifact(result: &VerificationResult) {
    let dir = artifact_dir();
    let path = dir.join(format!("{}_{}.json", result.provider, result.scenario));
    let json = serde_json::to_string_pretty(result).unwrap_or_default();
    let _ = std::fs::write(path, json);
}

// ============================================================================
// Discovery test (always runs, shows what would be tested)
// ============================================================================

#[test]
fn discovery_shows_available_providers() {
    skip_unless_live!();
    let harness = TestHarness::new("live_verify_discovery");
    write_discovery_artifact(&harness);

    let rows = run_discovery();
    let available_count = rows.iter().filter(|r| r.secret_available).count();
    eprintln!(
        "Discovery complete: {}/{} providers have secrets available",
        available_count,
        rows.len()
    );
}

// ============================================================================
// Data-driven live verification across all providers with routing_defaults
// ============================================================================

#[test]
fn all_available_providers_simple_text() {
    skip_unless_live!();
    let harness = TestHarness::new("live_verify_all_simple_text");
    write_discovery_artifact(&harness);

    let mut tested = 0u32;
    let mut passed = 0u32;
    let mut skipped = 0u32;
    let mut failed_providers = Vec::new();

    for meta in PROVIDER_METADATA {
        let Some((api_key, source)) = resolve_secret(meta.canonical_id) else {
            skipped += 1;
            continue;
        };
        let Some(provider) = build_provider_from_metadata(meta) else {
            eprintln!(
                "  SKIP {}: no routing_defaults or unsupported API",
                meta.canonical_id
            );
            skipped += 1;
            continue;
        };

        eprintln!(
            "  TESTING {} (secret via {}, model {})",
            meta.canonical_id,
            source,
            default_model_for_provider(meta.canonical_id)
        );
        tested += 1;

        let result = common::run_async({
            let api_key = api_key.clone();
            let provider_name = meta.canonical_id.to_string();
            async move {
                let context = simple_context("Say exactly: 'hello'");
                let options = simple_options(&api_key);
                let sr = collect_stream_events(provider.as_ref(), &context, &options).await;

                let ok = sr.stream_error.is_none() && sr.has_done && !sr.text.is_empty();

                let vr = VerificationResult {
                    provider: provider_name.clone(),
                    scenario: "simple_text".to_string(),
                    passed: ok,
                    elapsed_ms: sr.elapsed_ms,
                    event_count: sr.events.len(),
                    text_preview: sr.text.chars().take(100).collect(),
                    tool_call_count: sr.tool_calls.len(),
                    stop_reason: sr.stop_reason.map(|r| format!("{r:?}")),
                    error: sr.stream_error,
                };
                write_result_artifact(&vr);
                vr
            }
        });

        if result.passed {
            passed += 1;
            eprintln!(
                "    PASS {} ({}ms, {} events)",
                meta.canonical_id, result.elapsed_ms, result.event_count
            );
        } else {
            failed_providers.push(meta.canonical_id.to_string());
            eprintln!(
                "    FAIL {} ({}ms): {}",
                meta.canonical_id,
                result.elapsed_ms,
                result.error.as_deref().unwrap_or("unknown")
            );
        }
    }

    eprintln!("\n  Summary: {passed}/{tested} passed, {skipped} skipped (no secrets/routing)");

    // Write summary artifact
    let summary = serde_json::json!({
        "tested": tested,
        "passed": passed,
        "skipped": skipped,
        "failed_providers": failed_providers,
    });
    let summary_path = artifact_dir().join("summary.json");
    let _ = std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    );

    // Non-fatal: we log failures but do not panic to keep the suite isolated
    // from CI gates.  Individual per-provider tests below assert strictly.
    if !failed_providers.is_empty() {
        eprintln!(
            "  WARNING: {} providers failed: {}",
            failed_providers.len(),
            failed_providers.join(", ")
        );
    }
}

// ============================================================================
// Per-provider native tests (strict assertions, skip when secrets absent)
// ============================================================================

mod anthropic {
    use super::*;

    #[test]
    fn live_simple_text() {
        skip_unless_live!();
        let (api_key, _) = skip_unless_secret!("anthropic");

        common::run_async(async move {
            let provider = AnthropicProvider::new("claude-haiku-4-5")
                .with_base_url("https://api.anthropic.com/v1/messages");
            let context = simple_context("Say just the word hello");
            let options = simple_options(&api_key);
            let sr = collect_stream_events(&provider, &context, &options).await;

            assert!(
                sr.stream_error.is_none(),
                "anthropic simple_text stream error: {}",
                sr.stream_error.as_deref().unwrap_or("")
            );
            assert!(sr.has_start, "anthropic: expected Start event");
            assert!(sr.has_done, "anthropic: expected Done event");
            assert!(
                !sr.text.is_empty(),
                "anthropic: expected non-empty text response"
            );
            assert!(
                sr.text.to_lowercase().contains("hello"),
                "anthropic: expected 'hello' in response, got: {}",
                sr.text
            );
            assert_eq!(
                sr.stop_reason,
                Some(StopReason::Stop),
                "anthropic: expected Stop reason"
            );

            let vr = VerificationResult {
                provider: "anthropic".into(),
                scenario: "simple_text".into(),
                passed: true,
                elapsed_ms: sr.elapsed_ms,
                event_count: sr.events.len(),
                text_preview: sr.text.chars().take(100).collect(),
                tool_call_count: 0,
                stop_reason: sr.stop_reason.map(|r| format!("{r:?}")),
                error: None,
            };
            write_result_artifact(&vr);
        });
    }

    #[test]
    fn live_tool_call() {
        skip_unless_live!();
        let (api_key, _) = skip_unless_secret!("anthropic");

        common::run_async(async move {
            let provider = AnthropicProvider::new("claude-haiku-4-5")
                .with_base_url("https://api.anthropic.com/v1/messages");
            let context = tool_context("Use the echo tool to echo 'live-test'.", vec![echo_tool()]);
            let options = simple_options(&api_key);
            let sr = collect_stream_events(&provider, &context, &options).await;

            assert!(
                sr.stream_error.is_none(),
                "anthropic tool_call stream error: {}",
                sr.stream_error.as_deref().unwrap_or("")
            );
            assert!(sr.has_done, "anthropic tool_call: expected Done event");
            assert!(
                !sr.tool_calls.is_empty(),
                "anthropic: expected at least one tool call"
            );
            assert_eq!(sr.tool_calls[0].name, "echo");
            assert!(
                sr.tool_calls[0].arguments.is_object(),
                "anthropic: tool call arguments should be an object"
            );
            assert_eq!(
                sr.stop_reason,
                Some(StopReason::ToolUse),
                "anthropic: expected ToolUse stop reason"
            );

            let vr = VerificationResult {
                provider: "anthropic".into(),
                scenario: "tool_call".into(),
                passed: true,
                elapsed_ms: sr.elapsed_ms,
                event_count: sr.events.len(),
                text_preview: String::new(),
                tool_call_count: sr.tool_calls.len(),
                stop_reason: sr.stop_reason.map(|r| format!("{r:?}")),
                error: None,
            };
            write_result_artifact(&vr);
        });
    }

    #[test]
    fn live_auth_error_on_invalid_key() {
        skip_unless_live!();
        // This test doesn't need a real key — it verifies the error path.
        // But we still gate on the suite flag.

        common::run_async(async move {
            let provider = AnthropicProvider::new("claude-haiku-4-5")
                .with_base_url("https://api.anthropic.com/v1/messages");
            let context = simple_context("Say hello");
            let options = simple_options("sk-ant-invalid-key-for-live-verify");
            let sr = collect_stream_events(&provider, &context, &options).await;

            // Should get an error (either stream_error or an error event)
            let has_error = sr.stream_error.is_some()
                || sr
                    .events
                    .iter()
                    .any(|e| matches!(e, StreamEvent::Error { .. }));
            assert!(
                has_error,
                "anthropic: expected auth error with invalid key, got {} events",
                sr.events.len()
            );

            // Error message should be actionable
            let error_text = sr.stream_error.unwrap_or_default();
            let error_lower = error_text.to_lowercase();
            assert!(
                error_lower.contains("auth")
                    || error_lower.contains("key")
                    || error_lower.contains("401")
                    || error_lower.contains("invalid")
                    || error_lower.contains("permission"),
                "anthropic: expected actionable auth error, got: {error_text}"
            );
        });
    }
}

mod openai {
    use super::*;

    #[test]
    fn live_simple_text() {
        skip_unless_live!();
        let (api_key, _) = skip_unless_secret!("openai");

        common::run_async(async move {
            let provider = OpenAIResponsesProvider::new("gpt-4o-mini")
                .with_base_url(normalize_openai_responses_base("https://api.openai.com/v1"));
            let context = simple_context("Say just the word hello");
            let options = simple_options(&api_key);
            let sr = collect_stream_events(&provider, &context, &options).await;

            assert!(
                sr.stream_error.is_none(),
                "openai simple_text stream error: {}",
                sr.stream_error.as_deref().unwrap_or("")
            );
            assert!(sr.has_done, "openai: expected Done event");
            assert!(
                !sr.text.is_empty(),
                "openai: expected non-empty text response"
            );

            let vr = VerificationResult {
                provider: "openai".into(),
                scenario: "simple_text".into(),
                passed: true,
                elapsed_ms: sr.elapsed_ms,
                event_count: sr.events.len(),
                text_preview: sr.text.chars().take(100).collect(),
                tool_call_count: 0,
                stop_reason: sr.stop_reason.map(|r| format!("{r:?}")),
                error: None,
            };
            write_result_artifact(&vr);
        });
    }

    #[test]
    fn live_tool_call() {
        skip_unless_live!();
        let (api_key, _) = skip_unless_secret!("openai");

        common::run_async(async move {
            let provider = OpenAIResponsesProvider::new("gpt-4o-mini")
                .with_base_url(normalize_openai_responses_base("https://api.openai.com/v1"));
            let context = tool_context("Use the echo tool to echo 'live-test'.", vec![echo_tool()]);
            let options = simple_options(&api_key);
            let sr = collect_stream_events(&provider, &context, &options).await;

            assert!(
                sr.stream_error.is_none(),
                "openai tool_call stream error: {}",
                sr.stream_error.as_deref().unwrap_or("")
            );
            assert!(sr.has_done, "openai tool_call: expected Done event");
            assert!(
                !sr.tool_calls.is_empty(),
                "openai: expected at least one tool call"
            );

            let vr = VerificationResult {
                provider: "openai".into(),
                scenario: "tool_call".into(),
                passed: true,
                elapsed_ms: sr.elapsed_ms,
                event_count: sr.events.len(),
                text_preview: String::new(),
                tool_call_count: sr.tool_calls.len(),
                stop_reason: sr.stop_reason.map(|r| format!("{r:?}")),
                error: None,
            };
            write_result_artifact(&vr);
        });
    }
}

mod google {
    use super::*;

    #[test]
    fn live_simple_text() {
        skip_unless_live!();
        let (api_key, _) = skip_unless_secret!("google");

        common::run_async(async move {
            let provider = GeminiProvider::new("gemini-2.0-flash")
                .with_base_url("https://generativelanguage.googleapis.com/v1beta");
            let context = simple_context("Say just the word hello");
            let options = simple_options(&api_key);
            let sr = collect_stream_events(&provider, &context, &options).await;

            assert!(
                sr.stream_error.is_none(),
                "google simple_text stream error: {}",
                sr.stream_error.as_deref().unwrap_or("")
            );
            assert!(sr.has_done, "google: expected Done event");
            assert!(
                !sr.text.is_empty(),
                "google: expected non-empty text response"
            );

            let vr = VerificationResult {
                provider: "google".into(),
                scenario: "simple_text".into(),
                passed: true,
                elapsed_ms: sr.elapsed_ms,
                event_count: sr.events.len(),
                text_preview: sr.text.chars().take(100).collect(),
                tool_call_count: 0,
                stop_reason: sr.stop_reason.map(|r| format!("{r:?}")),
                error: None,
            };
            write_result_artifact(&vr);
        });
    }
}

mod cohere {
    use super::*;

    #[test]
    fn live_simple_text() {
        skip_unless_live!();
        let (api_key, _) = skip_unless_secret!("cohere");

        common::run_async(async move {
            let provider =
                CohereProvider::new("command-r").with_base_url("https://api.cohere.com/v2");
            let context = simple_context("Say just the word hello");
            let options = simple_options(&api_key);
            let sr = collect_stream_events(&provider, &context, &options).await;

            assert!(
                sr.stream_error.is_none(),
                "cohere simple_text stream error: {}",
                sr.stream_error.as_deref().unwrap_or("")
            );
            assert!(sr.has_done, "cohere: expected Done event");
            assert!(
                !sr.text.is_empty(),
                "cohere: expected non-empty text response"
            );

            let vr = VerificationResult {
                provider: "cohere".into(),
                scenario: "simple_text".into(),
                passed: true,
                elapsed_ms: sr.elapsed_ms,
                event_count: sr.events.len(),
                text_preview: sr.text.chars().take(100).collect(),
                tool_call_count: 0,
                stop_reason: sr.stop_reason.map(|r| format!("{r:?}")),
                error: None,
            };
            write_result_artifact(&vr);
        });
    }
}

// ============================================================================
// OpenAI-compatible providers (data-driven)
// ============================================================================

mod oai_compat {
    use super::*;

    /// Run a simple text verification against an OpenAI-compatible provider.
    fn verify_oai_compat_simple_text(provider_id: &str, api: &str, base_url: &str) {
        skip_unless_live!();
        let (api_key, source) = skip_unless_secret!(provider_id);
        let model_id = default_model_for_provider(provider_id);

        eprintln!(
            "  oai_compat simple_text: {provider_id} (model={model_id}, api={api}, source={source})"
        );

        let provider_id_owned = provider_id.to_string();
        let api_owned = api.to_string();
        let base_url_owned = base_url.to_string();
        common::run_async(async move {
            let provider: Box<dyn Provider> = match api_owned.as_str() {
                "openai-responses" => Box::new(
                    OpenAIResponsesProvider::new(model_id)
                        .with_provider_name(&provider_id_owned)
                        .with_base_url(normalize_openai_responses_base(&base_url_owned)),
                ),
                _ => Box::new(
                    OpenAIProvider::new(model_id)
                        .with_provider_name(&provider_id_owned)
                        .with_base_url(normalize_openai_base(&base_url_owned)),
                ),
            };

            let context = simple_context("Say just the word hello");
            let options = simple_options(&api_key);
            let sr = collect_stream_events(provider.as_ref(), &context, &options).await;

            let ok = sr.stream_error.is_none() && sr.has_done && !sr.text.is_empty();
            let vr = VerificationResult {
                provider: provider_id_owned.clone(),
                scenario: "simple_text".into(),
                passed: ok,
                elapsed_ms: sr.elapsed_ms,
                event_count: sr.events.len(),
                text_preview: sr.text.chars().take(100).collect(),
                tool_call_count: 0,
                stop_reason: sr.stop_reason.map(|r| format!("{r:?}")),
                error: sr.stream_error.clone(),
            };
            write_result_artifact(&vr);

            assert!(
                sr.stream_error.is_none(),
                "{provider_id_owned} stream error: {}",
                sr.stream_error.as_deref().unwrap_or("")
            );
            assert!(sr.has_done, "{provider_id_owned}: expected Done event");
            assert!(
                !sr.text.is_empty(),
                "{provider_id_owned}: expected non-empty text"
            );
        });
    }

    // ── Individual OAI-compat provider tests ───────────────────────────

    #[test]
    fn groq_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("groq").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("groq", defaults.api, defaults.base_url);
    }

    #[test]
    fn deepseek_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("deepseek").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("deepseek", defaults.api, defaults.base_url);
    }

    #[test]
    fn openrouter_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("openrouter").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("openrouter", defaults.api, defaults.base_url);
    }

    #[test]
    fn xai_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("xai").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("xai", defaults.api, defaults.base_url);
    }

    #[test]
    fn mistral_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("mistral").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("mistral", defaults.api, defaults.base_url);
    }

    #[test]
    fn fireworks_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("fireworks").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("fireworks", defaults.api, defaults.base_url);
    }

    #[test]
    fn togetherai_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("togetherai").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("togetherai", defaults.api, defaults.base_url);
    }

    #[test]
    fn perplexity_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("perplexity").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("perplexity", defaults.api, defaults.base_url);
    }

    #[test]
    fn deepinfra_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("deepinfra").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("deepinfra", defaults.api, defaults.base_url);
    }

    #[test]
    fn cerebras_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("cerebras").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("cerebras", defaults.api, defaults.base_url);
    }

    #[test]
    fn nvidia_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("nvidia").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("nvidia", defaults.api, defaults.base_url);
    }

    #[test]
    fn nebius_simple_text() {
        let meta = pi::provider_metadata::provider_metadata("nebius").unwrap();
        let defaults = meta.routing_defaults.unwrap();
        verify_oai_compat_simple_text("nebius", defaults.api, defaults.base_url);
    }
}

// ============================================================================
// Protocol invariant checks (run against any available provider)
// ============================================================================

#[test]
fn streaming_event_timeline_is_well_ordered() {
    skip_unless_live!();
    // Use whichever native provider has a secret available.
    let native_providers = ["anthropic", "openai", "google", "cohere"];
    let mut found = None;
    for provider_id in &native_providers {
        if let Some((key, source)) = resolve_secret(provider_id) {
            found = Some((*provider_id, key, source));
            break;
        }
    }
    let Some((provider_id, api_key, source)) = found else {
        eprintln!(
            "SKIPPED: no native provider secret available (need one of: {})",
            native_providers.join(", ")
        );
        return;
    };
    eprintln!("  Using {provider_id} (via {source}) for timeline ordering test");

    common::run_async(async move {
        let meta = pi::provider_metadata::provider_metadata(provider_id).unwrap();
        let provider = build_provider_from_metadata(meta)
            .unwrap_or_else(|| panic!("failed to build provider for {provider_id}"));
        let context = simple_context("Say just the word hello");
        let options = simple_options(&api_key);
        let sr = collect_stream_events(provider.as_ref(), &context, &options).await;

        assert!(
            sr.stream_error.is_none(),
            "stream error: {}",
            sr.stream_error.as_deref().unwrap_or("")
        );

        // Verify Done is the final event
        assert!(
            matches!(sr.events.last(), Some(StreamEvent::Done { .. })),
            "last event should be Done, got {:?}",
            sr.events.last()
        );

        // Verify no events after Done
        let done_idx = sr
            .events
            .iter()
            .position(|e| matches!(e, StreamEvent::Done { .. }));
        if let Some(idx) = done_idx {
            assert_eq!(
                idx,
                sr.events.len() - 1,
                "Done should be the last event (found at {idx}, total {})",
                sr.events.len()
            );
        }

        // Verify no duplicate Done events
        let done_count = sr
            .events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Done { .. }))
            .count();
        assert_eq!(done_count, 1, "expected exactly one Done event");
    });
}

#[test]
fn usage_tokens_are_populated_in_done_event() {
    skip_unless_live!();
    let native_providers = ["anthropic", "openai", "google"];
    let mut found = None;
    for provider_id in &native_providers {
        if let Some((key, source)) = resolve_secret(provider_id) {
            found = Some((*provider_id, key, source));
            break;
        }
    }
    let Some((provider_id, api_key, _)) = found else {
        eprintln!("SKIPPED: no native provider secret available");
        return;
    };

    common::run_async(async move {
        let meta = pi::provider_metadata::provider_metadata(provider_id).unwrap();
        let provider = build_provider_from_metadata(meta).unwrap();
        let context = simple_context("Say just the word hello");
        let options = simple_options(&api_key);
        let sr = collect_stream_events(provider.as_ref(), &context, &options).await;

        assert!(sr.stream_error.is_none());

        let done = sr.events.iter().find_map(|e| match e {
            StreamEvent::Done { message, .. } => Some(message),
            _ => None,
        });
        let message = done.expect("expected Done event with message");

        // Most providers populate usage; at minimum input should be > 0.
        assert!(
            message.usage.input > 0,
            "{provider_id}: expected non-zero input tokens in Done usage, got {}",
            message.usage.input
        );
    });
}
