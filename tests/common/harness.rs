//! Test harness for consistent setup/teardown and auto-logging.
//!
//! The `TestHarness` provides:
//! - A temporary directory for test files
//! - A test logger for detailed tracing
//! - Automatic log dump on test failure (panic)
//! - Timing information for performance analysis
//!
//! # Example
//!
//! ```ignore
//! #[test]
//! fn test_something() {
//!     let harness = TestHarness::new("test_something");
//!
//!     harness.log().info("setup", "Creating test environment");
//!     let test_file = harness.temp_path("data.txt");
//!     std::fs::write(&test_file, "test content").unwrap();
//!
//!     harness.log().info_ctx("action", "Processing file", |ctx| {
//!         ctx.push(("path".into(), test_file.display().to_string()));
//!     });
//!
//!     // On test failure, detailed logs are automatically printed
//!     assert!(std::fs::read_to_string(&test_file).unwrap().contains("test"));
//! }
//! ```

#![allow(dead_code)]

use super::logging::{LogLevel, TestLogger};
use std::env;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Test harness providing temp directories, logging, and cleanup.
pub struct TestHarness {
    /// Test name for identification in logs.
    name: String,
    /// Temporary directory for test files.
    temp_dir: TempDir,
    /// Test logger for detailed tracing.
    logger: TestLogger,
    /// Whether to use colored output.
    use_colors: bool,
}

#[allow(dead_code)]
impl TestHarness {
    /// Create a new test harness with the given test name.
    ///
    /// The test name is used to identify the test in log output.
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let logger = TestLogger::new();

        logger.info("harness", format!("Test '{name}' started"));
        logger.info_ctx("harness", "Temp directory created", |ctx| {
            ctx.push(("path".into(), temp_dir.path().display().to_string()));
        });

        Self {
            name,
            temp_dir,
            logger,
            use_colors: true,
        }
    }

    /// Create a harness without colored output.
    pub fn new_plain(name: impl Into<String>) -> Self {
        let mut harness = Self::new(name);
        harness.use_colors = false;
        harness
    }

    /// Get a reference to the test logger.
    pub const fn log(&self) -> &TestLogger {
        &self.logger
    }

    /// Get the path to the temporary directory.
    pub fn temp_dir(&self) -> &Path {
        self.temp_dir.path()
    }

    /// Get a path within the temporary directory.
    ///
    /// This is a convenience method that joins the given path to the temp directory.
    pub fn temp_path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.temp_dir.path().join(path)
    }

    /// Create a file in the temp directory with the given content.
    ///
    /// Returns the full path to the created file.
    pub fn create_file(&self, name: impl AsRef<Path>, content: impl AsRef<[u8]>) -> PathBuf {
        let path = self.temp_path(name);

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("Failed to create parent directories");
        }

        let content_ref = content.as_ref();
        std::fs::write(&path, content_ref).expect("Failed to create test file");

        self.logger.info_ctx("harness", "Created test file", |ctx| {
            ctx.push(("path".into(), path.display().to_string()));
            ctx.push(("size".into(), format!("{} bytes", content_ref.len())));
        });

        path
    }

    /// Create a directory in the temp directory.
    ///
    /// Returns the full path to the created directory.
    pub fn create_dir(&self, name: impl AsRef<Path>) -> PathBuf {
        let path = self.temp_path(name);
        std::fs::create_dir_all(&path).expect("Failed to create test directory");

        self.logger
            .info_ctx("harness", "Created test directory", |ctx| {
                ctx.push(("path".into(), path.display().to_string()));
            });

        path
    }

    /// Read a file from the temp directory.
    pub fn read_file(&self, name: impl AsRef<Path>) -> String {
        let path = self.temp_path(name);
        let content = std::fs::read_to_string(&path).expect("Failed to read test file");

        self.logger.debug_ctx("harness", "Read test file", |ctx| {
            ctx.push(("path".into(), path.display().to_string()));
            ctx.push(("size".into(), format!("{} bytes", content.len())));
        });

        content
    }

    /// Check if a file exists in the temp directory.
    pub fn file_exists(&self, name: impl AsRef<Path>) -> bool {
        self.temp_path(name).exists()
    }

    /// Log a test section start (useful for organizing multi-phase tests).
    pub fn section(&self, name: &str) {
        self.logger.info("section", format!("=== {name} ==="));
    }

    /// Log an assertion about to happen (useful for debugging which assertion failed).
    pub fn assert_log(&self, description: &str) {
        self.logger.debug("assert", description);
    }

    /// Get the test name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get elapsed time since harness creation.
    pub fn elapsed(&self) -> std::time::Duration {
        self.logger.elapsed()
    }

    /// Manually dump logs (useful for debugging passing tests).
    pub fn dump_logs(&self) {
        let header = format!("\n=== TEST LOGS: {} ===\n", self.name);
        if self.use_colors {
            eprint!("\x1b[1;36m{header}\x1b[0m");
            eprint!("{}", self.logger.dump_colored());
        } else {
            eprint!("{header}");
            eprint!("{}", self.logger.dump());
        }
        if self.logger.has_artifacts() {
            eprintln!("=== ARTIFACTS ===");
            eprint!("{}", self.logger.dump_artifacts());
            eprintln!("=== END ARTIFACTS ===");
        }
        eprintln!("=== END LOGS ===\n");
    }

    /// Record an artifact for this test.
    pub fn record_artifact(&self, name: impl Into<String>, path: impl AsRef<Path>) {
        self.logger.record_artifact(name, path);
    }
}

