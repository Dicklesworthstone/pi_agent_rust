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
pub mod tmux;

#[allow(unused_imports)]
pub use harness::TestHarness;
#[allow(unused_imports)]
pub use harness::{
    LIVE_E2E_GATE_ENV, LIVE_E2E_TIMEOUT, LIVE_SHORT_PROMPT, LiveE2eRegistry, LiveHttpTrace,
    LiveProviderRun, LiveProviderTarget, LiveStreamSummary, build_live_context,
    build_live_stream_options, ci_e2e_tests_enabled, create_anthropic_provider,
    create_deepseek_provider, create_gemini_provider, create_live_provider, create_openai_provider,
    create_openrouter_provider, create_xai_provider, load_vcr_trace, parse_http_status,
    run_live_provider_target, write_live_provider_runs_jsonl,
};
#[allow(unused_imports)]
pub use harness::{MockHttpResponse, MockHttpServer, TestEnv};
#[allow(unused_imports)]
pub use logging::{
    CostBudgetOutcome, CostThreshold, JsonlValidationError, check_cost_budget,
    default_cost_thresholds, find_unredacted_keys, redact_json_value, validate_jsonl,
    validate_jsonl_line,
};

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
