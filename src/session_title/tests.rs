//! Automatic naming through the shared client and the interactive session owner.
//! Provider replies are scripted; privacy, admission and durable session writes
//! use their production implementations.

use super::*;
use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::auth::{AuthCredential, AuthStorage};
use crate::compaction::ResolvedCompactionSettings;
use crate::model::{
    AssistantMessage, ImageContent, MediaContent, StreamEvent, TextContent, ThinkingContent,
};
use crate::models::ModelEntry;
use crate::provider::{InputType, Model, ModelCost};
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::{Runtime, RuntimeBuilder, RuntimeHandle};
use futures::channel::oneshot;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

struct CapturedRequest {
    messages: Vec<Message>,
    system_prompt: Option<String>,
    options: StreamOptions,
}

impl CapturedRequest {
    fn prompt(&self) -> &str {
        let [Message::User(user)] = self.messages.as_slice() else {
            panic!("one auxiliary user message is required"); // ubs:ignore[rust.ownership.panic-macro] -- Fail this fixture if the production request shape changes.
        };
        let UserContent::Text(text) = &user.content else {
            panic!("automatic titles must send only text"); // ubs:ignore[rust.ownership.panic-macro] -- Nontext provider input is the regression this fixture rejects.
        };
        text
    }
}

#[derive(Default)]
struct PendingState {
    polled: AtomicBool,
    dropped: AtomicBool,
}

struct StreamLifetime(Arc<PendingState>);

impl Drop for StreamLifetime {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}

enum Reply {
    Events(Vec<StreamEvent>),
    Pending(oneshot::Receiver<StreamEvent>, Arc<PendingState>),
}

struct RecordingProvider {
    replies: StdMutex<VecDeque<Reply>>,
    requests: StdMutex<Vec<CapturedRequest>>,
    calls: AtomicUsize,
}

impl RecordingProvider {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: StdMutex::new(replies.into()),
            requests: StdMutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait::async_trait]
impl Provider for RecordingProvider {
    fn name(&self) -> &str {
        "session-title-fixture"
    }

    fn api(&self) -> &str {
        "openai-completions"
    }

    fn model_id(&self) -> &str {
        "tiny-title-model"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(context.tools.is_empty(), "naming cannot execute tools"); // ubs:ignore[rust.panic.assert-macros] -- Tool-free request regression oracle.
        self.requests.lock().unwrap().push(CapturedRequest {
            // ubs:ignore[rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- A poisoned test recorder must fail the fixture.
            messages: context.messages.to_vec(),
            system_prompt: context.system_prompt.as_deref().map(str::to_string),
            options: options.clone(),
        });
        let reply = self // ubs:ignore[rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- Poisoned fixture state or an unscripted extra call must fail the test.
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected additional automatic title request");
        match reply {
            Reply::Events(events) => {
                Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
            }
            Reply::Pending(receiver, state) => {
                let lifetime = StreamLifetime(Arc::clone(&state));
                Ok(Box::pin(futures::stream::once(async move {
                    let _lifetime = lifetime;
                    state.polled.store(true, Ordering::SeqCst);
                    receiver
                        .await
                        .map_err(|_| Error::api("fixture completion was cancelled"))
                })))
            }
        }
    }
}

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread() // ubs:ignore[rust.ownership.unwrap-expect] -- Failure to construct the test runtime is a fixture failure.
        .with_reactor(create_reactor().expect("create title test reactor")) // ubs:ignore[rust.ownership.unwrap-expect] -- The test requires a reactor.
        .build()
        .expect("build title test runtime")
}

fn run_async<F: Future>(future: F) -> F::Output {
    runtime().block_on(Box::pin(future))
}

fn user(text: impl Into<String>) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.into()),
        timestamp: 0,
    })
}

fn assistant(text: impl Into<String>) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        stop_reason: StopReason::Stop,
        ..Default::default()
    }
}

fn exchange() -> Vec<Message> {
    vec![
        user("Fix the editor session startup"),
        Message::assistant(assistant(
            "The model selection now reaches the editor session.",
        )),
    ]
}

fn done(text: impl Into<String>) -> StreamEvent {
    StreamEvent::Done {
        reason: StopReason::Stop,
        message: assistant(text),
    }
}

fn delta(text: impl Into<String>) -> StreamEvent {
    StreamEvent::TextDelta {
        content_index: 0,
        delta: text.into(),
    }
}

fn client(
    provider: Arc<dyn Provider>,
    settings: Option<&crate::secrets::SecretsSettings>,
) -> Arc<TitleClient> {
    Arc::new(TitleClient {
        provider,
        options: StreamOptions {
            api_key: Some("title-fixture-key".to_string()),
            max_tokens: Some(96),
            ..Default::default()
        },
        credential_binding: None,
        privacy: Arc::new(AuxiliaryPrivacy::from_settings(settings)),
    })
}

fn agent(provider: Arc<dyn Provider>) -> Agent {
    Agent::new(
        provider,
        crate::tools::ToolRegistry::from_tools(Vec::new()),
        AgentConfig::default(),
    )
}

fn prepare(client: &TitleClient, messages: Vec<Message>) -> PreparedTitle {
    let mut agent = agent(Arc::clone(&client.provider));
    agent.replace_messages(messages);
    client // ubs:ignore[rust.ownership.unwrap-expect] -- This helper requires a complete eligible fixture exchange.
        .prepare_with_agent(&agent)
        .expect("screen complete exchange")
        .expect("completed exchange is eligible")
}

fn pending_provider() -> (
    Arc<RecordingProvider>,
    oneshot::Sender<StreamEvent>,
    Arc<PendingState>,
) {
    let (sender, receiver) = oneshot::channel();
    let state = Arc::new(PendingState::default());
    let provider = RecordingProvider::new(vec![Reply::Pending(receiver, Arc::clone(&state))]);
    (provider, sender, state)
}

