//! Integration tests for the project memory bank.
//!
//! Exercises real SQLite/FTS operations and the public reflection tool through
//! a Gemini provider and loopback HTTP/SSE, including terminal failures.

#![recursion_limit = "256"]

mod common;

use clap::Parser;
use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::provider::StreamOptions;
use pi::tools::{Tool, ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn first_text(output: &ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|block| match block {
            pi::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

fn assert_irreversible_redaction(text: &str) {
    assert!(text.contains("<pi-secret:redacted>"));
    assert!(
        !regex::Regex::new(r"<pi-secret:[0-9a-f]{6}>")
            .unwrap()
            .is_match(text),
        "auxiliary context must not carry a restorable vault id"
    );
}

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL v2 validation errors: {errors:?}");
}

fn block_on_local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

fn project_dir(harness: &TestHarness, name: &str) -> std::path::PathBuf {
    let dir = harness.temp_path(name);
    std::fs::create_dir_all(&dir).expect("project dir");
    dir
}

fn memory_config(backend: &str) -> pi::config::Config {
    pi::config::Config {
        memory: Some(pi::config::MemorySettings {
            backend: Some(backend.to_string()),
        }),
        ..Default::default()
    }
}

struct CapturedRequest {
    headers: HashMap<String, String>,
    body: Value,
}

struct ReflectionServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<Option<CapturedRequest>>>,
}

impl ReflectionServer {
    fn start(status: u16, body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind reflection server");
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let join = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut socket = loop {
                if stopped.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    return None;
                }
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("reflection accept: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            let header_end = loop {
                if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break index + 4;
                }
                assert!(bytes.len() < 64 * 1024, "bounded headers");
                let read = socket.read(&mut chunk).expect("read reflection headers");
                assert!(read > 0, "request closed before headers");
                bytes.extend_from_slice(&chunk[..read]);
            };
            let headers: HashMap<String, String> = String::from_utf8_lossy(&bytes[..header_end])
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_string()))
                .collect();
            let length: usize = headers["content-length"].parse().unwrap();
            assert!(length <= 1024 * 1024, "bounded fixture body");
            while bytes.len() - header_end < length {
                let read = socket.read(&mut chunk).expect("read reflection body");
                assert!(read > 0, "request closed before body");
                bytes.extend_from_slice(&chunk[..read]);
            }
            let request = CapturedRequest {
                headers,
                body: serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
            };
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .expect("write reflection response");
            Some(request)
        });
        Self {
            base_url,
            stop,
            join: Some(join),
        }
    }

    fn finish(mut self) -> CapturedRequest {
        self.stop.store(true, Ordering::Relaxed);
        self.join
            .take()
            .unwrap()
            .join()
            .expect("server thread")
            .expect("captured request")
    }

    fn assert_no_request(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        assert!(
            self.join
                .take()
                .unwrap()
                .join()
                .expect("server thread")
                .is_none(),
            "privacy refusal must precede reflection provider admission"
        );
    }
}

impl Drop for ReflectionServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn gemini_body(answer: &str, finish: Option<&str>) -> String {
    let mut body = format!(
        "data: {}\n\n",
        json!({
            "candidates": [{"content": {"parts": [{"text": answer}]}}]
        })
    );
    if let Some(finish) = finish {
        use std::fmt::Write as _;
        let _ = write!(
            body,
            "data: {}\n\n",
            json!({
                "candidates": [{"finishReason": finish}]
            })
        );
    }
    body
}

fn reflection_tool(
    store: Arc<pi::memory::MemoryStore>,
    server: &ReflectionServer,
) -> pi::memory::ReflectTool {
    let provider = pi::providers::gemini::GeminiProvider::new("reflection-test")
        .with_base_url(&server.base_url);
    pi::memory::ReflectTool::with_provider_and_options(
        store,
        Arc::new(provider),
        StreamOptions {
            api_key: Some("reflection-fixture-key".to_string()),
            headers: HashMap::from([(
                "x-session-binding".to_string(),
                "fixture-session".to_string(),
            )]),
            max_tokens: Some(2048),
            ..StreamOptions::default()
        },
    )
}

