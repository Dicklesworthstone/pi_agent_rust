//! Provider-matrix smoke harness (offline/mocked).
//!
//! Exercises minimal requests across every configured provider path to catch
//! routing/wiring regressions quickly. Verifies:
//! - Provider route selection (`create_provider` succeeds with correct name/api)
//! - Auth header construction (Bearer token or x-api-key)
//! - Baseline request assembly (valid JSON body, POST method, correct path)
//!
//! Scope intentionally excludes deep request/response contract validation
//! (owned by bd-3uqg.8.2) and streaming/tool-event parity conformance
//! (owned by bd-3uqg.8.3).
//!
//! bd-3uqg.8.4

mod common;

use common::{MockHttpResponse, TestHarness};
use futures::StreamExt;
use pi::model::{Message, UserContent, UserMessage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, StreamEvent, StreamOptions};
use pi::provider_metadata::{PROVIDER_METADATA, ProviderOnboardingMode, canonical_provider_id};
use pi::providers::create_provider;
use std::collections::HashMap;
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════
// Shared helpers
// ═══════════════════════════════════════════════════════════════════════

fn make_smoke_entry(provider: &str, model_id: &str, base_url: &str) -> ModelEntry {
    ModelEntry {
        model: Model {
            id: model_id.to_string(),
            name: format!("{provider} smoke model"),
            api: String::new(), // let metadata resolve
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 8192,
            max_tokens: 4096,
            headers: HashMap::new(),
        },
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn request_header(headers: &[(String, String)], key: &str) -> Option<String> {
    headers
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.clone())
}

fn drive_to_done(
    provider: Arc<dyn pi::provider::Provider>,
    context: Context<'static>,
    options: StreamOptions,
) {
    common::run_async(async move {
        let mut stream = provider
            .stream(&context, &options)
            .await
            .expect("provider stream should start");
        while let Some(event) = stream.next().await {
            if matches!(event.expect("stream event"), StreamEvent::Done { .. }) {
                return;
            }
        }
        panic!("provider stream ended before Done event");
    });
}

fn minimal_context() -> Context<'static> {
    Context::owned(
        Some("Be concise.".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text("Ping".to_string()),
            timestamp: 0,
        })],
        Vec::new(),
    )
}

fn text_event_stream_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// SSE response bodies by API family
// ═══════════════════════════════════════════════════════════════════════

fn openai_chat_sse() -> String {
    [
        r#"data: {"choices":[{"delta":{}}]}"#,
        "",
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

fn openai_responses_sse() -> String {
    [
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":"ok"}"#,
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
        "",
    ]
    .join("\n")
}

fn anthropic_messages_sse() -> String {
    [
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
        "",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        "",
        r#"data: {"type":"message_stop"}"#,
        "",
    ]
    .join("\n")
}

fn cohere_chat_sse() -> String {
    [
        r"event: message-start",
        r#"data: {"id":"smoke-cohere","type":"message-start","delta":{"message":{"role":"assistant","content":[]}}}"#,
        "",
        r"event: content-start",
        r#"data: {"type":"content-start","index":0,"delta":{"message":{"content":{"type":"text","text":""}}}}"#,
        "",
        r"event: content-end",
        r#"data: {"type":"content-end","index":0}"#,
        "",
        r"event: message-end",
        r#"data: {"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":{"billed_units":{"input_tokens":1,"output_tokens":1},"tokens":{"input_tokens":1,"output_tokens":1}}}}"#,
        "",
    ]
    .join("\n")
}

fn bedrock_converse_json() -> serde_json::Value {
    serde_json::json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [{"text": "pong"}]
            }
        },
        "stopReason": "end_turn",
        "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
    })
}

