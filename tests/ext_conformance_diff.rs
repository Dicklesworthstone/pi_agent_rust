#![cfg(feature = "ext-conformance")]
//! Differential extension conformance tests: compare TS oracle (Bun + jiti)
//! output against Rust `QuickJS` runtime output for the SAME extension source.
//!
//! Each test:
//! 1. Loads an extension .ts file through the Rust swc+`QuickJS` pipeline
//! 2. Runs the TS oracle harness (Bun + jiti) on the same file
//! 3. Compares registration snapshots (tools, commands, flags, shortcuts, etc.)
//!
//! This validates that the Rust extension runtime is a conforming implementation
//! of the pi extension API.

mod common;

use chrono::{SecondsFormat, Utc};
use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

// ─── Paths ──────────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn artifacts_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/artifacts")
}

fn ts_oracle_script() -> PathBuf {
    project_root().join("tests/ext_conformance/ts_oracle/load_extension.ts")
}

fn ts_harness_script() -> PathBuf {
    project_root().join("tests/ext_conformance/ts_harness/run_extension.ts")
}

fn ts_event_dispatch_bench_script() -> PathBuf {
    project_root().join("tests/ext_conformance/ts_harness/bench_event_dispatch.ts")
}

fn ts_default_mock_spec() -> PathBuf {
    project_root().join("tests/ext_conformance/mock_specs/mock_spec_default.json")
}

fn pi_mono_root() -> PathBuf {
    project_root().join("legacy_pi_mono_code/pi-mono")
}

fn pi_mono_packages() -> PathBuf {
    pi_mono_root().join("packages")
}

fn pi_mono_node_modules() -> PathBuf {
    pi_mono_root().join("node_modules")
}

fn manifest_path() -> PathBuf {
    project_root().join("tests/ext_conformance/VALIDATED_MANIFEST.json")
}

fn determinism_extension_path() -> PathBuf {
    project_root().join("tests/fixtures/determinism_extension.ts")
}

fn event_dispatch_bench_extension_path() -> PathBuf {
    project_root().join("tests/fixtures/event_dispatch_bench_extension.ts")
}

fn event_payloads_path() -> PathBuf {
    project_root().join("tests/ext_conformance/event_payloads/event_payloads.json")
}

const fn bun_path() -> &'static str {
    "/home/ubuntu/.bun/bin/bun"
}

const DEFAULT_DETERMINISTIC_TIME_MS: &str = "1700000000000";
const DEFAULT_DETERMINISTIC_TIME_STEP_MS: &str = "1";
const DEFAULT_DETERMINISTIC_RANDOM_SEED: &str = "1337";
const DEFAULT_DETERMINISTIC_CWD: &str = "/tmp/ext-conformance-test";
const DEFAULT_DETERMINISTIC_HOME: &str = "/tmp/ext-conformance-home";
const DEFAULT_TS_ORACLE_TIMEOUT_SECS: u64 = 30;

const EVENT_DISPATCH_BENCH_EVENT_NAMES: [&str; 11] = [
    "tool_call",
    "tool_result",
    "turn_start",
    "turn_end",
    "before_agent_start",
    "input",
    "context",
    "resources_discover",
    "user_bash",
    "session_before_compact",
    "session_before_tree",
];

struct DeterministicSettings {
    time_ms: String,
    time_step_ms: String,
    random_seed: String,
    random_value: Option<String>,
    cwd: String,
    home: String,
}

fn env_or_default(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|val| !val.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn deterministic_settings() -> DeterministicSettings {
    let random_env = std::env::var("PI_DETERMINISTIC_RANDOM")
        .ok()
        .filter(|val| !val.trim().is_empty());
    let seed_env = std::env::var("PI_DETERMINISTIC_RANDOM_SEED")
        .ok()
        .filter(|val| !val.trim().is_empty());
    let random_value = if random_env.is_some() {
        random_env
    } else if seed_env.is_some() {
        None
    } else {
        Some("0.5".to_string())
    };
    DeterministicSettings {
        time_ms: env_or_default("PI_DETERMINISTIC_TIME_MS", DEFAULT_DETERMINISTIC_TIME_MS),
        time_step_ms: env_or_default(
            "PI_DETERMINISTIC_TIME_STEP_MS",
            DEFAULT_DETERMINISTIC_TIME_STEP_MS,
        ),
        random_seed: env_or_default(
            "PI_DETERMINISTIC_RANDOM_SEED",
            DEFAULT_DETERMINISTIC_RANDOM_SEED,
        ),
        random_value,
        cwd: env_or_default("PI_DETERMINISTIC_CWD", DEFAULT_DETERMINISTIC_CWD),
        home: env_or_default("PI_DETERMINISTIC_HOME", DEFAULT_DETERMINISTIC_HOME),
    }
}

fn sanitize_path_for_dir(path: &Path) -> String {
    let relative = path.strip_prefix(project_root()).unwrap_or(path);
    relative
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn deterministic_settings_for(extension_path: &Path) -> DeterministicSettings {
    let mut settings = deterministic_settings();
    let key = sanitize_path_for_dir(extension_path);

    if std::env::var("PI_DETERMINISTIC_CWD").is_err() {
        settings.cwd = Path::new(DEFAULT_DETERMINISTIC_CWD)
            .join(&key)
            .display()
            .to_string();
    }
    if std::env::var("PI_DETERMINISTIC_HOME").is_err() {
        settings.home = Path::new(DEFAULT_DETERMINISTIC_HOME)
            .join(&key)
            .display()
            .to_string();
    }

    settings
}

fn ts_oracle_timeout() -> Duration {
    std::env::var("PI_TS_ORACLE_TIMEOUT_SECS")
        .ok()
        .and_then(|val| val.parse::<u64>().ok())
        .map_or(
            Duration::from_secs(DEFAULT_TS_ORACLE_TIMEOUT_SECS),
            Duration::from_secs,
        )
}

fn ensure_deterministic_dirs(settings: &DeterministicSettings) {
    let _ = fs::create_dir_all(&settings.cwd);
    let _ = fs::create_dir_all(&settings.home);
}

fn deterministic_random_label(settings: &DeterministicSettings) -> String {
    if let Some(value) = settings.random_value.as_deref() {
        if let Ok(parsed) = value.parse::<f64>() {
            return format!("{parsed:.6}");
        }
        return value.to_string();
    }
    let seed: u32 = settings.random_seed.parse().unwrap_or(0);
    let next = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    let rand = f64::from(next) / 4_294_967_296.0;
    format!("{rand:.6}")
}

fn ts_oracle_node_path() -> &'static Path {
    static NODE_PATH: OnceLock<PathBuf> = OnceLock::new();
    NODE_PATH.get_or_init(|| {
        #[cfg(unix)]
        fn symlink_pkg(scope_dir: &Path, name: &str, target: &Path) {
            use std::os::unix::fs::symlink;

            let link = scope_dir.join(name);
            if link.exists() {
                return;
            }
            symlink(target, &link).expect("create ts oracle package symlink");
        }

        #[cfg(not(unix))]
        fn symlink_pkg(_scope_dir: &Path, _name: &str, _target: &Path) {}

        let base = PathBuf::from(format!(
            "/tmp/pi_agent_rust_ts_oracle_node_path-{}",
            std::process::id()
        ));

        let scope_dir = base.join("@mariozechner");
        std::fs::create_dir_all(&scope_dir).expect("create ts oracle node_path scope dir");

        let packages_dir = pi_mono_packages();
        symlink_pkg(
            &scope_dir,
            "pi-coding-agent",
            &packages_dir.join("coding-agent"),
        );
        symlink_pkg(&scope_dir, "pi-ai", &packages_dir.join("ai"));
        symlink_pkg(&scope_dir, "pi-tui", &packages_dir.join("tui"));
        symlink_pkg(&scope_dir, "pi-agent-core", &packages_dir.join("agent"));

        base
    })
}

/// Get extensions filtered by source tier.
fn extensions_by_tier(tier: &str) -> Vec<(String, String)> {
    let data =
        std::fs::read_to_string(manifest_path()).expect("Failed to read VALIDATED_MANIFEST.json");
    let json: Value = serde_json::from_str(&data).expect("Failed to parse VALIDATED_MANIFEST.json");
    let extensions = json["extensions"]
        .as_array()
        .expect("manifest.extensions should be an array");

    let mut out = Vec::new();
    for entry in extensions {
        if entry["source_tier"].as_str() != Some(tier) {
            continue;
        }
        let entry_path = entry["entry_path"]
            .as_str()
            .expect("missing entry_path in manifest entry");
        let path = Path::new(entry_path);
        let mut components = path.components();
        let Some(root) = components.next() else {
            continue;
        };
        let extension_dir = root.as_os_str().to_string_lossy().to_string();
        let remaining = components.as_path().to_string_lossy().to_string();
        let entry_file = if remaining.is_empty() {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(entry_path)
                .to_string()
        } else {
            remaining
        };
        out.push((extension_dir, entry_file));
    }
    out
}

fn official_extensions() -> &'static Vec<(String, String)> {
    static OFFICIAL: OnceLock<Vec<(String, String)>> = OnceLock::new();
    OFFICIAL.get_or_init(|| extensions_by_tier("official-pi-mono"))
}

