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
//! Until we intentionally stabilize a library API, external consumers should treat
//! all modules/types as **unstable** and subject to change.
//!
//! Currently intended stable exports:
//! - [`Error`]
//! - [`PiResult`]

#![forbid(unsafe_code)]
#![allow(dead_code, clippy::unused_async, unused_attributes)]
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

pub mod agent;
pub mod agent_cx;
pub mod app;
pub mod auth;
pub mod autocomplete;
pub mod cli;
pub mod compaction;
pub mod config;
pub mod connectors;
pub mod error;
pub mod error_hints;
pub mod extensions;
pub mod extensions_js;
pub mod http;
pub mod interactive;
pub mod keybindings;
pub mod model;
pub mod model_selector;
pub mod models;
pub mod package_manager;
pub mod provider;
pub mod providers;
pub mod resources;
pub mod rpc;
pub mod scheduler;
pub mod session;
pub mod session_index;
pub mod session_picker;
pub mod sse;
pub mod theme;
pub mod tools;
pub mod tui;
pub mod vcr;

pub use error::{Error, Result as PiResult};
