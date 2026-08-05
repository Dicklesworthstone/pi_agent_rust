//! Pi - Native AI coding agent CLI
//!
//! This library provides the core functionality for the Pi CLI tool,
//! a Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance-oriented native architecture with instrumented startup and TUI paths
//! - Reliability through explicit errors, bounded cancellation, and conformance tests
//! - Distribution through one supported end-user binary in official release archives
//!
//! ## Public API policy
//!
//! The `pi` crate is primarily the implementation crate for the `pi` CLI binary.
//! External consumers should treat non-`sdk` modules/types as **unstable**
//! and subject to change. Use [`sdk`] as the stable library-facing surface.
//!
//! Currently intended stable exports:
//! - [`Error`]
//! - [`PiResult`]
//! - [`sdk`] module

#![forbid(unsafe_code)]
// rch clippy probes without these allowances still expose broad, cross-module
// dormant surfaces in extension/session/SDK paths. The no-allow inventory is
// tracked in bd-63x3v.5.1; keep this crate-wide guard until the remaining
// subsystems are narrowed in their own patches.
// `unused_async_trait_impl` is the nightly-2026-07-05 successor of
// `unused_async` for async-trait impl fns (new lint name, so the existing
// allow does not cover it); same rationale as above.
#![allow(dead_code, clippy::unused_async, clippy::unused_async_trait_impl)]
#![cfg_attr(
    test,
    allow(
        unused_variables,
        clippy::assertions_on_constants,
        clippy::match_same_arms,
        clippy::uninlined_format_args,
        clippy::missing_const_for_fn,
        clippy::collapsible_if
    )
)]
// Allow pedantic lints during early development - can tighten later
#![allow(
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::similar_names,
    clippy::wildcard_imports
)]

// Allow in-crate tests that include integration test helpers to resolve `pi::...`
// paths the same way integration tests do.
extern crate self as pi;