#[test]
fn retain_tool_redacts_secrets() {
    let case = "retain_tool_redacts_secrets";
    let harness = TestHarness::new(case);
    let root = project_dir(&harness, "proj");
    let store = Arc::new(pi::memory::MemoryStore::open(&root).expect("open"));
    let tool = pi::memory::RetainTool::new(store);
    let out = block_on_local(tool.execute(
        "call-1",
        json!({"content": "my api key = sk-abcdefghijklmnopqrstuvwxyz", "kind": "fact"}),
        None,
    ))
    .expect("execute");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("retain output: {text}"));
    assert!(text.contains("secret redacted"), "{text}");
    assert!(!text.contains("sk-abcdef"), "{text}");
    let details = out.details.as_ref().expect("details");
    let stored = details["content"].as_str().expect("stored content");
    assert!(stored.contains("[REDACTED_OPENAI_KEY]"), "{stored}");
    assert!(!stored.contains("sk-abcdef"), "{stored}");
    finish_case(&harness, case);
}

#[test]
fn configured_registry_screens_all_project_memory_writes_and_startup_reads() {
    let case = "configured_registry_screens_all_project_memory_writes_and_startup_reads";
    let harness = TestHarness::new(case);
    let root = project_dir(&harness, "proj");
    let mut config = memory_config("local");
    config.secrets = Some(pi::secrets::SecretsSettings {
        mode: Some("off".to_string()),
        extra_patterns: Some(vec![r"ACME-\d{6}".to_string()]),
    });
    // An older bank or an earlier policy can contain newly protected text.
    // The host must also thread the current policy into reads and startup.
    let historical = pi::memory::MemoryStore::open(&root).unwrap();
    historical
        .retain(
            pi::memory::MemoryKind::Fact,
            "legacy parser guidance ACME-777777",
            &[],
            None,
        )
        .unwrap();
    let registry = ToolRegistry::new(&["read"], &root, Some(&config));
    let run = |name: &str, input: Value| {
        let tool = registry
            .tools()
            .iter()
            .find(|tool| tool.name() == name)
            .expect("configured memory tool");
        let output = block_on_local(tool.execute("configured-memory", input, None)).unwrap();
        assert!(!output.is_error, "memory operation must succeed");
        output
    };
    let retained = run(
        "retain",
        json!({"content":"parser initially uses ACME-123456", "tags":["ACME-654321"]}),
    );
    let details = retained.details.unwrap();
    assert_eq!(
        details["content"],
        "parser initially uses [REDACTED_USER_PATTERN]"
    );
    assert_eq!(details["tags"][0], "[REDACTED_USER_PATTERN]");
    let id = details["id"].as_i64().unwrap();
    run(
        "memory_edit",
        json!({"id":id, "op":"update", "content":"parser now uses ACME-222222"}),
    );
    let learned = run(
        "learn",
        json!({"lesson":"parser lessons use ACME-333333", "context":"ACME-444444"}),
    );
    let learned = serde_json::to_string(&learned.details).unwrap();
    assert!(!learned.contains("ACME-333333"));
    assert!(!learned.contains("ACME-444444"));
    assert!(learned.contains("[REDACTED_USER_PATTERN]"));

    // Open without the policy to verify screening occurred before storage,
    // rather than merely hiding unscreened writes at the output boundary.
    let reopened = pi::memory::MemoryStore::open(&root).unwrap();
    let edited = reopened
        .list(10)
        .unwrap()
        .into_iter()
        .find(|memory| memory.id == id)
        .unwrap();
    assert_eq!(edited.content, "parser now uses [REDACTED_USER_PATTERN]");
    let recalled = run("recall", json!({"query":"parser"}));
    let recalled = serde_json::to_string(&recalled.details).unwrap();
    let prompt = build_prompt_for_test(&root, &config);
    for visible in [&recalled, &prompt] {
        for secret in [
            "ACME-123456",
            "ACME-222222",
            "ACME-333333",
            "ACME-444444",
            "ACME-654321",
            "ACME-777777",
        ] {
            assert!(
                !visible.contains(secret),
                "configured pattern escaped screening"
            );
        }
        assert!(visible.contains("[REDACTED_USER_PATTERN]"));
        assert!(visible.contains("legacy parser guidance"));
    }
    for query in ["ACME-777777", "ACME-888888"] {
        let output = run("recall", json!({"query": query}));
        let details = output.details.as_ref().unwrap();
        assert_eq!(details["query"], "[REDACTED_USER_PATTERN]");
        assert!(!serde_json::to_string(details).unwrap().contains(query));
        assert!(!first_text(&output).contains(query));
    }
    finish_case(&harness, case);
}

