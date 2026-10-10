//! E2E (bd-cv653.3.1 acceptance #2): subagent fan-out routes to the task/smol
//! role model end-to-end over real processes.
//!
//! Flow: a mock HTTP server fronts two OpenAI-compatible providers on distinct
//! path prefixes (`/default/v1` parent, `/role/v1` child). The parent `pi`
//! process runs the `e2edefault` provider; its scripted first response calls
//! the `subagent` tool on a `scout` agent whose definition pins NO model.
//! Settings assign `modelRoles.task = "e2erole/role-model"`. The child must
//! then reach the `/role/v1` prefix with model `role-model` in the request
//! body — proving parent → child role routing through the real binary
//! boundary (parent spawns the actual `pi` binary with `--model <role spec>`).
//!
//! No network beyond loopback; structured JSONL logs per tests/common/logging.rs.

mod common;

use common::TestHarness;
use common::harness::MockHttpResponse;
use common::logging::validate_jsonl_v2_only;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `OpenAI` `chat-completions` SSE: assistant message that calls the `subagent` tool.
fn tool_call_sse_body() -> String {
    [
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"subagent","arguments":"{\"agent\":\"scout\",\"task\":\"reply briefly\"}"}}]}}]}"#,
        "",
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

/// `OpenAI` `chat-completions` SSE: plain final text.
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