/// Serialize unit tests that temporarily change the process-wide current
/// directory. Rust's test runner executes modules concurrently, so separate
/// per-module locks do not prevent one module from observing another module's
/// temporary directory.
#[cfg(test)]
pub(crate) fn test_current_dir_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// Gap H: jemalloc allocator for allocation-heavy paths.
// Declared in the library so all project binaries/tests share allocator behavior.
// BSD-family targets stay on their platform allocator to avoid allocator-domain
// mismatch across libc/pthread and C dependencies such as QuickJS.
#[cfg(all(feature = "jemalloc", any(target_os = "linux", target_os = "macos")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[doc(hidden)]
pub mod acp;
#[doc(hidden)]
pub mod agent;
#[doc(hidden)]
pub mod agent_cx;
#[doc(hidden)]
pub mod app;
#[doc(hidden)]
pub mod auth;
#[doc(hidden)]
pub mod autocomplete;
#[doc(hidden)]
pub mod buffer_shim;
#[doc(hidden)]
pub mod cli;
#[doc(hidden)]
pub mod compaction;
#[doc(hidden)]
pub mod compaction_worker;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod conformance;
#[doc(hidden)]
pub mod conformance_shapes;
#[doc(hidden)]
pub mod connectors;
#[doc(hidden)]
pub mod crypto_shim;
#[doc(hidden)]
pub mod doctor;
#[doc(hidden)]
pub(crate) mod embedded_assets;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod error_hints;
#[doc(hidden)]
pub mod extension_conformance_matrix;
#[doc(hidden)]
pub mod extension_dispatcher;
#[doc(hidden)]
pub mod extension_events;
#[doc(hidden)]
pub mod extension_inclusion;
#[doc(hidden)]
pub mod extension_index;
#[doc(hidden)]
pub mod extension_license;
#[doc(hidden)]
pub mod extension_popularity;
#[doc(hidden)]
pub mod extension_preflight;
#[doc(hidden)]
pub mod extension_replay;
#[doc(hidden)]
pub mod extension_scoring;
#[doc(hidden)]
pub mod extension_tools;
#[doc(hidden)]
pub mod extension_validation;
#[doc(hidden)]
pub mod extensions;
#[doc(hidden)]
pub mod extensions_js;
#[doc(hidden)]
pub mod file_lock;
#[doc(hidden)]
pub mod flake_classifier;
#[doc(hidden)]
pub mod hostcall_amac;
#[doc(hidden)]
pub mod hostcall_io_uring_lane;
#[doc(hidden)]
pub mod hostcall_queue;
#[doc(hidden)]
pub mod hostcall_rewrite;
#[doc(hidden)]
pub mod hostcall_s3_fifo;
#[doc(hidden)]
pub mod hostcall_superinstructions;
#[doc(hidden)]
pub mod hostcall_trace_jit;
#[doc(hidden)]
pub mod http;
#[doc(hidden)]
pub mod http_shim;
#[cfg(feature = "tui")]
#[doc(hidden)]
pub mod interactive;
#[doc(hidden)]
pub mod keybindings;
#[doc(hidden)]
pub mod migrations;
#[doc(hidden)]
pub mod model;
#[doc(hidden)]
pub mod model_routing;
#[doc(hidden)]
pub mod model_selector;
#[doc(hidden)]
pub mod models;
#[doc(hidden)]
pub mod package_manager;
#[doc(hidden)]
pub mod perf_build;
#[doc(hidden)]
pub mod permissions;
#[cfg(feature = "wasm-host")]
#[doc(hidden)]
pub mod pi_wasm;
#[doc(hidden)]
pub mod platform;
#[doc(hidden)]
pub mod provider;
#[doc(hidden)]
pub mod provider_metadata;
#[doc(hidden)]
pub mod providers;
#[doc(hidden)]
pub mod resource_governor;
#[doc(hidden)]
pub mod resources;
#[doc(hidden)]
pub mod rpc;
#[doc(hidden)]
pub mod scheduler;
pub mod sdk;
#[doc(hidden)]
pub mod semantic_workspace_graph;
#[doc(hidden)]
pub mod session;
#[doc(hidden)]
pub mod session_index;
#[doc(hidden)]
pub mod session_metrics;
#[cfg(feature = "tui")]
#[doc(hidden)]
pub mod session_picker;
#[cfg(feature = "sqlite-sessions")]
#[doc(hidden)]
pub mod session_sqlite;
#[doc(hidden)]
pub mod session_store_v2;
#[doc(hidden)]
pub mod sse;
#[doc(hidden)]
pub mod subagents;
#[doc(hidden)]
pub mod swarm_activity_ledger;
#[doc(hidden)]
pub mod swarm_flight_recorder;
#[doc(hidden)]
pub mod swarm_progress_slo;
#[doc(hidden)]
pub mod swarm_replay;
#[doc(hidden)]
pub mod terminal_images;
#[doc(hidden)]
pub mod theme;
#[doc(hidden)]
pub mod tools;
#[doc(hidden)]
pub mod tui;
#[doc(hidden)]
pub mod validation_broker;
#[doc(hidden)]
pub mod vcr;
#[doc(hidden)]
pub mod version_check;

pub use error::{Error, Result as PiResult};
#[doc(hidden)]
pub use extension_dispatcher::ExtensionDispatcher;

// Conditional re-exports for fuzz harnesses.
// These expose internal parsing functions that are normally private,
// gated behind the `fuzzing` feature so they do not appear in the
// public API during normal builds.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzz_exports {
    //! Re-exports of internal parsing/deserialization functions for
    //! `cargo-fuzz` / `libFuzzer` harnesses.
    //!
    //! Enabled only when the `fuzzing` Cargo feature is active.
    //! The `fuzz/Cargo.toml` depends on this crate with
    //! `features = ["fuzzing"]`.

    pub use crate::config::Config;
    pub use crate::model::{
        AssistantMessage, ContentBlock, Message, StreamEvent, TextContent, ThinkingContent,
        ToolCall, ToolResultMessage, Usage, UserContent, UserMessage,
    };
    pub use crate::session::{Session, SessionEntry, SessionHeader, SessionMessage};
    pub use crate::sse::{SseEvent, SseParser};
    pub use crate::tools::{fuzz_normalize_dot_segments, fuzz_resolve_path};

    // Provider stream processor wrappers for coverage-guided fuzzing.
    pub use crate::providers::anthropic::fuzz::Processor as AnthropicProcessor;
    pub use crate::providers::azure::fuzz::Processor as AzureProcessor;
    pub use crate::providers::cohere::fuzz::Processor as CohereProcessor;
    pub use crate::providers::gemini::fuzz::Processor as GeminiProcessor;
    pub use crate::providers::openai::fuzz::Processor as OpenAIProcessor;
    pub use crate::providers::openai_responses::fuzz::Processor as OpenAIResponsesProcessor;
    pub use crate::providers::vertex::fuzz::Processor as VertexProcessor;
}
