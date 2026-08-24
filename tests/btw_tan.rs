#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
#[cfg(unix)]
use common::harness::MockHttpResponse;
use common::logging::validate_jsonl_v2_only;
use pi::btw::BTW_SYSTEM_PROMPT;
use pi::subagents::TanCompletion;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    assert!(harness.write_jsonl_logs(&path).is_ok(), "write JSONL logs");
    let payload = std::fs::read_to_string(&path).unwrap_or_default();
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn test_btw_ephemeral_prompt_and_isolation() {
    let harness = TestHarness::new("btw_ephemeral_isolation");

    // Verify /btw system prompt enforces no-tools and no-followups
    assert!(BTW_SYSTEM_PROMPT.contains("NEVER use tools"));
    assert!(BTW_SYSTEM_PROMPT.contains("NEVER ask follow-up questions"));

    // Verify /btw format with empty context
    let q = "What is the capital of France?";
    assert!(!q.is_empty());

    finish_case(&harness, "btw_ephemeral_isolation");
}

#[test]
fn test_tan_child_configuration_and_completion_formatting() {
    let harness = TestHarness::new("tan_child_lifecycle");

    let completion = TanCompletion {
        schema: "pi.background-tan.result.v1",
        hub_id: Some("tan-agent-1".to_string()),
        task: "update changelog".to_string(),
        status: "completed".to_string(),
        output: "Updated CHANGELOG.md with recent release notes.".to_string(),
        error: None,
        is_error: false,
    };

    let card_text = completion.card_text();
    assert!(card_text.starts_with("(/tan completed)"));
    assert!(card_text.contains("Updated CHANGELOG.md"));
    assert!(completion.follow_up_text().contains("settled: completed"));

    let failed_completion = TanCompletion {
        schema: "pi.background-tan.result.v1",
        hub_id: Some("tan-agent-2".to_string()),
        task: "broken task".to_string(),
        status: "failed".to_string(),
        output: String::new(),
        error: Some("Execution timed out".to_string()),
        is_error: true,
    };
    let fail_card = failed_completion.card_text();
    assert!(fail_card.starts_with("(/tan failed)"));
    assert!(fail_card.contains("Execution timed out"));

    finish_case(&harness, "tan_child_lifecycle");
}

#[cfg(unix)]
fn openai_sse(text: &str) -> MockHttpResponse {
    let delta = serde_json::json!({
        "choices": [{"index": 0, "delta": {"content": text}}]
    });
    let done = serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    let body = format!("data: {delta}\n\ndata: {done}\n\ndata: [DONE]\n\n");
    MockHttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: body.into_bytes(),
    }
}

#[cfg(unix)]
fn collect_jsonl_files(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(canonical_root) = root.canonicalize() else {
        return;
    };
    collect_jsonl_files_under(&canonical_root, &canonical_root, output);
}

#[cfg(unix)]
fn collect_jsonl_files_under(root: &Path, current: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let Ok(path) = entry.path().canonicalize() else {
            continue;
        };
        if !path.starts_with(root) {
            continue;
        }
        if file_type.is_dir() {
            collect_jsonl_files_under(root, &path, output);
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            output.push(path);
        }
    }
}