fn sse_response(body: String) -> MockHttpResponse {
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_subagent_child_uses_task_role_model() {
    let harness = TestHarness::new("e2e_subagent_child_uses_task_role_model");
    harness
        .log()
        .info("setup", "mock server + isolated pi env with two providers");

    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/default/v1/chat/completions",
        vec![
            sse_response(tool_call_sse_body()),
            sse_response(text_sse_body("parent done")),
        ],
    );
    server.add_route(
        "POST",
        "/role/v1/chat/completions",
        sse_response(text_sse_body("child ok")),
    );

    // Isolated environment (same isolation discipline as tests/e2e_cli.rs).
    let env_root = harness.temp_path("pi-env");
    std::fs::create_dir_all(env_root.join("agent/agents")).expect("mkdir agents");
    let home = env_root.join("home");
    std::fs::create_dir_all(&home).expect("mkdir home");

    // scout agent definition: deliberately NO model pin — the task role must win.
    std::fs::write(
        env_root.join("agent/agents/scout.md"),
        "---\nname: scout\ndescription: test scout\ntools: read\n---\nYou are a test scout.\n",
    )
    .expect("write scout agent");

    // Two OpenAI-compatible providers on distinct path prefixes.
    let models_json = format!(
        r#"{{"providers": {{
            "e2edefault": {{
                "api": "openai-completions",
                "baseUrl": "{}/default/v1",
                "apiKey": "test-key",
                "models": [{{"id": "parent-model", "contextWindow": 128000}}]
            }},
            "e2erole": {{
                "api": "openai-completions",
                "baseUrl": "{}/role/v1",
                "apiKey": "test-key",
                "models": [{{"id": "role-model", "contextWindow": 128000}}]
            }}
        }}}}"#,
        server.base_url(),
        server.base_url()
    );
    std::fs::write(env_root.join("agent/models.json"), models_json).expect("write models.json");

    // Task role assignment (bd-cv653.3.1).
    std::fs::write(
        env_root.join("settings.json"),
        r#"{"modelRoles": {"task": "e2erole/role-model"}, "checkForUpdates": false, "approval": {"mode": "yolo"}}"#,
    )
    .expect("write settings.json");

    harness
        .log()
        .info_ctx("action", "spawning parent pi", |ctx| {
            ctx.push((
                "settings".to_string(),
                env_root.join("settings.json").display().to_string(),
            ));
        });

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    let mut command = Command::new(binary);
    command
        .args([
            "--print",
            "--no-session",
            "--provider",
            "e2edefault",
            "--model",
            "parent-model",
            "--tools",
            "subagent",
        ])
        .arg("run the scout")
        .env("HOME", &home)
        .env("PI_CODING_AGENT_DIR", env_root.join("agent"))
        .env("PI_CONFIG_PATH", env_root.join("settings.json"))
        .env("PI_SESSIONS_DIR", env_root.join("sessions"))
        .env("PI_PACKAGE_DIR", env_root.join("packages"))
        .env("PI_NO_AUTO_UPDATE_CHECK", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in [
        "OPENAI_API_KEY",
        "GOOGLE_API_KEY",
        "XAI_API_KEY",
        "OPENROUTER_API_KEY",
        "DEEPSEEK_API_KEY",
    ] {
        command.env_remove(key);
    }

    let mut child = command.spawn().expect("spawn pi");

    // Poll the mock for the child request, bounded at 90s.
    let deadline = Instant::now() + Duration::from_secs(90);
    let (mut saw_parent, mut saw_child) = (false, false);
    let mut child_body_model = String::new();
    while Instant::now() < deadline && !(saw_parent && saw_child) {
        for request in server.requests() {
            if request.path == "/default/v1/chat/completions" {
                saw_parent = true;
            }
            if request.path == "/role/v1/chat/completions" {
                saw_child = true;
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&request.body) {
                    child_body_model = value["model"].as_str().unwrap_or_default().to_string();
                }
            }
        }
        if saw_parent && saw_child {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    harness
        .log()
        .info_ctx("verify", "requests observed", |ctx| {
            ctx.push(("saw_parent".to_string(), saw_parent.to_string()));
            ctx.push(("saw_child".to_string(), saw_child.to_string()));
            ctx.push(("child_body_model".to_string(), child_body_model.clone()));
        });

    let _ = child.kill();
    let output = child.wait_with_output().expect("wait");

    assert!(saw_parent, "parent never reached /default/v1 prefix");
    assert!(
        saw_child,
        "subagent child never reached /role/v1 prefix — task role routing failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        child_body_model, "role-model",
        "child request must carry the task-role model id"
    );

    let path = harness.temp_path("e2e_subagent_child_uses_task_role_model.jsonl");
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL schema violations: {errors:?}");
    harness.record_artifact("e2e_subagent_child_uses_task_role_model.jsonl", &path);
    harness.log().info("done", "case assertions passed");
}

#[test]
#[allow(clippy::too_many_lines)] // Exercise CLI selection through the real child/provider boundary.
fn e2e_unpinned_subagent_inherits_cli_model_instead_of_configured_defaults() {
    let harness = TestHarness::new(
        "e2e_unpinned_subagent_inherits_cli_model_instead_of_configured_defaults",
    );
    let server = harness.start_mock_http_server();
    server.add_route_queue(
        "POST",
        "/selected/v1/chat/completions",
        vec![
            sse_response(tool_call_sse_body()),
            sse_response(text_sse_body("child used selected model")),
            sse_response(text_sse_body("parent done")),
        ],
    );
    server.add_route(
        "POST",
        "/configured/v1/chat/completions",
        sse_response(text_sse_body("unexpected configured default")),
    );
    let root = harness.temp_path("pi-env");
    let agent_dir = root.join("agent");
    let home = root.join("home");
    let workspace = root.join("workspace");
    for directory in [agent_dir.join("agents"), home.clone(), workspace.clone()] {
        std::fs::create_dir_all(directory).expect("create isolated environment");
    }
    std::fs::write(
        agent_dir.join("agents/scout.md"),
        "---\nname: scout\ndescription: unpinned scout\ntools: read\n---\nYou are the child scout.\n",
    )
    .expect("write unpinned agent");
    let model_id = "vendor/selected-model:literal";
    let models = serde_json::json!({"providers":{
        "e2eselected":{
            "api":"openai-completions",
            "baseUrl":format!("{}/selected/v1", server.base_url()),
            "apiKey":"test-key",
            "models":[{"id":model_id,"contextWindow":128000}]
        },
        "e2econfigured":{
            "api":"openai-completions",
            "baseUrl":format!("{}/configured/v1", server.base_url()),
            "apiKey":"test-key",
            "models":[{"id":"configured-model","contextWindow":128000}]
        }
    }});
    std::fs::write(agent_dir.join("models.json"), models.to_string())
        .expect("write model registry");
    std::fs::write(
        root.join("settings.json"),
        serde_json::json!({
            "defaultProvider":"e2econfigured","defaultModel":"configured-model",
            "checkForUpdates":false,"approval":{"mode":"yolo"}
        })
        .to_string(),
    )
    .expect("write settings without task or smol roles");

    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command
        .args([
            "--mode",
            "json",
            "--print",
            "--no-session",
            "--provider",
            "e2eselected",
            "--model",
            model_id,
            "--tools",
            "subagent",
        ])
        .arg("run the scout")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("PI_CODING_AGENT_DIR", &agent_dir)
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
        "PI_SUBAGENT_PI_BINARY",
        "PI_SUBAGENT_DEPTH",
        "PI_SUBAGENT_PARENT_PID",
        "PI_SUBAGENT_RUN_ID",
        "PI_SUBAGENT_DEADLINE_UNIX_MS",
        "PI_SUBAGENT_TIMEOUT_SECS",
    ] {
        command.env_remove(key);
    }
    let mut child = command.spawn().expect("spawn native parent");
    let deadline = Instant::now() + Duration::from_secs(90);
    let finished = loop {
        if child.try_wait().expect("poll native parent").is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = child
        .wait_with_output()
        .expect("collect native parent output");
    assert!(
        finished && output.status.success(),
        "native delegation failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.requests();
    assert!(
        !requests
            .iter()
            .any(|request| request.path == "/configured/v1/chat/completions"),
        "an unpinned child reverted to the configured default provider"
    );
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .filter(|request| request.path == "/selected/v1/chat/completions")
        .map(|request| serde_json::from_slice(&request.body).expect("provider request JSON"))
        .collect();
    assert_eq!(bodies.len(), 3, "parent, child, and parent follow-up requests");
    assert!(bodies.iter().all(|body| body["model"] == model_id));
    assert!(
        bodies.iter().any(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "user"
                        && message["content"].to_string().contains("Task: reply briefly")
                })
            })
        }),
        "the selected provider must actually receive the child's assignment"
    );
}

