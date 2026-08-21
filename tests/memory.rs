//! Integration tests for the memory bank (bd-cv653.4.1).
//!
//! Acceptance coverage:
//! 1. Retain through the tool surface redacts secrets (acceptance #3).
//! 2. `memory.backend: local` exposes the four tools; off → absent (#5).
//! 3. `reflect` answers with the stub provider and cites memory ids (#4).
//! 4. Cross-instance persistence through the store (#1, tool-level).
//! 5. forget hard-deletes; invalidate tombstones excluded from recall (#2).
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use clap::Parser;
use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::model::StreamEvent;
use pi::provider::{Context, StreamOptions};
use pi::tools::{Tool, ToolOutput, ToolRegistry};
use serde_json::json;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

/// Memory tests share the per-project store dir under the harness temp
/// root; each test uses a unique project dir, so no global lock is needed.
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

/// Canned provider: answers with a fixed text citing the ids it saw in the
/// prompt (acceptance #4 needs citation ids in the answer).
struct StubProvider;

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl pi::provider::Provider for StubProvider {
    fn name(&self) -> &str {
        "stub"
    }

    fn api(&self) -> &str {
        "stub-api"
    }

    fn model_id(&self) -> &str {
        "stub-model"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        // Cite the memory ids that appeared in the prompt (Debug rendering
        // escapes newlines, so extract with a regex over the raw text).
        let prompt = context
            .messages
            .first()
            .map(|message| format!("{message:?}")) // ubs:ignore stub provider
            .unwrap_or_default();
        let corpus_re = regex::Regex::new(r"- \[(\d+)\]").expect("corpus regex"); // ubs:ignore static test regex
        let mut corpus_ids: Vec<String> = corpus_re
            .captures_iter(&prompt)
            .map(|capture| capture[1].to_string()) // ubs:ignore regex capture group 1 always present
            .collect();
        corpus_ids.sort();
        corpus_ids.dedup();
        let ids = corpus_ids
            .iter()
            .map(|id| format!("[{id}]")) // ubs:ignore stub formatting
            .collect::<Vec<_>>()
            .join(" ");
        let answer = format!("The answer, grounded in memories: {ids} — cargo check first."); // ubs:ignore stub
        Ok(Box::pin(futures::stream::iter(vec![Ok(
            StreamEvent::TextDelta {
                content_index: 0,
                delta: answer,
            },
        )])))
    }
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
    let stored = details["content"].as_str().expect("stored content"); // ubs:ignore test fixture
    assert!(stored.contains("[REDACTED_OPENAI_KEY]"), "{stored}");
    assert!(!stored.contains("sk-abcdef"), "{stored}");
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
    harness
        .log()
        .info("verify", format!("off tools: {off_names:?}"));
    for absent in ["retain", "recall", "reflect", "memory_edit"] {
        assert!(
            !off_names.contains(&absent),
            "backend=off must hide {absent}: {off_names:?}"
        );
    }

    // Default config (no memory section) is off too.
    let default = ToolRegistry::new(&["read"], &root, None::<&pi::config::Config>);
    let default_names: Vec<&str> = default.tools().iter().map(|tool| tool.name()).collect();
    assert!(
        !default_names.contains(&"retain"),
        "default posture must be off: {default_names:?}"
    );
    finish_case(&harness, case);
}

#[test]
fn reflect_cites_memory_ids_with_stub_provider() {
    let case = "reflect_cites_memory_ids_with_stub_provider";
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

    let tool = pi::memory::ReflectTool::with_provider(store, Arc::new(StubProvider));
    let out = block_on_local(tool.execute(
        "call-1",
        json!({"question": "what should run before committing?"}),
        None,
    ))
    .expect("execute");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("reflect answer: {text}"));
    assert!(!out.is_error, "{text}");
    let id_marker = format!("[{}]", memory.id);
    assert!(
        text.contains(&id_marker),
        "answer must cite the memory id {id_marker}: {text}"
    );
    let citations = out.details.as_ref().expect("details")["citations"] // ubs:ignore test fixture
        .as_array()
        .expect("citations");
    assert!(
        citations.iter().any(|id| id.as_i64() == Some(memory.id)),
        "citations must include {}: {citations:?}",
        memory.id
    );
    finish_case(&harness, case);
}

#[test]
fn cross_instance_persistence_and_tombstones() {
    let case = "cross_instance_persistence_and_tombstones";
    let harness = TestHarness::new(case);
    let root = project_dir(&harness, "proj");

    // Session A: retain two facts, invalidate one, forget nothing.
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

    // Session B (fresh store instance): kept fact recalls, tombstone does not.
    let store_b = pi::memory::MemoryStore::open(&root).expect("open B");
    let hits = store_b.recall("agent loop", None).expect("recall");
    harness
        .log()
        .info("verify", format!("session B recall: {hits:?}"));
    assert!(
        hits.iter().any(|hit| hit.id == kept_id),
        "session B must recall session A's fact: {hits:?}"
    );
    let tomb_hits = store_b.recall("scaffolding", None).expect("tomb recall");
    assert!(
        tomb_hits.iter().all(|hit| hit.id != tomb_id),
        "tombstone must be excluded: {tomb_hits:?}"
    );

    // Forget hard-deletes the tombstone row entirely.
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
        "backend=local must inject the mental model: {}...",
        &prompt[..prompt.len().min(400)]
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
        false, // memory injection is gated off under test_mode
        true,
        None,
        config,
    )
    .expect("build prompt")
}
