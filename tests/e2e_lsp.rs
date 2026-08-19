//! E2E: cross-file symbol rename via the `lsp` tool against a real
//! rust-analyzer, proven by `cargo check` on the rewritten crate
//! (bd-cv653.1.1).
//!
//! Flow: build a dependency-free fixture crate in a temp dir, rename a
//! function from a call site with the tool, verify both files changed with
//! zero dangling references (grep proof), then run `cargo check --offline`
//! on the rewritten crate to prove it still compiles. A second independent
//! registry re-executes the same definition call to prove the tool is
//! surface-agnostic (interactive/print/RPC/ACP hosts all build their
//! registries through the same `ToolRegistry::new` path).
//!
//! Skips honestly when rust-analyzer is not installed. No mocks, no network:
//! the fixture crate has zero dependencies and cargo runs with `--offline`.
//! Structured JSONL logs per the repo harness pattern
//! (tests/common/logging.rs).

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::model::ContentBlock;
use pi::tools::{ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

const CASE: &str = "e2e_lsp_rename_compile_proof";

fn first_text(output: &ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

fn output_json(output: &ToolOutput) -> Value {
    serde_json::from_str(first_text(output)).expect("tool output must be a JSON payload")
}

/// Probe one candidate binary; returns true when `--version` succeeds.
fn probe_rust_analyzer(command: &str) -> bool {
    Command::new(command) // ubs:ignore test probe of fixed candidates (PATH name or ~/.cargo/bin path)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Discover a working rust-analyzer command. Order: `PI_LSP_RUST_ANALYZER`
/// override, `rust-analyzer` on PATH, `$HOME/.cargo/bin/rust-analyzer` (the
/// rustup default — job environments with a minimal PATH still find it).
fn rust_analyzer_command() -> Option<String> {
    if let Ok(override_cmd) = std::env::var("PI_LSP_RUST_ANALYZER") {
        return probe_rust_analyzer(&override_cmd).then_some(override_cmd);
    }
    if probe_rust_analyzer("rust-analyzer") {
        return Some("rust-analyzer".to_string());
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = std::path::PathBuf::from(home).join(".cargo/bin/rust-analyzer");
        let candidate_str = candidate.to_string_lossy().into_owned();
        if probe_rust_analyzer(&candidate_str) {
            return Some(candidate_str);
        }
    }
    None
}

fn rust_analyzer_available() -> bool {
    rust_analyzer_command().is_some()
}

/// Whether the live lane must run (skip becomes a loud failure). Set
/// `PI_LSP_TEST_REQUIRE_RA=1` in environments that guarantee the binary.
fn rust_analyzer_required() -> bool {
    std::env::var("PI_LSP_TEST_REQUIRE_RA")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Config with the discovered rust-analyzer command injected (spawn must
/// use the same binary the probe found, even on minimal-PATH job runners).
fn ra_config(command: &str) -> pi::config::Config {
    let mut servers = std::collections::HashMap::new();
    servers.insert(
        "rust-analyzer".to_string(),
        pi::config::LspServerSettings {
            command: Some(command.to_string()),
            ..Default::default()
        },
    );
    pi::config::Config {
        lsp: Some(pi::config::LspSettings {
            servers: Some(servers),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Drive a borrowed tool future to completion on a fresh current-thread
/// runtime (no `'static` bound, unlike the shared runtime helper).
fn block_on_local<Fut: Future>(future: Fut) -> Fut::Output {
    // enable_parking(false): works around the asupersync scheduler parking
    // bug that can livelock sleep() wakeups (see tests/common/mod.rs).
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(1)
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

fn execute_lsp(registry: &ToolRegistry, input: Value) -> Result<ToolOutput, pi::error::Error> {
    let tool = registry
        .tools()
        .iter()
        .find(|tool| tool.name() == "lsp")
        .expect("lsp tool registered");
    block_on_local(tool.execute("e2e-call", input, None))
}

/// Stage the dependency-free fixture crate under `root`.
fn stage_fixture_crate(root: &Path) {
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"lsp_e2e_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::create_dir_all(root.join("src")).expect("src dir");
    std::fs::write(root.join("src/lib.rs"), "pub mod engine;\npub mod cli;\n").expect("lib.rs");
    std::fs::write(
        root.join("src/engine.rs"),
        "//! Engine internals.\n\n/// Compute a thrust value.\npub fn thrust_level(throttle: u64) -> u64 {\n    throttle * 9 / 10\n}\n",
    )
    .expect("engine.rs");
    std::fs::write(
        root.join("src/cli.rs"),
        "use crate::engine::thrust_level;\n\npub fn report() -> String {\n    let base = thrust_level(100);\n    let idle = thrust_level(0);\n    format!(\"base={base} idle={idle}\")\n}\n",
    )
    .expect("cli.rs");
}

/// Handle the no-rust-analyzer case: fail loudly when the lane is required,
/// else log the honest skip and record the JSONL artifact. Returns true when
/// the caller must return early.
fn skip_without_rust_analyzer(harness: &TestHarness) -> bool {
    if rust_analyzer_available() {
        return false;
    }
    let path_env = std::env::var("PATH").unwrap_or_else(|_| "<unset>".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "<unset>".to_string());
    assert!(
        !rust_analyzer_required(),
        "PI_LSP_TEST_REQUIRE_RA is set but rust-analyzer is not installed; \
         refusing to let the e2e lane skip its proof. \
         Diagnostics: PATH={path_env} HOME={home}"
    );
    harness.log().info(
        "skip",
        "rust-analyzer not installed; skipping honestly (install: rustup component add rust-analyzer)",
    );
    let path = harness.temp_path(format!("{CASE}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    assert!(validate_jsonl_v2_only(&payload).is_empty());
    harness.record_artifact(format!("{CASE}.jsonl"), &path);
    true
}

#[test]
fn e2e_lsp_rename_compile_proof() {
    let harness = TestHarness::new(CASE);
    harness
        .log()
        .info("setup", "staging dependency-free fixture crate");

    if skip_without_rust_analyzer(&harness) {
        return;
    }

    let root = harness.temp_path(".");
    let config = ra_config(&rust_analyzer_command().expect("rust-analyzer discovered"));
    stage_fixture_crate(&root);
    harness.log().info("setup", "fixture crate staged");

    let registry = ToolRegistry::new(&["lsp"], &root, Some(&config));

    // ── Phase 1: rename thrust_level -> thrust_output from a call site ──
    harness.log().info(
        "action",
        "rename thrust_level -> thrust_output via lsp tool",
    );
    let out = execute_lsp(
        &registry,
        json!({
            "action": "rename",
            "file": "src/cli.rs",
            "line": 4,
            "symbol": "thrust_level",
            "newName": "thrust_output",
            "timeout": 90,
        }),
    )
    .expect("rename executes");
    let payload = output_json(&out);
    harness
        .log()
        .info("verify", format!("rename payload: {payload}"));
    let changed: Vec<&str> = payload["filesChanged"]
        .as_array()
        .expect("filesChanged")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        changed.iter().any(|f| f.ends_with("engine.rs")),
        "rename must touch engine.rs: {changed:?}"
    );
    assert!(
        changed.iter().any(|f| f.ends_with("cli.rs")),
        "rename must touch cli.rs: {changed:?}"
    );

    // ── Phase 2: grep proof — no dangling references ────────────────────
    let engine = std::fs::read_to_string(root.join("src/engine.rs")).expect("engine.rs");
    let cli = std::fs::read_to_string(root.join("src/cli.rs")).expect("cli.rs");
    assert!(
        engine.contains("pub fn thrust_output"),
        "engine.rs: {engine}"
    );
    assert!(cli.contains("thrust_output(100)"), "cli.rs: {cli}");
    for (name, content) in [("engine.rs", &engine), ("cli.rs", &cli)] {
        assert!(
            !content.contains("thrust_level"),
            "dangling reference to thrust_level in {name}: {content}"
        );
    }
    harness
        .log()
        .info("verify", "grep proof: zero dangling references to old name");

    // ── Phase 3: the rewritten crate still compiles ─────────────────────
    let check = Command::new("cargo")
        .args(["check", "--offline"])
        .current_dir(&root)
        .output()
        .expect("cargo check runs");
    let stderr = String::from_utf8_lossy(&check.stderr);
    harness.log().info(
        "verify",
        format!("cargo check status={} stderr={}", check.status, stderr),
    );
    assert!(
        check.status.success(),
        "rewritten crate must compile: {stderr}"
    );

    // ── Phase 4: surface-agnostic parity ────────────────────────────────
    prove_surface_parity(&root, &config, &registry);
    harness
        .log()
        .info("verify", "surface-agnostic parity proven across registries");

    // ── Structured logs ─────────────────────────────────────────────────
    let path = harness.temp_path(format!("{CASE}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let logs = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&logs);
    assert!(errors.is_empty(), "JSONL schema violations: {errors:?}");
    harness.record_artifact(format!("{CASE}.jsonl"), &path);
    harness.log().info("verify", "e2e case complete");
}

/// Phase 4: surface-agnostic parity. Interactive, print, RPC, and ACP hosts
/// all construct their tool registries through `ToolRegistry::new`; two
/// independent registries must execute the same call identically.
fn prove_surface_parity(root: &Path, config: &pi::config::Config, registry: &ToolRegistry) {
    let second = ToolRegistry::new(&["lsp"], root, Some(config));
    let input = json!({
        "action": "definition",
        "file": "src/cli.rs",
        "line": 4,
        "symbol": "thrust_output",
        "timeout": 90,
    });
    let first = output_json(&execute_lsp(registry, input.clone()).expect("definition 1"));
    let repeated = output_json(&execute_lsp(&second, input).expect("definition 2"));
    assert_eq!(
        first["locations"], repeated["locations"],
        "surface-agnostic parity: independent registries must agree"
    );
    assert!(
        first["locations"]
            .as_array()
            .expect("locations")
            .iter()
            .any(|loc| loc["file"]
                .as_str()
                .is_some_and(|f| f.ends_with("engine.rs"))),
        "definition of thrust_output must land in engine.rs: {first}"
    );
}
