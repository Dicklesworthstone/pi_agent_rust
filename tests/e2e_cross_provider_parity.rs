//! Live cross-provider parity tests for the unified Provider interface.
//!
//! Gate: `CI_E2E_TESTS=1`

mod common;

use common::{
    LIVE_E2E_TIMEOUT, LIVE_SHORT_PROMPT, LiveE2eRegistry, LiveProviderTarget, TestHarness,
    build_live_context, build_live_stream_options, ci_e2e_tests_enabled, create_live_provider,
    parse_http_status,
};
use futures::{FutureExt, StreamExt, pin_mut};
use pi::model::StreamEvent;
use pi::provider::{Context, StreamOptions, ToolDef};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

const PARITY_TARGETS: [LiveProviderTarget; 11] = [
    LiveProviderTarget::new(
        "anthropic",
        "ANTHROPIC_TEST_MODEL",
        &[
            "claude-haiku-4-5",
            "claude-3-5-haiku-20241022",
            "claude-sonnet-4-5",
        ],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new(
        "openai",
        "OPENAI_TEST_MODEL",
        &["gpt-4o-mini", "gpt-4o", "gpt-5.1-codex"],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new(
        "google",
        "GOOGLE_TEST_MODEL",
        &["gemini-2.5-flash", "gemini-1.5-flash", "gemini-2.5-pro"],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new(
        "openrouter",
        "OPENROUTER_TEST_MODEL",
        &[],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new(
        "orcarouter",
        "ORCAROUTER_TEST_MODEL",
        &[],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new("xai", "XAI_TEST_MODEL", &[], LIVE_SHORT_PROMPT),
    LiveProviderTarget::new("deepseek", "DEEPSEEK_TEST_MODEL", &[], LIVE_SHORT_PROMPT),
    // Gap providers (bd-3uqg.11.11.4)
    LiveProviderTarget::new("groq", "GROQ_TEST_MODEL", &[], LIVE_SHORT_PROMPT),
    LiveProviderTarget::new("cerebras", "CEREBRAS_TEST_MODEL", &[], LIVE_SHORT_PROMPT),
    LiveProviderTarget::new(
        "moonshotai",
        "MOONSHOTAI_TEST_MODEL",
        &[],
        LIVE_SHORT_PROMPT,
    ),
    LiveProviderTarget::new("alibaba", "ALIBABA_TEST_MODEL", &[], LIVE_SHORT_PROMPT),
];

const TOOL_SCHEMA_TARGETS: [&str; 3] = ["anthropic", "openai", "google"];
const INVALID_KEY_SENTINEL: &str = "pi-invalid-key-for-parity";

#[derive(Debug, Clone, Default)]
struct EventStats {
    event_count: usize,
    text_chars: usize,
    thinking_chars: usize,
    tool_calls: usize,
    stop_reason: Option<String>,
    usage_total_tokens: u64,
    error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ParityRecord {
    check: String,
    provider: String,
    model: Option<String>,
    api: Option<String>,
    status: String,
    skip_reason: Option<String>,
    elapsed_ms: u64,
    event_count: usize,
    text_chars: usize,
    thinking_chars: usize,
    tool_calls: usize,
    stop_reason: Option<String>,
    usage_total_tokens: u64,
    sequence: Vec<String>,
    sequence_valid: bool,
    sequence_error: Option<String>,
    http_status: Option<u16>,
    error_kind: Option<String>,
    error: Option<String>,
}

fn target_by_provider(provider: &str) -> Option<LiveProviderTarget> {
    PARITY_TARGETS
        .iter()
        .copied()
        .find(|target| target.provider == provider)
}

fn shared_tool_schema() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "sum_numbers".to_string(),
        description: "Add two integers and return the total".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "a": {"type": "integer", "description": "first operand"},
                "b": {"type": "integer", "description": "second operand"}
            },
            "required": ["a", "b"],
            "additionalProperties": false
        }),
    }]
}

const fn event_kind(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::Start { .. } => "Start",
        StreamEvent::TextStart { .. } => "TextStart",
        StreamEvent::TextDelta { .. } => "TextDelta",
        StreamEvent::TextEnd { .. } => "TextEnd",
        StreamEvent::ThinkingStart { .. } => "ThinkingStart",
        StreamEvent::ThinkingDelta { .. } => "ThinkingDelta",
        StreamEvent::ThinkingEnd { .. } => "ThinkingEnd",
        StreamEvent::ToolCallStart { .. } => "ToolCallStart",
        StreamEvent::ToolCallDelta { .. } => "ToolCallDelta",
        StreamEvent::ToolCallEnd { .. } => "ToolCallEnd",
        StreamEvent::Done { .. } => "Done",
        StreamEvent::Error { .. } => "Error",
    }
}