#[test]
fn preparation_screens_individual_complete_fields_before_excerpting() {
    run_async(async {
        let secret = "r4nd0mCredentialValue123456";
        for mode in ["obfuscate", "off", "block"] {
            let provider =
                RecordingProvider::new(vec![Reply::Events(vec![done("Fix editor startup")])]);
            let settings = crate::secrets::SecretsSettings {
                mode: Some(mode.to_string()),
                extra_patterns: Some(vec![r"^ACME-\d{6}$".to_string()]),
            };
            let client = client(provider.clone(), Some(&settings));
            let messages = vec![
                Message::User(UserMessage {
                    content: UserContent::Blocks(vec![
                        ContentBlock::Text(TextContent::new("ACME-123456")),
                        ContentBlock::Text(TextContent::new(format!("echo {secret}"))),
                        ContentBlock::Image(ImageContent {
                            data: "PRIVATE-IMAGE".repeat(50_000),
                            mime_type: "image/png".to_string(),
                        }),
                        ContentBlock::Media(MediaContent {
                            data: "PRIVATE-AUDIO".to_string(),
                            mime_type: "audio/wav".to_string(),
                            name: Some("PRIVATE-FILENAME".to_string()),
                        }),
                    ]),
                    timestamp: 0,
                }),
                Message::assistant(AssistantMessage {
                    content: vec![
                        ContentBlock::Thinking(ThinkingContent {
                            thinking: "PRIVATE-REASONING".to_string(),
                            thinking_signature: None,
                        }),
                        ContentBlock::Text(TextContent::new(format!(
                            "{} API_KEY={secret}",
                            "safe explanation ".repeat(100),
                        ))),
                    ],
                    ..Default::default()
                }),
            ];
            let before = serde_json::to_value(&messages).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture serialization must succeed.
            let mut agent = agent(provider.clone());
            agent.replace_messages(messages);
            let prepared = client.prepare_with_agent(&agent);
            if mode == "block" {
                let error = prepared.err().expect("anchored source field must block"); // ubs:ignore[rust.ownership.unwrap-expect] -- The blocking regression must produce an error.
                assert!(error.to_string().contains("PI_SECRET_BLOCK")); // ubs:ignore[rust.panic.assert-macros] -- Privacy regression oracle.
                assert_eq!(provider.calls.load(Ordering::SeqCst), 0); // ubs:ignore[rust.panic.assert-macros] -- Blocked input must never call the provider.
            } else {
                assert_eq!(
                    // ubs:ignore[rust.panic.assert-macros] -- Sanitized eligible input must produce a title.
                    client.generate(prepared.unwrap().unwrap()).await.as_deref(), // ubs:ignore[rust.ownership.unwrap-expect] -- Nonblocking fixture input must remain eligible.
                    Some("Fix editor startup"),
                );
                let requests = provider.requests.lock().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- Poisoned fixture state must fail this test.
                let prompt = requests[0].prompt(); // ubs:ignore[rust.panic.direct-indexing] -- A successful fixture request must have been recorded.
                assert!(prompt.contains("echo <pi-secret:redacted>")); // ubs:ignore[rust.panic.assert-macros] -- Privacy regression oracle.
                assert!(
                    !prompt.contains(secret),
                    "later assignment screens earlier echo"
                ); // ubs:ignore[rust.panic.assert-macros] -- Privacy regression oracle.
                assert!(
                    !prompt.contains("ACME-"),
                    "anchored patterns see each original field"
                ); // ubs:ignore[rust.panic.assert-macros] -- Privacy regression oracle.
                assert!(
                    !prompt.contains("PRIVATE-"),
                    "attachments and reasoning are excluded"
                ); // ubs:ignore[rust.panic.assert-macros] -- Privacy regression oracle.
                assert!(!prompt.contains("<pi-secret:000001>")); // ubs:ignore[rust.panic.assert-macros] -- Privacy regression oracle.
                assert_eq!(
                    requests[0].options.api_key.as_deref(),
                    Some("title-fixture-key")
                ); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Recorded transport credentials are part of this regression oracle.
            }
            assert_eq!(serde_json::to_value(agent.messages()).unwrap(), before); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Serializable fixture history must remain unchanged.
        }
    });
}

#[test]
#[allow(clippy::significant_drop_tightening)]
fn preparation_retains_live_vault_discoveries_without_exporting_reversible_ids() {
    run_async(async {
        let provider =
            RecordingProvider::new(vec![Reply::Events(vec![done("Repair account setup")])]);
        let client = client(provider.clone(), None);
        let mut agent = agent(provider.clone());
        let secret = "rememberedCredentialValue123456";
        agent
            .secrets_transform_outbound_text(&format!("API_KEY={secret}"))
            .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture setup must populate the live secret vault.
        agent.replace_messages(vec![
            user(format!("Check echo {secret}")),
            Message::assistant(assistant("The configuration was repaired.")),
        ]);
        let request = client.prepare_with_agent(&agent).unwrap().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The complete fixture exchange must be eligible.
        assert_eq!(
            client.generate(request).await.as_deref(),
            Some("Repair account setup")
        ); // ubs:ignore[rust.panic.assert-macros] -- Title generation regression oracle.
        let requests = provider.requests.lock().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- Poisoned fixture state must fail this test.
        assert!(requests[0].prompt().contains("echo <pi-secret:redacted>")); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Recorded request privacy regression oracle.
        assert!(!requests[0].prompt().contains(secret)); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Recorded request privacy regression oracle.
        assert!(!requests[0].prompt().contains("<pi-secret:000001>")); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Recorded request privacy regression oracle.
        assert_eq!(agent.mask_secrets_text(secret), "<pi-secret:000001>"); // ubs:ignore[rust.panic.assert-macros] -- Auxiliary privacy must preserve the live vault.
    });
}

#[test]
fn unscannable_text_is_refused_without_sending_an_unscreened_prefix() {
    let provider = RecordingProvider::new(Vec::new());
    let client = client(provider.clone(), None);
    for content in [
        UserContent::Text(format!("PRIVATE-PREFIX{}", "x".repeat(MAX_INPUT_BYTES))),
        UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("small field"));
            257
        ]),
    ] {
        let mut agent = agent(provider.clone());
        agent.replace_messages(vec![
            Message::User(UserMessage {
                content,
                timestamp: 0,
            }),
            Message::assistant(assistant("Completed response")),
        ]);
        let error = client // ubs:ignore[rust.ownership.unwrap-expect] -- Oversized fixture input must be rejected.
            .prepare_with_agent(&agent)
            .err()
            .expect("complete input exceeds scan bound");
        assert!(error.to_string().contains("PI_AUXILIARY_INPUT_LIMIT")); // ubs:ignore[rust.panic.assert-macros] -- Input limit regression oracle.
        assert!(!error.to_string().contains("PRIVATE-PREFIX")); // ubs:ignore[rust.panic.assert-macros] -- Error messages must not expose input.
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0); // ubs:ignore[rust.panic.assert-macros] -- Rejected input must never call the provider.
}

#[test]
fn title_uses_the_clean_terminal_message_and_sanitizes_display_controls() {
    run_async(async {
        let provider = RecordingProvider::new(vec![Reply::Events(vec![
            delta("Wrong incomplete preview"),
            done("\"Fix\u{202e} editor\u{1b} startup.\"\nextra commentary"),
        ])]);
        let client = client(provider.clone(), None);
        assert_eq!(
            // ubs:ignore[rust.panic.assert-macros] -- Only the clean terminal response may become a title.
            client
                .generate(prepare(&client, exchange()))
                .await
                .as_deref(),
            Some("Fix editor startup"),
        );
        let requests = provider.requests.lock().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- Poisoned fixture state must fail this test.
        assert_eq!(requests[0].options.max_tokens, Some(96)); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Recorded request budget regression oracle.
        assert_eq!(requests[0].system_prompt.as_deref(), Some(SYSTEM_PROMPT)); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Recorded request instruction regression oracle.
    });
}

