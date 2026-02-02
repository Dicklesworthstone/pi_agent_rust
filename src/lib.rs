//! Pi - High-performance AI coding agent CLI
//!
//! This library provides the core functionality for the Pi CLI tool,
//! a Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance: Sub-100ms startup, smooth TUI at 60fps
//! - Reliability: No panics in normal operation
//! - Efficiency: Single binary, minimal dependencies

#![forbid(unsafe_code)]
#![allow(dead_code, clippy::unused_async)]
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
pub mod cli;
pub mod config;
pub mod error;
pub mod model;
pub mod provider;
pub mod providers;
pub mod session;
pub mod sse;
pub mod tools;
pub mod tui;

pub use error::{Error, Result as PiResult};