fn check_event_sequence(events: &[StreamEvent]) -> Result<(), String> {
    if events.is_empty() {
        return Err("no stream events emitted".to_string());
    }

    if !matches!(events.first(), Some(StreamEvent::Start { .. })) {
        return Err("first stream event must be Start".to_string());
    }

    if !matches!(
        events.last(),
        Some(StreamEvent::Done { .. } | StreamEvent::Error { .. })
    ) {
        return Err("last stream event must be Done or Error".to_string());
    }

    let mut in_text_block = false;
    let mut in_thinking_block = false;
    let mut in_tool_block = false;

    for event in events {
        match event {
            StreamEvent::TextStart { .. } => in_text_block = true,
            StreamEvent::TextDelta { .. } => {
                if !in_text_block {
                    return Err("TextDelta emitted before TextStart".to_string());
                }
            }
            StreamEvent::TextEnd { .. } => {
                if !in_text_block {
                    return Err("TextEnd emitted without TextStart".to_string());
                }
                in_text_block = false;
            }
            StreamEvent::ThinkingStart { .. } => in_thinking_block = true,
            StreamEvent::ThinkingDelta { .. } => {
                if !in_thinking_block {
                    return Err("ThinkingDelta emitted before ThinkingStart".to_string());
                }
            }
            StreamEvent::ThinkingEnd { .. } => {
                if !in_thinking_block {
                    return Err("ThinkingEnd emitted without ThinkingStart".to_string());
                }
                in_thinking_block = false;
            }
            StreamEvent::ToolCallStart { .. } => in_tool_block = true,
            StreamEvent::ToolCallDelta { .. } => {
                if !in_tool_block {
                    return Err("ToolCallDelta emitted before ToolCallStart".to_string());
                }
            }
            StreamEvent::ToolCallEnd { .. } => {
                if !in_tool_block {
                    return Err("ToolCallEnd emitted without ToolCallStart".to_string());
                }
                in_tool_block = false;
            }
            StreamEvent::Start { .. } | StreamEvent::Done { .. } | StreamEvent::Error { .. } => {}
        }
    }

    Ok(())
}

fn summarize_events(events: &[StreamEvent]) -> EventStats {
    let mut stats = EventStats::default();

    for event in events {
        stats.event_count = stats.event_count.saturating_add(1);
        match event {
            StreamEvent::TextDelta { delta, .. } => {
                stats.text_chars = stats.text_chars.saturating_add(delta.chars().count());
            }
            StreamEvent::TextEnd { content, .. } => {
                stats.text_chars = content.chars().count();
            }
            StreamEvent::ThinkingDelta { delta, .. } => {
                stats.thinking_chars = stats.thinking_chars.saturating_add(delta.chars().count());
            }
            StreamEvent::ThinkingEnd { content, .. } => {
                stats.thinking_chars = content.chars().count();
            }
            StreamEvent::ToolCallEnd { .. } => {
                stats.tool_calls = stats.tool_calls.saturating_add(1);
            }
            StreamEvent::Done { reason, message } => {
                stats.stop_reason = Some(format!("{reason:?}"));
                stats.usage_total_tokens = message.usage.total_tokens;
            }
            StreamEvent::Error { reason, error } => {
                stats.stop_reason = Some(format!("{reason:?}"));
                stats.usage_total_tokens = error.usage.total_tokens;
                stats.error_message = error
                    .error_message
                    .clone()
                    .or_else(|| Some("provider emitted stream error event".to_string()));
            }
            StreamEvent::Start { .. }
            | StreamEvent::TextStart { .. }
            | StreamEvent::ThinkingStart { .. }
            | StreamEvent::ToolCallStart { .. }
            | StreamEvent::ToolCallDelta { .. } => {}
        }
    }

    stats
}

fn classify_error_kind(http_status: Option<u16>, error: Option<&str>) -> String {
    if let Some(status) = http_status {
        if (400..500).contains(&status) {
            return "client".to_string();
        }
        if status >= 500 {
            return "server".to_string();
        }
    }

    let text = error.unwrap_or_default().to_ascii_lowercase();
    if text.contains("unauthorized")
        || text.contains("invalid api key")
        || text.contains("authentication")
        || text.contains("forbidden")
    {
        return "client".to_string();
    }
    if text.contains("timed out")
        || text.contains("timeout")
        || text.contains("dns")
        || text.contains("connection")
    {
        return "transport".to_string();
    }

    "unknown".to_string()
}