#[test]
fn backend_gate_controls_tool_presence() {
    let case = "backend_gate_controls_tool_presence";
    let harness = TestHarness::new(case);
    let root = project_dir(&harness, "proj");
    let local = ToolRegistry::new(&["read"], &root, Some(&memory_config("local")));
    let local_names: Vec<&str> = local.tools().iter().map(|tool| tool.name()).collect();
    harness
        .log()
        .info("verify", format!("local tools: {local_names:?}"));
    for expected in ["retain", "recall", "reflect", "memory_edit"] {
        assert!(
            local_names.contains(&expected),
            "backend=local must expose {expected}: {local_names:?}"
        );
    }
    let off = ToolRegistry::new(&["read"], &root, Some(&memory_config("off")));
    let off_names: Vec<&str> = off.tools().iter().map(|tool| tool.name()).collect();
    for absent in ["retain", "recall", "reflect", "memory_edit"] {
        assert!(
            !off_names.contains(&absent),
            "backend=off must hide {absent}: {off_names:?}"
        );
    }
    let default = ToolRegistry::new(&["read"], &root, None::<&pi::config::Config>);
    let default_names: Vec<&str> = default.tools().iter().map(|tool| tool.name()).collect();
    assert!(
        !default_names.contains(&"retain"),
        "default posture must be off: {default_names:?}"
    );
    finish_case(&harness, case);
}

#[test]
fn reflect_cites_memory_ids_through_provider_http() {
    let case = "reflect_cites_memory_ids_through_provider_http";
    let harness = TestHarness::new(case);
    let root = project_dir(&harness, "proj");
    let store = Arc::new(pi::memory::MemoryStore::open(&root).expect("open"));
    let memory = store
        .retain(
            pi::memory::MemoryKind::Lesson,
            "always run cargo check before committing",
            &[],
            None,
        )
        .expect("retain");
    let other = store
        .retain(
            pi::memory::MemoryKind::Lesson,
            "run tests before committing",
            &[],
            None,
        )
        .expect("retain another source");
    let server = ReflectionServer::start(
        200,
        gemini_body(
            &format!("Run cargo check first [{}].", memory.id),
            Some("STOP"),
        ),
    );
    let tool = reflection_tool(store, &server);
    let out = block_on_local(tool.execute(
        "call-1",
        json!({"question": "what should run before committing?"}),
        None,
    ))
    .expect("execute");
    assert!(!out.is_error);
    assert!(first_text(&out).contains(&format!("[{}]", memory.id)));
    let details = out.details.as_ref().expect("details");
    assert_eq!(details["citations"], json!([memory.id]));
    let sources = details["sourceMemoryIds"].as_array().unwrap();
    assert!(sources.contains(&json!(memory.id)));
    assert!(sources.contains(&json!(other.id)));
    assert_eq!(details["provider"], "google");
    let request = server.finish();
    assert_eq!(request.headers["x-goog-api-key"], "reflection-fixture-key");
    assert_eq!(request.headers["x-session-binding"], "fixture-session");
    assert_eq!(request.body["generationConfig"]["maxOutputTokens"], 2048);
    let prompt = request.body["contents"][0]["parts"][0]["text"]
        .as_str()
        .unwrap();
    assert!(prompt.contains(&format!("- [{}]", memory.id)));
    assert!(prompt.contains(&format!("- [{}]", other.id)));
    assert!(request.body.get("tools").is_none());
    finish_case(&harness, case);
}