fn cursor_connect_response() -> MockHttpResponse {
    fn len_delimited(field: u8, payload: &[u8]) -> Vec<u8> {
        assert!(field < 16, "single-byte protobuf field required");
        assert!(payload.len() < 128, "single-byte protobuf length required");
        let mut encoded = Vec::with_capacity(payload.len() + 2);
        encoded.push((field << 3) | 2);
        encoded.push(u8::try_from(payload.len()).expect("payload length checked above"));
        encoded.extend_from_slice(payload);
        encoded
    }

    fn connect_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(payload.len() + 5);
        frame.push(flags);
        frame.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("smoke payload fits in u32")
                .to_be_bytes(),
        );
        frame.extend_from_slice(payload);
        frame
    }

    let text_delta = len_delimited(1, b"pong");
    let interaction_update = len_delimited(1, &text_delta);
    let server_message = len_delimited(1, &interaction_update);
    let mut body = connect_frame(0, &server_message);
    body.extend(connect_frame(0b0000_0010, b"{}"));

    MockHttpResponse {
        status: 200,
        headers: vec![(
            "Content-Type".to_string(),
            "application/connect+proto".to_string(),
        )],
        body,
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Providers requiring special skip treatment
// ═══════════════════════════════════════════════════════════════════════

/// Providers that need runtime environment resolution or complex auth flows
/// and cannot be smoke-tested with a simple mock HTTP server.
const SKIP_STREAM_PROVIDERS: &[&str] = &[
    "google-vertex",  // needs VERTEX_PROJECT/VERTEX_LOCATION env vars
    "azure-openai",   // needs Azure host format + runtime resolution
    "github-copilot", // needs OAuth token exchange
    "gitlab",         // needs OAuth token exchange
    "sap-ai-core",    // needs multi-env runtime config
];

/// Providers that fail at `create_provider` without env vars (superset of stream skips).
const SKIP_ROUTE_PROVIDERS: &[&str] = &[
    "google-vertex",  // needs GOOGLE_CLOUD_PROJECT env var at create time
    "amazon-bedrock", // needs runtime region/endpoint resolution
    "azure-openai",   // needs Azure host parsing at create time
    "github-copilot", // needs OAuth at create time
    "gitlab",         // needs OAuth at create time
    "sap-ai-core",    // no routing_defaults (skipped by None check)
];

// ═══════════════════════════════════════════════════════════════════════
// Test 1: Route selection across full matrix
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that `create_provider` succeeds for every provider in `PROVIDER_METADATA`
/// that has routing defaults, and that the returned provider reports the correct API
/// family and model ID.
#[test]
fn smoke_route_selection_full_matrix() {
    let harness = TestHarness::new("smoke_route_selection_full_matrix");
    let mut tested = 0u32;
    let mut skipped_no_defaults = 0u32;
    let mut skipped_env = 0u32;

    for meta in PROVIDER_METADATA {
        let Some(defaults) = meta.routing_defaults else {
            harness
                .log()
                .info_ctx("route.skip", "no routing_defaults", |ctx| {
                    ctx.push(("provider".to_string(), meta.canonical_id.to_string()));
                });
            skipped_no_defaults += 1;
            continue;
        };

        if SKIP_ROUTE_PROVIDERS.contains(&meta.canonical_id) {
            harness
                .log()
                .info_ctx("route.skip", "needs env vars", |ctx| {
                    ctx.push(("provider".to_string(), meta.canonical_id.to_string()));
                });
            skipped_env += 1;
            continue;
        }

        let mut entry = make_smoke_entry(meta.canonical_id, "smoke-route-model", defaults.base_url);
        // Clear api to let metadata resolve it
        entry.model.api.clear();

        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider failed for {}: {e}", meta.canonical_id));

        harness
            .log()
            .info_ctx("route.ok", "factory routed provider", |ctx| {
                ctx.push(("provider".to_string(), meta.canonical_id.to_string()));
                ctx.push(("name".to_string(), provider.name().to_string()));
                ctx.push(("api".to_string(), provider.api().to_string()));
                ctx.push(("expected_api".to_string(), defaults.api.to_string()));
            });

        assert_eq!(
            provider.api(),
            defaults.api,
            "provider {} api mismatch: got {} expected {}",
            meta.canonical_id,
            provider.api(),
            defaults.api
        );
        assert_eq!(
            provider.model_id(),
            "smoke-route-model",
            "provider {} model_id mismatch",
            meta.canonical_id
        );
        tested += 1;
    }

    harness
        .log()
        .info_ctx("route.summary", "route selection complete", |ctx| {
            ctx.push(("tested".to_string(), tested.to_string()));
            ctx.push((
                "skipped_no_defaults".to_string(),
                skipped_no_defaults.to_string(),
            ));
            ctx.push(("skipped_env".to_string(), skipped_env.to_string()));
            ctx.push((
                "total_metadata".to_string(),
                PROVIDER_METADATA.len().to_string(),
            ));
        });

    // Sanity: we should test a significant number of providers
    assert!(
        tested >= 75,
        "expected at least 75 providers tested, got {tested}"
    );
}

/// Verifies alias resolution is consistent with canonical ID for every provider.
#[test]
fn smoke_alias_resolution_full_matrix() {
    let harness = TestHarness::new("smoke_alias_resolution_full_matrix");
    let mut checked = 0u32;

    for meta in PROVIDER_METADATA {
        // Canonical ID resolves to itself
        let resolved = canonical_provider_id(meta.canonical_id);
        assert_eq!(
            resolved,
            Some(meta.canonical_id),
            "canonical_id {} should resolve to itself",
            meta.canonical_id
        );
        checked += 1;

        // Each alias resolves to the canonical ID
        for alias in meta.aliases {
            let alias_resolved = canonical_provider_id(alias);
            assert_eq!(
                alias_resolved,
                Some(meta.canonical_id),
                "alias {alias} should resolve to {}",
                meta.canonical_id
            );
            checked += 1;
        }
    }

    harness
        .log()
        .info_ctx("alias.summary", "alias resolution complete", |ctx| {
            ctx.push(("checked".to_string(), checked.to_string()));
        });
}

// ═══════════════════════════════════════════════════════════════════════
// Test 2: OpenAI-completions matrix (Bearer auth + /chat/completions)
// ═══════════════════════════════════════════════════════════════════════

/// Smoke-tests every `openai-completions` provider through mock HTTP, verifying
/// the request hits `/chat/completions`, uses Bearer auth, and sends valid JSON.
#[test]
#[allow(clippy::too_many_lines)]
fn smoke_openai_completions_matrix() {
    let harness = TestHarness::new("smoke_openai_completions_matrix");
    let mut tested = 0u32;
    let mut skipped = 0u32;

    for (index, meta) in PROVIDER_METADATA.iter().enumerate() {
        let Some(defaults) = meta.routing_defaults else {
            continue;
        };
        if defaults.api != "openai-completions" {
            continue;
        }
        if SKIP_STREAM_PROVIDERS.contains(&meta.canonical_id) {
            harness
                .log()
                .info_ctx("oai.skip", "provider needs special setup", |ctx| {
                    ctx.push(("provider".to_string(), meta.canonical_id.to_string()));
                });
            skipped += 1;
            continue;
        }

        let server = harness.start_mock_http_server();
        let safe_id = meta.canonical_id.replace('-', "_");
        let path_prefix = format!("/smoke/oai/{index}/{safe_id}");
        let expected_path = format!("{path_prefix}/chat/completions");
        server.add_route(
            "POST",
            &expected_path,
            text_event_stream_response(openai_chat_sse()),
        );

        let mut entry = make_smoke_entry(
            meta.canonical_id,
            "smoke-oai-model",
            &format!("{}{path_prefix}", server.base_url()),
        );
        entry.model.api.clear();

        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider failed for {}: {e}", meta.canonical_id));
        assert_eq!(
            provider.api(),
            "openai-completions",
            "provider {} should route to openai-completions",
            meta.canonical_id
        );

        let api_key = format!("smoke-oai-key-{index}");
        let options = StreamOptions {
            api_key: Some(api_key.clone()),
            max_tokens: Some(64),
            ..Default::default()
        };
        drive_to_done(provider, minimal_context(), options);

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly one request for {}",
            meta.canonical_id
        );
        let request = &requests[0];

        // Verify path
        assert_eq!(
            request.path, expected_path,
            "path mismatch for {}",
            meta.canonical_id
        );

        // Verify auth: openai-completions providers with auth_header use Bearer
        if defaults.auth_header {
            let expected_auth = format!("Bearer {api_key}");
            assert_eq!(
                request_header(&request.headers, "authorization").as_deref(),
                Some(expected_auth.as_str()),
                "Bearer auth mismatch for {}",
                meta.canonical_id
            );
        }

        // Verify content-type
        assert_eq!(
            request_header(&request.headers, "content-type").as_deref(),
            Some("application/json"),
            "content-type mismatch for {}",
            meta.canonical_id
        );

        // Verify body is valid JSON
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_else(|e| {
            panic!(
                "request body for {} is not valid JSON: {e}",
                meta.canonical_id
            )
        });
        assert!(
            body.get("messages").is_some(),
            "request body for {} missing 'messages' field",
            meta.canonical_id
        );

        harness
            .log()
            .info_ctx("oai.ok", "openai-completions smoke passed", |ctx| {
                ctx.push(("provider".to_string(), meta.canonical_id.to_string()));
                ctx.push(("path".to_string(), request.path.clone()));
            });
        tested += 1;
    }

    harness
        .log()
        .info_ctx("oai.summary", "openai-completions matrix complete", |ctx| {
            ctx.push(("tested".to_string(), tested.to_string()));
            ctx.push(("skipped".to_string(), skipped.to_string()));
        });

    assert!(
        tested >= 55,
        "expected at least 55 openai-completions providers tested, got {tested}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Test 3: Anthropic-messages matrix (x-api-key + messages path)
// ═══════════════════════════════════════════════════════════════════════

/// Smoke-tests every `anthropic-messages` provider through mock HTTP, verifying
/// the request uses x-api-key auth and sends valid JSON.
#[test]
#[allow(clippy::too_many_lines)]
fn smoke_anthropic_messages_matrix() {
    let harness = TestHarness::new("smoke_anthropic_messages_matrix");
    let mut tested = 0u32;

    for (index, meta) in PROVIDER_METADATA.iter().enumerate() {
        let Some(defaults) = meta.routing_defaults else {
            continue;
        };
        if defaults.api != "anthropic-messages" {
            continue;
        }
        if SKIP_STREAM_PROVIDERS.contains(&meta.canonical_id) {
            continue;
        }

        let server = harness.start_mock_http_server();
        let safe_id = meta.canonical_id.replace('-', "_");
        let expected_path = format!("/smoke/anth/{index}/{safe_id}/v1/messages");
        server.add_route(
            "POST",
            &expected_path,
            text_event_stream_response(anthropic_messages_sse()),
        );

        // Anthropic-compatible bases are normalized to the messages endpoint.
        let mut entry = make_smoke_entry(
            meta.canonical_id,
            "smoke-anth-model",
            &format!("{}{expected_path}", server.base_url()),
        );
        entry.model.api.clear();

        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("create_provider failed for {}: {e}", meta.canonical_id));
        // Anthropic-messages providers route through the Anthropic impl
        assert_eq!(
            provider.api(),
            "anthropic-messages",
            "provider {} should route to anthropic-messages",
            meta.canonical_id
        );

        let api_key = format!("sk-ant-smoke-anth-key-{index}");
        let options = StreamOptions {
            api_key: Some(api_key.clone()),
            max_tokens: Some(64),
            ..Default::default()
        };
        drive_to_done(provider, minimal_context(), options);

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly one request for {}",
            meta.canonical_id
        );
        let request = &requests[0];

        // Verify path
        assert_eq!(
            request.path, expected_path,
            "path mismatch for {}",
            meta.canonical_id
        );

        // Verify auth: anthropic uses x-api-key
        assert_eq!(
            request_header(&request.headers, "x-api-key").as_deref(),
            Some(api_key.as_str()),
            "x-api-key auth mismatch for {}",
            meta.canonical_id
        );

        // Verify content-type
        assert_eq!(
            request_header(&request.headers, "content-type").as_deref(),
            Some("application/json"),
            "content-type mismatch for {}",
            meta.canonical_id
        );

        // Verify body is valid JSON with messages
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_else(|e| {
            panic!(
                "request body for {} is not valid JSON: {e}",
                meta.canonical_id
            )
        });
        assert!(
            body.get("messages").is_some(),
            "request body for {} missing 'messages' field",
            meta.canonical_id
        );

        // Verify anthropic-version header
        assert_eq!(
            request_header(&request.headers, "anthropic-version").as_deref(),
            Some("2023-06-01"),
            "anthropic-version header missing for {}",
            meta.canonical_id
        );

        harness
            .log()
            .info_ctx("anth.ok", "anthropic-messages smoke passed", |ctx| {
                ctx.push(("provider".to_string(), meta.canonical_id.to_string()));
                ctx.push(("path".to_string(), request.path.clone()));
            });
        tested += 1;
    }

    harness.log().info_ctx(
        "anth.summary",
        "anthropic-messages matrix complete",
        |ctx| {
            ctx.push(("tested".to_string(), tested.to_string()));
        },
    );

    assert!(
        tested >= 5,
        "expected at least 5 anthropic-messages providers tested, got {tested}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Test 4: Native providers baseline
// ═══════════════════════════════════════════════════════════════════════

/// Smoke-tests native Anthropic through mock HTTP.
#[test]
fn smoke_native_anthropic_baseline() {
    let harness = TestHarness::new("smoke_native_anthropic_baseline");
    let server = harness.start_mock_http_server();
    let expected_path = "/smoke/native/anthropic/v1/messages";
    server.add_route(
        "POST",
        expected_path,
        text_event_stream_response(anthropic_messages_sse()),
    );

    let mut entry = make_smoke_entry(
        "anthropic",
        "smoke-claude",
        &format!("{}{expected_path}", server.base_url()),
    );
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create native anthropic provider");
    assert_eq!(provider.name(), "anthropic");
    assert_eq!(provider.api(), "anthropic-messages");

    let api_key = "sk-ant-smoke-anthropic-native-key".to_string();
    let options = StreamOptions {
        api_key: Some(api_key.clone()),
        max_tokens: Some(64),
        ..Default::default()
    };
    drive_to_done(provider, minimal_context(), options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request_header(&request.headers, "x-api-key").as_deref(),
        Some(api_key.as_str())
    );
    assert_eq!(
        request_header(&request.headers, "content-type").as_deref(),
        Some("application/json")
    );

    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("valid JSON body");
    assert!(body.get("messages").is_some());

    harness
        .log()
        .info_ctx("native.ok", "anthropic baseline passed", |ctx| {
            ctx.push(("path".to_string(), request.path.clone()));
        });
}

/// Smoke-tests native `OpenAI` (defaults to responses API) through mock HTTP.
#[test]
fn smoke_native_openai_responses_baseline() {
    let harness = TestHarness::new("smoke_native_openai_responses_baseline");
    let server = harness.start_mock_http_server();
    let path_prefix = "/smoke/native/openai";
    let expected_path = format!("{path_prefix}/responses");
    server.add_route(
        "POST",
        &expected_path,
        text_event_stream_response(openai_responses_sse()),
    );

    let mut entry = make_smoke_entry(
        "openai",
        "smoke-gpt",
        &format!("{}{path_prefix}", server.base_url()),
    );
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create native openai provider");
    assert_eq!(provider.name(), "openai");
    assert_eq!(provider.api(), "openai-responses");

    let api_key = "smoke-openai-native-key".to_string();
    let options = StreamOptions {
        api_key: Some(api_key.clone()),
        max_tokens: Some(64),
        ..Default::default()
    };
    drive_to_done(provider, minimal_context(), options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request_header(&request.headers, "authorization").as_deref(),
        Some(format!("Bearer {api_key}").as_str())
    );
    assert_eq!(
        request_header(&request.headers, "content-type").as_deref(),
        Some("application/json")
    );

    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("valid JSON body");
    assert!(body.get("input").is_some() || body.get("messages").is_some());

    harness
        .log()
        .info_ctx("native.ok", "openai responses baseline passed", |ctx| {
            ctx.push(("path".to_string(), request.path.clone()));
        });
}

/// Smoke-tests native `OpenAI` completions API (explicit override) through mock HTTP.
#[test]
fn smoke_native_openai_completions_baseline() {
    let harness = TestHarness::new("smoke_native_openai_completions_baseline");
    let server = harness.start_mock_http_server();
    let path_prefix = "/smoke/native/openai_cc";
    let expected_path = format!("{path_prefix}/chat/completions");
    server.add_route(
        "POST",
        &expected_path,
        text_event_stream_response(openai_chat_sse()),
    );

    let mut entry = make_smoke_entry(
        "openai",
        "smoke-gpt-cc",
        &format!("{}{path_prefix}", server.base_url()),
    );
    entry.model.api = "openai-completions".to_string();
    let provider =
        create_provider(&entry, None).expect("create native openai completions provider");
    assert_eq!(provider.name(), "openai");
    assert_eq!(provider.api(), "openai-completions");

    let api_key = "smoke-openai-cc-key".to_string();
    let options = StreamOptions {
        api_key: Some(api_key.clone()),
        max_tokens: Some(64),
        ..Default::default()
    };
    drive_to_done(provider, minimal_context(), options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request_header(&request.headers, "authorization").as_deref(),
        Some(format!("Bearer {api_key}").as_str())
    );

    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("valid JSON body");
    assert!(body.get("messages").is_some());

    harness
        .log()
        .info_ctx("native.ok", "openai completions baseline passed", |ctx| {
            ctx.push(("path".to_string(), request.path.clone()));
        });
}

/// Smoke-tests native Gemini through mock HTTP (header-based API key auth).
#[test]
fn smoke_native_gemini_baseline() {
    let harness = TestHarness::new("smoke_native_gemini_baseline");
    let server = harness.start_mock_http_server();
    let api_key = "smoke-gemini-key";
    let model_id = "smoke-gemini-model";
    let expected_path = format!("/v1beta/models/{model_id}:streamGenerateContent?alt=sse");
    server.add_route(
        "POST",
        &expected_path,
        text_event_stream_response(gemini_sse()),
    );

    let mut entry = make_smoke_entry("google", model_id, &format!("{}/v1beta", server.base_url()));
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create native gemini provider");
    assert_eq!(provider.name(), "google");
    assert_eq!(provider.api(), "google-generative-ai");

    let options = StreamOptions {
        api_key: Some(api_key.to_string()),
        max_tokens: Some(64),
        ..Default::default()
    };
    drive_to_done(provider, minimal_context(), options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request_header(&request.headers, "x-goog-api-key").as_deref(),
        Some(api_key),
        "Gemini request should send API key in x-goog-api-key"
    );
    assert_eq!(
        request_header(&request.headers, "content-type").as_deref(),
        Some("application/json")
    );

    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("valid JSON body");
    assert!(body.get("contents").is_some());

    harness
        .log()
        .info_ctx("native.ok", "gemini baseline passed", |ctx| {
            ctx.push(("path".to_string(), request.path.clone()));
        });
}

fn gemini_sse() -> String {
    [
        r#"data: {"candidates":[{"content":{"parts":[{"text":"pong"}],"role":"model"}}]}"#,
        "",
        r#"data: {"candidates":[{"content":{"parts":[],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#,
        "",
    ]
    .join("\n")
}

/// Smoke-tests native Cohere through mock HTTP.
#[test]
fn smoke_native_cohere_baseline() {
    let harness = TestHarness::new("smoke_native_cohere_baseline");
    let server = harness.start_mock_http_server();
    let path_prefix = "/smoke/native/cohere";
    let expected_path = format!("{path_prefix}/chat");
    server.add_route(
        "POST",
        &expected_path,
        text_event_stream_response(cohere_chat_sse()),
    );

    let mut entry = make_smoke_entry(
        "cohere",
        "smoke-command",
        &format!("{}{path_prefix}", server.base_url()),
    );
    entry.model.api.clear();
    let provider = create_provider(&entry, None).expect("create native cohere provider");
    assert_eq!(provider.name(), "cohere");
    assert_eq!(provider.api(), "cohere-chat");

    let api_key = "smoke-cohere-key".to_string();
    let options = StreamOptions {
        api_key: Some(api_key),
        max_tokens: Some(64),
        ..Default::default()
    };
    drive_to_done(provider, minimal_context(), options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request_header(&request.headers, "content-type").as_deref(),
        Some("application/json")
    );

    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("valid JSON body");
    assert!(body.get("messages").is_some());

    harness
        .log()
        .info_ctx("native.ok", "cohere baseline passed", |ctx| {
            ctx.push(("path".to_string(), request.path.clone()));
        });
}

/// Smoke-tests native Bedrock through mock HTTP (JSON response, not SSE).
#[test]
fn smoke_native_bedrock_baseline() {
    let harness = TestHarness::new("smoke_native_bedrock_baseline");
    let server = harness.start_mock_http_server();
    let bedrock_model = "anthropic.claude-3-5-sonnet-v1";
    let expected_path = format!("/model/{bedrock_model}/converse");
    server.add_route(
        "POST",
        &expected_path,
        MockHttpResponse::json(200, &bedrock_converse_json()),
    );

    let mut entry = make_smoke_entry("amazon-bedrock", bedrock_model, &server.base_url());
    entry.model.api = "bedrock-converse-stream".to_string();
    let provider = create_provider(&entry, None).expect("create native bedrock provider");
    assert_eq!(provider.name(), "amazon-bedrock");
    assert_eq!(provider.api(), "bedrock-converse-stream");

    let api_key = "smoke-bedrock-token".to_string();
    let options = StreamOptions {
        api_key: Some(api_key.clone()),
        max_tokens: Some(64),
        ..Default::default()
    };
    drive_to_done(provider, minimal_context(), options);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, expected_path);
    assert_eq!(
        request_header(&request.headers, "authorization").as_deref(),
        Some(format!("Bearer {api_key}").as_str())
    );
    assert_eq!(
        request_header(&request.headers, "content-type").as_deref(),
        Some("application/json")
    );

    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("valid JSON body");
    assert!(body.get("messages").is_some());

    harness
        .log()
        .info_ctx("native.ok", "bedrock baseline passed", |ctx| {
            ctx.push(("path".to_string(), request.path.clone()));
        });
}

// ═══════════════════════════════════════════════════════════════════════
// Test 5: Structured smoke report
// ═══════════════════════════════════════════════════════════════════════

/// Generates a compact JSON smoke report covering every provider in the matrix.
/// The report is written to the harness artifact directory.
#[test]
fn smoke_report_artifact() {
    let harness = TestHarness::new("smoke_report_artifact");

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut total = 0u32;
    let mut route_pass = 0u32;
    let mut stream_pass = 0u32;
    let mut stream_skip = 0u32;

    for meta in PROVIDER_METADATA {
        total += 1;

        let Some(defaults) = meta.routing_defaults else {
            results.push(serde_json::json!({
                "provider": meta.canonical_id,
                "api": null,
                "route_ok": false,
                "stream_ok": null,
                "reason": "no routing_defaults"
            }));
            stream_skip += 1;
            continue;
        };

        // Skip providers that need env vars
        if SKIP_ROUTE_PROVIDERS.contains(&meta.canonical_id) {
            results.push(serde_json::json!({
                "provider": meta.canonical_id,
                "api": defaults.api,
                "route_ok": null,
                "stream_ok": null,
                "reason": "needs runtime env vars"
            }));
            stream_skip += 1;
            continue;
        }

        // Test route selection
        let mut entry =
            make_smoke_entry(meta.canonical_id, "smoke-report-model", defaults.base_url);
        entry.model.api.clear();

        let route_ok = create_provider(&entry, None).is_ok();
        if route_ok {
            route_pass += 1;
        }

        // Test stream (only for non-skip providers)
        let stream_ok = if SKIP_STREAM_PROVIDERS.contains(&meta.canonical_id) {
            stream_skip += 1;
            None
        } else {
            let result = run_smoke_stream(&harness, meta.canonical_id, defaults);
            if result {
                stream_pass += 1;
            }
            Some(result)
        };

        results.push(serde_json::json!({
            "provider": meta.canonical_id,
            "api": defaults.api,
            "onboarding": format!("{:?}", meta.onboarding),
            "route_ok": route_ok,
            "stream_ok": stream_ok,
        }));
    }

    let report = serde_json::json!({
        "schema": "pi.smoke_matrix.v1",
        "total": total,
        "route_pass": route_pass,
        "stream_pass": stream_pass,
        "stream_skip": stream_skip,
        "results": results,
    });

    let report_json = serde_json::to_string_pretty(&report).expect("serialize report");
    let report_path = harness.temp_path("smoke_report.json");
    std::fs::write(&report_path, &report_json).expect("write smoke report");
    harness.record_artifact("smoke_report.json", &report_path);

    harness
        .log()
        .info_ctx("report", "smoke report generated", |ctx| {
            ctx.push(("total".to_string(), total.to_string()));
            ctx.push(("route_pass".to_string(), route_pass.to_string()));
            ctx.push(("stream_pass".to_string(), stream_pass.to_string()));
            ctx.push(("stream_skip".to_string(), stream_skip.to_string()));
        });

    // All route selections must pass for providers with defaults
    assert!(
        route_pass >= 75,
        "expected at least 75 route selections to pass, got {route_pass}"
    );
    assert!(
        stream_pass >= 70,
        "expected at least 70 stream smokes to pass, got {stream_pass}"
    );
}

/// Runs a single smoke stream for a provider, returning true if it passes.
fn run_smoke_stream(
    harness: &TestHarness,
    provider_id: &str,
    defaults: pi::provider_metadata::ProviderRoutingDefaults,
) -> bool {
    let server = harness.start_mock_http_server();
    let safe_id = provider_id.replace('-', "_");

    let (base_url_str, expected_path) = match defaults.api {
        "openai-completions" => {
            let prefix = format!("/report/{safe_id}");
            let path = format!("{prefix}/chat/completions");
            server.add_route("POST", &path, text_event_stream_response(openai_chat_sse()));
            (format!("{}{prefix}", server.base_url()), path)
        }
        "openai-responses" => {
            let prefix = format!("/report/{safe_id}");
            let path = format!("{prefix}/responses");
            server.add_route(
                "POST",
                &path,
                text_event_stream_response(openai_responses_sse()),
            );
            (format!("{}{prefix}", server.base_url()), path)
        }
        "anthropic-messages" => {
            let path = format!("/report/{safe_id}");
            server.add_route(
                "POST",
                &path,
                text_event_stream_response(anthropic_messages_sse()),
            );
            (format!("{}{path}", server.base_url()), path)
        }
        "cohere-chat" => {
            let prefix = format!("/report/{safe_id}");
            let path = format!("{prefix}/chat");
            server.add_route("POST", &path, text_event_stream_response(cohere_chat_sse()));
            (format!("{}{prefix}", server.base_url()), path)
        }
        "google-generative-ai" => {
            let path =
                "/v1beta/models/smoke-report-model:streamGenerateContent?alt=sse".to_string();
            server.add_route("POST", &path, text_event_stream_response(gemini_sse()));
            (format!("{}/v1beta", server.base_url()), path)
        }
        "bedrock-converse-stream" => {
            let model = "smoke-report-model";
            let path = format!("/model/{model}/converse");
            server.add_route(
                "POST",
                &path,
                MockHttpResponse::json(200, &bedrock_converse_json()),
            );
            (server.base_url(), path)
        }
        "cursor-agent" => {
            let path = "/agent.v1.AgentService/Run".to_string();
            server.add_route("POST", &path, cursor_connect_response());
            (server.base_url(), path)
        }
        other => {
            harness
                .log()
                .info_ctx("report.skip", "unknown api family", |ctx| {
                    ctx.push(("provider".to_string(), provider_id.to_string()));
                    ctx.push(("api".to_string(), other.to_string()));
                });
            return false;
        }
    };

    let mut entry = make_smoke_entry(provider_id, "smoke-report-model", &base_url_str);
    entry.model.api.clear();

    let Ok(provider) = create_provider(&entry, None) else {
        return false;
    };

    let api_key = if defaults.api == "google-generative-ai" {
        "smoke-report-key".to_string()
    } else {
        format!("smoke-report-key-{safe_id}")
    };

    let options = StreamOptions {
        api_key: Some(api_key),
        max_tokens: Some(64),
        ..Default::default()
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drive_to_done(provider, minimal_context(), options);
    }));

    if result.is_err() {
        harness
            .log()
            .info_ctx("report.fail", "stream smoke failed", |ctx| {
                ctx.push(("provider".to_string(), provider_id.to_string()));
            });
        return false;
    }

    let requests = server.requests();
    if requests.is_empty() {
        return false;
    }

    requests[0].path == expected_path
}

