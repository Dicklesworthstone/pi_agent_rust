//! Pi - High-performance AI coding agent CLI
//!
//! This library provides the core functionality for the Pi CLI tool,
//! a Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance: Sub-100ms startup, smooth TUI at 60fps
//! - Reliability: No panics in normal operation
//! - Efficiency: Single binary, minimal dependencies
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

#![deny(unsafe_code)]
#![allow(dead_code, clippy::unused_async, unused_attributes)]
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

// Gap H: jemalloc allocator for allocation-heavy paths.
// Declared in the library so all project binaries/tests share allocator behavior.
// BSD-family targets stay on their platform allocator to avoid allocator-domain
// mismatch across libc/pthread and C dependencies such as QuickJS.
#[cfg(all(feature = "jemalloc", any(target_os = "linux", target_os = "macos")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

pub mod acp;
pub mod agent;
pub mod agent_cx;
pub mod app;
pub mod auth;
pub mod autocomplete;
pub mod buffer_shim;
pub mod cli;
pub mod compaction;
pub mod compaction_worker;
pub mod config;
pub mod conformance;
pub mod conformance_shapes;
pub mod connectors;
pub mod crypto_shim;
pub mod doctor;
pub mod error;
pub mod error_hints;
pub mod extension_conformance_matrix;
pub mod extension_dispatcher;
pub mod extension_events;
pub mod extension_inclusion;
pub mod extension_index;
pub mod extension_license;
pub mod extension_popularity;
pub mod extension_preflight;
pub mod extension_replay;
pub mod extension_scoring;
pub mod extension_tools;
pub mod extension_validation;
pub mod extensions;
pub mod extensions_js;
pub mod flake_classifier;
pub mod hostcall_amac;
pub mod hostcall_io_uring_lane;
pub mod hostcall_queue;
pub mod hostcall_rewrite;
pub mod hostcall_s3_fifo;
pub mod hostcall_superinstructions;
pub mod hostcall_trace_jit;
pub mod http;
pub mod http_shim;
pub mod interactive;
pub mod keybindings;
pub mod migrations;
pub mod model;
pub mod model_selector;
pub mod models;
pub mod package_manager;
pub mod perf_build;
pub mod permissions;
#[cfg(feature = "wasm-host")]
pub mod pi_wasm;
pub mod platform;
pub mod provider;
pub mod provider_metadata;
pub mod providers;
pub mod resources;
pub mod rpc;
pub mod scheduler;
pub mod sdk;
pub mod session;
pub mod session_index;
pub mod session_metrics;
pub mod session_picker;
#[cfg(feature = "sqlite-sessions")]
pub mod session_sqlite;
pub mod session_store_v2;
pub mod sse;
pub mod terminal_images;
pub mod theme;
pub mod tools;
pub mod tui;
pub mod vcr;
pub mod version_check;

pub use error::{Error, Result as PiResult};
pub use extension_dispatcher::ExtensionDispatcher;

// Conditional re-exports for fuzz harnesses.
// These expose internal parsing functions that are normally private,
// gated behind the `fuzzing` feature so they do not appear in the
// public API during normal builds.
#[cfg(feature = "fuzzing")]
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