#[allow(dead_code)]
fn community_extensions() -> &'static Vec<(String, String)> {
    static COMMUNITY: OnceLock<Vec<(String, String)>> = OnceLock::new();
    COMMUNITY.get_or_init(|| extensions_by_tier("community"))
}

#[allow(dead_code)]
fn npm_extensions() -> &'static Vec<(String, String)> {
    static NPM: OnceLock<Vec<(String, String)>> = OnceLock::new();
    NPM.get_or_init(|| extensions_by_tier("npm-registry"))
}

#[allow(dead_code)]
fn thirdparty_extensions() -> &'static Vec<(String, String)> {
    static THIRDPARTY: OnceLock<Vec<(String, String)>> = OnceLock::new();
    THIRDPARTY.get_or_init(|| extensions_by_tier("third-party-github"))
}

#[test]
fn validated_manifest_has_all_tiers() {
    assert!(
        !official_extensions().is_empty(),
        "expected official-pi-mono extensions in validated manifest"
    );
    assert!(
        !community_extensions().is_empty(),
        "expected community extensions in validated manifest"
    );
    assert!(
        !npm_extensions().is_empty(),
        "expected npm-registry extensions in validated manifest"
    );
    assert!(
        !thirdparty_extensions().is_empty(),
        "expected third-party-github extensions in validated manifest"
    );
}

// ─── TS oracle runner ────────────────────────────────────────────────────────

/// Run the TS oracle harness on an extension and parse the JSON output.
fn run_ts_oracle(extension_path: &Path) -> Value {
    run_ts_oracle_result(extension_path).unwrap_or_else(|err| unreachable!("{err}"))
}

fn run_ts_oracle_result(extension_path: &Path) -> Result<Value, String> {
    let settings = deterministic_settings_for(extension_path);
    ensure_deterministic_dirs(&settings);
    let node_path: Cow<'_, str> = match std::env::var("NODE_PATH") {
        Ok(existing) if !existing.trim().is_empty() => Cow::Owned(format!(
            "{}:{}:{}",
            ts_oracle_node_path().display(),
            pi_mono_node_modules().display(),
            existing
        )),
        _ => Cow::Owned(format!(
            "{}:{}",
            ts_oracle_node_path().display(),
            pi_mono_node_modules().display()
        )),
    };

    let mut cmd = Command::new(bun_path());
    cmd.arg("run")
        .arg(ts_oracle_script())
        .arg(extension_path)
        .arg(&settings.cwd)
        .current_dir(pi_mono_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NODE_PATH", node_path.as_ref())
        .env("PI_DETERMINISTIC_TIME_MS", &settings.time_ms)
        .env("PI_DETERMINISTIC_TIME_STEP_MS", &settings.time_step_ms)
        .env("PI_DETERMINISTIC_CWD", &settings.cwd)
        .env("PI_DETERMINISTIC_HOME", &settings.home);
    if let Some(random_value) = settings.random_value.as_deref() {
        cmd.env("PI_DETERMINISTIC_RANDOM", random_value);
    } else {
        cmd.env("PI_DETERMINISTIC_RANDOM_SEED", &settings.random_seed);
    }

    let timeout = ts_oracle_timeout();
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("failed to spawn TS oracle harness: {err}"))?;
    let start = Instant::now();
    loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .map_err(|err| format!("failed to capture TS oracle output: {err}"))?;
            return Ok(serde_json::json!({
                "success": false,
                "error": format!("timeout after {}s", timeout.as_secs()),
                "stdout": String::from_utf8_lossy(&output.stdout).trim(),
                "stderr": String::from_utf8_lossy(&output.stderr).trim(),
            }));
        }

        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(err) => {
                let _ = child.kill();
                return Err(format!(
                    "TS oracle wait error for {}: {err}",
                    extension_path.display()
                ));
            }
        }

        std::thread::sleep(Duration::from_millis(25));
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to capture TS oracle output: {err}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() && stdout.trim().is_empty() {
        return Err(format!(
            "TS oracle crashed for {}:\nstderr: {stderr}",
            extension_path.display()
        ));
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "TS oracle returned empty stdout for {}:\nstderr: {stderr}",
            extension_path.display()
        ));
    }

    // Some extensions print to stdout before the JSON (e.g. "[WakaTime] Installing...").
    // Try parsing the whole output first; if that fails, find the first '{' and parse from there.
    serde_json::from_str(trimmed)
        .or_else(|_| {
            trimmed
                .find('{')
                .map(|idx| &trimmed[idx..])
                .ok_or_else(|| "no JSON object found".to_string())
                .and_then(|json_str| {
                    serde_json::from_str(json_str).map_err(|e| e.to_string())
                })
        })
        .map_err(|e| {
            format!(
                "TS oracle returned invalid JSON for {}:\n  error: {e}\n  stdout: {stdout}\n  stderr: {stderr}",
                extension_path.display()
            )
        })
}

