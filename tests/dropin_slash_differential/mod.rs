//! Differential test harness for slash command parity between pi-mono and Rust Pi.
//!
//! This module drives slash-command-equivalent behavior through the shared RPC
//! protocol and fails closed when the legacy runtime is not provisioned.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const DIFFERENTIAL_RUNNER_UNAVAILABLE: &str =
    "RPC slash-command differential runner is unavailable";
const RPC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const PI_MONO_ROOT_RELATIVE: &str = "legacy_pi_mono_code/pi-mono";
const PI_MONO_TSX_RELATIVE: &str = "node_modules/tsx/dist/cli.mjs";
const PI_MONO_CLI_RELATIVE: &str = "packages/coding-agent/src/cli.ts";
const RPC_TEST_PROVIDER: &str = "slash-differential-fixture";
const RPC_TEST_MODEL: &str = "credential-free-fixture";
const RPC_TEST_BASE_URL: &str = "http://127.0.0.1:9/v1";
const RPC_TEST_API_KEY: &str = "not-used";

/// A slash command test scenario.
#[derive(Debug, Clone)]
pub struct SlashCommandScenario {
    /// Name of the test case
    pub name: String,
    /// The slash command to execute
    pub command: String,
    /// Expected behavior description
    pub description: String,
    /// Whether this command should work in streaming mode
    pub supports_streaming: bool,
    /// Additional setup needed before running the command
    pub setup: Vec<String>,
}

/// How a slash command can be observed by the mirrored RPC runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SlashCommandObservability {
    /// The command maps to shared, credential-free RPC requests today.
    RpcObservable,
    /// The command is slash/UI behavior, but needs an explicit RPC adapter first.
    UiOnlyNeedsAdapter,
    /// The command cannot be counted as credential-free mirrored pass evidence.
    ExcludedFromPassEvidence,
}

/// Classification result used to keep pass-evidence policy explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommandObservabilityPolicy {
    pub kind: SlashCommandObservability,
    pub reason: &'static str,
}

impl SlashCommandObservabilityPolicy {
    const fn rpc(reason: &'static str) -> Self {
        Self {
            kind: SlashCommandObservability::RpcObservable,
            reason,
        }
    }

    const fn adapter(reason: &'static str) -> Self {
        Self {
            kind: SlashCommandObservability::UiOnlyNeedsAdapter,
            reason,
        }
    }

    const fn excluded(reason: &'static str) -> Self {
        Self {
            kind: SlashCommandObservability::ExcludedFromPassEvidence,
            reason,
        }
    }
}

/// Classify slash inputs before translating them to RPC commands.
pub fn slash_command_observability(input: &str) -> SlashCommandObservabilityPolicy {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return SlashCommandObservabilityPolicy::excluded(
            "plain prompts require model execution and are not credential-free differential evidence",
        );
    }

    let (command, args) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed, ""));
    let command = command.to_ascii_lowercase();
    let args = args.trim();

    match command.as_str() {
        "/help" | "/h" | "/?" => SlashCommandObservabilityPolicy::rpc(
            "help maps to get_state plus get_commands over shared RPC",
        ),
        "/clear" | "/cls" => {
            SlashCommandObservabilityPolicy::rpc("clear maps to new_session over shared RPC")
        }
        "/model" | "/m" if args.is_empty() => SlashCommandObservabilityPolicy::rpc(
            "model selector inventory maps to get_available_models over shared RPC",
        ),
        "/thinking" | "/think" | "/t" if args.is_empty() || is_valid_thinking_level(args) => {
            SlashCommandObservabilityPolicy::rpc(
                "thinking level inspection or mutation maps to shared RPC",
            )
        }
        "/thinking" | "/think" | "/t" => SlashCommandObservabilityPolicy::excluded(
            "invalid thinking levels are negative slash parser cases, not credential-free pass evidence",
        ),
        "/session" | "/info" => SlashCommandObservabilityPolicy::rpc(
            "session info maps to get_state and get_session_stats over shared RPC",
        ),
        "/tree" => {
            SlashCommandObservabilityPolicy::rpc("tree maps to get_fork_messages over shared RPC")
        }
        "/compact" => SlashCommandObservabilityPolicy::excluded(
            "compaction requires model execution and is not credential-free mirrored pass evidence",
        ),
        "/theme" | "/history" | "/hist" => SlashCommandObservabilityPolicy::adapter(
            "interactive UI surface needs an RPC-observable adapter before pass evidence",
        ),
        "/exit" | "/quit" | "/q" => SlashCommandObservabilityPolicy::excluded(
            "process termination is not credential-free mirrored RPC pass evidence",
        ),
        _ => SlashCommandObservabilityPolicy::adapter(
            "slash parser error or extension command needs a shared RPC observation contract",
        ),
    }
}

