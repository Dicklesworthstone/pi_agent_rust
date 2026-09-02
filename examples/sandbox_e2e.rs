//! End-to-end acceptance for the bash sandbox (`sandbox.*`).
//!
//! Drives picrab's own `BashTool` (via `default_tool_registry`) through the
//! three enforcement checks of the A1 acceptance matrix, plus the PTY path:
//!
//! 1. `mode: off` — byte-equivalent to the pre-sandbox path (no profile, no proxy).
//! 2. `mode: auto` (filesystem) — reads of `~/.ssh` are denied.
//! 3. `mode: auto` (network allowlist) — blocked domains fail through the proxy.
//! 4. PTY spawn path under the sandbox — same denials with `bash.pty = always`.
//!
//! Run: cargo run -p picrab --example sandbox_e2e

use std::path::Path;

use pi::config::Config;
use pi::sdk::{ContentBlock, default_tool_registry};

fn output_text(output: &pi::tools::ToolOutput) -> String {
    output
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn make_config(settings_json: &str) -> Config {
    let mut config = Config::default();
    config.sandbox = Some(serde_json::from_str(settings_json).expect("sandbox settings"));
    config
}

async fn run_bash(config: &Config, cwd: &Path, command: &str) -> String {
    let registry = default_tool_registry(&["bash"], cwd, config);
    let tool = registry
        .get("bash")
        .unwrap_or_else(|| panic!("bash tool missing"));
    let output = tool
        .execute("sandbox-e2e", serde_json::json!({ "command": command }), None)
        .await
        .expect("bash execute");
    output_text(&output)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let reactor = asupersync::runtime::reactor::create_reactor().expect("reactor");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .expect("runtime");

    runtime.block_on(async { run().await })?;
    Ok(())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut failures: Vec<String> = Vec::new();
    let cwd = std::env::temp_dir().join("picrab-sandbox-e2e");
    std::fs::create_dir_all(&cwd)?;

    // ── 1. mode: off → unsandboxed behavior ─────────────────────────────
    let off = make_config(r#"{ "mode": "off" }"#);
    let out = run_bash(&off, &cwd, "echo OFF_OK; cat ~/.ssh/known_hosts >/dev/null 2>&1; echo cat_rc=$?").await;
    println!("[off] {out}");
    if !out.contains("OFF_OK") || out.contains("[SANDBOX]") {
        failures.push("mode=off produced sandboxed or broken output".into());
    }

    // ── 2. mode: auto → ~/.ssh read denied ──────────────────────────────
    let auto = make_config(r#"{ "mode": "auto" }"#);
    let out = run_bash(&auto, &cwd, "cat ~/.ssh/known_hosts 2>&1 | head -1; echo done").await;
    println!("[fs] {out}");
    if !out.contains("Operation not permitted") {
        failures.push("mode=auto did not deny ~/.ssh read".into());
    }
    if !out.contains("[SANDBOX]") {
        failures.push("mode=auto denial was not annotated with [SANDBOX] guidance".into());
    }

    // Write inside the workspace must succeed; outside must fail.
    let out = run_bash(&auto, &cwd, "echo hi > w.txt && rm w.txt && echo WS_WRITE_OK; echo x > ~/picrab-e2e-denied.txt 2>&1 || echo HOME_WRITE_DENIED").await;
    println!("[fs2] {out}");
    if !out.contains("WS_WRITE_OK") || !out.contains("HOME_WRITE_DENIED") {
        failures.push("mode=auto workspace write policy broken".into());
    }

    // ── 3. network allowlist → blocked domain refused ───────────────────
    let net = make_config(
        r#"{ "mode": "auto", "network": "allowlist", "allowedDomains": ["api.github.com"] }"#,
    );
    let out = run_bash(&net, &cwd, "curl -sS -m 15 https://example.com 2>&1 | head -2; echo probe_done").await;
    println!("[net] {out}");
    if out.contains("Example Domain") {
        failures.push("network allowlist let a blocked domain through".into());
    }

    // ── 4. PTY path under the sandbox ───────────────────────────────────
    let pty_cfg = make_config(r#"{ "mode": "auto" }"#);
    let mut pty = pty_cfg.clone();
    pty.bash = Some(pi::config::BashSettings {
        pty: Some("always".to_string()),
        ..Default::default()
    });
    let out = run_bash(&pty, &cwd, "cat ~/.ssh/known_hosts 2>&1 | head -1; echo pty_done").await;
    println!("[pty] {out}");
    if !out.contains("Operation not permitted") {
        failures.push("PTY path did not deny ~/.ssh read under sandbox".into());
    }

    // ── 5. two sandboxes, same policy → shared manager, both enforce ────
    let out_a = run_bash(&auto, &cwd, "cat ~/.ssh/known_hosts 2>&1 | head -1; echo a_done").await;
    let out_b = run_bash(&auto, &cwd, "cat ~/.ssh/known_hosts 2>&1 | head -1; echo b_done").await;
    if !out_a.contains("Operation not permitted") || !out_b.contains("Operation not permitted") {
        failures.push("second sandboxed execution (shared manager) failed to enforce".into());
    } else {
        println!("[concurrent] both executions enforced under the shared manager");
    }

    if failures.is_empty() {
        println!("\nSANDBOX E2E: ALL CHECKS PASSED");
    } else {
        println!("\nSANDBOX E2E FAILURES:");
        for f in &failures {
            println!("  - {f}");
        }
        std::process::exit(1);
    }
    Ok(())
}