// ═══════════════════════════════════════════════════════════════════════
// Test 6: Cross-cutting invariants
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that every provider's routing defaults (when present) specify
/// a known API family that the factory can handle.
#[test]
fn smoke_all_api_families_are_known() {
    let harness = TestHarness::new("smoke_all_api_families_are_known");
    let known_apis = [
        "openai-completions",
        "openai-responses",
        "anthropic-messages",
        "cohere-chat",
        "google-generative-ai",
        "google-vertex",
        "bedrock-converse-stream",
        "cursor-agent",
    ];

    for meta in PROVIDER_METADATA {
        if let Some(defaults) = meta.routing_defaults {
            assert!(
                known_apis.contains(&defaults.api),
                "provider {} has unknown api family: {}",
                meta.canonical_id,
                defaults.api
            );
        }
    }

    harness
        .log()
        .info_ctx("invariant.ok", "all API families known", |_ctx| {});
}

/// Verifies that every credentialed `openai-completions` preset has `auth_header=true`.
#[test]
fn smoke_credentialed_openai_completions_presets_use_bearer_auth() {
    let harness = TestHarness::new("smoke_credentialed_openai_completions_presets_use_bearer_auth");
    let mut checked = 0u32;

    for meta in PROVIDER_METADATA {
        if meta.onboarding != ProviderOnboardingMode::OpenAICompatiblePreset {
            continue;
        }
        if let Some(defaults) = meta.routing_defaults {
            if defaults.api == "openai-completions" {
                if meta.auth_env_keys.is_empty() {
                    assert!(
                        !defaults.auth_header,
                        "local/no-auth preset {} should not require bearer auth",
                        meta.canonical_id
                    );
                    continue;
                }
                assert!(
                    defaults.auth_header,
                    "openai-completions preset {} should have auth_header=true",
                    meta.canonical_id
                );
                checked += 1;
            }
        }
    }

    harness
        .log()
        .info_ctx("invariant.ok", "all OAI presets use bearer auth", |ctx| {
            ctx.push(("checked".to_string(), checked.to_string()));
        });

    assert!(
        checked >= 40,
        "expected at least 40 openai-completions presets checked, got {checked}"
    );
}

