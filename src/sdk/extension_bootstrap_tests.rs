use crate::model::{ContentBlock, StopReason, ThinkingLevel};
use crate::sdk::{SessionOptions, create_agent_session};
use std::path::{Path, PathBuf};

fn run_async<F: std::future::Future>(future: F) -> F::Output {
    let reactor = asupersync::runtime::reactor::create_reactor().expect("reactor");
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .expect("runtime")
        // Boxed for the same reason as `sdk::tests::run_async`, which see:
        // `create_agent_session`'s future does not fit a libtest thread's
        // 2 MiB default stack, and overflowing it aborts the process rather
        // than failing one test.
        .block_on(Box::pin(future))
}

fn extension(dir: &Path, provider: &str) -> PathBuf {
    let path = dir.join("provider.mjs");
    let provider = serde_json::to_string(provider).expect("provider JSON");
    let source = format!(
        r#"
let loads = 0;
let starts = 0;
export default function (pi) {{
    loads += 1;
    pi.on("startup", async () => {{ starts += 1; }});
    pi.registerProvider({provider}, {{
        api: "sdk-extension-test-api",
        baseUrl: "http://127.0.0.1:1/unreachable",
        apiKey: "sdk-test-key",
        headers: {{ "x-sdk-extension": "registered" }},
        models: ["fixture", "second"].map(id => ({{
            id, name: id, reasoning: true, input: ["text", "image"],
            cost: {{ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }},
            contextWindow: 64000, maxTokens: 777
        }})),
        streamSimple: async function* (model) {{
            yield `loads:${{loads}} starts:${{starts}} model:${{model.id}}`;
        }}
    }});
}}
"#
    );
    std::fs::write(&path, source).expect("write extension");
    path
}

fn options(dir: &Path) -> SessionOptions {
    SessionOptions {
        provider: Some("sdk-extension-fixture".to_string()),
        model: Some("fixture".to_string()),
        api_key: Some("sdk-explicit-test-key".to_string()),
        working_directory: Some(dir.to_path_buf()),
        enabled_tools: Some(Vec::new()),
        extension_paths: vec![extension(dir, "sdk-extension-fixture")],
        persist_extension_permissions: false,
        ..Default::default()
    }
}

fn text(message: &crate::model::AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn extension_only_provider_can_be_selected_and_prompted_at_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let mut handle = create_agent_session(options(dir.path()))
            .await
            .expect("session");
        assert_eq!(
            handle.model(),
            ("sdk-extension-fixture".into(), "fixture".into())
        );
        let message = handle.prompt("hello", |_| {}).await.expect("prompt");
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(text(&message), "loads:1 starts:1 model:fixture");
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}

#[test]
fn extension_model_limits_headers_and_thinking_replace_bootstrap_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut options = options(dir.path());
    options.thinking = Some(ThinkingLevel::High);
    run_async(async {
        let handle = create_agent_session(options).await.expect("session");
        assert_eq!(handle.max_tokens(), Some(777));
        assert_eq!(handle.compaction_settings().context_window_tokens, 64_000);
        assert_eq!(handle.thinking_level(), Some(ThinkingLevel::High));
        let stream = handle.session().agent.stream_options();
        assert_eq!(stream.api_key.as_deref(), Some("sdk-explicit-test-key"));
        assert_eq!(
            stream.headers.get("x-sdk-extension").map(String::as_str),
            Some("registered")
        );
        let state = handle.state().await.expect("state");
        assert_eq!(state.provider, "sdk-extension-fixture");
        assert_eq!(state.model_id, "fixture");
        handle
            .with_session(|session| {
                assert_eq!(
                    session.header.provider.as_deref(),
                    Some("sdk-extension-fixture")
                );
                assert_eq!(session.header.model_id.as_deref(), Some("fixture"));
            })
            .await
            .expect("session metadata");
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}

#[test]
fn explicit_compaction_window_survives_extension_provider_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut options = options(dir.path());
    options.compaction_settings = Some(crate::compaction::ResolvedCompactionSettings {
        context_window_tokens: 12_345,
        ..Default::default()
    });
    run_async(async {
        let handle = create_agent_session(options).await.expect("session");
        assert_eq!(handle.compaction_settings().context_window_tokens, 12_345);
        assert_eq!(handle.max_tokens(), Some(777));
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}

#[test]
fn registered_models_remain_available_for_later_model_switches() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let mut handle = create_agent_session(options(dir.path()))
            .await
            .expect("session");
        handle
            .set_model("sdk-extension-fixture", "second")
            .await
            .expect("switch");
        let message = handle.prompt("hello", |_| {}).await.expect("prompt");
        assert_eq!(text(&message), "loads:1 starts:1 model:second");
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}

#[test]
fn resumed_extension_identity_is_not_replaced_by_the_bootstrap_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut header = crate::session::SessionHeader::new();
    header.cwd = dir.path().display().to_string();
    header.provider = Some("sdk-extension-fixture".to_string());
    header.model_id = Some("second".to_string());
    let path = dir.path().join("resume.jsonl");
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&header).expect("header JSON")),
    )
    .expect("write session");
    let mut options = options(dir.path());
    options.provider = None;
    options.model = None;
    options.session_path = Some(path);
    options.no_session = false;
    run_async(async {
        let handle = create_agent_session(options).await.expect("resume");
        assert_eq!(
            handle.model(),
            ("sdk-extension-fixture".into(), "second".into())
        );
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}

#[test]
fn unresolved_explicit_provider_is_an_error_after_extensions_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut options = options(dir.path());
    options.provider = Some("unregistered-sdk-provider".to_string());
    let result = run_async(create_agent_session(options));
    assert!(
        result.is_err(),
        "bootstrap must never become a silent fallback"
    );
}

#[test]
fn configured_model_scope_can_resolve_extension_only_models() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".pi")).expect("project config dir");
    std::fs::write(
        dir.path().join(".pi/settings.json"),
        r#"{"enabledModels":["sdk-extension-fixture/second"]}"#,
    )
    .expect("project settings");
    let mut options = options(dir.path());
    options.provider = None;
    options.model = None;
    options.workspace_trusted = true;
    run_async(async {
        let handle = create_agent_session(options).await.expect("scoped session");
        assert_eq!(
            handle.model(),
            ("sdk-extension-fixture".into(), "second".into())
        );
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}