#[test]
fn reflect_screens_question_and_memories_before_http_without_changing_stored_facts() {
    const OPAQUE: &str = "opaqueReflectionCredential12345";
    let harness = TestHarness::new("reflect_privacy_http");
    let root = project_dir(&harness, "proj");
    let store = Arc::new(pi::memory::MemoryStore::open(&root).unwrap());
    let stored = store
        .retain(
            pi::memory::MemoryKind::Fact,
            &format!("parser uses {OPAQUE}"),
            &["ACME-123456".to_string()],
            None,
        )
        .unwrap();
    let settings = pi::secrets::SecretsSettings {
        // Secondary calls retain their irreversible privacy floor even when
        // reversible obfuscation is off for the main conversation.
        mode: Some("off".to_string()),
        extra_patterns: Some(vec![r"^ACME-\d{6}$".to_string()]),
    };
    let server = ReflectionServer::start(
        200,
        gemini_body(
            &format!("The parser uses the configured value [{}].", stored.id),
            Some("STOP"),
        ),
    );
    let tool = reflection_tool(Arc::clone(&store), &server).with_secrets_settings(Some(&settings));
    let output = block_on_local(tool.execute(
        "reflection-privacy",
        json!({"question": format!("parser api_key={OPAQUE}")}),
        None,
    ))
    .expect("screened reflection");
    let request = server.finish();
    let body = serde_json::to_string(&request.body).unwrap();
    assert!(!body.contains(OPAQUE));
    assert_irreversible_redaction(&body);
    assert_eq!(request.headers["x-goog-api-key"], "reflection-fixture-key");
    let details = serde_json::to_string(&output.details).unwrap();
    assert!(!details.contains(OPAQUE));
    assert!(!details.contains("ACME-123456"));
    assert_irreversible_redaction(&details);
    let original = store.recall("parser", None).unwrap();
    assert_eq!(original[0].content, stored.content);
    assert_eq!(original[0].tags, stored.tags);
}

#[test]
fn reflect_block_mode_rejects_secret_source_metadata_before_http() {
    let harness = TestHarness::new("reflect_privacy_block_http");
    let root = project_dir(&harness, "proj");
    let store = Arc::new(pi::memory::MemoryStore::open(&root).unwrap());
    store
        .retain(
            pi::memory::MemoryKind::Fact,
            "parser is incremental",
            &["ACME-123456".to_string()],
            None,
        )
        .unwrap();
    let server = ReflectionServer::start(200, gemini_body("must not be requested", Some("STOP")));
    let tool = reflection_tool(store, &server).with_secrets_settings(Some(
        &pi::secrets::SecretsSettings {
            mode: Some("block".to_string()),
            extra_patterns: Some(vec![r"^ACME-\d{6}$".to_string()]),
        },
    ));
    let error =
        block_on_local(tool.execute("reflection-block", json!({"question": "parser?"}), None))
            .expect_err("configured block mode must refuse source secrets");
    assert!(error.to_string().contains("PI_SECRET_BLOCK"), "{error}");
    assert!(!error.to_string().contains("ACME-123456"));
    server.assert_no_request();
}

struct ReflectionDriver {
    via_xdev: bool,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl pi::provider::Provider for ReflectionDriver {
    fn name(&self) -> &str {
        "reflection-driver"
    }

    fn api(&self) -> &str {
        "reflection-driver"
    }

    fn model_id(&self) -> &str {
        "reflection-driver"
    }

    async fn stream(
        &self,
        context: &pi::provider::Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = pi::error::Result<pi::model::StreamEvent>> + Send>,
        >,
    > {
        use pi::model::{
            AssistantMessage, ContentBlock, StopReason, StreamEvent, TextContent, ToolCall,
        };
        let turn = self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(turn < 2, "unexpected extra primary request");
        let mut message = AssistantMessage {
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            ..AssistantMessage::default()
        };
        if turn == 0 {
            let payload = serde_json::to_string(context.messages.as_ref()).unwrap();
            let start = payload.find("<pi-secret:").expect("live vault placeholder");
            let end = start + payload[start..].find('>').unwrap() + 1;
            let arguments = json!({"question": format!("parser {}", &payload[start..end])});
            message.stop_reason = StopReason::ToolUse;
            message.content = vec![ContentBlock::ToolCall(ToolCall {
                id: "reflect-live-vault".to_string(),
                name: if self.via_xdev { "xdev" } else { "reflect" }.to_string(),
                arguments: if self.via_xdev {
                    json!({"action":"run", "name":"reflect", "args":arguments})
                } else {
                    arguments
                },
                thought_signature: None,
            })];
        } else {
            message.content = vec![ContentBlock::Text(TextContent::new("reflection complete"))];
        }
        Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
            reason: message.stop_reason,
            message,
        })])))
    }
}

