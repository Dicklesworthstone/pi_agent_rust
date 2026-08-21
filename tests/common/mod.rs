//! Common test infrastructure for `pi_agent_rust`.
//!
//! This module provides shared utilities for integration and E2E tests:
//! - Verbose logging infrastructure with auto-dump on test failure
//! - Test harness for consistent setup/teardown
//! - Timing utilities for performance analysis

use std::future::Future;
use std::sync::OnceLock;

pub mod harness;
pub mod logging;
#[allow(
    dead_code,
    clippy::needless_pass_by_value,
    clippy::significant_drop_tightening,
    clippy::uninlined_format_args,
    clippy::missing_const_for_fn
)]
pub mod mocks;
#[cfg(unix)]
#[allow(dead_code)]
pub mod scenario_runner;
#[cfg(unix)]
pub mod tmux;
#[allow(dead_code)]
pub mod transcript_diff;

#[allow(unused_imports)]
pub use harness::TestHarness;
#[allow(unused_imports)]
pub use harness::{
    LIVE_E2E_EXECUTION_MODE, LIVE_E2E_GATE_ENV, LIVE_E2E_MAX_ATTEMPTS, LIVE_E2E_REPLAY_BOUNDARY,
    LIVE_E2E_RETRY_BACKOFF_MS, LIVE_E2E_RETRYABLE_HTTP_STATUS, LIVE_E2E_TIMEOUT,
    LIVE_E2E_TRACE_ORIGIN, LIVE_SHORT_PROMPT, LiveE2eRegistry, LiveHttpTrace, LiveProviderRun,
    LiveProviderTarget, LiveStreamSummary, build_live_context, build_live_stream_options,
    ci_e2e_tests_enabled, create_anthropic_provider, create_deepseek_provider,
    create_gemini_provider, create_live_provider, create_openai_provider,
    create_openrouter_provider, create_xai_provider, load_vcr_trace, parse_http_status,
    run_live_provider_target, write_live_provider_runs_jsonl,
};
#[allow(unused_imports)]
pub use harness::{MockHttpResponse, MockHttpServer, TestEnv};
#[allow(unused_imports)]
pub use logging::{
    CostBudgetOutcome, CostThreshold, EVIDENCE_CONTRACT_SCHEMA_V1, FAILURE_DIGEST_SCHEMA_V1,
    JsonlValidationError, PARITY_TEST_LOGGING_CONTRACT_SCHEMA_V1, TEST_ARTIFACT_SCHEMA_V1,
    TEST_LOG_SCHEMA_V2, check_cost_budget, default_cost_thresholds, find_unredacted_keys,
    redact_json_value, validate_jsonl, validate_jsonl_line,
};

/// Apply the prompt-cache wire shape (commits 1d817ab0/7bc8e6da) to a
/// legacy-shaped Anthropic request body so recorded cassettes match what the
/// provider now sends: `system` becomes a block array and the last user
/// content block carries an ephemeral `cache_control` marker (as does the
/// last tool definition, when tools are present).
#[allow(dead_code)]
pub fn apply_prompt_cache_wire_shape(body: &mut serde_json::Value) {
    fn ephemeral() -> serde_json::Value {
        serde_json::json!({"type": "ephemeral"})
    }
    if let Some(system) = body.get_mut("system")
        && let Some(text) = system.as_str().map(ToString::to_string)
    {
        *system = serde_json::json!([{
            "type": "text",
            "text": text,
            "cache_control": ephemeral(),
        }]);
    }
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut())
        && let Some(last_user) = messages
            .iter_mut()
            .rev()
            .find(|message| message.get("role").and_then(|r| r.as_str()) == Some("user"))
        && let Some(blocks) = last_user.get_mut("content").and_then(|c| c.as_array_mut())
        && let Some(last_block) = blocks.last_mut()
        && let Some(block) = last_block.as_object_mut()
    {
        block.insert("cache_control".to_string(), ephemeral());
    }
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut())
        && let Some(last_tool) = tools.last_mut()
        && let Some(tool) = last_tool.as_object_mut()
    {
        tool.insert("cache_control".to_string(), ephemeral());
    }
}

/// Seed interactive test configs so `PiApp::new()` stays hermetic.
///
/// Startup changelog bootstrapping persists `lastChangelogVersion` on first run. Tests that use
/// `Config::default()` do not want to mutate the real user config, so we mark the current version
/// as already seen unless a test intentionally overrides it.
#[allow(dead_code)]
pub fn hermetic_interactive_config(mut config: pi::config::Config) -> pi::config::Config {
    if config.last_changelog_version.is_none() {
        config.last_changelog_version = Some(pi::platform::VERSION.to_string());
    }
    config
}

/// Runs an async future to completion on an asupersync runtime.
///
/// Note: We spawn the future onto the runtime so it runs with a proper task context.
#[allow(dead_code)]
pub fn run_async<T, Fut>(future: Fut) -> T
where
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    // Reuse a single runtime across tests. Spinning up a fresh runtime per call is
    // extremely expensive and can distort perf/stress test measurements.
    static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
    let runtime = RT.get_or_init(|| {
        asupersync::runtime::RuntimeBuilder::new()
            // Work around an asupersync 0.1.0 scheduler parking bug where due timers can
            // livelock the idle backoff loop (prevents `sleep()` wakeups).
            .enable_parking(false)
            .worker_threads(1)
            .blocking_threads(1, 8)
            .build()
            .expect("build asupersync runtime")
    });

    let join = runtime.handle().spawn(future);
    // Await the JoinHandle on a minimal executor; the task itself runs on `runtime`.
    futures::executor::block_on(join)
}
