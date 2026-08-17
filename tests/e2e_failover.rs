//! E2E (bd-cv653.3.2): cross-model failover, auth-error refusal, and
//! credential rotation over real processes against a mock OpenAI-compatible
//! server. No network beyond loopback; structured JSONL logs per
//! tests/common/logging.rs.
//!
//! Case 1: primary 429s until the retry budget is spent → the fallback-chain
//!         entry completes the turn (print mode).
//! Case 2: primary 401s → loud error, the fallback entry is NEVER called.
//! Case 3: OPENAI_API_KEYS=k1,k2 with a 429 on k1 → the retry carries k2.

mod common;

use common::TestHarness;
use common::harness::MockHttpResponse;
use common::logging::validate_jsonl_v2_only;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn sse_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

fn error_response(status: u16, body: &str) -> MockHttpResponse {
    MockHttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: body.as_bytes().to_vec(),
    }
}

fn text_sse_body(text: &str) -> String {
    [
        format!(r#"data: {{"choices":[{{"index":0,"delta":{{"content":"{text}"}}}}]}}"#).as_str(),
        "",
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

/// OpenAI Responses-API SSE: output_text delta + response.completed.
fn responses_sse_body(text: &str) -> String {
    [
        format!(
            r#"data: {{"type":"response.output_text.delta","item_id":"msg_1","content_index":0,"delta":"{text}"}}"#
        )
        .as_str(),
        "",
        r#"data: {"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
        "",
    ]
    .join("\n")
}

struct PiEnv {
    root: std::path::PathBuf,
}

impl PiEnv {
    fn new(harness: &TestHarness) -> Self {
        let root = harness.temp_path("pi-env");
        std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
        std::fs::create_dir_all(root.join("home")).expect("mkdir home");
        std::fs::write(
            root.join("settings.json"),
            r#"{"retry": {"enabled": true, "maxRetries": 1, "fallbackChains": {"default": ["e2ebackup/backup-model"]}}, "checkForUpdates": false}"#,
        )
        .expect("write settings.json");
        Self { root }
    }

    fn write_models(&self, base_url: &str) {
        let models_json = format!(
            r#"{{"providers": {{
                "e2eprimary": {{
                    "api": "openai-completions",
                    "baseUrl": "{base_url}/primary/v1",
                    "apiKey": "primary-key",
                    "models": [{{"id": "primary-model", "contextWindow": 128000}}]
                }},
                "e2ebackup": {{
                    "api": "openai-completions",
                    "baseUrl": "{base_url}/backup/v1",
                    "apiKey": "backup-key",
                    "models": [{{"id": "backup-model", "contextWindow": 128000}}]
                }}
            }}}}"#
        );
        std::fs::write(self.root.join("agent/models.json"), models_json)
            .expect("write models.json");
    }

    fn command(&self, binary: &std::path::Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env("HOME", self.root.join("home"))
            .env("PI_CODING_AGENT_DIR", self.root.join("agent"))
            .env("PI_CONFIG_PATH", self.root.join("settings.json"))
            .env("PI_SESSIONS_DIR", self.root.join("sessions"))
            .env("PI_PACKAGE_DIR", self.root.join("packages"))
            .env("PI_NO_AUTO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_API_KEY",
            "XAI_API_KEY",
            "OPENROUTER_API_KEY",
            "DEEPSEEK_API_KEY",
        ] {
            command.env_remove(key);
        }
        command
    }
}

fn run_and_collect(mut child: std::process::Child, deadline_secs: u64) -> (String, String) {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().expect("collect output");
                return (
                    String::from_utf8_lossy(&output.stdout).to_string(),
                    String::from_utf8_lossy(&output.stderr).to_string(),
                );
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(deadline_secs) {
                    let _ = child.kill();
                    let output = child.wait_with_output().expect("collect output");
                    return (
                        String::from_utf8_lossy(&output.stdout).to_string(),
                        format!(
                            "TIMEOUT: killed after {deadline_secs}s\n{}",
                            String::from_utf8_lossy(&output.stderr)
                        ),
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => panic!("wait failed: {err}"),
        }
    }
}

#[test]
fn e2e_failover_429_walks_chain_and_completes() {
    let harness = TestHarness::new("e2e_failover_429_walks_chain_and_completes");
    harness
        .log()
        .info("setup", "primary 429s, backup serves text");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        sse_response(text_sse_body("backup ok")),
    );

    let env = PiEnv::new(&harness);
    env.write_models(&server.base_url());
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    command.args([
        "--print",
        "--no-session",
        "--provider",
        "e2eprimary",
        "--model",
        "primary-model",
        "ping",
    ]);
    harness
        .log()
        .info("action", "spawning pi --print on 429 primary");
    let child = command.spawn().expect("spawn pi");
    let (stdout, stderr) = run_and_collect(child, 90);
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push(("stdout".to_string(), stdout.clone()));
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(400).collect(),
        ));
    });

    assert!(
        stdout.contains("backup ok"),
        "failover should complete on the backup model; stdout: {stdout}\nstderr: {stderr}"
    );
    let backup_requests = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/backup/v1/chat/completions")
        .count();
    assert!(
        backup_requests >= 1,
        "backup provider must receive the continuation request"
    );
    assert!(
        server
            .requests()
            .iter()
            .filter(|r| r.path == "/primary/v1/chat/completions")
            .count()
            >= 2,
        "primary must exhaust the same-model retry budget first"
    );
    let path = harness.temp_path("e2e_failover_429.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_429.jsonl", &path);
}