impl TestLogger {
    /// Log a debug entry with context.
    pub fn debug_ctx<F>(&self, category: &str, message: impl Into<String>, f: F)
    where
        F: FnOnce(&mut Vec<(String, String)>),
    {
        self.with_context(LogLevel::Debug, category, message, f);
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // Log completion
        self.logger.info_ctx("harness", "Test completing", |ctx| {
            ctx.push((
                "elapsed".into(),
                format!("{:.3}s", self.elapsed().as_secs_f64()),
            ));
        });

        // Dump logs if we're panicking (test failure)
        if std::thread::panicking() {
            let header = format!("\n=== TEST FAILED: {} ===\n", self.name);
            if self.use_colors {
                eprint!("\x1b[1;31m{header}\x1b[0m");
                eprint!("{}", self.logger.dump_colored());
            } else {
                eprint!("{header}");
                eprint!("{}", self.logger.dump());
            }
            if self.logger.has_artifacts() {
                eprintln!("=== ARTIFACTS ===");
                eprint!("{}", self.logger.dump_artifacts());
                eprintln!("=== END ARTIFACTS ===");
            }
            eprintln!("=== END LOGS ===\n");

            if let Ok(path) = env::var("TEST_LOG_PATH") {
                if let Err(err) = self.logger.write_dump_to_path(&path) {
                    eprintln!("Failed to write test log to {path}: {err}");
                }
            }
        }
    }
}

/// Builder for configuring test harnesses.
pub struct TestHarnessBuilder {
    name: String,
    use_colors: bool,
    min_log_level: LogLevel,
}

impl TestHarnessBuilder {
    /// Create a new builder with the given test name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            use_colors: true,
            min_log_level: LogLevel::Debug,
        }
    }

    /// Disable colored output.
    pub const fn no_colors(mut self) -> Self {
        self.use_colors = false;
        self
    }

    /// Set minimum log level.
    pub const fn min_level(mut self, level: LogLevel) -> Self {
        self.min_log_level = level;
        self
    }

    /// Build the test harness.
    pub fn build(self) -> TestHarness {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let logger = TestLogger::with_min_level(self.min_log_level);
        let name = self.name;

        logger.info("harness", format!("Test '{name}' started"));
        logger.info_ctx("harness", "Temp directory created", |ctx| {
            ctx.push(("path".into(), temp_dir.path().display().to_string()));
        });

        TestHarness {
            name,
            temp_dir,
            logger,
            use_colors: self.use_colors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_harness_basic() {
        let harness = TestHarness::new("basic_test");

        // Verify temp dir exists
        assert!(harness.temp_dir().exists());

        // Create and verify file
        let path = harness.create_file("test.txt", "hello world");
        assert!(path.exists());
        assert_eq!(harness.read_file("test.txt"), "hello world");
    }

    #[test]
    fn test_harness_nested_files() {
        let harness = TestHarness::new("nested_test");

        // Create nested file
        let path = harness.create_file("subdir/deep/test.txt", "nested content");
        assert!(path.exists());
        assert_eq!(harness.read_file("subdir/deep/test.txt"), "nested content");
    }

    #[test]
    fn test_harness_logging() {
        let harness = TestHarness::new("logging_test");

        harness.log().info("test", "Custom log message");
        harness.section("Phase 1");
        harness.assert_log("Checking something");

        assert!(harness.log().entry_count() > 0);
    }

    #[test]
    fn test_builder() {
        let harness = TestHarnessBuilder::new("builder_test")
            .no_colors()
            .min_level(LogLevel::Info)
            .build();

        harness.log().debug("test", "Should be filtered");
        harness.log().info("test", "Should appear");

        // Debug should be filtered out
        let entries = harness.log().entries();
        let debug_count = entries
            .iter()
            .filter(|e| e.level == LogLevel::Debug)
            .count();
        assert_eq!(debug_count, 0);
    }
}