fn is_valid_thinking_level(args: &str) -> bool {
    let mut parts = args.split_whitespace();
    let Some(level) = parts.next() else {
        return true;
    };
    if parts.next().is_some() {
        return false;
    }
    matches!(
        level.to_ascii_lowercase().as_str(),
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}

/// Whether this scenario can count as credential-free mirrored pass evidence.
pub fn scenario_requires_pass_evidence(scenario: &SlashCommandScenario) -> bool {
    slash_command_observability(&scenario.command).kind == SlashCommandObservability::RpcObservable
}

/// Canonicalizes RPC response JSON by removing non-deterministic fields.
pub fn canonicalize_response(mut response: Value) -> Value {
    if let Some(number) = response.as_number() {
        const MAX_EXACT_F64_INTEGER: f64 = 9_007_199_254_740_991.0;

        if let Some(float) = number.as_f64()
            && float.is_finite()
            && float.fract() == 0.0
            && float.abs() <= MAX_EXACT_F64_INTEGER
            && let Ok(integer) = format!("{float:.0}").parse::<i64>()
        {
            return json!(integer);
        }
    }

    // Remove time-sensitive fields
    if let Some(obj) = response.as_object_mut() {
        obj.retain(|key, _| !is_nondeterministic_response_key(key));
        canonicalize_fixture_model_inventory(obj);

        // Canonicalize paths to be relative.
        if let Some(path) = obj.get_mut("path")
            && let Some(path_str) = path.as_str()
            && let Ok(canonical) = std::path::Path::new(path_str).canonicalize()
            && let Some(file_name) = canonical.file_name()
        {
            *path = json!(file_name);
        }

        // Recursively canonicalize nested objects
        for value in obj.values_mut() {
            *value = canonicalize_response(value.clone());
        }
    } else if let Some(arr) = response.as_array_mut() {
        for item in arr {
            *item = canonicalize_response(item.clone());
        }
    }

    response
}

fn canonicalize_fixture_model_inventory(obj: &mut serde_json::Map<String, Value>) {
    if obj.get("command").and_then(Value::as_str) != Some("get_available_models") {
        return;
    }

    let Some(models) = obj
        .get_mut("data")
        .and_then(Value::as_object_mut)
        .and_then(|data| data.get_mut("models"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };

    models.retain(model_is_fixture_inventory_entry);
    models.sort_by_key(model_inventory_sort_key);
}

fn model_is_fixture_inventory_entry(model: &Value) -> bool {
    string_field_matches(model, "provider", RPC_TEST_PROVIDER)
        || string_field_matches(model, "providerId", RPC_TEST_PROVIDER)
        || string_field_matches(model, "provider_id", RPC_TEST_PROVIDER)
        || string_field_matches(model, "id", RPC_TEST_MODEL)
        || string_field_matches(model, "model", RPC_TEST_MODEL)
        || string_field_matches(model, "modelId", RPC_TEST_MODEL)
        || string_field_matches(model, "model_id", RPC_TEST_MODEL)
}

fn string_field_matches(value: &Value, key: &str, expected: &str) -> bool {
    value.get(key).and_then(Value::as_str) == Some(expected)
}

fn model_inventory_sort_key(model: &Value) -> String {
    let provider = model
        .get("provider")
        .or_else(|| model.get("providerId"))
        .or_else(|| model.get("provider_id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = model
        .get("id")
        .or_else(|| model.get("model"))
        .or_else(|| model.get("modelId"))
        .or_else(|| model.get("model_id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = model
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!("{provider}\0{id}\0{name}")
}

fn is_nondeterministic_response_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("timestamp")
        || matches!(
            lower.as_str(),
            "id" | "duration"
                | "runner"
                | "sessionfile"
                | "sessionid"
                | "sessionname"
                | "previoussessionfile"
                | "pendingmessagecount"
                | "persistencestatus"
                | "autoretryenabled"
                | "durabilitymode"
                | "uxeventmarkers"
        )
}

/// Test runner for differential slash command testing.
pub struct DifferentialTester {
    temp_dir: TempDir,
    pub scenarios: Vec<SlashCommandScenario>,
}

impl DifferentialTester {
    /// Create a new differential tester.
    pub fn new() -> anyhow::Result<Self> {
        let temp_dir = tempfile::tempdir()?;

        Ok(Self {
            temp_dir,
            scenarios: Self::default_scenarios(),
        })
    }

    /// Default set of slash command scenarios to test.
    #[allow(clippy::too_many_lines)]
    fn default_scenarios() -> Vec<SlashCommandScenario> {
        vec![
            SlashCommandScenario {
                name: "help_basic".to_string(),
                command: "/help".to_string(),
                description: "Basic help command should show command list".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "help_alias_h".to_string(),
                command: "/h".to_string(),
                description: "Help alias /h should work identically to /help".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "help_alias_question".to_string(),
                command: "/?".to_string(),
                description: "Help alias /? should work identically to /help".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "clear_basic".to_string(),
                command: "/clear".to_string(),
                description: "Clear command should reset conversation history".to_string(),
                supports_streaming: false,
                setup: vec!["hello world".to_string()], // Add some history first
            },
            SlashCommandScenario {
                name: "clear_alias_cls".to_string(),
                command: "/cls".to_string(),
                description: "Clear alias /cls should work identically to /clear".to_string(),
                supports_streaming: false,
                setup: vec!["test message".to_string()],
            },
            SlashCommandScenario {
                name: "model_list".to_string(),
                command: "/model".to_string(),
                description: "Model command without args should show model selector".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "model_alias_m".to_string(),
                command: "/m".to_string(),
                description: "Model alias /m should work identically to /model".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "thinking_off".to_string(),
                command: "/thinking off".to_string(),
                description: "Thinking command should set thinking level to off".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "thinking_high".to_string(),
                command: "/thinking high".to_string(),
                description: "Thinking command should set thinking level to high".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "thinking_alias_t".to_string(),
                command: "/t medium".to_string(),
                description: "Thinking alias /t should work identically to /thinking".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "exit_basic".to_string(),
                command: "/exit".to_string(),
                description: "Exit command should terminate session".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "exit_alias_quit".to_string(),
                command: "/quit".to_string(),
                description: "Exit alias /quit should work identically to /exit".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "exit_alias_q".to_string(),
                command: "/q".to_string(),
                description: "Exit alias /q should work identically to /exit".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "session_info".to_string(),
                command: "/session".to_string(),
                description: "Session command should show session information".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "session_alias_info".to_string(),
                command: "/info".to_string(),
                description: "Session alias /info should work identically to /session".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "tree_basic".to_string(),
                command: "/tree".to_string(),
                description: "Tree command should show session tree structure".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "compact_basic".to_string(),
                command: "/compact".to_string(),
                description: "Compact command should trigger conversation compaction".to_string(),
                supports_streaming: false,
                setup: vec!["message 1".to_string(), "message 2".to_string()], // Need some history
            },
            SlashCommandScenario {
                name: "theme_list".to_string(),
                command: "/theme".to_string(),
                description: "Theme command without args should list available themes".to_string(),
                supports_streaming: false,
                setup: vec![],
            },
            SlashCommandScenario {
                name: "history_basic".to_string(),
                command: "/history".to_string(),
                description: "History command should show input history".to_string(),
                supports_streaming: false,
                setup: vec!["test input".to_string()],
            },
            SlashCommandScenario {
                name: "history_alias_hist".to_string(),
                command: "/hist".to_string(),
                description: "History alias /hist should work identically to /history".to_string(),
                supports_streaming: false,
                setup: vec!["another test".to_string()],
            },
        ]
    }

    /// Add a custom scenario to the test suite.
    pub fn add_scenario(&mut self, scenario: SlashCommandScenario) {
        self.scenarios.push(scenario);
    }

    /// Run all scenarios and return results.
    pub fn run_all_scenarios(&self) -> HashMap<String, TestResult> {
        let mut results = HashMap::new();

        for scenario in &self.scenarios {
            println!("Running scenario: {}", scenario.name);
            let result = self.run_scenario_in_temp_dir(scenario);
            results.insert(scenario.name.clone(), result);
        }

        results
    }

    /// Run a single scenario.
    pub fn run_scenario(scenario: &SlashCommandScenario) -> TestResult {
        Self::run_scenario_with_workspace(scenario, None)
    }

    fn run_scenario_in_temp_dir(&self, scenario: &SlashCommandScenario) -> TestResult {
        Self::run_scenario_with_workspace(scenario, Some(self.temp_dir.path()))
    }

    fn run_scenario_with_workspace(
        scenario: &SlashCommandScenario,
        workspace_root: Option<&Path>,
    ) -> TestResult {
        match run_real_differential_scenario(scenario, workspace_root) {
            Ok(result) => result,
            Err(err) => fail_closed_result(scenario, &err.to_string()),
        }
    }
}

fn fail_closed_result(scenario: &SlashCommandScenario, reason: &str) -> TestResult {
    let detail = format!("{DIFFERENTIAL_RUNNER_UNAVAILABLE}: {reason}");
    TestResult {
        scenario_name: scenario.name.clone(),
        success: false,
        rust_response: json!({
            "status": "blocked",
            "command": scenario.command,
            "reason": detail,
        }),
        pi_mono_response: json!({
            "status": "blocked",
            "command": scenario.command,
            "reason": detail,
        }),
        differences: vec![detail],
    }
}

fn run_real_differential_scenario(
    scenario: &SlashCommandScenario,
    workspace_root: Option<&Path>,
) -> anyhow::Result<TestResult> {
    let paths = RunnerPaths::discover()?;
    let commands = scenario_rpc_commands(scenario)?;
    let workspace = workspace_root.map_or_else(std::env::temp_dir, Path::to_path_buf);

    let rust_response = run_rust_rpc_sequence(&paths, scenario, &commands, &workspace)?;
    let pi_mono_response = run_pi_mono_rpc_sequence(&paths, scenario, &commands, &workspace)?;

    let rust_canonical = canonicalize_response(rust_response);
    let pi_mono_canonical = canonicalize_response(pi_mono_response);
    let success = rust_canonical == pi_mono_canonical;
    let differences = if success {
        Vec::new()
    } else {
        vec!["canonical Rust Pi and pi-mono RPC responses differ".to_string()]
    };

    Ok(TestResult {
        scenario_name: scenario.name.clone(),
        success,
        rust_response: rust_canonical,
        pi_mono_response: pi_mono_canonical,
        differences,
    })
}

#[derive(Debug)]
struct RunnerPaths {
    rust_pi: PathBuf,
    pi_mono_root: PathBuf,
    pi_mono_tsx: PathBuf,
    pi_mono_cli: PathBuf,
}

impl RunnerPaths {
    fn discover() -> anyhow::Result<Self> {
        let rust_pi = PathBuf::from(env!("CARGO_BIN_EXE_pi"));
        if !rust_pi.is_file() {
            anyhow::bail!("missing Rust Pi test binary: {}", rust_pi.display());
        }

        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let pi_mono_root = repo.join(PI_MONO_ROOT_RELATIVE);
        if !pi_mono_root.is_dir() {
            anyhow::bail!("missing pinned pi-mono root: {}", pi_mono_root.display());
        }

        let pi_mono_tsx = pi_mono_root.join(PI_MONO_TSX_RELATIVE);
        if !pi_mono_tsx.is_file() {
            anyhow::bail!(
                "missing legacy tsx runner: {}; provision pi-mono dependencies before counting slash-command parity",
                pi_mono_tsx.display()
            );
        }

        let pi_mono_cli = pi_mono_root.join(PI_MONO_CLI_RELATIVE);
        if !pi_mono_cli.is_file() {
            anyhow::bail!("missing legacy coding-agent CLI: {}", pi_mono_cli.display());
        }

        Ok(Self {
            rust_pi,
            pi_mono_root,
            pi_mono_tsx,
            pi_mono_cli,
        })
    }
}

#[derive(Debug, Clone)]
struct RpcStep {
    label: String,
    command: Value,
}

fn scenario_rpc_commands(scenario: &SlashCommandScenario) -> anyhow::Result<Vec<RpcStep>> {
    let mut commands = Vec::new();
    for setup in &scenario.setup {
        if slash_command_observability(setup).kind == SlashCommandObservability::RpcObservable {
            commands.extend(slash_input_to_rpc_steps(setup)?);
        }
    }
    commands.extend(slash_input_to_rpc_steps(&scenario.command)?);
    Ok(commands)
}

fn slash_input_to_rpc_steps(input: &str) -> anyhow::Result<Vec<RpcStep>> {
    let trimmed = input.trim();
    let policy = slash_command_observability(trimmed);
    if policy.kind != SlashCommandObservability::RpcObservable {
        anyhow::bail!(
            "slash input is not observable through the shared RPC protocol: {trimmed}; {}",
            policy.reason
        );
    }

    let (command, args) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed, ""));
    let command = command.to_ascii_lowercase();
    let args = args.trim();

    let steps = match command.as_str() {
        "/help" | "/h" | "/?" => vec![
            rpc_step("get_state", json!({"type": "get_state"})),
            rpc_step("get_commands", json!({"type": "get_commands"})),
        ],
        "/model" | "/m" if args.is_empty() => {
            vec![rpc_step(
                "get_available_models",
                json!({"type": "get_available_models"}),
            )]
        }
        "/thinking" | "/think" | "/t" if args.is_empty() => {
            vec![rpc_step("get_state", json!({"type": "get_state"}))]
        }
        "/thinking" | "/think" | "/t" => vec![
            rpc_step(
                "set_thinking_level",
                json!({"type": "set_thinking_level", "level": args}),
            ),
            rpc_step("get_state", json!({"type": "get_state"})),
        ],
        "/session" | "/info" => vec![
            rpc_step("get_state", json!({"type": "get_state"})),
            rpc_step("get_session_stats", json!({"type": "get_session_stats"})),
        ],
        "/tree" => vec![rpc_step(
            "get_fork_messages",
            json!({"type": "get_fork_messages"}),
        )],
        "/compact" => vec![rpc_step(
            "compact",
            json!({"type": "compact", "customInstructions": args}),
        )],
        "/clear" | "/cls" => vec![rpc_step("new_session", json!({"type": "new_session"}))],
        other => {
            anyhow::bail!(
                "slash command is not observable through the shared RPC protocol: {other}"
            );
        }
    };

    Ok(steps)
}

fn rpc_step(label: &str, command: Value) -> RpcStep {
    RpcStep {
        label: label.to_string(),
        command,
    }
}

fn run_rust_rpc_sequence(
    paths: &RunnerPaths,
    scenario: &SlashCommandScenario,
    commands: &[RpcStep],
    workspace_root: &Path,
) -> anyhow::Result<Value> {
    let workspace = workspace_root.join("rust").join(&scenario.name);
    std::fs::create_dir_all(&workspace)
        .map_err(|err| anyhow::anyhow!("create {}: {err}", workspace.display()))?;

    let agent_dir = workspace.join("agent");
    let sessions_dir = workspace.join("sessions");
    let package_dir = workspace.join("packages");
    let config_path = workspace.join("settings.json");
    for dir in [&agent_dir, &sessions_dir, &package_dir] {
        std::fs::create_dir_all(dir)
            .map_err(|err| anyhow::anyhow!("create {}: {err}", dir.display()))?;
    }
    write_fixture_models_json(&agent_dir)?;

    let mut child = Command::new(&paths.rust_pi)
        .args([
            "--mode",
            "rpc",
            "--provider",
            RPC_TEST_PROVIDER,
            "--model",
            RPC_TEST_MODEL,
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
        ])
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env("PI_CONFIG_PATH", &config_path)
        .env("PI_SESSIONS_DIR", &sessions_dir)
        .env("PI_PACKAGE_DIR", &package_dir)
        .current_dir(&workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| anyhow::anyhow!("spawn Rust Pi RPC: {err}"))?;

    run_rpc_sequence("rust", &mut child, commands)
}

/// Resolve the Node.js binary portably: prefer the pinned Linux CI path,
/// fall back to Homebrew's darwin location, then to `node` on PATH.
fn node_binary() -> PathBuf {
    for candidate in [
        "/usr/bin/node",
        "/usr/local/bin/node",
        "/opt/homebrew/bin/node",
    ] {
        let path = Path::new(candidate);
        if path.is_file() {
            return path.to_path_buf();
        }
    }
    PathBuf::from("node")
}

fn run_pi_mono_rpc_sequence(
    paths: &RunnerPaths,
    scenario: &SlashCommandScenario,
    commands: &[RpcStep],
    workspace_root: &Path,
) -> anyhow::Result<Value> {
    let workspace = workspace_root.join("pi-mono").join(&scenario.name);
    std::fs::create_dir_all(&workspace)
        .map_err(|err| anyhow::anyhow!("create {}: {err}", workspace.display()))?;

    let agent_dir = workspace.join("agent");
    std::fs::create_dir_all(&agent_dir)
        .map_err(|err| anyhow::anyhow!("create {}: {err}", agent_dir.display()))?;
    write_fixture_models_json(&agent_dir)?;

    let mut child = Command::new(node_binary())
        .arg(&paths.pi_mono_tsx)
        .args(["--tsconfig", "tsconfig.json"])
        .arg(&paths.pi_mono_cli)
        .args([
            "--mode",
            "rpc",
            "--provider",
            RPC_TEST_PROVIDER,
            "--model",
            RPC_TEST_MODEL,
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
        ])
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env("TZ", "UTC")
        .current_dir(&paths.pi_mono_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| anyhow::anyhow!("spawn pi-mono RPC: {err}"))?;

    run_rpc_sequence("pi-mono", &mut child, commands)
}

fn write_fixture_models_json(agent_dir: &Path) -> anyhow::Result<()> {
    let models_path = agent_dir.join("models.json");
    let payload = json!({
        "providers": {
            RPC_TEST_PROVIDER: {
                "baseUrl": RPC_TEST_BASE_URL,
                "api": "openai-completions",
                "apiKey": RPC_TEST_API_KEY,
                "models": [
                    {
                        "id": RPC_TEST_MODEL,
                        "name": "Credential-Free Slash Differential Fixture",
                        "api": "openai-completions",
                        "reasoning": false,
                        "input": ["text"],
                        "cost": {
                            "input": 0,
                            "output": 0,
                            "cacheRead": 0,
                            "cacheWrite": 0
                        },
                        "contextWindow": 4096,
                        "maxTokens": 1024
                    }
                ]
            }
        }
    });
    let content = serde_json::to_string_pretty(&payload)?;
    std::fs::write(&models_path, content)
        .map_err(|err| anyhow::anyhow!("write {}: {err}", models_path.display()))
}

fn run_rpc_sequence(label: &str, child: &mut Child, commands: &[RpcStep]) -> anyhow::Result<Value> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("{label} RPC child stdout was not piped"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("{label} RPC child stdin was not piped"))?;

    let (line_tx, line_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut responses = Vec::new();
    for (index, step) in commands.iter().enumerate() {
        let id = format!("cmd-{index}");
        let mut command = step.command.clone();
        let object = command
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("RPC command is not an object: {}", step.label))?;
        object.insert("id".to_string(), json!(id));

        writeln!(stdin, "{command}")
            .map_err(|err| anyhow::anyhow!("write {label} RPC command {}: {err}", step.label))?;
        stdin
            .flush()
            .map_err(|err| anyhow::anyhow!("flush {label} RPC command {}: {err}", step.label))?;

        responses.push(wait_for_rpc_response(
            label,
            &line_rx,
            &id,
            &step.label,
            RPC_RESPONSE_TIMEOUT,
        )?);
    }

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();

    Ok(json!({
        "status": "ok",
        "runner": label,
        "responses": responses,
    }))
}

fn wait_for_rpc_response(
    label: &str,
    line_rx: &mpsc::Receiver<Result<String, std::io::Error>>,
    id: &str,
    step_label: &str,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out waiting for {label} RPC response to {step_label}");
        }

        let line = match line_rx.recv_timeout(remaining) {
            Ok(Ok(line)) => line,
            Ok(Err(err)) => anyhow::bail!("read {label} RPC stdout: {err}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                anyhow::bail!("timed out waiting for {label} RPC response to {step_label}");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("{label} RPC stdout disconnected before {step_label} response");
            }
        };

        let value = serde_json::from_str::<Value>(&line)
            .map_err(|err| anyhow::anyhow!("parse {label} RPC JSON line {line:?}: {err}"))?;
        if !matches!(value.get("type").and_then(Value::as_str), Some("response")) {
            continue;
        }
        if value.get("id").and_then(Value::as_str) == Some(id) {
            return Ok(value);
        }
    }
}