#[test]
fn e2e_failover_401_never_fails_over() {
    let harness = TestHarness::new("e2e_failover_401_never_fails_over");
    harness.log().info("setup", "primary 401, backup ready");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/primary/v1/chat/completions",
        error_response(
            401,
            r#"{"error":{"type":"authentication_error","message":"invalid api key"}}"#,
        ),
    );
    server.add_route(
        "POST",
        "/backup/v1/chat/completions",
        sse_response(text_sse_body("backup ok")),
    );

    let env = PiEnv::new(&harness);
    env.write_models(&server.base_url());
    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = env.command(&binary);
    command.args([
        "--print",
        "--no-session",
        "--provider",
        "e2eprimary",
        "--model",
        "primary-model",
        "ping",
    ]);
    let child = command.spawn().expect("spawn pi");
    let (_stdout, stderr) = run_and_collect(child, 60);
    harness.log().info_ctx("verify", "process finished", |ctx| {
        ctx.push((
            "stderr_tail".to_string(),
            stderr.chars().take(400).collect(),
        ));
    });

    let backup_requests = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/backup/v1/chat/completions")
        .count();
    assert_eq!(
        backup_requests, 0,
        "auth errors must never fail over — backup must not be called"
    );
    let path = harness.temp_path("e2e_failover_401.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_failover_401.jsonl", &path);
}

#[test]
fn e2e_credential_rotation_swaps_key_on_429() {
    let harness = TestHarness::new("e2e_credential_rotation_swaps_key_on_429");
    harness
        .log()
        .info("setup", "openai override → mock, OPENAI_API_KEYS=k1,k2");
    let server = harness.start_mock_http_server();
    // Every chat call 429s once then succeeds: the FIRST key sees 429s, the
    // rotated key sees success. Use a queue: two 429s, then text. (The
    // built-in openai provider uses the Responses API for gpt-4o here.)
    server.add_route_queue(
        "POST",
        "/v1/responses",
        vec![
            error_response(
                429,
                r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
            ),
            error_response(
                429,
                r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
            ),
            sse_response(responses_sse_body("rotated ok")),
        ],
    );

    let root = harness.temp_path("pi-env-rotate");
    std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
    std::fs::create_dir_all(root.join("home")).expect("mkdir home");
    // Override the built-in openai provider's base_url at the mock so the
    // canonical OPENAI_API_KEYS plural var applies.
    std::fs::write(
        root.join("agent/models.json"),
        format!(
            r#"{{"providers": {{"openai": {{"baseUrl": "{}/v1"}}}}}}"#,
            server.base_url()
        ),
    )
    .expect("write models.json");
    std::fs::write(
        root.join("settings.json"),
        r#"{"retry": {"enabled": true, "maxRetries": 2}, "checkForUpdates": false}"#,
    )
    .expect("write settings.json");

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = Command::new(binary);
    command
        .args([
            "--print",
            "--no-session",
            "--provider",
            "openai",
            "--model",
            "gpt-4o",
            "ping",
        ])
        .env("HOME", root.join("home"))
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("PI_CONFIG_PATH", root.join("settings.json"))
        .env("PI_SESSIONS_DIR", root.join("sessions"))
        .env("PI_PACKAGE_DIR", root.join("packages"))
        .env("PI_NO_AUTO_UPDATE_CHECK", "1")
        .env("OPENAI_API_KEYS", "k-aaa,k-bbb")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("spawn pi");
    let (stdout, stderr) = run_and_collect(child, 90);

    let auth_headers: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/responses")
        .map(|r| {
            r.headers
                .iter()
                .rev()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        })
        .collect();
    harness
        .log()
        .info_ctx("verify", "auth headers observed", |ctx| {
            ctx.push(("auth_headers".to_string(), auth_headers.join(" | ")));
            ctx.push(("stdout".to_string(), stdout.clone()));
            ctx.push((
                "stderr_tail".to_string(),
                stderr.chars().take(300).collect(),
            ));
        });

    assert!(
        stdout.contains("rotated ok"),
        "rotation should complete; stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        auth_headers.len() >= 2,
        "expected at least two attempts, got {auth_headers:?}"
    );
    assert_eq!(
        auth_headers[0],
        format!("Bearer {}", auth_headers[0].trim_start_matches("Bearer ")),
        "first request carries a bearer header"
    );
    let distinct: std::collections::HashSet<_> = auth_headers.iter().collect();
    assert!(
        distinct.len() >= 2 || stdout.contains("rotated ok"),
        "a 429 on the first key must rotate to the sibling key (headers: {auth_headers:?})"
    );
    assert!(
        auth_headers.iter().any(|h| h == "Bearer k-bbb"),
        "after the 429 on k-aaa, a retry must carry k-bbb: {auth_headers:?}"
    );

    let path = harness.temp_path("e2e_rotation_429.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_rotation_429.jsonl", &path);
}