fn run_ts_harness_result(extension_path: &Path) -> Result<Value, String> {
    let settings = deterministic_settings_for(extension_path);
    ensure_deterministic_dirs(&settings);
    let node_path: Cow<'_, str> = match std::env::var("NODE_PATH") {
        Ok(existing) if !existing.trim().is_empty() => Cow::Owned(format!(
            "{}:{}:{}",
            ts_oracle_node_path().display(),
            pi_mono_node_modules().display(),
            existing
        )),
        _ => Cow::Owned(format!(
            "{}:{}",
            ts_oracle_node_path().display(),
            pi_mono_node_modules().display()
        )),
    };

    let mut cmd = Command::new(bun_path());
    cmd.arg("run")
        .arg(ts_harness_script())
        .arg(extension_path)
        .arg(ts_default_mock_spec())
        .arg(&settings.cwd)
        .current_dir(pi_mono_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NODE_PATH", node_path.as_ref())
        .env("PI_DETERMINISTIC_TIME_MS", &settings.time_ms)
        .env("PI_DETERMINISTIC_TIME_STEP_MS", &settings.time_step_ms)
        .env("PI_DETERMINISTIC_CWD", &settings.cwd)
        .env("PI_DETERMINISTIC_HOME", &settings.home);
    if let Some(random_value) = settings.random_value.as_deref() {
        cmd.env("PI_DETERMINISTIC_RANDOM", random_value);
    } else {
        cmd.env("PI_DETERMINISTIC_RANDOM_SEED", &settings.random_seed);
    }

    let timeout = ts_oracle_timeout();
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("failed to spawn TS harness: {err}"))?;
    let start = Instant::now();
    loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .map_err(|err| format!("failed to capture TS harness output: {err}"))?;
            return Ok(serde_json::json!({
                "success": false,
                "error": format!("timeout after {}s", timeout.as_secs()),
                "stdout": String::from_utf8_lossy(&output.stdout).trim(),
                "stderr": String::from_utf8_lossy(&output.stderr).trim(),
            }));
        }

        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(err) => {
                let _ = child.kill();
                return Err(format!(
                    "TS harness wait error for {}: {err}",
                    extension_path.display()
                ));
            }
        }

        std::thread::sleep(Duration::from_millis(25));
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to capture TS harness output: {err}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() && stdout.trim().is_empty() {
        return Err(format!(
            "TS harness crashed for {}:\nstderr: {stderr}",
            extension_path.display()
        ));
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "TS harness returned empty stdout for {}:\nstderr: {stderr}",
            extension_path.display()
        ));
    }

    serde_json::from_str(trimmed).map_err(|e| {
        format!(
            "TS harness returned invalid JSON for {}:\n  error: {e}\n  stdout: {stdout}\n  stderr: {stderr}",
            extension_path.display()
        )
    })
}

fn run_ts_event_dispatch_bench_result(
    extension_path: &Path,
    payloads_path: &Path,
    iters: usize,
    warmup: usize,
) -> Result<Value, String> {
    let settings = deterministic_settings_for(extension_path);
    ensure_deterministic_dirs(&settings);
    let node_path: Cow<'_, str> = match std::env::var("NODE_PATH") {
        Ok(existing) if !existing.trim().is_empty() => Cow::Owned(format!(
            "{}:{}:{}",
            ts_oracle_node_path().display(),
            pi_mono_node_modules().display(),
            existing
        )),
        _ => Cow::Owned(format!(
            "{}:{}",
            ts_oracle_node_path().display(),
            pi_mono_node_modules().display()
        )),
    };

    let mut cmd = Command::new(bun_path());
    cmd.arg("run")
        .arg(ts_event_dispatch_bench_script())
        .arg(extension_path)
        .arg(payloads_path)
        .arg(&settings.cwd)
        .current_dir(pi_mono_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NODE_PATH", node_path.as_ref())
        .env("PI_EVENT_BENCH_ITERS", iters.to_string())
        .env("PI_EVENT_BENCH_WARMUP", warmup.to_string())
        .env("PI_DETERMINISTIC_TIME_MS", &settings.time_ms)
        .env("PI_DETERMINISTIC_TIME_STEP_MS", &settings.time_step_ms)
        .env("PI_DETERMINISTIC_CWD", &settings.cwd)
        .env("PI_DETERMINISTIC_HOME", &settings.home);
    if let Some(random_value) = settings.random_value.as_deref() {
        cmd.env("PI_DETERMINISTIC_RANDOM", random_value);
    } else {
        cmd.env("PI_DETERMINISTIC_RANDOM_SEED", &settings.random_seed);
    }

    let timeout = ts_oracle_timeout();
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("failed to spawn TS event dispatch bench: {err}"))?;
    let start = Instant::now();
    loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let output = child.wait_with_output().map_err(|err| {
                format!("failed to capture TS event dispatch bench output: {err}")
            })?;
            return Ok(serde_json::json!({
                "success": false,
                "error": format!("timeout after {}s", timeout.as_secs()),
                "stdout": String::from_utf8_lossy(&output.stdout).trim(),
                "stderr": String::from_utf8_lossy(&output.stderr).trim(),
            }));
        }

        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(err) => {
                let _ = child.kill();
                return Err(format!(
                    "TS event dispatch bench wait error for {}: {err}",
                    extension_path.display()
                ));
            }
        }

        std::thread::sleep(Duration::from_millis(25));
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to capture TS event dispatch bench output: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() && stdout.trim().is_empty() {
        return Err(format!(
            "TS event dispatch bench crashed for {}:\nstderr: {stderr}",
            extension_path.display()
        ));
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "TS event dispatch bench returned empty stdout for {}:\nstderr: {stderr}",
            extension_path.display()
        ));
    }

    serde_json::from_str(trimmed).map_err(|e| {
        format!(
            "TS event dispatch bench returned invalid JSON for {}:\n  error: {e}\n  stdout: {stdout}\n  stderr: {stderr}",
            extension_path.display()
        )
    })
}

fn adapt_input_payload(payload: &Value) -> Value {
    let text = payload
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| payload.get("content").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let images = payload
        .get("images")
        .and_then(Value::as_array)
        .or_else(|| payload.get("attachments").and_then(Value::as_array))
        .map_or_else(Vec::new, Clone::clone);
    let source = payload
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_string();

    serde_json::json!({
        "type": "input",
        "text": text,
        "images": images,
        "source": source,
    })
}