#[test]
fn partial_failed_and_oversized_provider_responses_never_become_titles() {
    run_async(async {
        let mut truncated = assistant("Title cut short");
        truncated.stop_reason = StopReason::Length;
        let cases = vec![
            vec![delta("Interrupted title")],
            vec![
                delta("Interrupted title"),
                StreamEvent::Error {
                    reason: StopReason::Error,
                    error: AssistantMessage {
                        stop_reason: StopReason::Error,
                        error_message: Some("provider unavailable".to_string()),
                        ..Default::default()
                    },
                },
            ],
            vec![done("x".repeat(MAX_REPLY_BYTES + 1))],
            vec![delta("x".repeat(MAX_REPLY_BYTES + 1)), done("Looks short")],
            vec![StreamEvent::Done {
                reason: StopReason::Length,
                message: truncated,
            }],
        ];
        for events in cases {
            let provider = RecordingProvider::new(vec![Reply::Events(events)]);
            let client = client(provider.clone(), None);
            assert!(
                client
                    .generate(prepare(&client, exchange()))
                    .await
                    .is_none()
            ); // ubs:ignore[rust.panic.assert-macros] -- Invalid terminal responses must not become titles.
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Invalid responses must not trigger a retry.
        }
    });
}

#[test]
fn an_expired_foreground_budget_cannot_trigger_an_auxiliary_provider_call() {
    run_async(async {
        let provider = RecordingProvider::new(Vec::new());
        let client = client(provider.clone(), None);
        let mut agent = Agent::new(
            provider.clone(),
            crate::tools::ToolRegistry::from_tools(Vec::new()),
            AgentConfig {
                max_time: Some(Duration::ZERO),
                ..AgentConfig::default()
            },
        );
        agent
            .run("Do not exceed the run budget", |_| {})
            .await
            .expect("time cap returns a boundary marker"); // ubs:ignore[rust.ownership.unwrap-expect] -- Exercise the actual zero-budget foreground loop.
        let prepared = client
            .prepare_with_agent(&agent)
            .expect("screen the terminal marker"); // ubs:ignore[rust.ownership.unwrap-expect] -- The generated marker is valid input.
        assert!(prepared.is_none()); // ubs:ignore[rust.panic.assert-macros] -- A capped run must not admit a title.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0); // ubs:ignore[rust.panic.assert-macros] -- Neither foreground nor title work may call the provider.
    });
}

#[test]
fn title_deadline_retires_the_actual_pending_provider_stream() {
    run_async(async {
        let (provider, _sender, state) = pending_provider();
        let client = client(provider.clone(), None);
        let title = client
            .generate_with_timeout(prepare(&client, exchange()), Duration::from_millis(15))
            .await;
        assert!(title.is_none()); // ubs:ignore[rust.panic.assert-macros] -- A deadline cannot publish a title.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- A deadline cannot start a retry.
        assert!(
            state.polled.load(Ordering::SeqCst),
            "provider stream reached Pending"
        ); // ubs:ignore[rust.panic.assert-macros] -- The deadline must exercise an actual pending stream.
        assert!(
            state.dropped.load(Ordering::SeqCst),
            "deadline must retire provider ownership"
        ); // ubs:ignore[rust.panic.assert-macros] -- Pending provider ownership must be retired.
    });
}

fn entry(provider: &str, url: &str, key: Option<&str>) -> ModelEntry {
    ModelEntry {
        model: Model {
            id: "tiny-title-model".to_string(),
            name: "Tiny title model".to_string(),
            api: "openai-completions".to_string(),
            provider: provider.to_string(),
            base_url: url.to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                audio: None,
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 8_192,
            max_tokens: 2_048,
            headers: HashMap::new(),
        },
        api_key: key.map(str::to_string),
        headers: HashMap::from([("x-title-route".to_string(), "tiny".to_string())]),
        auth_header: true,
        compat: None,
        oauth_config: None,
    }
}

#[test]
fn retained_title_client_rechecks_rotation_and_exhaustion_at_dispatch() {
    const CHILD: &str = "PI_TITLE_CREDENTIAL_ADMISSION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session_title::tests::retained_title_client_rechecks_rotation_and_exhaustion_at_dispatch",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("OPENAI_API_KEYS", "pool-key-one,pool-key-two")
            .env("OPENAI_API_KEY", "singular-bypass")
            .output()
            .expect("run isolated title credential fixture");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }

    crate::auth::reset_credential_rings_for_tests();
    run_async(async {
        let directory = tempfile::tempdir().unwrap();
        let mut auth = AuthStorage::empty_at(directory.path().join("auth.json"));
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "stored-bypass".to_string(),
            },
        );
        let role = entry("openai", "https://title.invalid/v1", Some("catalog-bypass"));
        let mut client = TitleClient::for_model_entry(&role, &role, None, &auth, None)
            .expect("pool is initially available");
        let initial_key = client.options.api_key.clone().unwrap();
        let provider = RecordingProvider::new(vec![
            Reply::Events(vec![done("Fix editor startup")]),
            Reply::Events(vec![done("Fix editor startup")]),
        ]);
        Arc::get_mut(&mut client).unwrap().provider = provider.clone();
        let first = prepare(&client, exchange());
        let after_exhaustion = prepare(&client, exchange());

        crate::auth::report_provider_rate_limit("openai", &initial_key);
        let sibling = auth.resolve_api_key("openai", None).unwrap();
        assert_ne!(initial_key, sibling);
        assert!(client.generate(first).await.is_some());
        assert_eq!(
            provider.requests.lock().unwrap()[0]
                .options
                .api_key
                .as_deref(),
            Some(sibling.as_str())
        );

        crate::auth::report_provider_rate_limit("openai", &sibling);
        assert!(client.generate(after_exhaustion).await.is_none());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        let mut pinned =
            TitleClient::for_model_entry(&role, &role, Some("explicit-title-key"), &auth, None)
                .expect("same-destination explicit override remains available");
        Arc::get_mut(&mut pinned).unwrap().provider = provider.clone();
        assert!(
            pinned
                .generate(prepare(&pinned, exchange()))
                .await
                .is_some()
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            provider.requests.lock().unwrap()[1]
                .options
                .api_key
                .as_deref(),
            Some("explicit-title-key")
        );
    });
}