fn write_jsonl<T: Serialize>(path: &Path, records: &[T]) -> std::io::Result<()> {
    let mut out = String::new();
    for record in records {
        let line = serde_json::to_string(record)
            .unwrap_or_else(|_| "{\"status\":\"serialization_error\"}".to_string());
        out.push_str(&line);
        out.push('\n');
    }
    std::fs::write(path, out)
}

fn write_markdown_report(path: &Path, records: &[ParityRecord]) -> std::io::Result<()> {
    let mut by_check: BTreeMap<&str, Vec<&ParityRecord>> = BTreeMap::new();
    for record in records {
        by_check
            .entry(record.check.as_str())
            .or_default()
            .push(record);
    }

    let mut markdown = String::new();
    markdown.push_str("# Cross-Provider Parity Report\n\n");

    for check in [
        "same_prompt",
        "tool_schema",
        "streaming_event_parity",
        "error_handling_parity",
    ] {
        let _ = write!(markdown, "## {check}\n\n");
        markdown.push_str(
            "| provider | status | elapsed_ms | total_tokens | text_chars | tool_calls | stop_reason | http_status | error_kind | sequence_valid |\n",
        );
        markdown.push_str("| --- | --- | ---: | ---: | ---: | ---: | --- | --- | --- | --- |\n");

        if let Some(rows) = by_check.get(check) {
            for row in rows {
                let stop_reason = row.stop_reason.as_deref().unwrap_or("-");
                let http_status = row
                    .http_status
                    .map_or_else(|| "-".to_string(), |status| status.to_string());
                let error_kind = row.error_kind.as_deref().unwrap_or("-");
                let _ = writeln!(
                    markdown,
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    row.provider,
                    row.status,
                    row.elapsed_ms,
                    row.usage_total_tokens,
                    row.text_chars,
                    row.tool_calls,
                    stop_reason,
                    http_status,
                    error_kind,
                    row.sequence_valid,
                );
            }
        } else {
            markdown.push_str("| (no records) | - | - | - | - | - | - | - | - | - |\n");
        }

        markdown.push('\n');
    }

    std::fs::write(path, markdown)
}