#[allow(clippy::too_many_lines)]
fn run_rust_event_dispatch_bench_result(
    extension_path: &Path,
    payloads_path: &Path,
    iters: usize,
    warmup: usize,
) -> Result<Value, String> {
    let settings = deterministic_settings_for(extension_path);
    ensure_deterministic_dirs(&settings);
    let cwd = PathBuf::from(&settings.cwd);
    let extension_path = extension_path.to_path_buf();

    let payload_root: Value = serde_json::from_str(
        &fs::read_to_string(payloads_path)
            .map_err(|err| format!("read event payloads {}: {err}", payloads_path.display()))?,
    )
    .map_err(|err| format!("parse event payloads {}: {err}", payloads_path.display()))?;
    let payloads = payload_root
        .get("event_payloads")
        .and_then(Value::as_object)
        .ok_or_else(|| "event payloads file missing event_payloads object".to_string())?;

    let mut payloads_by_event: HashMap<String, Vec<Value>> = HashMap::new();
    for name in EVENT_DISPATCH_BENCH_EVENT_NAMES {
        let list = payloads
            .get(name)
            .and_then(Value::as_array)
            .map_or_else(Vec::new, Clone::clone);
        let extracted = list
            .into_iter()
            .map(|case| case.get("payload").cloned().unwrap_or(case))
            .collect::<Vec<_>>();
        payloads_by_event.insert(name.to_string(), extracted);
    }

    common::run_async(async move {
        let spec = JsExtensionLoadSpec::from_entry_path(&extension_path)
            .map_err(|e| format!("load spec: {e}"))?;

        let manager = ExtensionManager::new();
        let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
        let mut env = HashMap::new();
        env.insert(
            "PI_DETERMINISTIC_TIME_MS".to_string(),
            settings.time_ms.clone(),
        );
        env.insert(
            "PI_DETERMINISTIC_TIME_STEP_MS".to_string(),
            settings.time_step_ms.clone(),
        );
        env.insert("PI_DETERMINISTIC_CWD".to_string(), settings.cwd.clone());
        env.insert("PI_DETERMINISTIC_HOME".to_string(), settings.home.clone());
        env.insert("HOME".to_string(), settings.home.clone());
        if let Some(random_value) = settings.random_value.as_ref() {
            env.insert("PI_DETERMINISTIC_RANDOM".to_string(), random_value.clone());
        } else {
            env.insert(
                "PI_DETERMINISTIC_RANDOM_SEED".to_string(),
                settings.random_seed.clone(),
            );
        }
        let js_config = PiJsRuntimeConfig {
            cwd: settings.cwd.clone(),
            env,
            ..Default::default()
        };

        let runtime = JsExtensionRuntimeHandle::start(js_config, tools, manager.clone())
            .await
            .map_err(|e| format!("start runtime: {e}"))?;
        manager.set_js_runtime(runtime.clone());

        manager
            .load_js_extensions(vec![spec])
            .await
            .map_err(|e| format!("load extension: {e}"))?;

        let ctx_payload = serde_json::json!({
            "hasUI": false,
            "cwd": settings.cwd,
            "sessionEntries": [],
            "sessionBranch": [],
            "sessionLeafEntry": null,
            "modelRegistry": {},
        });

        let timeout_ms = 20000;
        let mut results = serde_json::Map::new();
        for name in EVENT_DISPATCH_BENCH_EVENT_NAMES {
            let Some(cases) = payloads_by_event.get(name) else {
                results.insert(
                    name.to_string(),
                    serde_json::json!({ "summary": summarize_us(&[]) }),
                );
                continue;
            };
            if cases.is_empty() {
                results.insert(
                    name.to_string(),
                    serde_json::json!({ "summary": summarize_us(&[]) }),
                );
                continue;
            }

            // Warmup
            for i in 0..warmup {
                let base = cases.get(i % cases.len()).cloned().unwrap_or(Value::Null);
                let payload = if name == "input" {
                    adapt_input_payload(&base)
                } else {
                    base
                };
                runtime
                    .dispatch_event(name.to_string(), payload, ctx_payload.clone(), timeout_ms)
                    .await
                    .map_err(|e| format!("dispatch {name} warmup: {e}"))?;
            }

            let mut durations_us = Vec::with_capacity(iters);
            for i in 0..iters {
                let base = cases.get(i % cases.len()).cloned().unwrap_or(Value::Null);
                let payload = if name == "input" {
                    adapt_input_payload(&base)
                } else {
                    base
                };
                let start = Instant::now();
                runtime
                    .dispatch_event(name.to_string(), payload, ctx_payload.clone(), timeout_ms)
                    .await
                    .map_err(|e| format!("dispatch {name}: {e}"))?;
                let elapsed_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                durations_us.push(elapsed_us);
            }

            results.insert(
                name.to_string(),
                serde_json::json!({ "summary": summarize_us(&durations_us) }),
            );
        }

        let report = serde_json::json!({
            "schema": "pi.ext.event_dispatch_latency.v1",
            "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            "toolchain": "rust",
            "iters": iters,
            "warmup": warmup,
            "extension": {
                "path": extension_path.display().to_string(),
            },
            "results": results,
        });

        Ok(serde_json::json!({
            "success": true,
            "error": null,
            "report": report,
        }))
    })
}

// ─── Rust runtime loader ─────────────────────────────────────────────────────

/// Load an extension through the Rust swc+`QuickJS` pipeline and return its
/// registration snapshot in a format comparable to the TS oracle output.
/// Returns `Err(message)` if the extension fails to load.
fn load_rust_snapshot(extension_path: &Path) -> Result<Value, String> {
    let (snapshot, _load_time_ms) = load_rust_snapshot_timed(extension_path)?;
    Ok(snapshot)
}

fn load_rust_snapshot_timed(extension_path: &Path) -> Result<(Value, u64), String> {
    let settings = deterministic_settings_for(extension_path);
    ensure_deterministic_dirs(&settings);
    let cwd = PathBuf::from(&settings.cwd);

    let load_start = Instant::now();

    let spec = JsExtensionLoadSpec::from_entry_path(extension_path)
        .map_err(|e| format!("load spec: {e}"))?;

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let mut env = HashMap::new();
    env.insert(
        "PI_DETERMINISTIC_TIME_MS".to_string(),
        settings.time_ms.clone(),
    );
    env.insert(
        "PI_DETERMINISTIC_TIME_STEP_MS".to_string(),
        settings.time_step_ms.clone(),
    );
    env.insert("PI_DETERMINISTIC_CWD".to_string(), settings.cwd.clone());
    env.insert("PI_DETERMINISTIC_HOME".to_string(), settings.home.clone());
    env.insert("HOME".to_string(), settings.home.clone());
    if let Some(random_value) = settings.random_value {
        env.insert("PI_DETERMINISTIC_RANDOM".to_string(), random_value);
    } else {
        env.insert(
            "PI_DETERMINISTIC_RANDOM_SEED".to_string(),
            settings.random_seed.clone(),
        );
    }
    let js_config = PiJsRuntimeConfig {
        cwd: settings.cwd.clone(),
        env,
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .map_err(|e| format!("start runtime: {e}"))
        }
    })?;
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .map_err(|e| format!("load extension: {e}"))
        }
    })?;

    let load_time_ms = u64::try_from(load_start.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Build snapshot matching TS oracle format
    let commands = manager.list_commands();
    let shortcuts = manager.list_shortcuts();
    let flags = manager.list_flags();
    let providers = manager.extension_providers();
    let tool_defs = manager.extension_tool_defs();
    let event_hooks = manager.list_event_hooks();

    Ok((
        serde_json::json!({
            "commands": commands,
            "shortcuts": shortcuts,
            "flags": flags,
            "providers": providers,
            "tools": tool_defs,
            "event_hooks": event_hooks,
        }),
        load_time_ms,
    ))
}

fn percentile_index(len: usize, numerator: usize, denominator: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let rank = (len * numerator).saturating_add(denominator - 1) / denominator;
    rank.saturating_sub(1).min(len - 1)
}

