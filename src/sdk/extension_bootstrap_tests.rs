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

fn cli(args: &[&str]) -> crate::cli::Cli {
    use clap::Parser as _;
    crate::cli::Cli::parse_from(std::iter::once("pi").chain(args.iter().copied()))
}

#[test]
fn refresh_scope_follows_explicit_provider_precedence_and_aliases() {
    let request = cli(&["--provider", "KIMI-CODE", "--model", "openai/fixture"]);
    assert!(super::refresh_matches_request(&request, "kimi-for-coding"));
    assert!(!super::refresh_matches_request(&request, "openai"));
    let request = cli(&["--provider", "Acme-Fixture"]);
    assert!(super::refresh_matches_request(&request, "acme-fixture"));
    assert!(!super::refresh_matches_request(&request, "other-fixture"));
}

#[test]
fn qualified_model_refreshes_only_its_provider() {
    let request = cli(&["--model", "openrouter/vendor/model"]);
    assert!(super::refresh_matches_request(&request, "openrouter"));
    assert!(!super::refresh_matches_request(&request, "openai"));
}

#[test]
fn automatic_and_ambiguous_bare_model_selection_keep_refresh_candidates() {
    for request in [cli(&[]), cli(&["--model", "fixture"])] {
        for provider in ["acme-fixture", "other-fixture"] {
            assert!(super::refresh_matches_request(&request, provider));
        }
    }
}

fn oauth_credential(expires: i64) -> crate::auth::AuthCredential {
    crate::auth::AuthCredential::OAuth {
        extra: std::collections::HashMap::new(),
        access_token: "old-access".to_string(),
        refresh_token: "old-refresh".to_string(),
        expires,
        token_url: None,
        client_id: None,
    }
}

fn oauth_config(provider: &str) -> crate::models::OAuthConfig {
    crate::models::OAuthConfig {
        auth_url: format!("https://{provider}.invalid/authorize"),
        token_url: format!("https://{provider}.invalid/token"),
        client_id: "sdk-oauth-fixture".to_string(),
        scopes: vec!["read".to_string()],
        redirect_uri: None,
    }
}

/// Playback has no fallback to live networking. An unexpected provider refresh
/// is a recorded failure, so checking the failure map catches a removed filter.
fn refresh_client(dir: &Path, responses: &[(&str, u16)]) -> crate::http::client::Client {
    use crate::vcr::{
        Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder,
    };
    let recorder = VcrRecorder::new_with("sdk-oauth-refresh", VcrMode::Playback, dir);
    let interactions = responses
        .iter()
        .map(|(provider, status)| Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: oauth_config(provider).token_url,
                headers: Vec::new(),
                body: Some(serde_json::json!({
                    "grant_type": "refresh_token",
                    "client_id": "sdk-oauth-fixture",
                    "refresh_token": "[REDACTED]"
                })),
                body_text: None,
            },
            response: RecordedResponse {
                status: *status,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body_chunks: vec![if *status == 200 {
                    serde_json::json!({
                        "access_token": "fresh-access",
                        "refresh_token": "fresh-refresh",
                        "expires_in": 7200
                    })
                    .to_string()
                } else {
                    serde_json::json!({"error": "invalid_grant"}).to_string()
                }],
                body_chunks_base64: None,
            },
        })
        .collect();
    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: "sdk-oauth-refresh".to_string(),
        recorded_at: "2026-09-22T00:00:00.000Z".to_string(),
        interactions,
    };
    std::fs::write(
        recorder.cassette_path(),
        serde_json::to_vec(&cassette).unwrap(),
    )
    .unwrap();
    crate::http::client::Client::new().with_vcr(recorder)
}