#[test]
#[allow(clippy::significant_drop_tightening)]
fn resolved_credentials_and_headers_reach_the_role_without_cross_destination_override() {
    run_async(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let mut auth = AuthStorage::load(dir.path().join("auth.json")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture credential storage must load.
        auth.set(
            "title-secondary",
            AuthCredential::ApiKey {
                key: "secondary-stored-key".to_string(),
            },
        );
        let primary = entry("title-primary", "https://primary.invalid/v1", None);
        let mut same = entry(
            "title-primary",
            "https://primary.invalid/v1",
            Some("catalog-key"),
        );
        same.model.max_tokens = 32;
        let other_provider = entry(
            "title-secondary",
            "https://primary.invalid/v1",
            Some("catalog-key"),
        );
        let other_endpoint = entry(
            "title-primary",
            "https://other.invalid/v1",
            Some("endpoint-key"),
        );
        let alias_primary = entry("openrouter", "https://alias.invalid/v1", None);
        let alias_role = entry("open-router", "https://alias.invalid/v1", None);
        for (role, original, expected) in [
            (&same, &primary, "foreground-cli-key"),
            (&other_provider, &primary, "secondary-stored-key"),
            (&other_endpoint, &primary, "endpoint-key"),
            (&alias_role, &alias_primary, "foreground-cli-key"),
        ] {
            let mut client = TitleClient::for_model_entry(
                // ubs:ignore[rust.ownership.unwrap-expect] -- Every authorized fixture role must resolve.
                role,
                original,
                Some(" foreground-cli-key "),
                &auth,
                None,
            )
            .expect("role has its own authorized credential");
            let provider =
                RecordingProvider::new(vec![Reply::Events(vec![done("Fix editor startup")])]);
            Arc::get_mut(&mut client) // ubs:ignore[rust.ownership.unwrap-expect] -- The newly constructed fixture client has one owner.
                .expect("unique newly constructed client")
                .provider = provider.clone();
            assert!(
                client
                    .generate(prepare(&client, exchange()))
                    .await
                    .is_some()
            ); // ubs:ignore[rust.panic.assert-macros] -- An authorized role must generate a title.
            let requests = provider.requests.lock().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- Poisoned fixture state must fail this test.
            assert_eq!(requests[0].options.api_key.as_deref(), Some(expected)); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Recorded credential isolation regression oracle.
            assert_eq!(
                requests[0].options.max_tokens,
                Some(role.model.max_tokens.min(96))
            ); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Recorded model limit regression oracle.
            assert_eq!(
                // ubs:ignore[rust.panic.assert-macros] -- Configured headers must reach the title provider.
                requests[0] // ubs:ignore[rust.panic.direct-indexing] -- A successful fixture request must have been recorded.
                    .options
                    .headers
                    .get("x-title-route")
                    .map(String::as_str),
                Some("tiny"),
            );
            assert_eq!(requests[0].options.thinking_level, Some(ThinkingLevel::Off)); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Recorded reasoning setting regression oracle.
            assert!(!requests[0].prompt().contains(expected)); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- Credentials must stay out of the prompt.
        }
        let missing = entry(
            "title-uncredentialed",
            "https://uncredentialed.invalid/v1",
            None,
        );
        assert!(
            // ubs:ignore[rust.panic.assert-macros] -- Unrelated roles cannot borrow the foreground override.
            TitleClient::for_model_entry(
                &missing,
                &primary,
                Some("foreground-cli-key"),
                &auth,
                None,
            )
            .is_none()
        );
        let mut local = entry("title-local", "http://127.0.0.1:11434/v1", None);
        local.auth_header = false;
        assert!(TitleClient::for_model_entry(&local, &primary, None, &auth, None).is_some()); // ubs:ignore[rust.panic.assert-macros] -- Credential-free local roles must remain usable.
    });
}

async fn saved_session(root: &Path, filename: &'static str) -> Session {
    let mut session = Session::create_with_dir(Some(root.to_path_buf()));
    session.path = Some(root.join(filename)); // ubs:ignore[rust.security.path-traversal] -- Every caller supplies a fixed fixture basename under its temporary root.
    session.save().await.expect("persist empty session"); // ubs:ignore[rust.ownership.unwrap-expect] -- The durability fixture requires an initial saved session.
    session
}

fn agent_session(provider: Arc<dyn Provider>, session: Session, save: bool) -> AgentSession {
    AgentSession::new(
        agent(provider),
        Arc::new(asupersync::sync::Mutex::new(session)),
        save,
        ResolvedCompactionSettings::default(),
    )
}

#[allow(clippy::future_not_send)]
async fn persist_exchange(agent_session: &mut AgentSession, messages: Vec<Message>) {
    let cx = crate::agent_cx::AgentCx::for_request();
    let mut session = agent_session.session.lock(cx.cx()).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture setup requires the session lock.
    for message in &messages {
        let id = session.append_model_message(message.clone());
        if matches!(message, Message::User(_)) {
            crate::memory::extraction::record_authored_input(&mut session, &id)
                .expect("record fixture operator input"); // ubs:ignore[rust.ownership.unwrap-expect] -- Authored fixture messages need the same durable provenance as actual input.
        }
    }
    session
        .save()
        .await
        .expect("persist completed foreground exchange"); // ubs:ignore[rust.ownership.unwrap-expect] -- A setup save failure must fail the durability test.
    drop(session);
    agent_session.agent.replace_messages(messages);
}

fn prepare_refresh(
    client: &TitleClient,
    owner: &AgentSession,
    interval: u32,
) -> Option<PreparedTitleRefresh> {
    let session = owner.session.try_lock().expect("read idle fixture session"); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture preparation must not race a foreground writer.
    client
        .prepare_refresh_with_agent(&owner.agent, &session, interval)
        .expect("screen refresh evidence") // ubs:ignore[rust.ownership.unwrap-expect] -- The fixture evidence is within the privacy budget.
}