/// Verifies that every provider with routing defaults has non-empty `base_url`
/// and positive `context_window` / `max_tokens`.
#[test]
fn smoke_routing_defaults_sanity() {
    let harness = TestHarness::new("smoke_routing_defaults_sanity");
    let mut checked = 0u32;

    for meta in PROVIDER_METADATA {
        if let Some(defaults) = meta.routing_defaults {
            // Skip providers with runtime-resolved base URLs
            if SKIP_ROUTE_PROVIDERS.contains(&meta.canonical_id) {
                continue;
            }
            assert!(
                !defaults.base_url.is_empty(),
                "provider {} has empty base_url",
                meta.canonical_id
            );
            assert!(
                defaults.context_window > 0,
                "provider {} has zero context_window",
                meta.canonical_id
            );
            assert!(
                defaults.max_tokens > 0,
                "provider {} has zero max_tokens",
                meta.canonical_id
            );
            assert!(
                defaults.context_window >= defaults.max_tokens,
                "provider {} context_window ({}) < max_tokens ({})",
                meta.canonical_id,
                defaults.context_window,
                defaults.max_tokens
            );
            checked += 1;
        }
    }

    harness
        .log()
        .info_ctx("invariant.ok", "routing defaults sanity passed", |ctx| {
            ctx.push(("checked".to_string(), checked.to_string()));
        });
}

/// Verifies that the skip list contains only providers that actually exist
/// in the metadata registry.
#[test]
fn smoke_skip_list_validity() {
    let harness = TestHarness::new("smoke_skip_list_validity");

    for skip_id in SKIP_STREAM_PROVIDERS {
        let found = PROVIDER_METADATA.iter().any(|m| m.canonical_id == *skip_id);
        assert!(
            found,
            "skip list entry '{skip_id}' not found in PROVIDER_METADATA"
        );
    }

    harness
        .log()
        .info_ctx("invariant.ok", "skip list valid", |ctx| {
            ctx.push((
                "entries".to_string(),
                SKIP_STREAM_PROVIDERS.len().to_string(),
            ));
        });
}