#[test]
fn explicit_route_refreshes_its_expired_token_without_touching_unrelated_login() {
    let temp = tempfile::tempdir().unwrap();
    let auth_path = temp.path().join("auth.json");
    let mut auth = crate::auth::AuthStorage::empty_at(auth_path.clone());
    auth.set("Acme-Fixture", oauth_credential(0));
    auth.set("other-fixture", oauth_credential(0));
    let unrelated_before = serde_json::to_value(auth.get("other-fixture")).unwrap();
    let configs = vec![
        ("other-fixture".to_string(), oauth_config("other-fixture")),
        ("acme-fixture".to_string(), oauth_config("acme-fixture")),
    ];
    let client = refresh_client(temp.path(), &[("acme-fixture", 200)]);
    let failures = run_async(super::refresh_extension_credentials(
        &mut auth,
        &cli(&["--provider", "ACME-FIXTURE"]),
        &configs,
        &client,
    ));
    assert!(failures.is_empty(), "unexpected refresh: {failures:?}");
    assert_eq!(
        auth.api_key("acme-fixture").as_deref(),
        Some("fresh-access")
    );
    assert_eq!(
        serde_json::to_value(auth.get("other-fixture")).unwrap(),
        unrelated_before
    );
    let reopened = crate::auth::AuthStorage::load(auth_path).unwrap();
    assert_eq!(
        reopened.api_key("Acme-Fixture").as_deref(),
        Some("fresh-access")
    );
    assert_eq!(
        serde_json::to_value(reopened.get("other-fixture")).unwrap(),
        unrelated_before
    );
}

#[test]
fn explicit_key_does_not_refresh_or_persist_any_extension_login() {
    let temp = tempfile::tempdir().unwrap();
    let auth_path = temp.path().join("auth.json");
    let mut auth = crate::auth::AuthStorage::empty_at(auth_path.clone());
    auth.set("acme-fixture", oauth_credential(0));
    let before = serde_json::to_value(auth.get("acme-fixture")).unwrap();
    let configs = vec![("acme-fixture".to_string(), oauth_config("acme-fixture"))];
    let client = refresh_client(temp.path(), &[]);
    let failures = run_async(super::refresh_extension_credentials(
        &mut auth,
        &cli(&["--provider", "acme-fixture", "--api-key", "override"]),
        &configs,
        &client,
    ));
    assert!(failures.is_empty());
    assert_eq!(
        serde_json::to_value(auth.get("acme-fixture")).unwrap(),
        before
    );
    assert!(!auth_path.exists());
}

#[test]
fn selected_refresh_failure_is_retained_and_never_overwrites_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let auth_path = temp.path().join("auth.json");
    let mut auth = crate::auth::AuthStorage::empty_at(auth_path.clone());
    auth.set("acme-fixture", oauth_credential(0));
    auth.set("other-fixture", oauth_credential(0));
    let before = serde_json::to_value(auth.get("acme-fixture")).unwrap();
    let configs = vec![
        ("other-fixture".to_string(), oauth_config("other-fixture")),
        ("acme-fixture".to_string(), oauth_config("acme-fixture")),
    ];
    let client = refresh_client(temp.path(), &[("acme-fixture", 400)]);
    let failures = run_async(super::refresh_extension_credentials(
        &mut auth,
        &cli(&["--provider", "acme-fixture"]),
        &configs,
        &client,
    ));
    assert_eq!(failures.len(), 1);
    assert!(failures.contains_key("acme-fixture"));
    assert_eq!(
        serde_json::to_value(auth.get("acme-fixture")).unwrap(),
        before
    );
    assert!(!auth_path.exists());
}

#[test]
fn automatic_refresh_continues_after_one_candidate_fails() {
    let temp = tempfile::tempdir().unwrap();
    let mut auth = crate::auth::AuthStorage::empty_at(temp.path().join("auth.json"));
    auth.set("acme-fixture", oauth_credential(0));
    auth.set("other-fixture", oauth_credential(0));
    let configs = vec![
        ("other-fixture".to_string(), oauth_config("other-fixture")),
        ("acme-fixture".to_string(), oauth_config("acme-fixture")),
    ];
    let client = refresh_client(
        temp.path(),
        &[("other-fixture", 400), ("acme-fixture", 200)],
    );
    let failures = run_async(super::refresh_extension_credentials(
        &mut auth,
        &cli(&[]),
        &configs,
        &client,
    ));
    assert_eq!(failures.len(), 1);
    assert!(failures.contains_key("other-fixture"));
    assert_eq!(
        auth.api_key("acme-fixture").as_deref(),
        Some("fresh-access")
    );
}