async fn collect_stream_events(
    provider: std::sync::Arc<dyn pi::provider::Provider>,
    context: Context<'static>,
    options: StreamOptions,
) -> Result<Vec<StreamEvent>, String> {
    let now = asupersync::Cx::current()
        .and_then(|cx| cx.timer_driver())
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());

    let timeout_fut = asupersync::time::sleep(now, LIVE_E2E_TIMEOUT).fuse();
    let run_fut = async move {
        let stream = provider
            .stream(&context, &options)
            .await
            .map_err(|err| err.to_string())?;
        let mut stream = std::pin::pin!(stream);
        let mut events = Vec::new();

        while let Some(item) = stream.next().await {
            let event = item.map_err(|err| err.to_string())?;
            let terminal = matches!(event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
            events.push(event);
            if terminal {
                break;
            }
        }

        Ok(events)
    }
    .fuse();

    pin_mut!(timeout_fut, run_fut);
    match futures::future::select(run_fut, timeout_fut).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => Err(format!(
            "request timed out after {}s",
            LIVE_E2E_TIMEOUT.as_secs()
        )),
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_parity_case(
    registry: &LiveE2eRegistry,
    target: &LiveProviderTarget,
    check: &str,
    prompt: &str,
    tools: Vec<ToolDef>,
    override_api_key: Option<&str>,
    max_tokens: u32,
) -> ParityRecord {
    let requested_model = std::env::var(target.model_env_var)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let Some(entry) = registry.select_entry(target, requested_model.as_deref()) else {
        return ParityRecord {
            check: check.to_string(),
            provider: target.provider.to_string(),
            model: requested_model,
            api: None,
            status: "skipped".to_string(),
            skip_reason: Some(format!(
                "no model with API key for provider '{}'",
                target.provider
            )),
            elapsed_ms: 0,
            event_count: 0,
            text_chars: 0,
            thinking_chars: 0,
            tool_calls: 0,
            stop_reason: None,
            usage_total_tokens: 0,
            sequence: Vec::new(),
            sequence_valid: false,
            sequence_error: Some("skipped".to_string()),
            http_status: None,
            error_kind: None,
            error: None,
        };
    };

    let api_key = if let Some(override_key) = override_api_key {
        override_key.to_string()
    } else if let Some(key) = registry.resolve_api_key(&entry) {
        key
    } else {
        return ParityRecord {
            check: check.to_string(),
            provider: entry.model.provider.clone(),
            model: Some(entry.model.id.clone()),
            api: Some(entry.model.api.clone()),
            status: "skipped".to_string(),
            skip_reason: Some("API key missing".to_string()),
            elapsed_ms: 0,
            event_count: 0,
            text_chars: 0,
            thinking_chars: 0,
            tool_calls: 0,
            stop_reason: None,
            usage_total_tokens: 0,
            sequence: Vec::new(),
            sequence_valid: false,
            sequence_error: Some("skipped".to_string()),
            http_status: None,
            error_kind: None,
            error: None,
        };
    };

    let client = pi::http::client::Client::new();
    let provider = match create_live_provider(&entry, client) {
        Ok(provider) => provider,
        Err(err) => {
            let error_text = err.to_string();
            let http_status = parse_http_status(&error_text);
            return ParityRecord {
                check: check.to_string(),
                provider: entry.model.provider.clone(),
                model: Some(entry.model.id.clone()),
                api: Some(entry.model.api.clone()),
                status: "failed".to_string(),
                skip_reason: None,
                elapsed_ms: 0,
                event_count: 0,
                text_chars: 0,
                thinking_chars: 0,
                tool_calls: 0,
                stop_reason: None,
                usage_total_tokens: 0,
                sequence: Vec::new(),
                sequence_valid: false,
                sequence_error: Some("provider construction failed".to_string()),
                http_status,
                error_kind: Some(classify_error_kind(http_status, Some(&error_text))),
                error: Some(error_text),
            };
        }
    };

    let mut context = build_live_context(prompt);
    context.tools = tools.into();

    let mut options = build_live_stream_options(&entry, api_key);
    options.max_tokens = Some(max_tokens);

    let started = Instant::now();
    match collect_stream_events(provider, context, options).await {
        Ok(events) => {
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let sequence: Vec<String> = events
                .iter()
                .map(|event| event_kind(event).to_string())
                .collect();
            let sequence_check = check_event_sequence(&events);
            let stats = summarize_events(&events);
            let is_failed = stats.error_message.is_some()
                || matches!(events.last(), Some(StreamEvent::Error { .. }));
            let http_status = stats.error_message.as_deref().and_then(parse_http_status);

            ParityRecord {
                check: check.to_string(),
                provider: entry.model.provider.clone(),
                model: Some(entry.model.id.clone()),
                api: Some(entry.model.api.clone()),
                status: if is_failed {
                    "failed".to_string()
                } else {
                    "passed".to_string()
                },
                skip_reason: None,
                elapsed_ms,
                event_count: stats.event_count,
                text_chars: stats.text_chars,
                thinking_chars: stats.thinking_chars,
                tool_calls: stats.tool_calls,
                stop_reason: stats.stop_reason,
                usage_total_tokens: stats.usage_total_tokens,
                sequence,
                sequence_valid: sequence_check.is_ok(),
                sequence_error: sequence_check.err(),
                http_status,
                error_kind: stats
                    .error_message
                    .as_deref()
                    .map(|message| classify_error_kind(http_status, Some(message))),
                error: stats.error_message,
            }
        }
        Err(err) => {
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let http_status = parse_http_status(&err);
            ParityRecord {
                check: check.to_string(),
                provider: entry.model.provider.clone(),
                model: Some(entry.model.id.clone()),
                api: Some(entry.model.api.clone()),
                status: "failed".to_string(),
                skip_reason: None,
                elapsed_ms,
                event_count: 0,
                text_chars: 0,
                thinking_chars: 0,
                tool_calls: 0,
                stop_reason: None,
                usage_total_tokens: 0,
                sequence: Vec::new(),
                sequence_valid: false,
                sequence_error: Some("stream setup failed".to_string()),
                http_status,
                error_kind: Some(classify_error_kind(http_status, Some(&err))),
                error: Some(err),
            }
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_cross_provider_parity() {
    let harness = TestHarness::new("e2e_cross_provider_parity");

    if !ci_e2e_tests_enabled() {
        harness.log().warn(
            "cross_provider_parity",
            "Skipping live cross-provider parity (set CI_E2E_TESTS=1 to enable)",
        );
        return;
    }

    let registry = LiveE2eRegistry::load(harness.log())
        .unwrap_or_else(|err| panic!("failed to load live E2E registry: {err}"));

    asupersync::test_utils::run_test(|| {
        let harness_ref = &harness;
        let registry = registry.clone();
        async move {
            let mut records = Vec::new();

            for target in PARITY_TARGETS {
                let record = run_parity_case(
                    &registry,
                    &target,
                    "same_prompt",
                    target.prompt,
                    Vec::new(),
                    None,
                    64,
                )
                .await;
                records.push(record);
            }

            for provider in TOOL_SCHEMA_TARGETS {
                let Some(target) = target_by_provider(provider) else {
                    continue;
                };
                let record = run_parity_case(
                    &registry,
                    &target,
                    "tool_schema",
                    "Use the sum_numbers tool with a=2 and b=3. Do not explain.",
                    shared_tool_schema(),
                    None,
                    128,
                )
                .await;
                records.push(record);
            }

            for target in PARITY_TARGETS {
                let record = run_parity_case(
                    &registry,
                    &target,
                    "error_handling_parity",
                    LIVE_SHORT_PROMPT,
                    Vec::new(),
                    Some(INVALID_KEY_SENTINEL),
                    32,
                )
                .await;
                records.push(record);
            }

            let same_prompt_active: Vec<&ParityRecord> = records
                .iter()
                .filter(|record| record.check == "same_prompt" && record.status != "skipped")
                .collect();
            assert!(
                !same_prompt_active.is_empty(),
                "no providers available for same_prompt parity check"
            );
            for record in &same_prompt_active {
                assert_eq!(
                    record.status, "passed",
                    "same_prompt failed for {}: {:?}",
                    record.provider, record.error
                );
                assert!(
                    record.text_chars > 0,
                    "same_prompt yielded empty text for {}",
                    record.provider
                );
                assert!(
                    record.sequence_valid,
                    "invalid sequence for {}: {:?}",
                    record.provider, record.sequence_error
                );
            }

            let tool_schema_active: Vec<&ParityRecord> = records
                .iter()
                .filter(|record| record.check == "tool_schema" && record.status != "skipped")
                .collect();
            assert!(
                !tool_schema_active.is_empty(),
                "no providers available for tool schema parity check"
            );
            for record in &tool_schema_active {
                assert_eq!(
                    record.status, "passed",
                    "tool_schema failed for {}: {:?}",
                    record.provider, record.error
                );
                assert!(
                    record.sequence_valid,
                    "invalid tool-schema sequence for {}: {:?}",
                    record.provider, record.sequence_error
                );
            }

            for record in &same_prompt_active {
                assert!(
                    matches!(record.sequence.first().map(String::as_str), Some("Start")),
                    "streaming parity violation for {}: missing Start",
                    record.provider
                );
                assert!(
                    matches!(record.sequence.last().map(String::as_str), Some("Done")),
                    "streaming parity violation for {}: expected terminal Done",
                    record.provider
                );
                assert!(
                    record
                        .sequence
                        .iter()
                        .any(|kind| kind == "TextDelta" || kind == "TextEnd"),
                    "streaming parity violation for {}: no text events",
                    record.provider
                );
            }

            let mut streaming_records: Vec<ParityRecord> = same_prompt_active
                .iter()
                .map(|record| ParityRecord {
                    check: "streaming_event_parity".to_string(),
                    ..(*record).clone()
                })
                .collect();
            records.append(&mut streaming_records);

            let error_active: Vec<&ParityRecord> = records
                .iter()
                .filter(|record| {
                    record.check == "error_handling_parity" && record.status != "skipped"
                })
                .collect();
            assert!(
                !error_active.is_empty(),
                "no providers available for error parity check"
            );

            let mut observed_kinds = Vec::new();
            for record in &error_active {
                assert_eq!(
                    record.status, "failed",
                    "error parity expected failure for {}, got status={} error={:?}",
                    record.provider, record.status, record.error
                );

                let kind = record
                    .error_kind
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                observed_kinds.push((record.provider.clone(), kind));
            }

            let first_kind = &observed_kinds[0].1;
            for (provider, kind) in &observed_kinds {
                assert_eq!(
                    kind, first_kind,
                    "error parity mismatch for {provider}: expected {first_kind}, got {kind}"
                );
            }

            let jsonl_path = harness_ref.temp_path("e2e_cross_provider_parity.jsonl");
            write_jsonl(&jsonl_path, &records)
                .unwrap_or_else(|err| panic!("write parity jsonl artifact: {err}"));
            harness_ref.record_artifact("e2e_cross_provider_parity.jsonl", &jsonl_path);

            let markdown_path = harness_ref.temp_path("e2e_cross_provider_parity.md");
            write_markdown_report(&markdown_path, &records)
                .unwrap_or_else(|err| panic!("write parity markdown artifact: {err}"));
            harness_ref.record_artifact("e2e_cross_provider_parity.md", &markdown_path);
        }
    });
}
