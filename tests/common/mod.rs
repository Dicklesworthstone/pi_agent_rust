//! Common test infrastructure for `pi_agent_rust`.
//!
//! This module provides shared utilities for integration and E2E tests:
//! - Verbose logging infrastructure with auto-dump on test failure
//! - Test harness for consistent setup/teardown
//! - Timing utilities for performance analysis

use std::future::Future;

pub mod harness;
pub mod logging;

pub use harness::TestHarness;

/// Runs an async future to completion on an asupersync runtime.
///
/// Note: We spawn the future onto the runtime so it runs with a proper task context.
#[allow(dead_code)]
pub fn run_async<T, Fut>(future: Fut) -> T
where
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .blocking_threads(1, 8)
        .build()
        .expect("build asupersync runtime");

    let join = runtime.handle().spawn(future);
    runtime.block_on(join)
}