fn registry_identity(registry: &crate::models::ModelRegistry) -> Vec<(String, String)> {
    registry
        .models()
        .iter()
        .map(|entry| (entry.model.provider.clone(), entry.model.id.clone()))
        .collect()
}

fn selection_inputs<'a>(
    cli: &'a crate::cli::Cli,
    config: &'a crate::config::Config,
    dir: &'a Path,
    refresh: &'a crate::auth::OAuthRefreshReport,
) -> super::SelectionInputs<'a> {
    super::SelectionInputs {
        cli,
        config,
        scoped_patterns: &[],
        global_dir: dir,
        oauth_refresh: refresh,
        preserve_compaction_window: false,
    }
}

#[test]
fn failed_final_selection_does_not_publish_the_candidate_registry() {
    let temp = tempfile::tempdir().unwrap();
    run_async(async {
        let mut handle = create_agent_session(options(temp.path())).await.unwrap();
        let mut auth = crate::auth::AuthStorage::empty_at(temp.path().join("auth.json"));
        let mut registry = crate::models::ModelRegistry::load(&auth, None);
        let before = registry_identity(&registry);
        let request = cli(&[
            "--provider",
            "unregistered-sdk-provider",
            "--api-key",
            "explicit",
        ]);
        let config = crate::config::Config::default();
        let refresh = crate::auth::OAuthRefreshReport::default();
        // Exercise the actual installation body without the outer startup-only
        // shutdown, so the same live runtime can witness failure isolation.
        let result = super::finish_selection_inner(
            &mut handle.session,
            &mut registry,
            &mut auth,
            selection_inputs(&request, &config, temp.path(), &refresh),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(registry_identity(&registry), before);
        assert!(registry.find("sdk-extension-fixture", "fixture").is_none());
        assert_eq!(
            handle.model(),
            ("sdk-extension-fixture".into(), "fixture".into())
        );
        handle
            .with_session(|session| {
                assert_eq!(session.header.model_id.as_deref(), Some("fixture"));
            })
            .await
            .unwrap();
        let message = handle.prompt("still usable", |_| {}).await.unwrap();
        assert_eq!(text(&message), "loads:1 starts:1 model:fixture");
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}

#[test]
fn cancelled_owner_cannot_publish_a_new_model_or_registry() {
    let temp = tempfile::tempdir().unwrap();
    run_async(async {
        let mut handle = create_agent_session(options(temp.path())).await.unwrap();
        let mut auth = crate::auth::AuthStorage::empty_at(temp.path().join("auth.json"));
        let mut registry = crate::models::ModelRegistry::load(&auth, None);
        let before = registry_identity(&registry);
        let request = cli(&[
            "--provider",
            "sdk-extension-fixture",
            "--model",
            "second",
            "--api-key",
            "replacement",
        ]);
        let config = crate::config::Config::default();
        let refresh = crate::auth::OAuthRefreshReport::default();
        let owner = crate::agent_cx::AgentCx::for_request();
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel bootstrap"),
        );
        let error = owner
            .with_current(super::finish_selection_inner(
                &mut handle.session,
                &mut registry,
                &mut auth,
                selection_inputs(&request, &config, temp.path(), &refresh),
            ))
            .await
            .expect_err("cancelled owner must not become a fresh request");
        assert!(
            error
                .to_string()
                .contains("SDK_EXTENSION_STARTUP_CANCELLED")
        );
        assert_eq!(registry_identity(&registry), before);
        assert_eq!(
            handle.model(),
            ("sdk-extension-fixture".into(), "fixture".into())
        );
        assert_eq!(
            handle.session().agent.stream_options().api_key.as_deref(),
            Some("sdk-explicit-test-key")
        );
        assert!(!temp.path().join("auth.json").exists());
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}

#[test]
fn cancellation_while_waiting_for_session_lock_does_not_install_after_release() {
    let temp = tempfile::tempdir().unwrap();
    run_async(async {
        let mut handle = create_agent_session(options(temp.path())).await.unwrap();
        let mut auth = crate::auth::AuthStorage::empty_at(temp.path().join("auth.json"));
        let mut registry = crate::models::ModelRegistry::load(&auth, None);
        let before = registry_identity(&registry);
        let request = cli(&[
            "--provider",
            "sdk-extension-fixture",
            "--model",
            "second",
            "--api-key",
            "replacement",
        ]);
        let config = crate::config::Config::default();
        let refresh = crate::auth::OAuthRefreshReport::default();
        let parent = crate::agent_cx::AgentCx::for_current_or_request();
        let owner = crate::agent_cx::AgentCx::for_request();
        let store = std::sync::Arc::clone(&handle.session.session);
        let held = store.lock(parent.cx()).await.unwrap();
        let mut selection = Box::pin(owner.with_current(super::finish_selection_inner(
            &mut handle.session,
            &mut registry,
            &mut auth,
            selection_inputs(&request, &config, temp.path(), &refresh),
        )));
        assert!(futures::poll!(selection.as_mut()).is_pending());
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel lock waiter"),
        );
        drop(held);
        // Poll once after release: neither cancellation nor an uncontended lock
        // needs a timer. A regression cannot hang this test in an infinite wait.
        let outcome = futures::poll!(selection.as_mut());
        assert!(matches!(outcome, std::task::Poll::Ready(Err(_))));
        drop(selection);
        assert!(!parent.cx().is_cancel_requested());
        assert_eq!(registry_identity(&registry), before);
        assert_eq!(
            handle.model(),
            ("sdk-extension-fixture".into(), "fixture".into())
        );
        handle
            .with_session(|session| {
                assert_eq!(session.header.model_id.as_deref(), Some("fixture"));
            })
            .await
            .unwrap();
        let message = handle.prompt("after cancellation", |_| {}).await.unwrap();
        assert_eq!(text(&message), "loads:1 starts:1 model:fixture");
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}

#[test]
fn successful_final_selection_installs_registry_provider_options_and_header_together() {
    let temp = tempfile::tempdir().unwrap();
    run_async(async {
        let mut handle = create_agent_session(options(temp.path())).await.unwrap();
        let mut auth = crate::auth::AuthStorage::empty_at(temp.path().join("auth.json"));
        let mut registry = crate::models::ModelRegistry::load(&auth, None);
        assert!(registry.find("sdk-extension-fixture", "second").is_none());
        let request = cli(&[
            "--provider",
            "sdk-extension-fixture",
            "--model",
            "second",
            "--api-key",
            "replacement",
        ]);
        let config = crate::config::Config::default();
        let refresh = crate::auth::OAuthRefreshReport::default();
        super::finish_selection_inner(
            &mut handle.session,
            &mut registry,
            &mut auth,
            selection_inputs(&request, &config, temp.path(), &refresh),
        )
        .await
        .unwrap();
        assert!(registry.find("sdk-extension-fixture", "second").is_some());
        assert_eq!(
            handle.model(),
            ("sdk-extension-fixture".into(), "second".into())
        );
        assert_eq!(
            handle.session().agent.stream_options().api_key.as_deref(),
            Some("replacement")
        );
        handle
            .with_session(|session| {
                assert_eq!(session.header.model_id.as_deref(), Some("second"));
            })
            .await
            .unwrap();
        let message = handle.prompt("new selection", |_| {}).await.unwrap();
        assert_eq!(text(&message), "loads:1 starts:1 model:second");
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
    });
}