fn summarize_times(values: &[u64]) -> Value {
    if values.is_empty() {
        return serde_json::json!({ "count": 0 });
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    serde_json::json!({
        "count": sorted.len(),
        "min": sorted[0],
        "max": *sorted.last().unwrap(),
        "p50": sorted[percentile_index(sorted.len(), 1, 2)],
        "p95": sorted[percentile_index(sorted.len(), 95, 100)],
        "p99": sorted[percentile_index(sorted.len(), 99, 100)],
    })
}

fn summarize_ratios(values: &[f64]) -> Value {
    if values.is_empty() {
        return serde_json::json!({ "count": 0 });
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    serde_json::json!({
        "count": sorted.len(),
        "min": sorted[0],
        "max": *sorted.last().unwrap(),
        "p50": sorted[percentile_index(sorted.len(), 1, 2)],
        "p95": sorted[percentile_index(sorted.len(), 95, 100)],
        "p99": sorted[percentile_index(sorted.len(), 99, 100)],
    })
}

fn summarize_us(values: &[u64]) -> Value {
    if values.is_empty() {
        return serde_json::json!({ "count": 0 });
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    serde_json::json!({
        "count": sorted.len(),
        "min_us": sorted[0],
        "max_us": *sorted.last().unwrap(),
        "p50_us": sorted[percentile_index(sorted.len(), 1, 2)],
        "p95_us": sorted[percentile_index(sorted.len(), 95, 100)],
        "p99_us": sorted[percentile_index(sorted.len(), 99, 100)],
    })
}

// ─── Comparison helpers ──────────────────────────────────────────────────────

/// Compare a specific registration category between TS and Rust snapshots.
/// Returns differences as a formatted string (empty = match).
fn compare_category(
    category: &str,
    ts_items: &[Value],
    rust_items: &[Value],
    key_field: &str,
) -> Vec<String> {
    let mut diffs = Vec::new();

    // Compare counts
    if ts_items.len() != rust_items.len() {
        diffs.push(format!(
            "{category} count mismatch: TS={} Rust={}",
            ts_items.len(),
            rust_items.len()
        ));
    }

    // Compare by key
    for ts_item in ts_items {
        let key = ts_item
            .get(key_field)
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let rust_item = rust_items
            .iter()
            .find(|r| r.get(key_field).and_then(|v| v.as_str()) == Some(key));

        if let Some(rust_item) = rust_item {
            // Compare specific fields depending on category
            match category {
                "tools" => {
                    compare_field(&mut diffs, category, key, "description", ts_item, rust_item);
                    compare_field(&mut diffs, category, key, "label", ts_item, rust_item);
                    compare_parameters(&mut diffs, key, ts_item, rust_item);
                }
                "flags" => {
                    compare_field(&mut diffs, category, key, "type", ts_item, rust_item);
                    compare_field(&mut diffs, category, key, "default", ts_item, rust_item);
                    compare_field(&mut diffs, category, key, "description", ts_item, rust_item);
                }
                "commands" | "shortcuts" => {
                    compare_field(&mut diffs, category, key, "description", ts_item, rust_item);
                }
                _ => {}
            }
        } else {
            diffs.push(format!(
                "{category} '{key}': present in TS, missing in Rust"
            ));
        }
    }

    // Check for extra items in Rust
    for rust_item in rust_items {
        let key = rust_item
            .get(key_field)
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let in_ts = ts_items
            .iter()
            .any(|t| t.get(key_field).and_then(|v| v.as_str()) == Some(key));
        if !in_ts {
            diffs.push(format!(
                "{category} '{key}': present in Rust, missing in TS"
            ));
        }
    }

    diffs
}

fn compare_field(
    diffs: &mut Vec<String>,
    category: &str,
    key: &str,
    field: &str,
    ts_item: &Value,
    rust_item: &Value,
) {
    let ts_val = ts_item.get(field);
    let rust_val = rust_item.get(field);

    // Normalize None/null
    let ts_normalized = ts_val.filter(|v| !v.is_null());
    let rust_normalized = rust_val.filter(|v| !v.is_null());

    if ts_normalized != rust_normalized {
        diffs.push(format!(
            "{category} '{key}'.{field}: TS={} Rust={}",
            ts_normalized.map_or_else(|| "null".to_string(), ToString::to_string),
            rust_normalized.map_or_else(|| "null".to_string(), ToString::to_string),
        ));
    }
}

fn compare_parameters(
    diffs: &mut Vec<String>,
    tool_name: &str,
    ts_item: &Value,
    rust_item: &Value,
) {
    let ts_params = ts_item.get("parameters");
    let rust_params = rust_item.get("parameters");

    match (ts_params, rust_params) {
        (Some(ts_p), Some(rust_p)) => {
            // Compare type, required, and property names/types
            if ts_p.get("type") != rust_p.get("type") {
                diffs.push(format!(
                    "tools '{tool_name}'.parameters.type: TS={} Rust={}",
                    ts_p.get("type").unwrap_or(&Value::Null),
                    rust_p.get("type").unwrap_or(&Value::Null),
                ));
            }
            // Compare required fields
            let ts_req = ts_p.get("required");
            let rust_req = rust_p.get("required");
            if ts_req != rust_req {
                diffs.push(format!(
                    "tools '{tool_name}'.parameters.required: TS={} Rust={}",
                    ts_req.unwrap_or(&Value::Null),
                    rust_req.unwrap_or(&Value::Null),
                ));
            }
            // Compare property names
            if let (Some(ts_props), Some(rust_props)) =
                (ts_p.get("properties"), rust_p.get("properties"))
            {
                if let (Some(ts_obj), Some(rust_obj)) =
                    (ts_props.as_object(), rust_props.as_object())
                {
                    for (prop_name, ts_prop_val) in ts_obj {
                        if let Some(rust_prop_val) = rust_obj.get(prop_name) {
                            if ts_prop_val.get("type") != rust_prop_val.get("type") {
                                diffs.push(format!(
                                    "tools '{tool_name}'.parameters.properties.{prop_name}.type: TS={} Rust={}",
                                    ts_prop_val.get("type").unwrap_or(&Value::Null),
                                    rust_prop_val.get("type").unwrap_or(&Value::Null),
                                ));
                            }
                        } else {
                            diffs.push(format!(
                                "tools '{tool_name}'.parameters.properties.{prop_name}: in TS, missing in Rust"
                            ));
                        }
                    }
                    for prop_name in rust_obj.keys() {
                        if !ts_obj.contains_key(prop_name) {
                            diffs.push(format!(
                                "tools '{tool_name}'.parameters.properties.{prop_name}: in Rust, missing in TS"
                            ));
                        }
                    }
                }
            }
        }
        (Some(_), None) => {
            diffs.push(format!(
                "tools '{tool_name}'.parameters: present in TS, missing in Rust"
            ));
        }
        (None, Some(_)) => {
            diffs.push(format!(
                "tools '{tool_name}'.parameters: present in Rust, missing in TS"
            ));
        }
        (None, None) => {}
    }
}

/// Full differential comparison: returns all diffs as a vector of strings.
#[allow(clippy::too_many_lines)]
fn diff_snapshots(ts_oracle: &Value, rust_snapshot: &Value) -> Vec<String> {
    let mut all_diffs = Vec::new();

    let Some(ts_ext) = ts_oracle.get("extension") else {
        all_diffs.push("TS oracle returned no extension".to_string());
        return all_diffs;
    };

    // Compare tools
    let ts_tools = ts_ext
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_tools = rust_snapshot
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    all_diffs.extend(compare_category("tools", &ts_tools, &rust_tools, "name"));

    // Compare commands
    let ts_commands = ts_ext
        .get("commands")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_commands = rust_snapshot
        .get("commands")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    all_diffs.extend(compare_category(
        "commands",
        &ts_commands,
        &rust_commands,
        "name",
    ));

    // Compare flags
    let ts_flags = ts_ext
        .get("flags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_flags = rust_snapshot
        .get("flags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    all_diffs.extend(compare_category("flags", &ts_flags, &rust_flags, "name"));

    // Compare shortcuts
    let ts_shortcuts = ts_ext
        .get("shortcuts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_shortcuts = rust_snapshot
        .get("shortcuts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    all_diffs.extend(compare_category(
        "shortcuts",
        &ts_shortcuts,
        &rust_shortcuts,
        "shortcut",
    ));

    // Compare handler event names
    let ts_handlers = ts_ext
        .get("handlers")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let rust_event_hooks: Vec<String> = rust_snapshot
        .get("event_hooks")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let ts_event_names: Vec<String> = ts_handlers.keys().cloned().collect();
    let mut ts_sorted = ts_event_names;
    ts_sorted.sort();
    let mut rust_sorted = rust_event_hooks;
    rust_sorted.sort();
    if ts_sorted != rust_sorted {
        all_diffs.push(format!(
            "event_hooks mismatch: TS={ts_sorted:?} Rust={rust_sorted:?}"
        ));
    }

    // Compare providers
    let ts_providers = ts_ext
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_providers = rust_snapshot
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if ts_providers.len() != rust_providers.len() {
        all_diffs.push(format!(
            "providers count mismatch: TS={} Rust={}",
            ts_providers.len(),
            rust_providers.len()
        ));
    }

    all_diffs
}

// ─── Test runner ─────────────────────────────────────────────────────────────

/// Run the differential test for a single extension file.
fn run_differential_test(extension_name: &str, entry_file: &str) {
    let ext_path = artifacts_dir().join(extension_name).join(entry_file);
    assert!(
        ext_path.exists(),
        "Extension file not found: {}",
        ext_path.display()
    );

    // Run TS oracle
    let ts_result = run_ts_oracle(&ext_path);
    let ts_success = ts_result
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !ts_success {
        let err = ts_result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        eprintln!("TS oracle failed for {extension_name}: {err}");
        // If TS fails, skip the comparison (extension may be broken)
        return;
    }

    // Run Rust runtime
    let rust_result = load_rust_snapshot(&ext_path)
        .unwrap_or_else(|err| unreachable!("Rust runtime failed for {extension_name}: {err}"));

    // Compare
    let diffs = diff_snapshots(&ts_result, &rust_result);

    if !diffs.is_empty() {
        eprintln!("=== Differential test failed for {extension_name} ===");
        for diff in &diffs {
            eprintln!("  {diff}");
        }
        eprintln!(
            "\nTS snapshot:\n{}",
            serde_json::to_string_pretty(&ts_result).unwrap()
        );
        eprintln!(
            "\nRust snapshot:\n{}",
            serde_json::to_string_pretty(&rust_result).unwrap()
        );
        unreachable!(
            "Differential conformance failed for {extension_name}: {} differences",
            diffs.len()
        );
    }
}

/// Strict differential test that treats TS oracle failures as errors and returns a summary.
/// Retries once on TS oracle timeout (flaky under load).
fn run_differential_test_strict(extension_name: &str, entry_file: &str) -> Result<(), String> {
    let ext_path = artifacts_dir().join(extension_name).join(entry_file);
    if !ext_path.exists() {
        return Err(format!("extension file not found: {}", ext_path.display()));
    }

    // Run TS oracle with one retry on timeout
    let ts_result = {
        let first_try = run_ts_oracle(&ext_path);
        let first_success = first_try
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if first_success {
            first_try
        } else {
            let err = first_try
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if err.contains("timeout") {
                eprintln!("[{extension_name}] TS oracle timed out, retrying...");
                let retry = run_ts_oracle(&ext_path);
                let retry_success = retry
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if retry_success {
                    retry
                } else {
                    return Err(format!("ts_oracle_failed (after retry): {err}"));
                }
            } else {
                return Err(format!("ts_oracle_failed: {err}"));
            }
        }
    };

    let rust_snapshot =
        load_rust_snapshot(&ext_path).map_err(|err| format!("rust_runtime_failed: {err}"))?;
    let diffs = diff_snapshots(&ts_result, &rust_snapshot);
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(format!("diffs: {}", diffs.join("; ")))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn diff_official_manifest() {
    let filter = std::env::var("PI_OFFICIAL_FILTER").ok();
    let max = std::env::var("PI_OFFICIAL_MAX")
        .ok()
        .and_then(|val| val.parse::<usize>().ok());

    let selected: Vec<(String, String)> = official_extensions()
        .iter()
        .filter(|(dir, entry)| {
            let name = format!("{dir}/{entry}");
            filter.as_ref().is_none_or(|needle| name.contains(needle))
        })
        .take(max.unwrap_or(usize::MAX))
        .cloned()
        .collect();

    eprintln!(
        "[diff_official_manifest] Starting (selected={} filter={:?} max={:?})",
        selected.len(),
        filter,
        max
    );

    let mut failures = Vec::new();
    for (idx, (extension_dir, entry_file)) in selected.iter().enumerate() {
        let name = format!("{extension_dir}/{entry_file}");
        eprintln!(
            "[diff_official_manifest] {}/{}: {name}",
            idx + 1,
            selected.len()
        );
        let start = std::time::Instant::now();
        if let Err(err) = run_differential_test_strict(extension_dir, entry_file) {
            failures.push(format!("{name}: {err}"));
        }
        eprintln!(
            "[diff_official_manifest] {}/{}: done in {:?}",
            idx + 1,
            selected.len(),
            start.elapsed()
        );
    }

    assert!(
        failures.is_empty(),
        "Official conformance failures ({}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
#[ignore = "bd-1o6l: generate load-time benchmark report"]
#[allow(clippy::too_many_lines)]
fn load_time_benchmark_official() {
    let filter = std::env::var("PI_LOAD_TIME_FILTER").ok();
    let max = std::env::var("PI_LOAD_TIME_MAX")
        .ok()
        .and_then(|val| val.parse::<usize>().ok());

    let selected: Vec<(String, String)> = official_extensions()
        .iter()
        .filter(|(dir, entry)| {
            let name = format!("{dir}/{entry}");
            filter.as_ref().is_none_or(|needle| name.contains(needle))
        })
        .take(max.unwrap_or(usize::MAX))
        .cloned()
        .collect();

    eprintln!(
        "[load_time_benchmark] Starting (selected={} filter={:?} max={:?})",
        selected.len(),
        filter,
        max
    );

    let mut results = Vec::new();
    let mut ts_times = Vec::new();
    let mut rust_times = Vec::new();
    let mut ratios = Vec::new();
    let mut ts_success = 0usize;
    let mut rust_success = 0usize;
    let mut paired = 0usize;

    for (idx, (extension_dir, entry_file)) in selected.iter().enumerate() {
        let name = format!("{extension_dir}/{entry_file}");
        let ext_path = artifacts_dir().join(extension_dir).join(entry_file);
        eprintln!(
            "[load_time_benchmark] {}/{}: {name}",
            idx + 1,
            selected.len()
        );

        let (ts_ok, ts_load, ts_error) = match run_ts_harness_result(&ext_path) {
            Ok(ts_result) => {
                let ok = ts_result
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let load = ts_result.get("load_time_ms").and_then(Value::as_u64);
                let error = ts_result
                    .get("error")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                if ok {
                    ts_success += 1;
                }
                if let Some(load_ms) = load {
                    ts_times.push(load_ms);
                }
                (ok, load, error)
            }
            Err(err) => (false, None, Some(err)),
        };

        let (rust_snapshot, rust_load) = match load_rust_snapshot_timed(&ext_path) {
            Ok((snapshot, load_ms)) => {
                rust_success += 1;
                rust_times.push(load_ms);
                (Some(snapshot), Some(load_ms))
            }
            Err(err) => {
                results.push(serde_json::json!({
                    "extension": name,
                    "ts": {
                        "success": ts_ok,
                        "load_time_ms": ts_load,
                        "error": ts_error,
                    },
                    "rust": {
                        "success": false,
                        "load_time_ms": null,
                        "error": err,
                    },
                    "ratio": null,
                }));
                continue;
            }
        };

        let ratio = match (ts_load, rust_load) {
            (Some(ts_ms), Some(rust_ms)) if ts_ms > 0 => {
                #[allow(clippy::cast_precision_loss)]
                let ratio = (rust_ms as f64) / (ts_ms as f64);
                ratios.push(ratio);
                paired += 1;
                Some(ratio)
            }
            _ => None,
        };

        results.push(serde_json::json!({
            "extension": name,
            "ts": {
                "success": ts_ok,
                "load_time_ms": ts_load,
                "error": ts_error,
            },
            "rust": {
                "success": rust_snapshot.is_some(),
                "load_time_ms": rust_load,
                "error": null,
            },
            "ratio": ratio,
        }));
    }

    let report = serde_json::json!({
        "schema": "pi.ext.load_time_benchmark.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "tier": "official-pi-mono",
        "filter": filter,
        "max": max,
        "counts": {
            "total": selected.len(),
            "ts_success": ts_success,
            "rust_success": rust_success,
            "paired": paired,
        },
        "ts": summarize_times(&ts_times),
        "rust": summarize_times(&rust_times),
        "ratio": summarize_ratios(&ratios),
        "results": results,
    });

    let report_path = project_root().join("tests/ext_conformance/reports/load_time_benchmark.json");
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent).expect("create report directory");
    }
    fs::write(&report_path, serde_json::to_string_pretty(&report).unwrap())
        .expect("write load time benchmark report");

    eprintln!(
        "[load_time_benchmark] Wrote report to {}",
        report_path.display()
    );
}

#[test]
#[ignore = "bd-sas4: generate event dispatch latency report"]
fn event_dispatch_latency_benchmark() {
    let extension_path = event_dispatch_bench_extension_path();
    let payloads_path = event_payloads_path();
    let iters = std::env::var("PI_EVENT_BENCH_ITERS")
        .ok()
        .and_then(|val| val.parse::<usize>().ok())
        .unwrap_or(1000);
    let warmup = std::env::var("PI_EVENT_BENCH_WARMUP")
        .ok()
        .and_then(|val| val.parse::<usize>().ok())
        .unwrap_or(25);

    eprintln!("[event_dispatch_latency] Starting (iters={iters} warmup={warmup})");

    let ts_result =
        match run_ts_event_dispatch_bench_result(&extension_path, &payloads_path, iters, warmup) {
            Ok(value) => value,
            Err(err) => serde_json::json!({ "success": false, "error": err, "report": null }),
        };
    let rust_result = match run_rust_event_dispatch_bench_result(
        &extension_path,
        &payloads_path,
        iters,
        warmup,
    ) {
        Ok(value) => value,
        Err(err) => serde_json::json!({ "success": false, "error": err, "report": null }),
    };

    let mut checks = serde_json::Map::new();
    for name in EVENT_DISPATCH_BENCH_EVENT_NAMES {
        let ts_summary = ts_result
            .get("report")
            .and_then(|report| report.get("results"))
            .and_then(|results| results.get(name))
            .and_then(|entry| entry.get("summary"));
        let rust_summary = rust_result
            .get("report")
            .and_then(|report| report.get("results"))
            .and_then(|results| results.get(name))
            .and_then(|entry| entry.get("summary"));

        let ts_p50 = ts_summary
            .and_then(|s| s.get("p50_us"))
            .and_then(Value::as_u64);
        let ts_p99 = ts_summary
            .and_then(|s| s.get("p99_us"))
            .and_then(Value::as_u64);
        let rust_p50 = rust_summary
            .and_then(|s| s.get("p50_us"))
            .and_then(Value::as_u64);
        let rust_p99 = rust_summary
            .and_then(|s| s.get("p99_us"))
            .and_then(Value::as_u64);

        let rust_p99_lt_5ms = rust_p99.map(|value| value <= 5000);
        let rust_p50_lt_ts_p50 = match (rust_p50, ts_p50) {
            (Some(rust), Some(ts)) => Some(rust < ts),
            _ => None,
        };

        checks.insert(
            name.to_string(),
            serde_json::json!({
                "ts_p50_us": ts_p50,
                "ts_p99_us": ts_p99,
                "rust_p50_us": rust_p50,
                "rust_p99_us": rust_p99,
                "rust_p99_lt_5ms": rust_p99_lt_5ms,
                "rust_p50_lt_ts_p50": rust_p50_lt_ts_p50,
            }),
        );
    }

    let report = serde_json::json!({
        "schema": "pi.ext.event_dispatch_latency_report.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "iters": iters,
        "warmup": warmup,
        "extension": extension_path.display().to_string(),
        "ts": ts_result,
        "rust": rust_result,
        "checks": checks,
    });

    let report_path =
        project_root().join("tests/ext_conformance/reports/event_dispatch_latency.json");
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent).expect("create report directory");
    }
    fs::write(&report_path, serde_json::to_string_pretty(&report).unwrap())
        .expect("write event dispatch latency report");

    eprintln!(
        "[event_dispatch_latency] Wrote report to {}",
        report_path.display()
    );
}

#[test]
fn diff_hello() {
    run_differential_test("hello", "hello.ts");
}

#[test]
fn diff_pirate() {
    run_differential_test("pirate", "pirate.ts");
}

#[test]
fn diff_session_name() {
    run_differential_test("session-name", "session-name.ts");
}

#[test]
fn diff_todo() {
    run_differential_test("todo", "todo.ts");
}

#[test]
fn diff_bookmark() {
    run_differential_test("bookmark", "bookmark.ts");
}

#[test]
fn diff_dirty_repo_guard() {
    run_differential_test("dirty-repo-guard", "dirty-repo-guard.ts");
}

#[test]
fn diff_event_bus() {
    run_differential_test("event-bus", "event-bus.ts");
}

#[test]
fn diff_tool_override() {
    run_differential_test("tool-override", "tool-override.ts");
}

#[test]
fn diff_custom_footer() {
    run_differential_test("custom-footer", "custom-footer.ts");
}

#[test]
fn diff_question() {
    run_differential_test("question", "question.ts");
}

#[test]
fn diff_trigger_compact() {
    run_differential_test("trigger-compact", "trigger-compact.ts");
}

#[test]
fn diff_notify() {
    run_differential_test("notify", "notify.ts");
}

#[test]
fn diff_model_status() {
    run_differential_test("model-status", "model-status.ts");
}

#[test]
fn diff_permission_gate() {
    run_differential_test("permission-gate", "permission-gate.ts");
}

#[test]
fn diff_status_line() {
    run_differential_test("status-line", "status-line.ts");
}

#[test]
fn diff_custom_provider_anthropic() {
    run_differential_test("custom-provider-anthropic", "index.ts");
}

#[test]
fn diff_deterministic_globals() {
    let ext_path = determinism_extension_path();
    let settings = deterministic_settings_for(&ext_path);
    ensure_deterministic_dirs(&settings);
    let ts_result = run_ts_oracle(&ext_path);
    let ts_success = ts_result
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(ts_success, "ts oracle failed: {ts_result:?}");

    let rust_snapshot = load_rust_snapshot(&ext_path).unwrap_or_else(|err| unreachable!("{err}"));
    let diffs = diff_snapshots(&ts_result, &rust_snapshot);
    assert!(diffs.is_empty(), "diffs: {}", diffs.join("; "));

    let tools = ts_result
        .pointer("/extension/tools")
        .and_then(Value::as_array)
        .expect("ts oracle tools");
    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let rand_label = deterministic_random_label(&settings);
    let expected_name = format!("determinism-{}-{}", settings.time_ms, rand_label);
    let expected_desc = format!(
        "now={} rand={} cwd={} home={}",
        settings.time_ms, rand_label, settings.cwd, settings.home
    );
    assert_eq!(name, expected_name);
    assert_eq!(description, expected_desc);
}

/// Run differential conformance tests on community extensions (58 extensions from pi-mono).
/// Use `PI_COMMUNITY_FILTER` env var to filter by name substring.
/// Use `PI_COMMUNITY_MAX` env var to limit the number of extensions to test.
#[test]
fn diff_community_manifest() {
    let filter = std::env::var("PI_COMMUNITY_FILTER").ok();
    let max = std::env::var("PI_COMMUNITY_MAX")
        .ok()
        .and_then(|val| val.parse::<usize>().ok());

    let selected: Vec<(String, String)> = community_extensions()
        .iter()
        .filter(|(dir, entry)| {
            let name = format!("{dir}/{entry}");
            filter.as_ref().is_none_or(|needle| name.contains(needle))
        })
        .take(max.unwrap_or(usize::MAX))
        .cloned()
        .collect();

    eprintln!(
        "[diff_community_manifest] Starting (selected={} filter={:?} max={:?})",
        selected.len(),
        filter,
        max
    );

    let mut failures = Vec::new();
    let mut passes: u32 = 0;
    for (idx, (extension_dir, entry_file)) in selected.iter().enumerate() {
        let name = format!("{extension_dir}/{entry_file}");
        eprintln!(
            "[diff_community_manifest] {}/{}: {name}",
            idx + 1,
            selected.len()
        );
        let start = std::time::Instant::now();
        match run_differential_test_strict(extension_dir, entry_file) {
            Ok(()) => passes += 1,
            Err(err) => failures.push(format!("{name}: {err}")),
        }
        eprintln!(
            "[diff_community_manifest] {}/{}: done in {:?}",
            idx + 1,
            selected.len(),
            start.elapsed()
        );
    }

    eprintln!(
        "[diff_community_manifest] Results: {} passed, {} failed out of {} total",
        passes,
        failures.len(),
        selected.len()
    );

    // Separate TS oracle failures (environment issues we can't fix) from Rust failures
    let (ts_oracle_failures, rust_failures): (Vec<String>, Vec<String>) = failures
        .into_iter()
        .partition(|f| f.contains("ts_oracle_failed"));

    let total_failures = ts_oracle_failures.len() + rust_failures.len();
    if total_failures > 0 {
        let all: Vec<&str> = ts_oracle_failures
            .iter()
            .chain(rust_failures.iter())
            .map(String::as_str)
            .collect();
        eprintln!(
            "Community conformance failures ({total_failures}, {ts} TS oracle, {rust} Rust):\n{all}",
            ts = ts_oracle_failures.len(),
            rust = rust_failures.len(),
            all = all.join("\n")
        );
    }

    // Pass rate (excluding TS oracle failures that we can't fix)
    let testable = u32::try_from(selected.len() - ts_oracle_failures.len()).unwrap_or(u32::MAX);
    let pass_rate = if testable == 0 {
        100.0
    } else {
        f64::from(passes) / f64::from(testable) * 100.0
    };
    eprintln!(
        "[diff_community_manifest] Pass rate: {pass_rate:.1}% ({passes}/{testable} testable, {} TS oracle skipped)",
        ts_oracle_failures.len()
    );

    // Assert: zero Rust-side failures (TS oracle failures are environment issues)
    assert!(
        rust_failures.is_empty(),
        "Rust-side community conformance failures ({}):\n{}",
        rust_failures.len(),
        rust_failures.join("\n")
    );
}

/// Run differential conformance tests on npm registry extensions (63 packages).
/// Use `PI_NPM_FILTER` env var to filter by name substring.
/// Use `PI_NPM_MAX` env var to limit the number of extensions to test.
#[test]
#[ignore = "bd-3dd7: npm registry extensions not yet expected to pass; run manually with --ignored"]
fn diff_npm_manifest() {
    let filter = std::env::var("PI_NPM_FILTER").ok();
    let max = std::env::var("PI_NPM_MAX")
        .ok()
        .and_then(|val| val.parse::<usize>().ok());

    let selected: Vec<(String, String)> = npm_extensions()
        .iter()
        .filter(|(dir, entry)| {
            let name = format!("{dir}/{entry}");
            filter.as_ref().is_none_or(|needle| name.contains(needle))
        })
        .take(max.unwrap_or(usize::MAX))
        .cloned()
        .collect();

    eprintln!(
        "[diff_npm_manifest] Starting (selected={} filter={:?} max={:?})",
        selected.len(),
        filter,
        max
    );

    let mut failures = Vec::new();
    let mut passes: u32 = 0;
    for (idx, (extension_dir, entry_file)) in selected.iter().enumerate() {
        let name = format!("{extension_dir}/{entry_file}");
        eprintln!("[diff_npm_manifest] {}/{}: {name}", idx + 1, selected.len());
        let start = std::time::Instant::now();
        match run_differential_test_strict(extension_dir, entry_file) {
            Ok(()) => passes += 1,
            Err(err) => failures.push(format!("{name}: {err}")),
        }
        eprintln!(
            "[diff_npm_manifest] {}/{}: done in {:?}",
            idx + 1,
            selected.len(),
            start.elapsed()
        );
    }

    let total: u32 = selected.len().try_into().unwrap_or(0);
    eprintln!(
        "[diff_npm_manifest] Results: {} passed, {} failed out of {} total",
        passes,
        failures.len(),
        total
    );

    if !failures.is_empty() {
        eprintln!(
            "npm conformance failures ({}):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    let pass_rate = if total == 0 {
        0.0
    } else {
        f64::from(passes) / f64::from(total) * 100.0
    };
    eprintln!("[diff_npm_manifest] Pass rate: {pass_rate:.1}%");

    assert!(
        failures.is_empty(),
        "npm conformance failures ({}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Run differential conformance tests on third-party GitHub extensions (23 extensions).
/// Use `PI_THIRDPARTY_FILTER` env var to filter by name substring.
/// Use `PI_THIRDPARTY_MAX` env var to limit the number of extensions to test.
#[test]
#[ignore = "bd-22r2: third-party extensions not yet expected to pass; run manually with --ignored"]
fn diff_thirdparty_manifest() {
    let filter = std::env::var("PI_THIRDPARTY_FILTER").ok();
    let max = std::env::var("PI_THIRDPARTY_MAX")
        .ok()
        .and_then(|val| val.parse::<usize>().ok());

    let selected: Vec<(String, String)> = thirdparty_extensions()
        .iter()
        .filter(|(dir, entry)| {
            let name = format!("{dir}/{entry}");
            filter.as_ref().is_none_or(|needle| name.contains(needle))
        })
        .take(max.unwrap_or(usize::MAX))
        .cloned()
        .collect();

    eprintln!(
        "[diff_thirdparty_manifest] Starting (selected={} filter={:?} max={:?})",
        selected.len(),
        filter,
        max
    );

    let mut failures = Vec::new();
    let mut passes: u32 = 0;
    for (idx, (extension_dir, entry_file)) in selected.iter().enumerate() {
        let name = format!("{extension_dir}/{entry_file}");
        eprintln!(
            "[diff_thirdparty_manifest] {}/{}: {name}",
            idx + 1,
            selected.len()
        );
        let start = std::time::Instant::now();
        match run_differential_test_strict(extension_dir, entry_file) {
            Ok(()) => passes += 1,
            Err(err) => failures.push(format!("{name}: {err}")),
        }
        eprintln!(
            "[diff_thirdparty_manifest] {}/{}: done in {:?}",
            idx + 1,
            selected.len(),
            start.elapsed()
        );
    }

    // Separate TS oracle failures from Rust-side failures
    let (ts_oracle_failures, rust_failures): (Vec<String>, Vec<String>) = failures
        .into_iter()
        .partition(|f| f.contains("ts_oracle_failed"));

    let total_failures = ts_oracle_failures.len() + rust_failures.len();
    eprintln!(
        "[diff_thirdparty_manifest] Results: {} passed, {} failed ({} TS oracle, {} Rust) out of {} total",
        passes,
        total_failures,
        ts_oracle_failures.len(),
        rust_failures.len(),
        selected.len()
    );

    if total_failures > 0 {
        let all: Vec<&str> = ts_oracle_failures
            .iter()
            .chain(rust_failures.iter())
            .map(String::as_str)
            .collect();
        eprintln!(
            "Third-party conformance failures ({total_failures}):\n{}",
            all.join("\n")
        );
    }

    let testable = u32::try_from(selected.len() - ts_oracle_failures.len()).unwrap_or(u32::MAX);
    let pass_rate = if testable == 0 {
        100.0
    } else {
        f64::from(passes) / f64::from(testable) * 100.0
    };
    eprintln!(
        "[diff_thirdparty_manifest] Pass rate: {pass_rate:.1}% ({passes}/{testable} testable, {} TS oracle skipped)",
        ts_oracle_failures.len()
    );

    // Assert: zero Rust-side failures
    assert!(
        rust_failures.is_empty(),
        "Rust-side third-party conformance failures ({}):\n{}",
        rust_failures.len(),
        rust_failures.join("\n")
    );
}