/// Result of running a differential test scenario.
#[derive(Debug)]
pub struct TestResult {
    pub scenario_name: String,
    pub success: bool,
    pub rust_response: Value,
    pub pi_mono_response: Value,
    pub differences: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonicalize_response() {
        let response = json!({
            "status": "success",
            "timestamp": "2024-04-22T10:30:00Z",
            "id": "req-123",
            "command": "get_available_models",
            "runner": "rust",
            "path": "/tmp/session-abc",
            "data": {
                "nested_timestamp": "2024-04-22T10:30:01Z",
                "sessionFile": "/tmp/pi-session.jsonl",
                "autoRetryEnabled": true,
                "durabilityMode": "fsync",
                "pendingMessageCount": 3,
                "persistenceStatus": {
                    "event": "session.persistence.backlog"
                },
                "uxEventMarkers": [
                    {
                        "event": "session.persistence.backlog"
                    }
                ],
                "value": 42.0,
                "models": [
                    {
                        "provider": "openai",
                        "id": "gpt-4o"
                    },
                    {
                        "provider": RPC_TEST_PROVIDER,
                        "id": RPC_TEST_MODEL,
                        "name": "Credential-Free Slash Differential Fixture",
                        "cost": {
                            "input": 0.0,
                            "output": 0.0
                        }
                    }
                ]
            }
        });

        let canonicalized = canonicalize_response(response);

        // Timestamps and IDs should be removed
        assert!(canonicalized.get("timestamp").is_none());
        assert!(canonicalized.get("id").is_none());
        assert!(canonicalized.get("runner").is_none());
        assert!(canonicalized["data"].get("nested_timestamp").is_none());
        assert!(canonicalized["data"].get("sessionFile").is_none());
        assert!(canonicalized["data"].get("autoRetryEnabled").is_none());
        assert!(canonicalized["data"].get("durabilityMode").is_none());
        assert!(canonicalized["data"].get("pendingMessageCount").is_none());
        assert!(canonicalized["data"].get("persistenceStatus").is_none());
        assert!(canonicalized["data"].get("uxEventMarkers").is_none());

        // Other fields should be preserved
        assert_eq!(canonicalized["status"], "success");
        assert_eq!(canonicalized["data"]["value"], 42);
        assert_eq!(canonicalized["data"]["models"].as_array().unwrap().len(), 1);
        assert_eq!(
            canonicalized["data"]["models"][0]["provider"],
            RPC_TEST_PROVIDER
        );
        assert_eq!(canonicalized["data"]["models"][0]["cost"]["input"], 0);
    }