#[test]
fn e2e_path_scope_pins_repo_model_set() {
    let harness = TestHarness::new("e2e_path_scope_pins_repo_model_set");
    harness
        .log()
        .info("setup", "scope override pins repo A; repo B uses global");
    let server = harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/scoped/v1/chat/completions",
        sse_response(text_sse_body("scoped ok")),
    );
    server.add_route(
        "POST",
        "/global/v1/chat/completions",
        sse_response(text_sse_body("global ok")),
    );

    let root = harness.temp_path("pi-env-scope");
    let repo_a = harness.temp_path("repo-a");
    let repo_b = harness.temp_path("repo-b");
    std::fs::create_dir_all(root.join("agent")).expect("mkdir agent");
    std::fs::create_dir_all(root.join("home")).expect("mkdir home");
    std::fs::create_dir_all(&repo_a).expect("mkdir repo a");
    std::fs::create_dir_all(&repo_b).expect("mkdir repo b");

    std::fs::write(
        root.join("agent/models.json"),
        format!(
            r#"{{"providers": {{
                "e2escoped": {{
                    "api": "openai-completions",
                    "baseUrl": "{}/scoped/v1",
                    "apiKey": "test-key",
                    "models": [{{"id": "scoped-model", "contextWindow": 128000}}]
                }},
                "e2eglobal": {{
                    "api": "openai-completions",
                    "baseUrl": "{}/global/v1",
                    "apiKey": "test-key",
                    "models": [{{"id": "global-model", "contextWindow": 128000}}]
                }}
            }}}}"#,
            server.base_url(),
            server.base_url()
        ),
    )
    .expect("write models.json");

    let settings = format!(
        r#"{{"enabledModels": ["e2eglobal/global-model"],
           "modelScopeOverrides": [{{"path": "{}", "enabledModels": ["e2escoped/scoped-model"]}}],
           "checkForUpdates": false}}"#,
        repo_a.display()
    );
    std::fs::write(root.join("settings.json"), settings).expect("write settings.json");

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let run_in = |cwd: &std::path::Path| {
        let mut command = Command::new(&binary);
        command
            .args(["--print", "--no-session", "ping"])
            .current_dir(cwd)
            .env("HOME", root.join("home"))
            .env("PI_CODING_AGENT_DIR", root.join("agent"))
            .env("PI_CONFIG_PATH", root.join("settings.json"))
            .env("PI_SESSIONS_DIR", root.join("sessions"))
            .env("PI_PACKAGE_DIR", root.join("packages"))
            .env("PI_NO_AUTO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_API_KEY",
            "XAI_API_KEY",
            "OPENROUTER_API_KEY",
            "DEEPSEEK_API_KEY",
        ] {
            command.env_remove(key);
        }
        let child = command.spawn().expect("spawn pi");
        run_and_collect(child, 90)
    };

    let (stdout_a, stderr_a) = run_in(&repo_a);
    let (stdout_b, stderr_b) = run_in(&repo_b);
    harness
        .log()
        .info_ctx("verify", "both runs finished", |ctx| {
            ctx.push(("stdout_a".to_string(), stdout_a.clone()));
            ctx.push(("stdout_b".to_string(), stdout_b.clone()));
            ctx.push((
                "stderr_a_tail".to_string(),
                stderr_a.chars().take(300).collect(),
            ));
            ctx.push((
                "stderr_b_tail".to_string(),
                stderr_b.chars().take(300).collect(),
            ));
        });

    assert!(
        stdout_a.contains("scoped ok"),
        "repo A must run the scoped model; stdout: {stdout_a}\nstderr: {stderr_a}"
    );
    assert!(
        stdout_b.contains("global ok"),
        "repo B must run the global default; stdout: {stdout_b}\nstderr: {stderr_b}"
    );

    let path = harness.temp_path("e2e_path_scope.jsonl");
    harness.write_jsonl_logs(&path).expect("write logs");
    let errors = validate_jsonl_v2_only(&std::fs::read_to_string(&path).expect("read logs"));
    assert!(errors.is_empty(), "JSONL violations: {errors:?}");
    harness.record_artifact("e2e_path_scope.jsonl", &path);
}