#[cfg(unix)]
fn hub_tool_call_sse(id: &str, tool: &str, arguments: serde_json::Value) -> String {
    let call = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{
        "index":0,"id":id,"type":"function",
        "function":{"name":tool,"arguments":arguments.to_string()}
    }]}}]});
    let done = serde_json::json!({
        "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    });
    format!("data: {call}\n\ndata: {done}\n\ndata: [DONE]\n\n")
}

#[cfg(unix)]
fn hub_provider_tool_texts(
    requests: &[common::harness::MockHttpRequest],
    path: &str,
) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request.path == path)
        .flat_map(|request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("provider request JSON");
            body["messages"]
                .as_array()
                .expect("provider messages")
                .iter()
                .filter(|message| message["role"] == "tool")
                .map(|message| message["content"].as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_native_children_exchange_hub_messages_through_the_parent() {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;

    let harness = TestHarness::new("e2e_native_children_exchange_hub_messages_through_the_parent");
    let server = harness.start_mock_http_server();
    let call = |id: &str, tool: &str, arguments| {
        sse_response(hub_tool_call_sse(id, tool, arguments))
    };
    server.add_route_queue(
        "POST",
        "/parent/v1/chat/completions",
        vec![
            call("delegate", "subagent", serde_json::json!({
                "tasks":[
                    {"agent":"receiver","task":"Wait for the sender, then read your inbox."},
                    {"agent":"sender","task":"Exchange a message and report back."}
                ],
                "concurrency":2
            })),
            call("parent-inbox", "hub", serde_json::json!({
                "op":"agent","action":"inbox","name":"parent"
            })),
            sse_response(text_sse_body("parent received the child report")),
        ],
    );
    server.add_route_queue(
        "POST",
        "/receiver/v1/chat/completions",
        vec![
            // A filesystem barrier keeps the receiver alive until the send
            // has been acknowledged. It does not carry the actual message.
            call("await-peer", "bash", serde_json::json!({
                "command":"i=0; while [ ! -f hub-exchange-ready ]; do i=$((i + 1)); [ \"$i\" -lt 1500 ] || exit 71; sleep 0.02; done"
            })),
            call("receiver-inbox", "hub", serde_json::json!({
                "op":"agent","action":"inbox"
            })),
            sse_response(text_sse_body("receiver checked its inbox")),
        ],
    );
    server.add_route_queue(
        "POST",
        "/sender/v1/chat/completions",
        vec![
            call("peer-roster", "hub", serde_json::json!({
                "op":"agent","action":"roster"
            })),
            call("spoof-parent", "hub", serde_json::json!({
                "op":"agent","action":"send","name":"receiver-1",
                "from":"parent","text":"forged-parent-message"
            })),
            call("send-peer", "hub", serde_json::json!({
                "op":"agent","action":"send","name":"receiver-1",
                "text":"native-peer-message"
            })),
            call("release-peer", "bash", serde_json::json!({
                "command":"printf ready > hub-exchange-ready"
            })),
            call("report-parent", "hub", serde_json::json!({
                "op":"agent","action":"send","name":"parent",
                "text":"native-parent-report"
            })),
            call("foreign-inbox", "hub", serde_json::json!({
                "op":"agent","action":"inbox","name":"receiver-1"
            })),
            call("forbidden-control", "hub", serde_json::json!({
                "op":"agent","action":"kill","name":"receiver-1"
            })),
            call("forbidden-revive", "hub", serde_json::json!({
                "op":"agent","action":"revive","name":"receiver-1"
            })),
            sse_response(text_sse_body("sender completed the exchange")),
        ],
    );

    let root = harness.temp_path("pi-env");
    let agent_dir = root.join("agent");
    let home = root.join("home");
    let workspace = root.join("workspace");
    for directory in [agent_dir.join("agents"), home.clone(), workspace.clone()] {
        std::fs::create_dir_all(directory).expect("create isolated hub environment");
    }
    for name in ["receiver", "sender"] {
        std::fs::write(
            agent_dir.join(format!("agents/{name}.md")),
            format!(
                "---\nname: {name}\ndescription: native hub test\ntools: hub, bash\nmodel: e2e{name}/{name}-model\n---\nFollow the assigned exchange.\n"
            ),
        )
        .expect("write messaging agent");
    }
    let mut providers = serde_json::Map::new();
    for name in ["parent", "receiver", "sender"] {
        providers.insert(format!("e2e{name}"), serde_json::json!({
            "api":"openai-completions",
            "baseUrl":format!("{}/{name}/v1", server.base_url()),
            "apiKey":"test-key",
            "models":[{"id":format!("{name}-model"),"contextWindow":128000}]
        }));
    }
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::json!({"providers":providers}).to_string(),
    )
    .expect("write hub model registry");
    std::fs::write(
        root.join("settings.json"),
        serde_json::json!({
            "checkForUpdates":false,"approval":{"mode":"yolo"}
        })
        .to_string(),
    )
    .expect("write hub settings");

    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command
        .args([
            "--print", "--no-session",
            "--provider", "e2eparent", "--model", "parent-model",
            "--tools", "subagent,hub",
        ])
        .arg("Run the two messaging agents and read their parent report.")
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env("PI_CONFIG_PATH", root.join("settings.json"))
        .env("PI_SESSIONS_DIR", root.join("sessions"))
        .env("PI_PACKAGE_DIR", root.join("packages"))
        .env("PI_NO_AUTO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for key in [
        "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GOOGLE_API_KEY",
        "XAI_API_KEY", "OPENROUTER_API_KEY", "DEEPSEEK_API_KEY",
        "PI_SUBAGENT_PI_BINARY", "PI_SUBAGENT_DEPTH", "PI_SUBAGENT_PARENT_PID",
        "PI_SUBAGENT_RUN_ID", "PI_SUBAGENT_STEER_FILE",
        "PI_SUBAGENT_DEADLINE_UNIX_MS", "PI_SUBAGENT_TIMEOUT_SECS",
        "PI_SUBAGENT_HUB_ADDRESS", "PI_SUBAGENT_HUB_TOKEN",
    ] {
        command.env_remove(key);
    }
    let mut child = command.spawn().expect("spawn native messaging parent");
    let stdout = child.stdout.take().expect("capture parent stdout");
    let stderr = child.stderr.take().expect("capture parent stderr");
    let read_output = |reader: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            reader
                .take(2 * 1024 * 1024)
                .read_to_end(&mut bytes)
                .expect("read bounded parent output");
            bytes
        })
    };
    let stdout = read_output(Box::new(stdout));
    let stderr = read_output(Box::new(stderr));
    let deadline = Instant::now() + Duration::from_secs(90);
    let (finished, status) = loop {
        if let Some(status) = child.try_wait().expect("poll messaging parent") {
            break (true, status);
        }
        if Instant::now() >= deadline {
            pi::tools::kill_process_group_tree(Some(child.id()));
            break (false, child.wait().expect("reap timed-out messaging parent"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    while !stdout.is_finished() || !stderr.is_finished() {
        if Instant::now() >= deadline {
            pi::tools::kill_process_group_tree(Some(child.id()));
            panic!("messaging descendants did not close the parent output pipes before the deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let stdout = stdout.join().expect("join stdout reader");
    let stderr = stderr.join().expect("join stderr reader");
    assert!(
        finished && status.success(),
        "native hub exchange failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    );

    let requests = server.requests();
    let sender = hub_provider_tool_texts(&requests, "/sender/v1/chat/completions");
    assert!(
        sender.iter().any(|text| {
            text.contains("You are sender-2.") && text.contains("receiver-1")
        }),
        "the native sender must see its parent-owned roster: {sender:?}",
    );
    for refusal in ["PI_HUB_IPC_SENDER", "PI_HUB_IPC_INBOX", "PI_HUB_IPC_ACTION"] {
        assert!(
            sender.iter().any(|text| text.contains(refusal)),
            "missing {refusal} refusal: {sender:?}",
        );
    }
    assert!(
        sender.iter().any(|text| text.contains("Message queued for receiver-1")),
        "the peer send must be acknowledged by the parent: {sender:?}",
    );
    for call_id in ["forbidden-control", "forbidden-revive"] {
        assert!(
            requests.iter().any(|request| {
                if request.path != "/sender/v1/chat/completions" {
                    return false;
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("sender provider request JSON");
                body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "tool"
                            && message["tool_call_id"] == call_id
                            && message["content"]
                                .as_str()
                                .is_some_and(|text| text.contains("PI_HUB_IPC_ACTION"))
                    })
                })
            }),
            "{call_id} must be refused by the inherited channel",
        );
    }
    let receiver = hub_provider_tool_texts(&requests, "/receiver/v1/chat/completions");
    assert!(
        receiver.iter().any(|text| {
            text.contains("from sender-2: native-peer-message")
        }),
        "the native receiver must read the authenticated sender's message: {receiver:?}",
    );
    assert!(
        !receiver.iter().any(|text| text.contains("forged-parent-message")),
        "sender impersonation must not reach the recipient",
    );
    let parent = hub_provider_tool_texts(&requests, "/parent/v1/chat/completions");
    assert!(
        parent.iter().any(|text| text.contains("from sender-2: native-parent-report")),
        "the parent must receive a message through its inbox: {parent:?}",
    );
}