    #[test]
    fn test_scenario_creation() {
        let tester = DifferentialTester::new().unwrap();
        assert!(tester.temp_dir.path().is_dir());
        assert!(!tester.scenarios.is_empty());

        // Verify we have basic commands covered
        let scenario_names: Vec<&String> = tester.scenarios.iter().map(|s| &s.name).collect();
        assert!(scenario_names.iter().any(|name| name.contains("help")));
        assert!(scenario_names.iter().any(|name| name.contains("clear")));
        assert!(scenario_names.iter().any(|name| name.contains("model")));
        assert!(scenario_names.iter().any(|name| name.contains("exit")));
        assert!(
            tester
                .scenarios
                .iter()
                .all(|scenario| !scenario.description.is_empty())
        );
        assert!(
            tester
                .scenarios
                .iter()
                .all(|scenario| scenario.setup.iter().all(|entry| !entry.is_empty()))
        );
        assert!(
            tester
                .scenarios
                .iter()
                .all(|scenario| !scenario.supports_streaming)
        );
    }

    #[test]
    fn test_scenario_run_fails_closed_until_runner_is_available() {
        let tester = DifferentialTester::new().unwrap();

        if let Some(scenario) = tester.scenarios.first() {
            let result = DifferentialTester::run_scenario(scenario);
            assert_eq!(result.scenario_name, scenario.name);
            if result.success {
                assert_eq!(result.rust_response, result.pi_mono_response);
                assert_ne!(result.rust_response["status"], "blocked");
                assert!(result.differences.is_empty());
            } else {
                assert!(
                    !result.differences.is_empty(),
                    "failed differential scenario should explain the gap"
                );
            }
        }
    }

    #[test]
    fn rpc_mapping_covers_supported_slash_commands() {
        for command in [
            "/help",
            "/h",
            "/?",
            "/model",
            "/m",
            "/thinking off",
            "/thinking high",
            "/t medium",
            "/session",
            "/info",
            "/tree",
            "/clear",
            "/cls",
        ] {
            let steps = slash_input_to_rpc_steps(command).expect("supported slash command");
            assert!(!steps.is_empty(), "expected RPC steps for {command}");
        }
    }

    #[test]
    fn rpc_mapping_rejects_non_rpc_observable_commands() {
        let err = slash_input_to_rpc_steps("/exit").expect_err("exit is not RPC-observable");
        assert!(err.to_string().contains("not observable"));

        let err =
            slash_input_to_rpc_steps("/thinking invalid_level").expect_err("invalid thinking");
        assert!(err.to_string().contains("not credential-free"));

        let err = slash_input_to_rpc_steps("/compact notes").expect_err("compact needs a model");
        assert!(err.to_string().contains("not credential-free"));

        let err = slash_input_to_rpc_steps("plain prompt").expect_err("plain prompt needs model");
        assert!(err.to_string().contains("not credential-free"));
    }
}