#[test]
#[allow(clippy::future_not_send)]
fn refresh_counts_authored_durable_prompts_and_checkpoints_a_fitting_title() {
    run_async(async {
        let dir = tempfile::tempdir().unwrap();
        let session = saved_session(dir.path(), "refresh-fit.jsonl").await;
        let id = session.header.id.clone();
        let path = session.path.clone().unwrap();
        let provider = RecordingProvider::new(vec![Reply::Events(vec![done(r#"{"fits":0.9}"#)])]);
        let client = client(provider.clone(), None);
        let mut owner = agent_session(provider.clone(), session, true);
        persist_exchange(&mut owner, exchange()).await;
        assert!(
            save_if_unnamed(&owner, &id, "Fix editor startup", None)
                .await
                .unwrap()
        );

        // Generated user-role nudges are retained in the transcript but have
        // no authored-input marker and must not advance the refresh interval.
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = owner.session.lock(cx.cx()).await.unwrap();
        session.append_model_message(user("Generated recovery advice with substantial text"));
        session.append_model_message(Message::assistant(assistant("Recovery complete")));
        session.save().await.unwrap();
        drop(session);
        assert!(prepare_refresh(&client, &owner, 1).is_none());

        persist_exchange(
            &mut owner,
            vec![
                user("Add regression coverage"),
                Message::assistant(assistant("Tests added")),
            ],
        )
        .await;
        assert!(prepare_refresh(&client, &owner, 2).is_none());
        persist_exchange(
            &mut owner,
            vec![
                user("Verify the cold start"),
                Message::assistant(assistant("Startup verified")),
            ],
        )
        .await;
        // The live context contains only the latest exchange. The earlier
        // authored prompt still counts through the durable branch history.
        let prepared = prepare_refresh(&client, &owner, 2).expect("two durable operator prompts");
        let outcome = client
            .refresh(prepared)
            .await
            .expect("valid fit assessment");
        assert!(outcome.title.is_none());
        assert!(
            save_refresh_if_current(&owner, &outcome, None)
                .await
                .unwrap()
        );
        let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
        assert_eq!(reopened.get_name().as_deref(), Some("Fix editor startup"));
        assert_eq!(reopened.name_source(), Some(SessionTitleSource::Auto));
        owner.session = Arc::new(asupersync::sync::Mutex::new(reopened));
        assert!(prepare_refresh(&client, &owner, 1).is_none());
        persist_exchange(
            &mut owner,
            vec![
                user("Check launch configuration"),
                Message::assistant(assistant("Configuration checked")),
            ],
        )
        .await;
        persist_exchange(
            &mut owner,
            vec![
                user("continue"),
                Message::assistant(assistant("Already verified")),
            ],
        )
        .await;
        assert!(prepare_refresh(&client, &owner, 2).is_none());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn stale_title_refresh_screens_full_fields_before_bounding_both_model_requests() {
    run_async(async {
        let dir = tempfile::tempdir().unwrap();
        let session = saved_session(dir.path(), "refresh-stale.jsonl").await;
        let id = session.header.id.clone();
        let provider = RecordingProvider::new(vec![
            Reply::Events(vec![done(r#"{"fits":0.1}"#)]),
            Reply::Events(vec![done("Repair account provisioning")]),
        ]);
        let settings = crate::secrets::SecretsSettings {
            mode: Some("obfuscate".to_string()),
            extra_patterns: Some(vec![r"^ACME-\d{6}$".to_string()]),
        };
        let client = client(provider.clone(), Some(&settings));
        let mut owner = agent_session(provider.clone(), session, true);
        persist_exchange(&mut owner, exchange()).await;
        assert!(
            save_if_unnamed(&owner, &id, "Fix editor startup", None)
                .await
                .unwrap()
        );
        let secret = "refreshCredentialValue123456";
        persist_exchange(
            &mut owner,
            vec![
                Message::User(UserMessage {
                    content: UserContent::Blocks(vec![
                        ContentBlock::Text(TextContent::new("ACME-123456")),
                        ContentBlock::Text(TextContent::new(format!(
                            "Repair account echo {secret}"
                        ))),
                        ContentBlock::Image(ImageContent {
                            data: "PRIVATE-IMAGE".to_string(),
                            mime_type: "image/png".to_string(),
                        }),
                    ]),
                    timestamp: 0,
                }),
                Message::assistant(AssistantMessage {
                    content: vec![
                        ContentBlock::Thinking(ThinkingContent {
                            thinking: "PRIVATE-REASONING".to_string(),
                            thinking_signature: None,
                        }),
                        ContentBlock::Text(TextContent::new(format!(
                            "{} API_KEY={secret}",
                            "Account configuration repaired. ".repeat(100),
                        ))),
                    ],
                    ..Default::default()
                }),
            ],
        )
        .await;
        let prepared = prepare_refresh(&client, &owner, 1).expect("new operator task");
        let outcome = client
            .refresh(prepared)
            .await
            .expect("judge and replacement complete");
        assert_eq!(
            outcome.title.as_deref(),
            Some("Repair account provisioning")
        );
        assert!(
            save_refresh_if_current(&owner, &outcome, None)
                .await
                .unwrap()
        );
        assert_eq!(
            current_name(&owner).await.as_deref(),
            Some("Repair account provisioning")
        );
        assert!(prepare_refresh(&client, &owner, 1).is_none());
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert!(request.prompt().contains("echo <pi-secret:redacted>"));
            assert!(!request.prompt().contains(secret));
            assert!(!request.prompt().contains("ACME-"));
            assert!(!request.prompt().contains("PRIVATE-"));
        }
        let judgment: serde_json::Value = serde_json::from_str(requests[0].prompt()).unwrap();
        let evidence_chars: usize = judgment["state"]["recent"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["text"].as_str().unwrap().chars().count())
            .sum();
        assert!(evidence_chars <= REFRESH_CONTEXT_CHARS);
    });
}

#[test]
#[allow(clippy::future_not_send)]
fn title_refresh_cannot_overwrite_manual_names_or_changed_branch_positions() {
    run_async(async {
        let dir = tempfile::tempdir().unwrap();
        for (file, change) in [
            ("refresh-rename.jsonl", "rename"),
            ("refresh-branch.jsonl", "branch"),
            ("refresh-cancel.jsonl", "cancel"),
        ] {
            let session = saved_session(dir.path(), file).await;
            let id = session.header.id.clone();
            let provider = RecordingProvider::new(Vec::new());
            let client = client(provider.clone(), None);
            let mut owner = agent_session(provider.clone(), session, true);
            persist_exchange(&mut owner, exchange()).await;
            assert!(
                save_if_unnamed(&owner, &id, "Original title", None)
                    .await
                    .unwrap()
            );
            persist_exchange(
                &mut owner,
                vec![
                    user("Implement a different task"),
                    Message::assistant(assistant("New task complete")),
                ],
            )
            .await;
            let prepared = prepare_refresh(&client, &owner, 1).expect("eligible generated name");
            let outcome = TitleRefreshOutcome {
                target: prepared.target,
                title: Some("Late generated replacement".to_string()),
            };
            let (cancellation, _registration) = AbortHandle::new_pair();
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = owner.session.lock(cx.cx()).await.unwrap();
            match change {
                "rename" => {
                    session.set_name("Original title");
                }
                "branch" => {
                    assert!(session.navigate_to(&outcome.target.name_entry_id));
                }
                _ => cancellation.abort(),
            }
            session.save().await.unwrap();
            drop(session);
            assert!(
                !save_refresh_if_current(&owner, &outcome, Some(&cancellation))
                    .await
                    .unwrap()
            );
            assert_eq!(
                current_name(&owner).await.as_deref(),
                Some("Original title")
            );
            if change == "rename" {
                assert!(prepare_refresh(&client, &owner, 1).is_none());
            }
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        }
    });
}

#[test]
fn title_refresh_context_keeps_the_latest_user_anchor_within_unicode_budget() {
    let mut turns = vec![("user", "修复账户".repeat(100))];
    turns.extend((0..12).map(|_| ("assistant", "检查完成".repeat(250))));
    let bounded = bounded_recent_turns(turns);
    assert_eq!(bounded.first().map(|(role, _)| *role), Some("user"));
    assert!(
        bounded
            .iter()
            .map(|(_, text)| text.chars().count())
            .sum::<usize>()
            <= REFRESH_CONTEXT_CHARS
    );
}

#[test]
fn direct_prompt_provenance_is_available_without_memory_and_excludes_generated_input() {
    run_async(async {
        let dir = tempfile::tempdir().unwrap();
        let session = saved_session(dir.path(), "operator-provenance.jsonl").await;
        let provider = RecordingProvider::new(vec![
            Reply::Events(vec![done("First task complete")]),
            Reply::Events(vec![done("Generated continuation complete")]),
            Reply::Events(vec![done("Structured task complete")]),
        ]);
        let mut owner = agent_session(provider, session, true);
        owner
            .run_text_with_abort("First operator task".to_string(), None, |_| {})
            .await
            .unwrap();
        owner
            .agent
            .set_magic_keyword_scan_override(Some(String::new()));
        owner
            .run_text_with_abort("Generated continuation".to_string(), None, |_| {})
            .await
            .unwrap();
        owner
            .run_with_content(
                vec![ContentBlock::Text(TextContent::new(
                    "Structured operator task",
                ))],
                |_| {},
            )
            .await
            .unwrap();
        let session = owner.session.try_lock().unwrap();
        let marked: Vec<_> = session
            .entries
            .iter()
            .filter_map(|entry| {
                let SessionEntry::Custom(entry) = entry else {
                    return None;
                };
                if entry.custom_type != crate::memory::extraction::INPUT_ENTRY {
                    return None;
                }
                let id = entry.data.as_ref()?.get("messageId")?.as_str()?;
                let SessionEntry::Message(input) = session.get_entry(id)? else {
                    return None;
                };
                let SessionMessage::User { content, .. } = &input.message else {
                    return None;
                };
                Some(content)
            })
            .collect();
        assert_eq!(marked.len(), 2);
        assert!(marked.iter().all(|content| substantive_prompt(content)));
        assert!(!marked.iter().any(|content| {
            matches!(content, UserContent::Text(text) if text == "Generated continuation")
        }));
    });
}

#[test]
#[allow(clippy::future_not_send)]
fn pending_refresh_is_cancelled_when_the_user_keeps_the_same_name() {
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(Box::pin(async {
        let dir = tempfile::tempdir().unwrap();
        let session = saved_session(dir.path(), "refresh-pending.jsonl").await;
        let id = session.header.id.clone();
        let (provider, sender, state) = pending_provider();
        let client = client(provider.clone(), None);
        let mut owner = agent_session(provider.clone(), session, true);
        persist_exchange(&mut owner, exchange()).await;
        assert!(
            save_if_unnamed(&owner, &id, "Original title", None)
                .await
                .unwrap()
        );
        persist_exchange(
            &mut owner,
            vec![
                user("Work on a different task"),
                Message::assistant(assistant("Task complete")),
            ],
        )
        .await;
        let mut controller = AutoTitleController::default();
        assert!(
            controller
                .tick(&owner, Some(&client), 1, &handle)
                .await
                .is_none()
        );
        wait_until(|| state.polled.load(Ordering::SeqCst)).await;
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = owner.session.lock(cx.cx()).await.unwrap();
        session.set_name("Original title");
        session.save().await.unwrap();
        drop(session);
        assert!(
            controller
                .tick(&owner, Some(&client), 1, &handle)
                .await
                .is_none()
        );
        wait_until(|| state.dropped.load(Ordering::SeqCst)).await;
        assert!(sender.send(done(r#"{"fits":0.1}"#)).is_err());
        assert_eq!(
            current_name(&owner).await.as_deref(),
            Some("Original title")
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }));
}

async fn current_name(agent_session: &AgentSession) -> Option<String> {
    let cx = crate::agent_cx::AgentCx::for_request();
    agent_session
        .session
        .lock(cx.cx())
        .await
        .unwrap()
        .get_name() // ubs:ignore[rust.ownership.unwrap-expect] -- This observation must not hide a failed test-session lock.
}

async fn wait_until(predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "background title operation did not settle"
        ); // ubs:ignore[rust.panic.assert-macros] -- Bounded asynchronous test wait.
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(2)).await;
    }
}

async fn wait_for_title(
    controller: &mut AutoTitleController,
    agent_session: &AgentSession,
    client: &Arc<TitleClient>,
    runtime: &RuntimeHandle,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(title) = controller
            .tick(agent_session, Some(client), 0, runtime)
            .await
        {
            return title;
        }
        assert!(
            Instant::now() < deadline,
            "automatic title was never published"
        ); // ubs:ignore[rust.panic.assert-macros] -- Bounded publication regression oracle.
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(2)).await;
    }
}

#[test]
fn controller_publishes_one_durable_title_and_never_charges_for_later_turns() {
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(Box::pin(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let session = saved_session(dir.path(), "conversation.jsonl").await;
        let path = session.path.clone().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The saved fixture must have a path.
        let mut controller = AutoTitleController::for_session(&session);
        let provider =
            RecordingProvider::new(vec![Reply::Events(vec![done("Fix editor startup")])]);
        let client = client(provider.clone(), None);
        let mut owner = agent_session(provider.clone(), session, true);
        persist_exchange(&mut owner, exchange()).await;

        let title = wait_for_title(&mut controller, &owner, &client, &handle).await;
        assert_eq!(title, "Fix editor startup"); // ubs:ignore[rust.panic.assert-macros] -- Publication regression oracle.
        assert_eq!(current_name(&owner).await.as_deref(), Some(title.as_str())); // ubs:ignore[rust.panic.assert-macros] -- Published runtime metadata must match.
        let reopened = Session::open(path.to_str().unwrap()).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Reopening the UTF-8 fixture path verifies durable publication.
        assert_eq!(reopened.get_name().as_deref(), Some(title.as_str())); // ubs:ignore[rust.panic.assert-macros] -- Published disk metadata must match.
        assert_eq!(
            // ubs:ignore[rust.panic.assert-macros] -- Exactly one durable name transition is required.
            reopened
                .entries
                .iter()
                .filter(|entry| matches!(entry, SessionEntry::SessionInfo(_)))
                .count(),
            1,
            "exactly one durable name transition",
        );
        assert!(owner.ensure_provider_reentry_allowed().is_ok()); // ubs:ignore[rust.panic.assert-macros] -- Successful publication must leave admission available.

        let mut later = exchange();
        later.push(user("Now improve terminal rendering"));
        later.push(Message::assistant(assistant(
            "The terminal redraw was improved.",
        )));
        owner.agent.replace_messages(later);
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- Later turns must not request another title.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Automatic titling has one provider attempt.
    }));
}

#[test]
fn resumed_and_ephemeral_sessions_do_not_start_automatic_title_requests() {
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(Box::pin(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let provider = RecordingProvider::new(Vec::new());
        let client = client(provider.clone(), None);
        let session = saved_session(dir.path(), "resumed.jsonl").await;
        let path = session.path.clone().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The saved fixture must have a path.
        let mut original = agent_session(provider.clone(), session, true);
        persist_exchange(&mut original, exchange()).await;

        let resumed = Session::open(path.to_str().unwrap()).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- This fixture must resume its saved exchange.
        let mut controller = AutoTitleController::for_session(&resumed);
        let mut owner = agent_session(provider.clone(), resumed, true);
        owner.agent.replace_messages(exchange());
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- Resumed sessions are ineligible for automatic titling.

        let ephemeral = Session::in_memory();
        let mut controller = AutoTitleController::for_session(&ephemeral);
        let mut owner = agent_session(provider.clone(), ephemeral, false);
        owner.agent.replace_messages(exchange());
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- Ephemeral sessions are ineligible for automatic titling.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0); // ubs:ignore[rust.panic.assert-macros] -- Ineligible sessions must never call the provider.
    }));
}

#[test]
fn failed_foreground_turn_is_not_named_and_first_success_remains_eligible() {
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(Box::pin(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let session = saved_session(dir.path(), "retry.jsonl").await;
        let mut controller = AutoTitleController::for_session(&session);
        let provider =
            RecordingProvider::new(vec![Reply::Events(vec![done("Recover editor startup")])]);
        let client = client(provider.clone(), None);
        let mut owner = agent_session(provider.clone(), session, true);
        let mut failed = assistant("Partial foreground response");
        failed.stop_reason = StopReason::Error;
        failed.error_message = Some("provider connection failed".to_string());
        persist_exchange(
            &mut owner,
            vec![user("Fix editor startup"), Message::assistant(failed)],
        )
        .await;
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- A failed foreground turn must not be named.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0); // ubs:ignore[rust.panic.assert-macros] -- A failed foreground turn must not spend a title request.

        let recovered = exchange();
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = owner.session.lock(cx.cx()).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Recovery fixture setup requires the session lock.
        session.append_model_message(recovered.last().unwrap().clone()); // ubs:ignore[rust.ownership.unwrap-expect] -- The complete fixture exchange contains a terminal response.
        session
            .save()
            .await
            .expect("persist recovered foreground response"); // ubs:ignore[rust.ownership.unwrap-expect] -- The recovered foreground response must be durable.
        drop(session);
        owner.agent.replace_messages(recovered);
        assert_eq!(
            // ubs:ignore[rust.panic.assert-macros] -- The first successful exchange remains eligible.
            wait_for_title(&mut controller, &owner, &client, &handle).await,
            "Recover editor startup",
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Recovery permits exactly one title request.
    }));
}

#[test]
fn a_failed_title_request_is_not_retried_on_subsequent_driver_ticks() {
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(Box::pin(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let session = saved_session(dir.path(), "optional-failure.jsonl").await;
        let mut controller = AutoTitleController::for_session(&session);
        let (provider, sender, state) = pending_provider();
        let client = client(provider.clone(), None);
        let mut owner = agent_session(provider.clone(), session, true);
        persist_exchange(&mut owner, exchange()).await;
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- Starting a pending request cannot publish a title.
        wait_until(|| state.polled.load(Ordering::SeqCst)).await;
        sender.send(delta("unfinished title")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The active fixture stream must accept its scripted response.
        wait_until(|| state.dropped.load(Ordering::SeqCst)).await;
        for _ in 0..3 {
            assert!(
                controller
                    .tick(&owner, Some(&client), 0, &handle)
                    .await
                    .is_none()
            ); // ubs:ignore[rust.panic.assert-macros] -- Driver ticks cannot retry a failed optional request.
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(2)).await;
        }
        assert!(current_name(&owner).await.is_none()); // ubs:ignore[rust.panic.assert-macros] -- A partial response cannot name the session.
        assert!(owner.ensure_provider_reentry_allowed().is_ok()); // ubs:ignore[rust.panic.assert-macros] -- Optional provider failure must leave foreground admission available.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Driver ticks must preserve the one-attempt limit.
    }));
}

#[test]
fn a_manual_name_wins_over_an_already_completed_background_suggestion() {
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(Box::pin(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let session = saved_session(dir.path(), "manual.jsonl").await;
        let path = session.path.clone().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The saved fixture must have a path.
        let mut controller = AutoTitleController::for_session(&session);
        let (provider, sender, state) = pending_provider();
        let client = client(provider.clone(), None);
        let mut owner = agent_session(provider.clone(), session, true);
        persist_exchange(&mut owner, exchange()).await;
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- Starting the pending request cannot publish a title.
        wait_until(|| state.polled.load(Ordering::SeqCst)).await;
        sender
            .send(done("Automatic suggestion"))
            .expect("deliver title response"); // ubs:ignore[rust.ownership.unwrap-expect] -- The active fixture stream must accept its response.
        wait_until(|| state.dropped.load(Ordering::SeqCst)).await;

        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = owner.session.lock(cx.cx()).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The manual-name fixture requires the session lock.
        session.set_name("My explicit session name");
        session.save().await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The manual name must be saved before publication races it.
        drop(session);
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- An existing manual name must win the race.
        assert_eq!(
            current_name(&owner).await.as_deref(),
            Some("My explicit session name")
        ); // ubs:ignore[rust.panic.assert-macros] -- Runtime metadata must preserve the manual name.
        assert_eq!(
            // ubs:ignore[rust.panic.assert-macros] -- Disk metadata must preserve the manual name.
            Session::open(path.to_str().unwrap())
                .await
                .unwrap()
                .get_name()
                .as_deref(), // ubs:ignore[rust.ownership.unwrap-expect] -- Reopen the saved UTF-8 fixture path to verify durability.
            Some("My explicit session name"),
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- The manual-name race must not trigger another request.
    }));
}

#[test]
fn replacing_the_session_retires_the_old_provider_stream_without_naming_either_session() {
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(Box::pin(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let session = saved_session(dir.path(), "old.jsonl").await;
        let old_path = session.path.clone().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The saved fixture must have a path.
        let mut controller = AutoTitleController::for_session(&session);
        let (provider, sender, state) = pending_provider();
        let client = client(provider.clone(), None);
        let mut owner = agent_session(provider.clone(), session, true);
        persist_exchange(&mut owner, exchange()).await;
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- Starting the pending request cannot publish a title.
        wait_until(|| state.polled.load(Ordering::SeqCst)).await;

        let replacement = saved_session(dir.path(), "new.jsonl").await;
        let new_path = replacement.path.clone().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The replacement fixture must have a saved path.
        owner.session = Arc::new(asupersync::sync::Mutex::new(replacement));
        owner.agent.clear_messages();
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- A replacement cannot receive the old title.
        wait_until(|| state.dropped.load(Ordering::SeqCst)).await;
        assert!(
            sender.send(done("Stale old title")).is_err(),
            "old receiver was retired"
        ); // ubs:ignore[rust.panic.assert-macros] -- Replacing the session must cancel provider ownership.
        assert!(current_name(&owner).await.is_none()); // ubs:ignore[rust.panic.assert-macros] -- The replacement runtime metadata must remain unnamed.
        for path in [old_path, new_path] {
            assert!(
                Session::open(path.to_str().unwrap())
                    .await
                    .unwrap()
                    .get_name()
                    .is_none()
            ); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Reopening both fixture paths must show no stale title.
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1); // ubs:ignore[rust.panic.assert-macros] -- Session replacement cannot retry the stale request.
    }));
}

#[test]
fn another_persistence_quarantine_discards_a_ready_title_before_publication() {
    let runtime = runtime();
    let handle = runtime.handle();
    runtime.block_on(Box::pin(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let session = saved_session(dir.path(), "quarantined.jsonl").await;
        let path = session.path.clone().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The saved fixture must have a path.
        let mut controller = AutoTitleController::for_session(&session);
        let (provider, sender, state) = pending_provider();
        let client = client(provider.clone(), None);
        let mut owner = agent_session(provider.clone(), session, true);
        persist_exchange(&mut owner, exchange()).await;
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- Starting the pending request cannot publish a title.
        wait_until(|| state.polled.load(Ordering::SeqCst)).await;
        sender.send(done("Must remain unpublished")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The active fixture stream must accept its response.
        wait_until(|| state.dropped.load(Ordering::SeqCst)).await;

        owner
            .provider_admission_gate()
            .block("another transition has an indeterminate save".to_string());
        assert!(
            controller
                .tick(&owner, Some(&client), 0, &handle)
                .await
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros] -- A quarantine must prevent title publication.
        assert!(current_name(&owner).await.is_none()); // ubs:ignore[rust.panic.assert-macros] -- Quarantined runtime metadata must remain unchanged.
        assert!(
            Session::open(path.to_str().unwrap())
                .await
                .unwrap()
                .get_name()
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Reopening the fixture must show no quarantined publication.
        assert!(
            // ubs:ignore[rust.panic.assert-macros] -- Optional titling cannot clear another persistence quarantine.
            owner.ensure_provider_reentry_allowed().is_err(),
            "title cannot clear another quarantine"
        );
    }));
}

#[test]
fn failed_title_persistence_keeps_live_metadata_unchanged_and_quarantines_reentry() {
    run_async(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let session = saved_session(dir.path(), "durable.jsonl").await;
        let path = session.path.clone().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The saved fixture must have a path.
        let id = session.header.id.clone();
        let provider = RecordingProvider::new(Vec::new());
        let mut owner = agent_session(provider.clone(), session, true);
        persist_exchange(&mut owner, exchange()).await;
        let blocker = dir.path().join("blocked.jsonl");
        std::fs::create_dir(&blocker).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The fixture must create an unwritable file destination.
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = owner.session.lock(cx.cx()).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Failure fixture setup requires the session lock.
        let header_before = serde_json::to_value(&session.header).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture metadata must be serializable for comparison.
        let entries_before = serde_json::to_value(&session.entries).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture history must be serializable for comparison.
        session.path = Some(blocker);
        drop(session);
        let messages_before = serde_json::to_value(owner.agent.messages()).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture runtime history must be serializable for comparison.

        let error = save_if_unnamed(&owner, &id, "Undurable suggestion", None)
            .await
            .expect_err("both writes to a directory must fail");
        assert!(error.is_session_persistence()); // ubs:ignore[rust.panic.assert-macros] -- A failed save must report a persistence error.
        let session = owner.session.lock(cx.cx()).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Failure observations require the session lock.
        assert_eq!(
            serde_json::to_value(&session.header).unwrap(),
            header_before
        ); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Failed saves must preserve serializable live metadata.
        assert_eq!(
            serde_json::to_value(&session.entries).unwrap(),
            entries_before
        ); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Failed saves must preserve serializable live history.
        assert!(session.get_name().is_none()); // ubs:ignore[rust.panic.assert-macros] -- Undurable titles cannot become live metadata.
        drop(session);
        assert_eq!(
            serde_json::to_value(owner.agent.messages()).unwrap(),
            messages_before
        ); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Failed saves must preserve serializable runtime history.
        assert!(owner.ensure_provider_reentry_allowed().is_err()); // ubs:ignore[rust.panic.assert-macros] -- An unresolved persistence failure must quarantine provider reentry.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0); // ubs:ignore[rust.panic.assert-macros] -- Metadata failure cannot invoke a provider.
        assert!(
            Session::open(path.to_str().unwrap())
                .await
                .unwrap()
                .get_name()
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Reopening the original fixture must show unchanged durable metadata.
    });
}

#[test]
fn cancelling_while_waiting_for_admission_prevents_title_persistence() {
    run_async(async {
        let dir = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Isolated fixture storage is required.
        let session = saved_session(dir.path(), "cancelled-save.jsonl").await;
        let path = session.path.clone().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The saved fixture must have a path.
        let id = session.header.id.clone();
        let provider = RecordingProvider::new(Vec::new());
        let mut owner = agent_session(provider, session, true);
        persist_exchange(&mut owner, exchange()).await;
        let cx = crate::agent_cx::AgentCx::for_request();
        let gate = owner.provider_admission_gate();
        let permit = gate.acquire(cx.cx()).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The fixture must hold provider admission to exercise the race.
        let (cancellation, _registration) = AbortHandle::new_pair();
        let mut publish = Box::pin(save_if_unnamed(
            &owner,
            &id,
            "Cancelled suggestion",
            Some(&cancellation),
        ));
        assert!(
            futures::poll!(publish.as_mut()).is_pending(),
            "provider admission is held"
        ); // ubs:ignore[rust.panic.assert-macros] -- The publication future must actually wait for admission.
        cancellation.abort();
        drop(permit);
        assert!(!publish.await.unwrap()); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Cancellation must finish cleanly without publishing.
        assert!(current_name(&owner).await.is_none()); // ubs:ignore[rust.panic.assert-macros] -- Cancelled publication must leave runtime metadata unnamed.
        assert!(
            Session::open(path.to_str().unwrap())
                .await
                .unwrap()
                .get_name()
                .is_none()
        ); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Reopening the fixture must show no cancelled publication.
        assert!(owner.ensure_provider_reentry_allowed().is_ok()); // ubs:ignore[rust.panic.assert-macros] -- Clean cancellation must leave foreground admission available.
    });
}
