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
#[cfg(unix)]
pub mod tmux;

#[allow(unused_imports)]
pub use harness::TestHarness;
#[allow(unused_imports)]
pub use harness::{MockHttpResponse, MockHttpServer, TestEnv};

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
            .blocking_threads(1, 8)
            .build()
            .expect("build asupersync runtime")
    });

    let join = runtime.handle().spawn(future);
    // Await the JoinHandle on a minimal executor; the task itself runs on `runtime`.
    futures::executor::block_on(join)
}