#[test]
fn direct_and_xdev_reflection_use_the_current_agents_vault_and_patterns() {
    const OPAQUE: &str = "opaqueLiveSessionCredential12345";
    let harness = TestHarness::new("reflect_live_agent_privacy");
    for via_xdev in [false, true] {
        let root = project_dir(&harness, if via_xdev { "xdev" } else { "direct" });
        let store = Arc::new(pi::memory::MemoryStore::open(&root).unwrap());
        let stored = store
            .retain(
                pi::memory::MemoryKind::Fact,
                &format!("parser uses {OPAQUE} and ACME-123456"),
                &[],
                None,
            )
            .unwrap();
        let server = ReflectionServer::start(
            200,
            gemini_body(
                &format!("Use the parser setting [{}].", stored.id),
                Some("STOP"),
            ),
        );
        let mut registry = ToolRegistry::new(&["xdev"], &root, Some(&memory_config("local")));
        assert!(registry.is_discoverable("reflect"));
        registry.push(Box::new(reflection_tool(Arc::clone(&store), &server)));
        if !via_xdev {
            registry.mark_promoted("reflect");
        }
        let provider = Arc::new(ReflectionDriver {
            via_xdev,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let config = pi::agent::AgentConfig {
            secrets: Some(pi::secrets::SecretsSettings {
                mode: Some("obfuscate".to_string()),
                extra_patterns: Some(vec![r"ACME-\d{6}".to_string()]),
            }),
            ..pi::agent::AgentConfig::default()
        };
        let mut agent = pi::agent::Agent::new(provider.clone(), registry, config);
        let tool_errors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&tool_errors);
        let answer = block_on_local(agent.run(
            format!("api_key={OPAQUE}\nReflect on parser"),
            move |event| {
                if let pi::agent::AgentEvent::ToolExecutionEnd { result, .. } = event {
                    captured.lock().unwrap().push(result);
                }
            },
        ))
        .expect("real reflection tool turn");
        assert_eq!(answer.stop_reason, pi::model::StopReason::Stop);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        let outputs = tool_errors.lock().unwrap();
        assert_eq!(outputs.len(), 1);
        assert!(!outputs[0].is_error, "{:?}", outputs[0]);
        let request = server.finish();
        let body = serde_json::to_string(&request.body).unwrap();
        assert!(
            !body.contains(OPAQUE),
            "live secret leaked via xdev={via_xdev}"
        );
        assert!(!body.contains("ACME-123456"));
        assert_irreversible_redaction(&body);
        assert_eq!(
            store.recall("parser", None).unwrap()[0].content,
            stored.content
        );
    }
}

#[test]
fn reflect_rejects_truncated_failed_and_invented_citation_responses() {
    let harness = TestHarness::new("reflect_terminal_errors");
    let root = project_dir(&harness, "proj");
    let store = Arc::new(pi::memory::MemoryStore::open(&root).unwrap());
    let memory = store
        .retain(
            pi::memory::MemoryKind::Fact,
            "parser is incremental",
            &[],
            None,
        )
        .unwrap();
    for (body, expected) in [
        (gemini_body("partial", None), "without Done event"),
        (
            gemini_body("blocked", Some("SAFETY")),
            "did not finish cleanly",
        ),
        (
            gemini_body("truncated", Some("MAX_TOKENS")),
            "did not finish cleanly",
        ),
        (
            gemini_body(&format!("invented [{}]", memory.id + 1), Some("STOP")),
            "not supplied",
        ),
    ] {
        let server = ReflectionServer::start(200, body);
        let tool = reflection_tool(Arc::clone(&store), &server);
        let error = block_on_local(tool.execute("call-1", json!({"question": "parser?"}), None))
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        server.finish();
    }
}

#[test]
fn reflect_redacts_credentials_in_http_failures() {
    let harness = TestHarness::new("reflect_redacted_http_error");
    let root = project_dir(&harness, "proj");
    let store = Arc::new(pi::memory::MemoryStore::open(&root).unwrap());
    store
        .retain(
            pi::memory::MemoryKind::Fact,
            "parser is incremental",
            &[],
            None,
        )
        .unwrap();
    let server = ReflectionServer::start(500, "upstream echoed reflection-fixture-key".to_string());
    let tool = reflection_tool(store, &server);
    let error =
        block_on_local(tool.execute("call-1", json!({"question": "parser?"}), None)).unwrap_err();
    assert!(!error.to_string().contains("reflection-fixture-key"));
    assert!(error.to_string().contains("REDACTED"));
    server.finish();
}

#[test]
fn reflect_validates_input_and_skips_provider_resolution_without_sources() {
    let harness = TestHarness::new("reflect_empty_bank");
    let root = project_dir(&harness, "proj");
    let store = Arc::new(pi::memory::MemoryStore::open(&root).unwrap());
    let tool = pi::memory::ReflectTool::new(store);
    assert!(block_on_local(tool.execute("call-1", json!({"question": "   "}), None)).is_err());
    assert!(
        block_on_local(tool.execute("call-1", json!({"question": "x".repeat(8193)}), None))
            .is_err()
    );
    let output =
        block_on_local(tool.execute("call-1", json!({"question": "unknown parser"}), None))
            .unwrap();
    assert!(!output.is_error);
    assert_eq!(output.details.unwrap()["citations"], json!([]));
}

#[test]
fn cross_instance_persistence_and_tombstones() {
    let case = "cross_instance_persistence_and_tombstones";
    let harness = TestHarness::new(case);
    let root = project_dir(&harness, "proj");
    let (kept_id, tomb_id) = {
        let store = pi::memory::MemoryStore::open(&root).expect("open A");
        let kept = store
            .retain(
                pi::memory::MemoryKind::Fact,
                "the agent loop lives in src/agent.rs",
                &[],
                None,
            )
            .expect("retain kept");
        let tomb = store
            .retain(
                pi::memory::MemoryKind::Fact,
                "temporary scaffolding note",
                &[],
                None,
            )
            .expect("retain tomb");
        store
            .edit(tomb.id, pi::memory::MemoryEditOp::Invalidate, None)
            .expect("invalidate");
        (kept.id, tomb.id)
    };
    let store_b = pi::memory::MemoryStore::open(&root).expect("open B");
    let hits = store_b.recall("agent loop", None).expect("recall");
    assert!(
        hits.iter().any(|hit| hit.id == kept_id),
        "session B must recall session A's fact: {hits:?}"
    );
    let tomb_hits = store_b.recall("scaffolding", None).expect("tomb recall");
    assert!(
        tomb_hits.iter().all(|hit| hit.id != tomb_id),
        "tombstone must be excluded: {tomb_hits:?}"
    );
    store_b
        .edit(tomb_id, pi::memory::MemoryEditOp::Forget, None)
        .expect("forget");
    let listed = store_b.list(50).expect("list");
    assert!(
        listed.iter().all(|hit| hit.id != tomb_id),
        "forget must hard-delete: {listed:?}"
    );
    finish_case(&harness, case);
}

#[test]
fn startup_injection_includes_mental_model_when_local() {
    let case = "startup_injection_includes_mental_model_when_local";
    let harness = TestHarness::new(case);
    let root = project_dir(&harness, "proj");
    let store = pi::memory::MemoryStore::open(&root).expect("open");
    store
        .retain(
            pi::memory::MemoryKind::Decision,
            "chose fsqlite over rusqlite for the store",
            &[],
            None,
        )
        .expect("retain");
    let prompt = build_prompt_for_test(&root, &memory_config("local"));
    harness.log().info(
        "verify",
        format!(
            "prompt contains memory block: {}",
            prompt.contains("Project Memory")
        ),
    );
    assert!(
        prompt.contains("Project Memory"),
        "backend=local must inject the mental model"
    );
    assert!(
        prompt.contains("fsqlite over rusqlite"),
        "mental model must carry the retained decision"
    );
    let off_prompt = build_prompt_for_test(&root, &memory_config("off"));
    assert!(
        !off_prompt.contains("Project Memory"),
        "backend=off must not inject"
    );
    finish_case(&harness, case);
}

fn build_prompt_for_test(cwd: &Path, config: &pi::config::Config) -> String {
    let cli = pi::cli::Cli::parse_from(["pi"]);
    pi::app::build_system_prompt(
        &cli,
        cwd,
        &["read"],
        None,
        &pi::config::Config::global_dir(),
        cwd,
        false,
        true,
        None,
        config,
    )
    .expect("build prompt")
}