#[test]
#[cfg(unix)]
#[allow(clippy::too_many_lines)]
fn e2e_tan_runs_in_background_and_delivers_at_next_turn_boundary() {
    let mut session = common::tmux::TuiSession::new("e2e_tan_background_delivery")
        .expect("tmux is required for the /tan interactive E2E test");
    let server = session.harness.start_mock_http_server();
    server.add_route(
        "POST",
        "/tan-role/v1/chat/completions",
        openai_sse("tan child summary marker"),
    );
    server.add_route_queue(
        "POST",
        "/parent/v1/chat/completions",
        vec![
            openai_sse("parent main turn marker"),
            openai_sse("parent processed tan follow-up marker"),
        ],
    );

    let env_root = session.harness.temp_path("tan-env");
    let coding_dir = env_root.join("agent");
    let sessions_dir = env_root.join("sessions");
    let packages_dir = env_root.join("packages");
    std::fs::create_dir_all(&coding_dir).expect("create coding dir");
    std::fs::create_dir_all(&sessions_dir).expect("create sessions dir");
    std::fs::create_dir_all(&packages_dir).expect("create packages dir");

    let models = serde_json::json!({
        "providers": {
            "parent": {
                "api": "openai-completions",
                "baseUrl": format!("{}/parent/v1", server.base_url()),
                "apiKey": "test-key",
                "models": [{"id": "parent-model", "contextWindow": 128000}]
            },
            "tan-role": {
                "api": "openai-completions",
                "baseUrl": format!("{}/tan-role/v1", server.base_url()),
                "apiKey": "test-key",
                "models": [{"id": "task-model", "contextWindow": 128000}]
            }
        }
    });
    std::fs::write(
        coding_dir.join("models.json"),
        serde_json::to_vec_pretty(&models).expect("serialize models"),
    )
    .expect("write models");
    let settings_path = env_root.join("settings.json");
    std::fs::write(
        &settings_path,
        r#"{"modelRoles":{"task":"tan-role/task-model"},"checkForUpdates":false,"approval":{"mode":"yolo"}}"#,
    )
    .expect("write settings");

    session.set_env("PI_CODING_AGENT_DIR", &coding_dir.display().to_string());
    session.set_env("PI_CONFIG_PATH", &settings_path.display().to_string());
    session.set_env("PI_SESSIONS_DIR", &sessions_dir.display().to_string());
    session.set_env("PI_PACKAGE_DIR", &packages_dir.display().to_string());
    session.set_env("PI_NO_AUTO_UPDATE_CHECK", "1");
    session.launch(&[
        "--provider",
        "parent",
        "--model",
        "parent-model",
        "--tools",
        "subagent",
        "--no-skills",
        "--no-prompt-templates",
        "--no-extensions",
        "--no-themes",
        "--thinking",
        "off",
        "--system-prompt",
        "tan e2e parent",
    ]);

    let startup = session.wait_and_capture("startup", "Welcome to Pi!", Duration::from_secs(20));
    assert!(
        startup.contains("Welcome to Pi!"),
        "TUI did not start: {startup}"
    );
    let started = session.send_text_and_wait(
        "start_tan",
        "/tan update the changelog",
        "(/tan started)",
        Duration::from_secs(20),
    );
    assert!(started.contains("update the changelog"));
    let completed =
        session.wait_and_capture("tan_completed", "(/tan completed)", Duration::from_secs(60));
    assert!(completed.contains("tan child summary marker"));

    let main_turn = session.send_text_and_wait(
        "main_turn_continues",
        "continue main work",
        "parent main turn marker",
        Duration::from_secs(30),
    );
    assert!(main_turn.contains("parent main turn marker"));
    let follow_up = session.wait_and_capture(
        "tan_follow_up_boundary",
        "parent processed tan follow-up marker",
        Duration::from_secs(30),
    );
    assert!(follow_up.contains("parent processed tan follow-up marker"));

    let requests = server.requests();
    let role_requests = requests
        .iter()
        .filter(|request| request.path == "/tan-role/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(role_requests.len(), 1, "expected one tan child request");
    let role_body = role_requests
        .first()
        .map(|request| String::from_utf8_lossy(&request.body))
        .unwrap_or_default();
    assert!(role_body.contains("Task: update the changelog"));
    let parent_requests = requests
        .iter()
        .filter(|request| request.path == "/parent/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(
        parent_requests.len(),
        2,
        "expected main + tan follow-up turns"
    );
    let follow_up_body = parent_requests
        .get(1)
        .map(|request| String::from_utf8_lossy(&request.body))
        .unwrap_or_default();
    assert!(follow_up_body.contains("[background tan"));
    assert!(follow_up_body.contains("tan child summary marker"));

    session.exit_gracefully();
    session.write_artifacts();
    let mut session_files = Vec::new();
    collect_jsonl_files(&sessions_dir, &mut session_files);
    assert!(
        !session_files.is_empty(),
        "interactive session JSONL was not written"
    );
    let session_payload = session_files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect::<Vec<_>>()
        .join("\n");
    for line in session_payload
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_ok(),
            "valid session JSONL line"
        );
    }
    assert!(session_payload.contains("[background tan"));
    assert!(session_payload.contains("tan child summary marker"));
    finish_case(&session.harness, "e2e_tan_background_delivery");
}
