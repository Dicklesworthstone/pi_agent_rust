//! Production SDK entrypoint regressions using the existing provider fixture.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug)]
enum Entrypoint {
    Prompt,
    PromptWithAbort,
    Continue,
    ContinueWithAbort,
}

const ENTRYPOINTS: [Entrypoint; 4] = [
    Entrypoint::Prompt,
    Entrypoint::PromptWithAbort,
    Entrypoint::Continue,
    Entrypoint::ContinueWithAbort,
];

async fn invoke(
    handle: &mut AgentSessionHandle,
    entrypoint: Entrypoint,
    callback: EventSubscriber,
) -> Result<AssistantMessage> {
    let (_abort, signal) = AbortHandle::new();
    match entrypoint {
        Entrypoint::Prompt => {
            handle
                .prompt("one user input", move |event| callback(event))
                .await
        }
        Entrypoint::PromptWithAbort => {
            handle
                .prompt_with_abort("one user input", signal, move |event| callback(event))
                .await
        }
        Entrypoint::Continue => handle.continue_turn(move |event| callback(event)).await,
        Entrypoint::ContinueWithAbort => {
            handle
                .continue_turn_with_abort(signal, move |event| callback(event))
                .await
        }
    }
}

#[derive(Default)]
struct PromptObservations {
    prompts: Vec<Option<String>>,
    thinking_levels: Vec<Option<crate::model::ThinkingLevel>>,
    messages: Vec<Vec<Message>>,
}

struct PromptProbeProvider {
    inner: Arc<dyn crate::provider::Provider>,
    observations: Arc<Mutex<PromptObservations>>,
    pending_call: Option<usize>,
}

#[async_trait::async_trait]
impl crate::provider::Provider for PromptProbeProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn api(&self) -> &str {
        self.inner.api()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    async fn stream(
        &self,
        context: &crate::provider::Context<'_>,
        options: &crate::provider::StreamOptions,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<crate::model::StreamEvent>> + Send>>,
    > {
        let call = {
            let mut observed = self.observations.lock().unwrap();
            let call = observed.prompts.len();
            observed
                .prompts
                .push(context.system_prompt.as_deref().map(str::to_string));
            observed.thinking_levels.push(options.thinking_level);
            observed.messages.push(context.messages.to_vec());
            call
        };
        if self.pending_call == Some(call) {
            futures::future::pending::<()>().await;
        }
        self.inner.stream(context, options).await
    }
}

fn record_system_prompts(
    handle: &mut AgentSessionHandle,
    pending_call: Option<usize>,
) -> Arc<Mutex<PromptObservations>> {
    let observations = Arc::new(Mutex::new(PromptObservations::default()));
    let provider = PromptProbeProvider {
        inner: handle.session.agent.provider(),
        observations: Arc::clone(&observations),
        pending_call,
    };
    handle.session.agent.set_provider(Arc::new(provider));
    observations
}

/// Use the real native extension dispatcher. Its custom message is observable
/// evidence that recovery has not replayed `before_agent_start` a second time.
fn install_system_prompt_hook(handle: &mut AgentSessionHandle, prompt: Option<&str>) {
    let output = prompt.map_or_else(
        || serde_json::json!({}),
        |prompt| {
            serde_json::json!({
                "systemPrompt": prompt,
                "messages": [{
                    "customType": "prompt-scope-hook",
                    "content": "hook ran once",
                    "display": false,
                }],
            })
        },
    );
    install_native_event_hooks(handle, &serde_json::json!({"before_agent_start": output}));
}

fn install_native_event_hooks(handle: &mut AgentSessionHandle, responses: &Value) {
    let manager = crate::extensions::ExtensionManager::new();
    let temp = tempdir().unwrap();
    let entry = temp.path().join("turn-system-prompt.native.json");
    let hooks = responses
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let descriptor = serde_json::json!({
        "id": "turn-system-prompt-test",
        "name": "turn-system-prompt-test",
        "version": "1.0.0",
        "apiVersion": crate::extensions::PROTOCOL_VERSION,
        "eventHooks": hooks,
        "eventResponses": responses,
    });
    std::fs::write(&entry, serde_json::to_vec(&descriptor).unwrap()).unwrap();
    run_async(async {
        manager.set_native_runtime(
            crate::extensions::NativeRustExtensionRuntimeHandle::start()
                .await
                .unwrap(),
        );
        manager
            .load_native_extensions(vec![
                crate::extensions::NativeRustExtensionLoadSpec::from_entry_path(&entry).unwrap(),
            ])
            .await
            .unwrap();
    });
    handle.session.extensions = Some(crate::extensions::ExtensionRegion::new(manager));
}

async fn prompt_with_optional_image(
    handle: &mut AgentSessionHandle,
    with_image: bool,
) -> Result<AssistantMessage> {
    if with_image {
        handle
            .prompt_with_images(
                "ultrathink orchestrate one logical turn",
                vec![ImageContent {
                    data: "aGVsbG8=".to_string(),
                    mime_type: "image/png".to_string(),
                }],
                |_| {},
            )
            .await
    } else {
        handle
            .prompt("ultrathink orchestrate one logical turn", |_| {})
            .await
    }
}

fn assert_prompt_hook_ran_once(handle: &AgentSessionHandle) {
    let messages = run_async(handle.messages()).unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(
                message,
                Message::Custom(custom) if custom.custom_type == "prompt-scope-hook"
            ))
            .count(),
        1,
        "retry/failover must not replay the hook's custom message",
    );
}

fn native_content_fixture() -> Vec<ContentBlock> {
    vec![
        ContentBlock::Text(TextContent::new("describe the first scene")),
        ContentBlock::Image(ImageContent {
            data: "Zmlyc3Q=".to_string(),
            mime_type: "image/png".to_string(),
        }),
        ContentBlock::Media(MediaContent {
            data: "YXVkaW8=".to_string(),
            mime_type: "audio/mpeg".to_string(),
            name: Some("voice.mp3".to_string()),
        }),
        ContentBlock::Text(TextContent::new("compare the final scene")),
        ContentBlock::Image(ImageContent {
            data: "c2Vjb25k".to_string(),
            mime_type: "image/jpeg".to_string(),
        }),
        ContentBlock::Media(MediaContent {
            data: "dmlkZW8=".to_string(),
            mime_type: "video/mp4".to_string(),
            name: Some("motion.mp4".to_string()),
        }),
        ContentBlock::Text(TextContent::new(" \t\n")),
    ]
}

fn saving_native_content_handle(
    dir: &Path,
    failures: usize,
) -> (
    AgentSessionHandle,
    Arc<AtomicUsize>,
    Arc<Mutex<PromptObservations>>,
) {
    let mut handle = saving_handle(dir).with_retry(Some(fast_retry_policy(1)));
    // Both doubles below keep the handle's model identity. A bare
    // `set_provider` resets image support fail-closed (77ef109ca), so
    // reinstall the selected model's capability as catalog-aware callers do.
    let accepts_images = handle.session.agent.model_accepts_images();
    let calls = Arc::new(AtomicUsize::new(0));
    let (name, model) = handle.model();
    handle
        .session
        .agent
        .set_provider(Arc::new(FlakyThenOkProvider {
            failures,
            calls: Arc::clone(&calls),
            name,
            model,
            input_tokens: 0,
        }));
    let observations = record_system_prompts(&mut handle, None);
    handle
        .session
        .agent
        .set_model_accepts_images(accepts_images);
    (handle, calls, observations)
}

fn install_native_content_hooks(handle: &mut AgentSessionHandle, input: &Value) {
    install_native_event_hooks(
        handle,
        &serde_json::json!({
            "input": input,
            "before_agent_start": {
                "systemPrompt": "native-content-hook",
                "messages": [{
                    "customType": "prompt-scope-hook",
                    "content": "hook ran once",
                    "display": false,
                }],
            },
        }),
    );
}

fn assert_one_native_user_message(messages: &[Message], expected: &[ContentBlock]) {
    let users = messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 1, "one user message for the logical turn");
    let UserContent::Blocks(actual) = &users[0].content else {
        panic!("native input must remain structured user content");
    };
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap(),
        "preserve native payloads, text slots, and their exact order"
    );
}

fn assert_native_user_persisted_once(handle: &AgentSessionHandle, expected: &[ContentBlock]) {
    let path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .expect("native prompt must create a saved session");
    let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
    assert_eq!(
        reopened
            .entries
            .iter()
            .filter(|entry| matches!(
                entry,
                crate::session::SessionEntry::Message(entry)
                    if matches!(&entry.message, crate::session::SessionMessage::User { .. })
            ))
            .count(),
        1,
        "recovery must not append another user entry to the session file"
    );
    assert_one_native_user_message(&reopened.to_messages_for_current_path(), expected);
}

#[test]
fn native_content_retries_preserve_order_and_persist_one_user_message() {
    let dir = tempdir().unwrap();
    let (mut handle, calls, observations) = saving_native_content_handle(dir.path(), 1);
    handle
        .session
        .agent
        .set_system_prompt(Some("ordinary-system".to_string()));
    install_native_content_hooks(&mut handle, &serde_json::json!({"action":"continue"}));
    let content = native_content_fixture();

    let result = run_async(handle.prompt_with_content(content.clone(), |_| {})).unwrap();

    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one transient retry");
    {
        let observed = observations.lock().unwrap();
        assert_eq!(observed.messages.len(), 2);
        assert_eq!(
            observed.prompts,
            vec![Some("native-content-hook".to_string()); 2]
        );
        for messages in &observed.messages {
            assert_one_native_user_message(messages, &content);
        }
    }
    assert_prompt_hook_ran_once(&handle);
    assert_eq!(
        handle.session.agent.system_prompt(),
        Some("ordinary-system")
    );
    assert_native_user_persisted_once(&handle, &content);
}

#[test]
fn native_input_hooks_replace_add_and_remove_text_and_images_without_losing_media() {
    let original = native_content_fixture();
    let replacements = [
        ImageContent {
            data: "bmV3LW9uZQ==".to_string(),
            mime_type: "image/webp".to_string(),
        },
        ImageContent {
            data: "bmV3LXR3bw==".to_string(),
            mime_type: "image/png".to_string(),
        },
        ImageContent {
            data: "bmV3LXRocmVl".to_string(),
            mime_type: "image/jpeg".to_string(),
        },
    ];
    let rewritten = ContentBlock::Text(TextContent::new("hook-selected description"));
    let cases = [
        (
            "replace",
            serde_json::json!({
                "action":"transform",
                "text":"hook-selected description",
                "images":&replacements[..2],
            }),
            vec![
                rewritten.clone(),
                ContentBlock::Image(replacements[0].clone()),
                original[2].clone(),
                ContentBlock::Image(replacements[1].clone()),
                original[5].clone(),
            ],
        ),
        (
            "add",
            serde_json::json!({
                "action":"transform",
                "text":"hook-selected description",
                "images":replacements,
            }),
            vec![
                rewritten,
                ContentBlock::Image(replacements[0].clone()),
                original[2].clone(),
                ContentBlock::Image(replacements[1].clone()),
                original[5].clone(),
                ContentBlock::Image(replacements[2].clone()),
            ],
        ),
        (
            "remove text and images",
            serde_json::json!({"action":"transform", "text":"", "images":[]}),
            vec![original[2].clone(), original[5].clone()],
        ),
        (
            "remove only images",
            serde_json::json!({"action":"transform", "images":[]}),
            vec![
                original[0].clone(),
                original[2].clone(),
                original[3].clone(),
                original[5].clone(),
                original[6].clone(),
            ],
        ),
    ];
    for (case, response, expected) in cases {
        let dir = tempdir().unwrap();
        let (mut handle, calls, observations) = saving_native_content_handle(dir.path(), 1);
        install_native_content_hooks(&mut handle, &response);

        let result = run_async(handle.prompt_with_content(original.clone(), |_| {}))
            .unwrap_or_else(|error| panic!("{case}: {error}"));

        assert_eq!(result.stop_reason, StopReason::Stop, "{case}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{case}");
        for messages in &observations.lock().unwrap().messages {
            assert_one_native_user_message(messages, &expected);
        }
        assert_prompt_hook_ran_once(&handle);
        assert_native_user_persisted_once(&handle, &expected);
    }
}

#[test]
fn media_only_prompts_accept_hooks_that_add_text_and_images_without_existing_slots() {
    let original = native_content_fixture();
    let media = vec![original[2].clone(), original[5].clone()];
    let image = ImageContent {
        data: "bmV3LWltYWdl".to_string(),
        mime_type: "image/png".to_string(),
    };
    let cases = [
        (serde_json::json!({"action":"continue"}), media.clone()),
        (
            serde_json::json!({
                "action":"transform",
                "text":"describe these clips",
                "images":[image],
            }),
            vec![
                ContentBlock::Text(TextContent::new("describe these clips")),
                media[0].clone(),
                media[1].clone(),
                ContentBlock::Image(image),
            ],
        ),
    ];
    for (response, expected) in cases {
        let dir = tempdir().unwrap();
        let (mut handle, calls, observations) = saving_native_content_handle(dir.path(), 0);
        install_native_content_hooks(&mut handle, &response);
        let result = run_async(handle.prompt_with_content(media.clone(), |_| {})).unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_one_native_user_message(&observations.lock().unwrap().messages[0], &expected);
        assert_prompt_hook_ran_once(&handle);
        assert_native_user_persisted_once(&handle, &expected);
    }
}

#[test]
fn invalid_native_content_refuses_before_provider_calls_or_session_persistence() {
    let assistant_blocks = [
        ContentBlock::Thinking(ThinkingContent {
            thinking: "not user input".to_string(),
            thinking_signature: None,
        }),
        ContentBlock::RedactedThinking(crate::model::RedactedThinkingContent {
            data: "opaque".to_string(),
        }),
        ContentBlock::ToolCall(ToolCall {
            id: "not-a-user-call".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({"path":"private.txt"}),
            thought_signature: None,
        }),
    ];
    let mut invalid = vec![Vec::new()];
    invalid.extend(assistant_blocks.iter().cloned().map(|block| vec![block]));
    invalid.extend(
        assistant_blocks
            .into_iter()
            .map(|block| vec![ContentBlock::Text(TextContent::new("valid prefix")), block]),
    );
    for content in invalid {
        let dir = tempdir().unwrap();
        let (mut handle, calls, observations) = saving_native_content_handle(dir.path(), 0);
        // Invalid original content must not become accepted just because an
        // input hook could otherwise replace the visible text/images.
        install_native_content_hooks(
            &mut handle,
            &serde_json::json!({"action":"transform", "text":"replacement", "images":[]}),
        );
        let error = run_async(handle.prompt_with_content(content, |_| {})).unwrap_err();
        assert!(error.to_string().contains("PI_INPUT_CONTENT"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(observations.lock().unwrap().messages.is_empty());
        assert!(handle.session.agent.messages().is_empty());
        let store = handle.session_store();
        let session = store.try_lock().unwrap();
        assert!(session.entries.is_empty());
        assert!(session.path.is_none());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}

#[test]
fn input_hooks_cannot_turn_a_native_prompt_into_an_empty_persisted_turn() {
    let original = native_content_fixture();
    let content = vec![original[0].clone(), original[1].clone()];
    let dir = tempdir().unwrap();
    let (mut handle, calls, observations) = saving_native_content_handle(dir.path(), 0);
    install_native_content_hooks(
        &mut handle,
        &serde_json::json!({"action":"transform", "text":"", "images":[]}),
    );

    let error = run_async(handle.prompt_with_content(content, |_| {})).unwrap_err();

    assert!(error.to_string().contains("PI_INPUT_CONTENT"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(observations.lock().unwrap().messages.is_empty());
    assert!(handle.session.agent.messages().is_empty());
    let store = handle.session_store();
    let session = store.try_lock().unwrap();
    assert!(session.entries.is_empty());
    assert!(session.path.is_none());
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[test]
fn native_content_explicit_abort_prevents_admission_and_retry_reentry() {
    for pre_aborted in [true, false] {
        let dir = tempdir().unwrap();
        let (mut handle, calls, observations) = saving_native_content_handle(dir.path(), 1);
        install_native_content_hooks(&mut handle, &serde_json::json!({"action":"continue"}));
        let content = native_content_fixture();
        let (abort, signal) = AbortHandle::new();
        if pre_aborted {
            abort.abort();
        }
        let retry_starts = Arc::new(AtomicUsize::new(0));
        let retries = Arc::clone(&retry_starts);
        let result = run_async(handle.prompt_with_content_with_abort(
            content.clone(),
            signal,
            move |event| {
                if matches!(event, AgentEvent::AutoRetryStart { .. }) {
                    retries.fetch_add(1, Ordering::SeqCst);
                    abort.abort();
                }
            },
        ));
        assert!(matches!(result, Err(Error::Aborted)));
        if pre_aborted {
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(retry_starts.load(Ordering::SeqCst), 0);
            assert!(observations.lock().unwrap().messages.is_empty());
            assert!(handle.session.agent.messages().is_empty());
            let store = handle.session_store();
            let session = store.try_lock().unwrap();
            assert!(session.entries.is_empty());
            assert!(session.path.is_none());
            assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        } else {
            assert_eq!(calls.load(Ordering::SeqCst), 1, "no request after abort");
            assert_eq!(retry_starts.load(Ordering::SeqCst), 1);
            let observed = observations.lock().unwrap();
            assert_eq!(observed.messages.len(), 1);
            assert_one_native_user_message(&observed.messages[0], &content);
            drop(observed);
            assert_prompt_hook_ran_once(&handle);
            assert_native_user_persisted_once(&handle, &content);
        }
    }
}

#[test]
fn retry_preserves_extension_prompt_without_repeating_directives_or_leaking_to_next_input() {
    for with_image in [false, true] {
        for failures in [1, 2] {
            let (handle, calls) = flaky_handle(failures);
            let mut handle = handle.with_retry(Some(fast_retry_policy(1)));
            handle
                .session
                .agent
                .set_system_prompt(Some("base-system".to_string()));
            let prompts = record_system_prompts(&mut handle, None);
            install_system_prompt_hook(&mut handle, Some("hook-system"));

            let result = run_async(prompt_with_optional_image(&mut handle, with_image)).unwrap();
            assert_eq!(
                result.stop_reason,
                if failures == 1 {
                    StopReason::Stop
                } else {
                    StopReason::Error
                },
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            let expected = format!(
                "hook-system\n\n{}",
                crate::magic_keywords::ORCHESTRATE_DIRECTIVE,
            );
            assert_eq!(
                prompts.lock().unwrap().prompts,
                vec![Some(expected.clone()), Some(expected)],
            );
            assert_eq!(handle.session.agent.system_prompt(), Some("base-system"));
            assert_prompt_hook_ran_once(&handle);

            // The next hook makes no mutation. Neither the old override nor
            // its keyword directive may survive into this unrelated input.
            install_system_prompt_hook(&mut handle, None);
            run_async(handle.prompt("another question", |_| {})).unwrap();
            assert_eq!(
                prompts.lock().unwrap().prompts.last().unwrap().as_deref(),
                Some("base-system"),
            );
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            assert_prompt_hook_ran_once(&handle);
        }
    }
}

#[test]
fn dropping_first_or_retried_provider_future_restores_prompt_and_thinking() {
    for with_image in [false, true] {
        for pending_call in [0, 1] {
            let (handle, _) = flaky_handle(pending_call);
            let mut handle = handle.with_retry(Some(fast_retry_policy(1)));
            handle
                .session
                .agent
                .set_system_prompt(Some("base-system".to_string()));
            let prompts = record_system_prompts(&mut handle, Some(pending_call));
            handle.session.agent.stream_options_mut().thinking_level =
                Some(crate::model::ThinkingLevel::Off);
            handle
                .session
                .agent
                .set_keyword_max_thinking_level(crate::model::ThinkingLevel::High);
            install_system_prompt_hook(&mut handle, Some("hook-system"));
            run_async(async {
                let mut turn = Box::pin(prompt_with_optional_image(&mut handle, with_image));
                let reached_provider = futures::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(turn.as_mut(), cx).is_pending());
                    if prompts.lock().unwrap().prompts.len() == pending_call + 1 {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                });
                asupersync::time::timeout(
                    asupersync::time::wall_now(),
                    std::time::Duration::from_secs(5),
                    reached_provider,
                )
                .await
                .expect("the production turn must reach its pending provider attempt");
                drop(turn);
            });
            assert_eq!(handle.session.agent.system_prompt(), Some("base-system"));
            assert_eq!(
                handle.session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off),
                "dropping ultrathink must immediately restore the selected model baseline",
            );
            let expected = format!(
                "hook-system\n\n{}",
                crate::magic_keywords::ORCHESTRATE_DIRECTIVE,
            );
            assert_eq!(
                prompts.lock().unwrap().prompts,
                vec![Some(expected); pending_call + 1],
            );
            assert_eq!(
                prompts.lock().unwrap().thinking_levels,
                vec![Some(crate::model::ThinkingLevel::High); pending_call + 1],
            );
            assert_eq!(
                handle
                    .session
                    .agent
                    .messages()
                    .iter()
                    .filter(|message| matches!(
                        message,
                        Message::Custom(custom) if custom.custom_type == "prompt-scope-hook"
                    ))
                    .count(),
                1,
            );
            // 721500d25: dropping an admitted turn before its outcome and save
            // were observed fences provider re-entry on this handle until a
            // new or resumed session is started. The refusal must neither
            // reach the provider nor disturb the restored baseline above.
            install_system_prompt_hook(&mut handle, None);
            let before = serde_json::to_value(handle.session.agent.messages()).unwrap();
            let error = run_async(handle.prompt("after cancellation", |_| {}))
                .expect_err("a dropped admitted turn must fence provider re-entry");
            assert!(error.is_session_persistence(), "{error}");
            assert!(
                error.to_string().contains("SDK_TURN_INTERRUPTED"),
                "{error}"
            );
            assert_eq!(
                prompts.lock().unwrap().prompts.len(),
                pending_call + 1,
                "the fenced prompt must not reach the provider",
            );
            assert_eq!(
                serde_json::to_value(handle.session.agent.messages()).unwrap(),
                before
            );
            assert_eq!(handle.session.agent.system_prompt(), Some("base-system"));
            assert_eq!(
                handle.session.agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off),
            );
        }
    }
}

#[test]
fn every_entrypoint_resumes_instead_of_replaying_user_input() {
    for entrypoint in ENTRYPOINTS {
        let (handle, calls) = flaky_handle(1);
        let mut handle = handle.with_retry(Some(fast_retry_policy(2)));
        let message =
            run_async(invoke(&mut handle, entrypoint, Arc::new(|_| {}))).expect("retry completes");
        assert_eq!(message.stop_reason, StopReason::Stop, "{entrypoint:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{entrypoint:?}");
        let messages = run_async(handle.messages()).expect("session messages");
        let users = messages
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .count();
        let expected_users = usize::from(matches!(
            entrypoint,
            Entrypoint::Prompt | Entrypoint::PromptWithAbort
        ));
        assert_eq!(
            users, expected_users,
            "{entrypoint:?}: input must not replay"
        );
        assert!(
            !messages.iter().any(|message| matches!(
                message,
                Message::Assistant(assistant) if assistant.stop_reason == StopReason::Error
            )),
            "{entrypoint:?}: incomplete error tail must be removed"
        );
    }
}

#[test]
fn no_policy_preserves_the_first_error_on_every_entrypoint() {
    for entrypoint in ENTRYPOINTS {
        let (mut handle, calls) = flaky_handle(1);
        let recoveries = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&recoveries);
        let message = run_async(invoke(
            &mut handle,
            entrypoint,
            Arc::new(move |event| {
                if matches!(
                    event,
                    AgentEvent::AutoRetryStart { .. } | AgentEvent::FailoverStart { .. }
                ) {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
            }),
        ))
        .expect("first errored message");
        assert_eq!(message.stop_reason, StopReason::Error, "{entrypoint:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{entrypoint:?}");
        assert_eq!(recoveries.load(Ordering::SeqCst), 0, "{entrypoint:?}");
    }
}

#[test]
fn recovery_events_reach_subscribers_without_double_firing_typed_hooks() {
    for entrypoint in ENTRYPOINTS {
        let (handle, _) = flaky_handle(1);
        let mut handle = handle.with_retry(Some(fast_retry_policy(2)));
        let subscribed = Arc::new(Mutex::new(Vec::<Value>::new()));
        let per_prompt = Arc::new(Mutex::new(Vec::<Value>::new()));
        let streams = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&subscribed);
        handle.subscribe(move |event| {
            observed
                .lock()
                .unwrap()
                .push(serde_json::to_value(event).unwrap());
        });
        let observed_streams = Arc::clone(&streams);
        handle.listeners_mut().on_stream_event = Some(Arc::new(move |_| {
            observed_streams.fetch_add(1, Ordering::SeqCst);
        }));
        let observed = Arc::clone(&per_prompt);
        run_async(invoke(
            &mut handle,
            entrypoint,
            Arc::new(move |event| {
                observed
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(event).unwrap());
            }),
        ))
        .expect("retry completes");
        let subscribed = subscribed.lock().unwrap();
        let per_prompt = per_prompt.lock().unwrap();
        assert_eq!(
            *subscribed, *per_prompt,
            "{entrypoint:?}: one shared fan-out"
        );
        for name in ["auto_retry_start", "auto_retry_end"] {
            assert_eq!(
                subscribed
                    .iter()
                    .filter(|event| event["type"] == name)
                    .count(),
                1,
                "{entrypoint:?}: missing or duplicated {name}"
            );
        }
        assert_eq!(
            streams.load(Ordering::SeqCst),
            2,
            "{entrypoint:?}: one error and one done stream event, not double-dispatched"
        );
    }
}

#[test]
fn pre_aborted_entrypoints_do_not_append_or_contact_a_provider() {
    for continuation in [false, true] {
        let (handle, calls) = flaky_handle(0);
        let mut handle = handle.with_retry(Some(fast_retry_policy(2)));
        let (abort, signal) = AbortHandle::new();
        abort.abort();
        let seen = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&seen);
        let result = run_async(async {
            if continuation {
                handle
                    .continue_turn_with_abort(signal, move |_| {
                        observed.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
            } else {
                handle
                    .prompt_with_abort("must not append", signal, move |_| {
                        observed.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
            }
        });
        assert!(matches!(result, Err(Error::Aborted)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
        assert!(run_async(handle.messages()).unwrap().is_empty());
        assert!(handle.session.agent.messages().is_empty());
    }
}

#[test]
fn aborting_at_retry_start_returns_abort_not_the_original_capacity_error() {
    for continuation in [false, true] {
        let (handle, calls) = flaky_handle(1);
        let mut handle = handle.with_retry(Some(fast_retry_policy(3)));
        let (abort, signal) = AbortHandle::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        let callback = move |event| {
            if matches!(&event, AgentEvent::AutoRetryStart { .. }) {
                abort.abort();
            }
            observed
                .lock()
                .unwrap()
                .push(serde_json::to_value(event).unwrap());
        };
        let result = run_async(async {
            if continuation {
                handle.continue_turn_with_abort(signal, callback).await
            } else {
                handle.prompt_with_abort("hello", signal, callback).await
            }
        });
        assert!(matches!(result, Err(Error::Aborted)));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no replay after cancellation"
        );
        let events = events.lock().unwrap().clone();
        let ends: Vec<_> = events
            .iter()
            .filter(|event| event["type"] == "auto_retry_end")
            .collect();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["success"], false);
        assert!(
            ends[0]["finalError"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains("abort")
        );
    }
}

#[test]
fn a_public_continuation_restores_the_primary_before_provider_reentry() {
    let mut handle = handle_after_one_failover(0);
    let (abort, signal) = AbortHandle::new();
    let restored = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&restored);
    handle.subscribe(move |event| {
        if matches!(
            event,
            AgentEvent::FailoverEnd {
                restored_primary: true,
                ..
            }
        ) {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    });
    let result = run_async(handle.continue_turn_with_abort(signal, move |event| {
        if matches!(
            event,
            AgentEvent::FailoverEnd {
                restored_primary: true,
                ..
            }
        ) {
            // Cancel at the preflight boundary: no external primary request.
            abort.abort();
        }
    }));
    assert!(matches!(result, Err(Error::Aborted)));
    assert_eq!(handle.model().1, "claude-3-5-haiku-latest");
    assert_eq!(restored.load(Ordering::SeqCst), 1);
    assert!(handle.failover_state.primary().is_none());
}

#[test]
fn known_model_capacity_blocks_silent_overflow_recovery_on_every_entrypoint() {
    for entrypoint in ENTRYPOINTS {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(FlakyThenOkProvider {
            failures: 1,
            calls: Arc::clone(&calls),
            name: "test-provider".to_string(),
            model: "test-model".to_string(),
            input_tokens: 8_193,
        });
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig::default(),
        );
        let mut stored = Session::in_memory();
        stored.header.provider = Some("test-provider".to_string());
        stored.header.model_id = Some("test-model".to_string());
        let mut session = AgentSession::new(
            agent,
            Arc::new(AsyncMutex::new(stored)),
            false,
            ResolvedCompactionSettings {
                enabled: false,
                ..ResolvedCompactionSettings::default()
            },
        );
        let dir = tempdir().unwrap();
        let auth = AuthStorage::empty_at(dir.path().join("auth.json"));
        let mut registry = ModelRegistry::load(&auth, None);
        let mut entry = crate::models::ad_hoc_model_entry("openai", "test-model").unwrap();
        entry.model.provider = "test-provider".to_string();
        entry.model.context_window = 8_192;
        // Hermetic credential: without it the run depended on the host having
        // OPENAI_API_KEY (or a login), and failed as `Auth` on clean workers.
        entry.api_key = Some("fixture-key".to_string()); // ubs:ignore test fixture credential
        registry.merge_entries(vec![entry]);
        session.set_model_registry(registry);
        let mut handle =
            AgentSessionHandle::from_session_with_listeners(session, EventListeners::new())
                .with_retry(Some(fast_retry_policy(3)));
        assert_eq!(
            handle
                .session
                .current_model_entry()
                .unwrap()
                .model
                .context_window,
            8_192
        );
        let message = run_async(invoke(&mut handle, entrypoint, Arc::new(|_| {}))).unwrap();
        assert_eq!(message.stop_reason, StopReason::Error, "{entrypoint:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{entrypoint:?}: an oversized request cannot recover by replay"
        );
    }
}

fn saving_recovery_handle(dir: &Path) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let mut handle = saving_handle(dir);
    let calls = Arc::new(AtomicUsize::new(0));
    handle
        .session
        .agent
        .set_provider(Arc::new(FlakyThenOkProvider {
            failures: usize::MAX,
            calls: Arc::clone(&calls),
            name: "anthropic".to_string(),
            model: "claude-3-5-haiku-latest".to_string(),
            input_tokens: 0,
        }));
    (handle, calls)
}

fn saving_fallback_selection_handle(dir: &Path) -> AgentSessionHandle {
    let mut handle = saving_handle(dir);
    let (provider, model) = handle.model();
    let provenance = crate::session::ModelChangeFailover {
        primary_provider: "anthropic".to_string(),
        primary_model_id: "claude-3-5-haiku-latest".to_string(),
        primary_thinking_level: Some("off".to_string()),
        fallback_provider: provider.clone(),
        fallback_model_id: model.clone(),
        chain_position: Some(2),
        cooldown_deadline: Some(chrono::Utc::now().to_rfc3339()),
        cooldown_secs: Some(0),
        lifecycle_id: Some("selection-fixture".to_string()),
    };
    let store = handle.session_store();
    {
        let mut session = store.try_lock().unwrap();
        session.set_model_header(
            Some(provider.clone()),
            Some(model.clone()),
            Some("off".into()),
        );
        session.append_model_change_with_role_and_failover(
            provider,
            model,
            Some("failover".into()),
            Some(provenance.clone()),
        );
    }
    run_async(handle.session.persist_session()).expect("persist fallback provenance");
    handle = handle.with_failover(Some(FailoverOptions {
        chains: HashMap::from([(
            "default".to_string(),
            vec!["openai/gpt-4o-mini".to_string()],
        )]),
        available_models: Vec::new(),
        auth: AuthStorage::empty_at(dir.join("auth.json")),
        cli_api_key: Some("fixture-key".to_string()),
        cooldown_secs: 0,
    }));
    handle.failover_state = crate::failover::FailoverState::reconstruct_from_provenance(
        &provenance,
        0,
        chrono::Utc::now(),
    );
    handle
}

#[test]
fn explicitly_selecting_the_fallback_cancels_restoration_durably() {
    let dir = tempdir().unwrap();
    let mut handle = saving_fallback_selection_handle(dir.path());
    let (provider, model) = handle.model();
    assert!(
        handle
            .failover_state
            .should_restore_primary(std::time::Instant::now())
    );
    run_async(handle.set_model(&provider, &model)).expect("choose the active fallback");

    assert!(handle.failover_state.primary().is_none());
    assert!(handle.failover_state.active().is_none());
    assert!(handle.failover_state.lifecycle_id().is_none());
    assert_eq!(handle.failover_state.chain_position(), 0);
    let path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .unwrap();
    let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
    assert!(
        reopened
            .active_failover_provenance_for_current_path()
            .is_none()
    );
    assert_eq!(
        reopened.effective_model_for_current_path(),
        Some((provider.clone(), model.clone()))
    );
    let model_changes = reopened
        .entries_for_current_path()
        .into_iter()
        .filter(|entry| matches!(entry, crate::session::SessionEntry::ModelChange(_)))
        .count();
    assert_eq!(model_changes, 2, "one fallback and one explicit choice");

    // A repeated selection stays a no-op once the automatic cycle is retired.
    run_async(handle.set_model(&provider, &model)).expect("repeat explicit choice");
    let repeated = run_async(Session::open(&path.display().to_string())).unwrap();
    assert_eq!(
        repeated
            .entries_for_current_path()
            .into_iter()
            .filter(|entry| matches!(entry, crate::session::SessionEntry::ModelChange(_)))
            .count(),
        model_changes
    );

    let (events, callback) = event_log();
    let message = run_async(
        handle.prompt("continue on my selected model", move |event| {
            callback(event);
        }),
    )
    .expect("selected fixture model answers");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(handle.model(), (provider, model));
    assert!(!events.lock().unwrap().iter().any(|event| {
        matches!(
            event["type"].as_str(),
            Some("failover_start" | "failover_end")
        )
    }));
}

#[test]
fn rejected_model_selection_keeps_the_previous_fallback_cycle() {
    let dir = tempdir().unwrap();
    let mut handle = saving_fallback_selection_handle(dir.path());
    let original_model = handle.model();
    let path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .unwrap();
    let original_bytes = std::fs::read(&path).unwrap();
    run_async(handle.set_model("missing-provider", "missing-model"))
        .expect_err("invalid selection is refused");
    assert_eq!(handle.model(), original_model);
    assert_eq!(
        handle.failover_state.lifecycle_id(),
        Some("selection-fixture")
    );
    assert_eq!(handle.failover_state.chain_position(), 2);
    assert_eq!(std::fs::read(&path).unwrap(), original_bytes);

    let blocked = dir.path().join("blocked-selection.jsonl");
    std::fs::create_dir(&blocked).unwrap();
    handle.session_store().try_lock().unwrap().path = Some(blocked);
    let result = run_async(handle.set_model(&original_model.0, &original_model.1));
    assert!(result.as_ref().is_err_and(Error::is_session_persistence));
    assert_eq!(handle.model(), original_model);
    assert_eq!(
        handle.failover_state.lifecycle_id(),
        Some("selection-fixture")
    );
    assert_eq!(handle.failover_state.chain_position(), 2);
    assert!(
        handle
            .session_store()
            .try_lock()
            .unwrap()
            .active_failover_provenance_for_current_path()
            .is_some()
    );
    assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
}

fn assert_quarantined_entrypoints(handle: &mut AgentSessionHandle, calls: &AtomicUsize) {
    let before = calls.load(Ordering::SeqCst);
    for entrypoint in ENTRYPOINTS {
        let result = run_async(invoke(handle, entrypoint, Arc::new(|_| {})));
        assert!(
            result.as_ref().is_err_and(Error::is_session_persistence),
            "{entrypoint:?}: uncertain durability must remain quarantined: {result:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), before, "{entrypoint:?}");
    }
}

#[test]
fn retry_save_failure_quarantines_later_calls_even_after_the_path_is_repaired() {
    let dir = tempdir().unwrap();
    let blocked = dir.path().join("cannot-replace-a-directory.jsonl");
    std::fs::create_dir(&blocked).unwrap();
    let (handle, calls) = saving_recovery_handle(dir.path());
    let mut handle = handle.with_retry(Some(fast_retry_policy(1)));
    let store = handle.session_store();
    let injected = Arc::new(Mutex::new(None::<(PathBuf, Value)>));
    let recorded = Arc::clone(&injected);
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = Arc::clone(&events);
    let result = run_async(handle.prompt("keep this input", move |event| {
        if matches!(&event, AgentEvent::AutoRetryStart { .. }) {
            // The failed attempt has already persisted. Only the subsequent
            // private retry candidate sees the injected filesystem failure.
            let mut session = store.try_lock().expect("between-attempt lock");
            *recorded.lock().unwrap() = Some((
                session.path.clone().expect("first attempt persisted"),
                serde_json::to_value(session.to_messages_for_current_path()).unwrap(),
            ));
            session.path = Some(blocked.clone());
        }
        observed
            .lock()
            .unwrap()
            .push(serde_json::to_value(event).unwrap());
    }));
    assert!(result.as_ref().is_err_and(Error::is_session_persistence));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (original_path, expected) = injected.lock().unwrap().clone().expect("fault injected");
    assert_eq!(
        serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
        expected
    );
    let reopened = run_async(Session::open(&original_path.display().to_string())).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.to_messages_for_current_path()).unwrap(),
        expected
    );
    {
        let events = events.lock().unwrap().clone();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "auto_retry_end")
                .count(),
            1
        );
        assert_eq!(events.last().unwrap()["type"], "agent_end");
        assert!(
            events.last().unwrap()["error"]
                .as_str()
                .unwrap()
                .contains(Error::SESSION_PERSISTENCE_PREFIX)
        );
    }
    handle.session_store().try_lock().unwrap().path = Some(original_path);
    assert_quarantined_entrypoints(&mut handle, &calls);
}

#[test]
fn failover_save_failure_preserves_source_state_and_quarantines_reentry() {
    let dir = tempdir().unwrap();
    let blocked = dir.path().join("blocked.jsonl");
    std::fs::create_dir(&blocked).unwrap();
    let (mut handle, calls) = saving_recovery_handle(dir.path());
    let first = run_async(handle.prompt("source input", |_| {})).unwrap();
    assert_eq!(first.stop_reason, StopReason::Error);
    let original_path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .unwrap();
    let expected = serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap();
    let mut fallback = crate::models::ad_hoc_model_entry("openai", "gpt-4o-mini").unwrap();
    // This test calls the real candidate/commit path, not the target provider.
    fallback.model.base_url = "http://127.0.0.1:1/v1".to_string();
    handle = handle.with_failover(Some(FailoverOptions {
        chains: HashMap::from([(
            "default".to_string(),
            vec!["openai/gpt-4o-mini".to_string()],
        )]),
        available_models: vec![fallback],
        auth: AuthStorage::empty_at(dir.path().join("auth.json")),
        cli_api_key: Some("test-key".to_string()),
        cooldown_secs: 300,
    }));
    handle.session_store().try_lock().unwrap().path = Some(blocked);
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = Arc::clone(&events);
    let callback: EventSubscriber = Arc::new(move |event| {
        observed
            .lock()
            .unwrap()
            .push(serde_json::to_value(event).unwrap());
    });
    let result = run_async(handle.try_chain_failover(&Ok(first), true, None, 1, &callback));
    assert!(result.as_ref().is_err_and(Error::is_session_persistence));
    assert_eq!(handle.model().1, "claude-3-5-haiku-latest");
    assert!(handle.failover_state.primary().is_none());
    assert!(handle.failover_state.lifecycle_id().is_none());
    assert_eq!(handle.failover_state.chain_position(), 0);
    assert!(
        events.lock().unwrap().is_empty(),
        "no successful swap was published"
    );
    assert_eq!(
        serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
        expected
    );
    let reopened = run_async(Session::open(&original_path.display().to_string())).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.to_messages_for_current_path()).unwrap(),
        expected
    );
    handle.session_store().try_lock().unwrap().path = Some(original_path);
    assert_quarantined_entrypoints(&mut handle, &calls);
}

fn saving_handle_after_failover(dir: &Path) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let (handle, calls) = saving_recovery_handle(dir);
    let mut handle = with_chain_cooldown(
        handle.with_retry(Some(crate::failover::RetryPolicy {
            max_retries: 0,
            max_failovers_per_turn: 1,
            model_fallback: true,
            fallback_on: crate::failover::FallbackOn::Any,
            base_delay_ms: 0,
            max_delay_ms: 0,
        })),
        "openai/gpt-4o-mini",
        0,
    );
    let _ = run_async(handle.prompt("fail over once", |_| {}));
    assert_eq!(handle.model().1, "gpt-4o-mini");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    (handle, calls)
}

#[test]
fn invalid_native_content_does_not_restore_an_expired_fallback_or_mutate_its_session() {
    let dir = tempdir().unwrap();
    let (mut handle, calls) = saving_handle_after_failover(dir.path());
    assert!(
        handle
            .failover_state
            .should_restore_primary(std::time::Instant::now()),
        "the setup must make primary restoration immediately eligible"
    );
    let original_model = handle.model();
    let original_primary = handle.failover_state.primary().cloned();
    let original_active = handle.failover_state.active().cloned();
    let original_lifecycle = handle.failover_state.lifecycle_id().map(str::to_string);
    let original_position = handle.failover_state.chain_position();
    let (path, original_provenance) = {
        let store = handle.session_store();
        let session = store.try_lock().unwrap();
        (
            session.path.clone().unwrap(),
            serde_json::to_value(
                session
                    .active_failover_provenance_for_current_path()
                    .expect("saved fallback provenance"),
            )
            .unwrap(),
        )
    };
    let original_bytes = std::fs::read(&path).unwrap();
    let original_calls = calls.load(Ordering::SeqCst);
    let observations = record_system_prompts(&mut handle, None);
    let original_provider = handle.session.agent.provider();
    let (events, callback) = event_log();
    let (abort, signal) = AbortHandle::new();

    let error =
        run_async(
            handle.prompt_with_content_with_abort(Vec::new(), signal, move |event| {
                // A regressed preflight could emit primary restoration before
                // validating input. Stop there so that failure cannot contact the
                // real reconstructed primary while still recording the defect.
                abort.abort();
                callback(event);
            }),
        )
        .expect_err("invalid original blocks must refuse before restoration");

    assert!(error.to_string().contains("PI_INPUT_CONTENT"), "{error}");
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), original_calls);
    assert!(observations.lock().unwrap().messages.is_empty());
    assert_eq!(handle.model(), original_model);
    assert!(Arc::ptr_eq(
        &handle.session.agent.provider(),
        &original_provider
    ));
    assert_eq!(handle.failover_state.primary(), original_primary.as_ref());
    assert_eq!(handle.failover_state.active(), original_active.as_ref());
    assert_eq!(
        handle.failover_state.lifecycle_id(),
        original_lifecycle.as_deref()
    );
    assert_eq!(handle.failover_state.chain_position(), original_position);
    assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
    let store = handle.session_store();
    let session = store.try_lock().unwrap();
    assert_eq!(session.path.as_ref(), Some(&path));
    assert_eq!(
        serde_json::to_value(session.active_failover_provenance_for_current_path()).unwrap(),
        original_provenance
    );
}

#[test]
fn first_failover_uses_one_lifecycle_identity_in_memory_and_on_reopen() {
    let dir = tempdir().unwrap();
    let (handle, _) = saving_handle_after_failover(dir.path());
    let path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .unwrap();
    let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
    let provenance = reopened
        .active_failover_provenance_for_current_path()
        .unwrap();
    assert!(provenance.lifecycle_id.is_some());
    assert_eq!(
        provenance.lifecycle_id.as_deref(),
        handle.failover_state.lifecycle_id()
    );
    let reconstructed =
        crate::failover::FailoverState::reconstruct_from_session(&reopened, 0, chrono::Utc::now());
    assert_eq!(
        reconstructed.lifecycle_id(),
        handle.failover_state.lifecycle_id()
    );
    assert_eq!(
        reconstructed.chain_position(),
        handle.failover_state.chain_position()
    );
}

#[test]
fn lenient_primary_restore_cannot_hide_indeterminate_persistence() {
    let dir = tempdir().unwrap();
    let (mut handle, calls) = saving_handle_after_failover(dir.path());
    let blocked = dir.path().join("blocked-primary-restore.jsonl");
    std::fs::create_dir(&blocked).unwrap();
    let original_path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .unwrap();
    let expected = serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap();
    handle.session_store().try_lock().unwrap().path = Some(blocked);
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = Arc::clone(&events);
    let result = run_async(handle.prompt("must not reach the fallback", move |event| {
        observed
            .lock()
            .unwrap()
            .push(serde_json::to_value(event).unwrap());
    }));
    assert!(result.as_ref().is_err_and(Error::is_session_persistence));
    assert_eq!(handle.model().1, "gpt-4o-mini");
    assert!(handle.failover_state.primary().is_some());
    assert!(
        events.lock().unwrap().is_empty(),
        "neither restoration nor a new turn committed"
    );
    assert_eq!(
        serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
        expected
    );
    let reopened = run_async(Session::open(&original_path.display().to_string())).unwrap();
    assert!(
        reopened
            .active_failover_provenance_for_current_path()
            .is_some()
    );
    handle.session_store().try_lock().unwrap().path = Some(original_path);
    assert_quarantined_entrypoints(&mut handle, &calls);
}

fn event_log() -> (Arc<Mutex<Vec<Value>>>, EventSubscriber) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&events);
    let callback: EventSubscriber = Arc::new(move |event| {
        recorded
            .lock()
            .unwrap()
            .push(serde_json::to_value(event).unwrap());
    });
    (events, callback)
}

fn lifecycle_names(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event["type"].as_str()? {
            "agent_start" => Some("agent_start".to_string()),
            "agent_end" => Some("agent_end".to_string()),
            "auto_retry_start" => Some(format!("retry_start:{}", event["attempt"])),
            "auto_retry_end" => Some(format!(
                "retry_end:{}:{}",
                event["attempt"], event["success"]
            )),
            "failover_start" => Some(format!(
                "failover_start:{}:{}:{}",
                event["attempt"],
                event["chainIndex"],
                event["toModel"].as_str().unwrap(),
            )),
            "failover_end" => Some(format!(
                "failover_end:{}:{}",
                event["model"].as_str().unwrap(),
                event["success"],
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn every_logical_turn_has_one_terminal_event_after_all_recovery_events() {
    for entrypoint in ENTRYPOINTS {
        let (handle, calls) = flaky_handle(2);
        let mut handle = handle.with_retry(Some(fast_retry_policy(3)));
        let (events, callback) = event_log();
        let result = run_async(invoke(&mut handle, entrypoint, callback)).unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let events = events.lock().unwrap();
        assert_eq!(
            lifecycle_names(&events),
            [
                "agent_start",
                "retry_start:1",
                "retry_end:1:false",
                "retry_start:2",
                "retry_end:2:true",
                "agent_end",
            ],
            "{entrypoint:?}"
        );
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], "agent_end");
        assert!(terminal.get("error").is_none());
        assert_eq!(
            terminal["messages"],
            serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
            "terminal payload retains completed work, not reverted error attempts"
        );
        assert_eq!(events[0]["sessionId"], terminal["sessionId"]);
    }
}

#[test]
fn the_terminal_event_excludes_history_from_earlier_public_calls() {
    let (mut handle, _) = flaky_handle(0);
    run_async(handle.prompt("earlier input", |_| {})).unwrap();
    let (events, callback) = event_log();
    run_async(handle.prompt("new input", move |event| callback(event))).unwrap();
    let events = events.lock().unwrap().clone();
    let new_messages = events.last().unwrap()["messages"].as_array().unwrap();
    assert_eq!(new_messages.len(), 2);
    assert!(
        !serde_json::to_string(new_messages)
            .unwrap()
            .contains("earlier input")
    );
    assert_eq!(run_async(handle.messages()).unwrap().len(), 4);
}

#[test]
fn an_aborted_retry_reports_one_failed_terminal_event_not_a_premature_503_end() {
    let (handle, calls) = flaky_handle(1);
    let mut handle = handle.with_retry(Some(fast_retry_policy(2)));
    let (abort, signal) = AbortHandle::new();
    let (events, callback) = event_log();
    let result = run_async(handle.prompt_with_abort("hello", signal, move |event| {
        if matches!(event, AgentEvent::AutoRetryStart { .. }) {
            abort.abort();
        }
        callback(event);
    }));
    assert!(matches!(result, Err(Error::Aborted)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let events = events.lock().unwrap().clone();
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "retry_start:1",
            "retry_end:1:false",
            "agent_end"
        ]
    );
    assert!(
        events.last().unwrap()["error"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("abort")
    );
}

/// A bounded local HTTP fixture: exercises the real provider factory, request
/// serialization, SSE parsing and AgentSession loop, without live credentials.
struct RecoveryHttpFixture {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl RecoveryHttpFixture {
    #[allow(clippy::too_many_lines)]
    fn new(responses: Vec<(u16, &'static str, String)>) -> Self {
        use std::io::{Read as _, Write as _};
        use std::time::{Duration, Instant};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for (status, content_type, body) in responses {
                let deadline = Instant::now() + Duration::from_secs(30);
                let mut stream = loop {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "request fixture timed out");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                let (header_end, content_length) = loop {
                    assert!(Instant::now() < deadline, "request headers timed out");
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0, "request ended before headers");
                    request.extend_from_slice(&buffer[..read]);
                    assert!(request.len() <= 128 * 1024, "oversized fixture request");
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&request[..end]).unwrap();
                        assert!(
                            headers
                                .lines()
                                .next()
                                .unwrap()
                                .contains("/chat/completions")
                        );
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .expect("content-length header");
                        assert!(length <= 128 * 1024, "oversized fixture body");
                        break (end + 4, length);
                    }
                };
                while request.len() < header_end + content_length {
                    assert!(Instant::now() < deadline, "request body timed out");
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0, "request ended before body");
                    request.extend_from_slice(&buffer[..read]);
                    assert!(request.len() <= 256 * 1024);
                }
                let value =
                    serde_json::from_slice(&request[header_end..header_end + content_length])
                        .expect("provider request JSON");
                captured.lock().unwrap().push(value);
                let reason = if status == 200 { "OK" } else { "Fixture Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker
            .take()
            .unwrap()
            .join()
            .expect("HTTP fixture worker");
    }
}

impl Drop for RecoveryHttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn capacity_response() -> (u16, &'static str, String) {
    (
        503,
        "application/json",
        r#"{"error":{"message":"503 service unavailable","type":"server_error"}}"#.to_string(),
    )
}

fn completion_response() -> (u16, &'static str, String) {
    (
        200,
        "text/event-stream",
        concat!(
            "data: {\"id\":\"chatcmpl-fixture\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fallback-b\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Recovered\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string(),
    )
}

fn http_chain_handle(url: &str, cap: u32) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let (handle, calls) = flaky_handle_as(usize::MAX, "anthropic", "claude-x");
    let entries = ["fallback-a", "fallback-b"]
        .into_iter()
        .map(|model| {
            let mut entry = crate::models::ad_hoc_model_entry("openai", model).unwrap();
            entry.model.api = "openai-completions".to_string();
            entry.model.base_url = url.to_string();
            entry
        })
        .collect();
    let handle = handle
        .with_retry(Some(crate::failover::RetryPolicy {
            max_retries: 0,
            max_failovers_per_turn: cap,
            model_fallback: true,
            fallback_on: crate::failover::FallbackOn::Any,
            base_delay_ms: 0,
            max_delay_ms: 0,
        }))
        .with_failover(Some(FailoverOptions {
            chains: HashMap::from([(
                "default".to_string(),
                vec![
                    "anthropic/claude-x".to_string(),
                    "not-a-spec".to_string(),
                    "openai/fallback-a".to_string(),
                    "OPENAI/FALLBACK-A".to_string(),
                    "openai/fallback-b".to_string(),
                ],
            )]),
            available_models: entries,
            auth: AuthStorage::empty_at(PathBuf::from("unused-fixture-auth.json")),
            cli_api_key: Some("test-key".to_string()),
            cooldown_secs: 300,
        }));
    (handle, calls)
}

#[test]
fn configured_fallback_controls_preserve_retries_and_gate_actual_provider_requests() {
    for (model_fallback, filter, expected_swap) in [
        (false, "any", false),
        (true, "usage-limit", false),
        (true, "except-usage-limit", true),
        (true, "any", true),
    ] {
        let mut server = RecoveryHttpFixture::new(vec![completion_response()]);
        let (mut handle, calls) = http_chain_handle(&server.url, 1);
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "retry": {
                "enabled": true,
                "maxRetries": 1,
                "baseDelayMs": 0,
                "maxDelayMs": 0,
                "modelFallback": model_fallback,
                "fallbackOn": filter,
            }
        }))
        .expect("fallback policy");
        handle.set_retry_policy(crate::failover::RetryPolicy::from_config(&config));
        let (events, callback) = event_log();
        let result = run_async(handle.prompt("recover once", move |event| callback(event)))
            .expect("provider returns a completed or errored message");
        server.finish();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "same-model retry is retained"
        );
        assert_eq!(
            result.stop_reason,
            if expected_swap {
                StopReason::Stop
            } else {
                StopReason::Error
            }
        );
        assert_eq!(
            handle.model().1,
            if expected_swap {
                "fallback-a"
            } else {
                "claude-x"
            }
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), usize::from(expected_swap));
        if expected_swap {
            assert_eq!(requests[0]["model"], "fallback-a");
        }
        drop(requests);
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "failover_start")
                .count(),
            usize::from(expected_swap),
        );
        drop(events);
        let messages = run_async(handle.messages()).expect("transcript");
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::User(_)))
                .count(),
            1,
            "recovery must not duplicate the user turn"
        );
    }
}

#[test]
fn every_failover_hop_keeps_the_extension_prompt_then_next_turn_restores_base() {
    let mut server = RecoveryHttpFixture::new(vec![
        capacity_response(),
        completion_response(),
        completion_response(),
    ]);
    let (mut handle, calls) = http_chain_handle(&server.url, 2);
    handle
        .session
        .agent
        .set_system_prompt(Some("base-system".to_string()));
    let primary_prompts = record_system_prompts(&mut handle, None);
    install_system_prompt_hook(&mut handle, Some("hook-system"));

    let result = run_async(handle.prompt("orchestrate the recovery", |_| {})).unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(handle.model().1, "fallback-b");
    assert_eq!(handle.session.agent.system_prompt(), Some("base-system"));
    assert_prompt_hook_ran_once(&handle);

    install_system_prompt_hook(&mut handle, None);
    run_async(handle.prompt("a separate question", |_| {})).unwrap();
    server.finish();
    assert_prompt_hook_ran_once(&handle);
    assert_eq!(handle.session.agent.system_prompt(), Some("base-system"));

    let expected = format!(
        "hook-system\n\n{}",
        crate::magic_keywords::ORCHESTRATE_DIRECTIVE,
    );
    assert_eq!(
        primary_prompts.lock().unwrap().prompts,
        vec![Some(expected.clone())],
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let expected_prompts = [expected.as_str(), expected.as_str(), "base-system"];
    for (request, expected) in requests.iter().zip(expected_prompts) {
        let system_prompt = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| matches!(message["role"].as_str(), Some("system" | "developer")))
            .and_then(|message| message["content"].as_str());
        assert_eq!(system_prompt, Some(expected));
    }
}

#[test]
fn real_transport_multi_hop_failover_pairs_each_committed_hop_before_terminal_end() {
    let mut server = RecoveryHttpFixture::new(vec![capacity_response(), completion_response()]);
    let (mut handle, calls) = http_chain_handle(&server.url, 2);
    let (events, callback) = event_log();
    let result = run_async(handle.prompt("recover across the chain", move |event| callback(event)))
        .expect("second fallback succeeds");
    server.finish();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(handle.model().1, "fallback-b");
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["fallback-a", "fallback-b"]
    );
    let events = events.lock().unwrap();
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "failover_start:1:2:fallback-a",
            "failover_end:fallback-a:false",
            "failover_start:2:4:fallback-b",
            "failover_end:fallback-b:true",
            "agent_end",
        ]
    );
    assert!(events.last().unwrap().get("error").is_none());
    assert_eq!(
        events.last().unwrap()["messages"],
        serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap()
    );
}

#[test]
fn abort_after_the_second_swap_closes_both_hops_without_contacting_its_provider() {
    let mut server = RecoveryHttpFixture::new(vec![capacity_response(), completion_response()]);
    let (mut handle, _) = http_chain_handle(&server.url, 2);
    let (abort, signal) = AbortHandle::new();
    let (events, callback) = event_log();
    let result = run_async(handle.prompt_with_abort("hello", signal, move |event| {
        if matches!(event, AgentEvent::FailoverStart { attempt: 2, .. }) {
            abort.abort();
        }
        callback(event);
    }));
    server.finish();
    assert!(matches!(result, Err(Error::Aborted)));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    let events = events.lock().unwrap().clone();
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "failover_start:1:2:fallback-a",
            "failover_end:fallback-a:false",
            "failover_start:2:4:fallback-b",
            "failover_end:fallback-b:false",
            "agent_end",
        ]
    );
    assert!(
        events.last().unwrap()["error"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("abort")
    );
}

#[test]
fn a_swap_cap_counts_commits_not_skipped_specs_or_terminal_events() {
    let mut server = RecoveryHttpFixture::new(vec![capacity_response(), completion_response()]);
    let (mut handle, _) = http_chain_handle(&server.url, 1);
    let (events, callback) = event_log();
    let result = run_async(handle.prompt("bounded chain", move |event| callback(event)));
    server.finish();
    assert!(
        result.is_err()
            || result
                .as_ref()
                .is_ok_and(|message| message.stop_reason == StopReason::Error)
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(handle.model().1, "fallback-a");
    let events = events.lock().unwrap().clone();
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "failover_start:1:2:fallback-a",
            "failover_end:fallback-a:false",
            "agent_end",
        ]
    );
    assert!(events.last().unwrap()["error"].is_string());
}

fn write_tool_response() -> (u16, &'static str, String) {
    let arguments =
        serde_json::json!({"path": "result.txt", "content": "saved exactly once"}).to_string();
    let chunk = serde_json::json!({
        "id": "chatcmpl-tool-fixture", "object": "chat.completion.chunk", "created": 0,
        "model": "fallback-b",
        "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
            "index": 0, "id": "write-once", "type": "function",
            "function": {"name": "write", "arguments": arguments}
        }]}, "finish_reason": null}]
    });
    let end = serde_json::json!({
        "id": "chatcmpl-tool-fixture",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    (
        200,
        "text/event-stream",
        format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
    )
}

fn prewalk_tool_batch(calls: &[(&str, &str, Value)]) -> (u16, &'static str, String) {
    let calls = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            serde_json::json!({
                "index": index, "id": id, "type": "function",
                "function": {"name": name, "arguments": args.to_string()}
            })
        })
        .collect::<Vec<_>>();
    let chunk = serde_json::json!({
        "id": "chatcmpl-prewalk", "object": "chat.completion.chunk", "created": 0,
        "model": "walk-source",
        "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": calls}, "finish_reason": null}]
    });
    let end = serde_json::json!({
        "id": "chatcmpl-prewalk",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    (
        200,
        "text/event-stream",
        format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
    )
}

fn prewalk_http_handle(
    dir: &Path,
    source_url: &str,
    target_url: &str,
    with_todo: bool,
    max_tool_iterations: usize,
) -> AgentSessionHandle {
    let mut source = crate::models::ad_hoc_model_entry("openai", "walk-source").unwrap();
    source.model.api = "openai-completions".to_string();
    source.model.base_url = source_url.to_string();
    source.api_key = Some("source-fixture-key".to_string());
    let mut target = crate::models::ad_hoc_model_entry("groq", "walk-target").unwrap();
    target.model.api = "openai-completions".to_string();
    target.model.base_url = target_url.to_string();
    target.model.context_window = 65_536;
    target.model.max_tokens = 8_192;
    target.model.input = vec![crate::provider::InputType::Text];
    target.api_key = Some("target-fixture-key".to_string());
    let provider = crate::providers::create_provider(&source, None).unwrap();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&["write", "read"], dir, None),
        AgentConfig {
            system_prompt: Some("ordinary-system".to_string()),
            max_tool_iterations,
            stream_options: StreamOptions {
                api_key: source.api_key.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let mut stored = Session::create_with_dir(Some(dir.join("sessions")));
    stored.header.cwd = dir.display().to_string();
    stored.header.provider = Some(source.model.provider.clone());
    stored.header.model_id = Some(source.model.id.clone());
    let mut session = AgentSession::new(
        agent,
        Arc::new(AsyncMutex::new(stored)),
        true,
        ResolvedCompactionSettings {
            enabled: false,
            ..Default::default()
        },
    );
    session.set_model_registry(ModelRegistry::from_entries_for_tests(vec![source, target]));
    session.set_auth_storage(AuthStorage::empty_at(dir.join("auth.json")));
    if with_todo {
        let stored = Arc::clone(&session.session);
        session.agent.extend_tools(vec![
            Box::new(crate::todo::TodoTool::new(stored)) as Box<dyn crate::tools::Tool>
        ]);
    }
    AgentSessionHandle::from_session_with_listeners(session, EventListeners::new())
}

fn prewalk_destination() -> crate::prewalk::PrewalkTarget {
    crate::prewalk::PrewalkTarget {
        provider: "groq".to_string(),
        model: "walk-target".to_string(),
        thinking: Some(crate::model::ThinkingLevel::Off),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn prewalk_cross_provider_persists_the_whole_batch_and_retains_hook_prompt_across_retry() {
    let dir = tempdir().unwrap();
    let mut source = RecoveryHttpFixture::new(vec![prewalk_tool_batch(&[
        (
            "walk-plan",
            "todo",
            serde_json::json!({"op":"init", "tasks":["implement both files"]}),
        ),
        (
            "walk-first",
            "write",
            serde_json::json!({"path":"first.txt", "content":"first"}),
        ),
        (
            "walk-second",
            "write",
            serde_json::json!({"path":"second.txt", "content":"second"}),
        ),
    ])]);
    let mut target = RecoveryHttpFixture::new(vec![capacity_response(), completion_response()]);
    let mut handle = prewalk_http_handle(dir.path(), &source.url, &target.url, true, 32)
        .with_retry(Some(fast_retry_policy(1)));
    install_native_content_hooks(&mut handle, &serde_json::json!({"action":"continue"}));
    assert!(
        run_async(handle.arm_prewalk(prewalk_destination()))
            .unwrap()
            .is_some()
    );
    let (events, callback) = event_log();
    let store = handle.session_store();
    let result = run_async(handle.prompt("implement both files", move |event| {
        if matches!(event, AgentEvent::PrewalkSwitched { .. }) {
            let saved = store.try_lock().expect("handoff releases session lock");
            let durable = std::fs::read_to_string(saved.path.as_ref().unwrap()).unwrap();
            assert!(durable.contains("walk-first") && durable.contains("walk-second"));
            assert_eq!(saved.header.provider.as_deref(), Some("groq"));
            assert_eq!(saved.header.model_id.as_deref(), Some("walk-target"));
        }
        callback(event);
    }))
    .unwrap();
    source.finish();
    target.finish();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(
        handle.model(),
        ("groq".to_string(), "walk-target".to_string())
    );
    assert!(
        handle.prewalk_target().is_none(),
        "one-shot handoff is consumed"
    );
    assert_eq!(handle.compaction_settings().context_window_tokens, 65_536);
    assert_eq!(
        handle.session.agent.stream_options().max_tokens,
        Some(8_192)
    );
    assert!(!handle.session.agent.model_accepts_images());
    assert_eq!(
        handle.session.agent.system_prompt(),
        Some("ordinary-system")
    );
    assert_prompt_hook_ran_once(&handle);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("first.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("second.txt")).unwrap(),
        "second"
    );
    let source_requests = source.requests.lock().unwrap();
    assert_eq!(
        source_requests.len(),
        1,
        "strong model stops at the first completed edit batch"
    );
    assert!(source_requests[0].to_string().contains("## Prewalk"));
    drop(source_requests);
    let target_requests = target.requests.lock().unwrap();
    assert_eq!(target_requests.len(), 2, "target retry stays on the target");
    for request in &*target_requests {
        let messages = request["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "tool")
                .count(),
            3
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| {
                    message["role"] == "user"
                        && message["content"]
                            .to_string()
                            .contains("implement both files")
                })
                .count(),
            1
        );
        assert!(request.to_string().contains("native-content-hook"));
        assert!(!request.to_string().contains("## Prewalk"));
    }
    drop(target_requests);
    let events = events.lock().unwrap();
    for kind in ["agent_start", "agent_end", "prewalk_switched"] {
        assert_eq!(
            events.iter().filter(|event| event["type"] == kind).count(),
            1,
            "{kind}"
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "tool_execution_start")
            .count(),
        3
    );
    let path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .unwrap();
    let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
    assert_eq!(
        reopened.effective_model_for_current_path(),
        Some(handle.model())
    );
    assert!(!crate::todo::latest_from_entries(reopened.entries_for_current_path()).is_empty());
    assert_eq!(
        reopened
            .to_messages_for_current_path()
            .iter()
            .filter(|message| matches!(message, Message::ToolResult(_)))
            .count(),
        3
    );
}

#[test]
fn prewalk_preserves_the_original_tool_budget_across_the_handoff() {
    let dir = tempdir().unwrap();
    let mut source = RecoveryHttpFixture::new(vec![write_tool_response()]);
    let mut target = RecoveryHttpFixture::new(vec![prewalk_tool_batch(&[(
        "over-budget",
        "write",
        serde_json::json!({"path":"second.txt", "content":"must not run"}),
    )])]);
    let mut handle = prewalk_http_handle(dir.path(), &source.url, &target.url, false, 1);
    run_async(handle.arm_prewalk(prewalk_destination())).unwrap();
    let result = run_async(handle.prompt("write two files", |_| {})).unwrap();
    source.finish();
    target.finish();
    assert_eq!(source.requests.lock().unwrap().len(), 1);
    assert_eq!(target.requests.lock().unwrap().len(), 1);
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(
        result
            .error_message
            .unwrap()
            .contains("Maximum tool iterations (1)")
    );
    assert!(dir.path().join("result.txt").is_file());
    assert!(!dir.path().join("second.txt").exists());
}

#[test]
fn prewalk_cancellation_at_the_completed_batch_never_calls_the_destination() {
    let dir = tempdir().unwrap();
    let mut source = RecoveryHttpFixture::new(vec![write_tool_response()]);
    let mut target = RecoveryHttpFixture::new(vec![completion_response()]);
    let mut handle = prewalk_http_handle(dir.path(), &source.url, &target.url, false, 32);
    run_async(handle.arm_prewalk(prewalk_destination())).unwrap();
    let (abort, signal) = AbortHandle::new();
    let result = run_async(handle.prompt_with_abort("write once", signal, move |event| {
        if matches!(event, AgentEvent::TurnEnd { ref tool_results, .. } if !tool_results.is_empty()) {
            abort.abort();
        }
    }));
    source.finish();
    target.finish();
    assert!(
        matches!(&result, Err(Error::Aborted))
            || matches!(&result, Ok(message) if message.stop_reason == StopReason::Aborted)
    );
    assert_eq!(handle.model().0, "openai");
    assert!(handle.prewalk_target().is_none());
    assert!(target.requests.lock().unwrap().is_empty());
    assert!(dir.path().join("result.txt").is_file());
}

#[test]
fn prewalk_waits_for_a_real_todo_list_when_the_tool_is_available() {
    let dir = tempdir().unwrap();
    let mut source = RecoveryHttpFixture::new(vec![write_tool_response(), completion_response()]);
    let mut target = RecoveryHttpFixture::new(vec![completion_response()]);
    let mut handle = prewalk_http_handle(dir.path(), &source.url, &target.url, true, 32);
    run_async(handle.arm_prewalk(prewalk_destination())).unwrap();
    let result = run_async(handle.prompt("edit without creating todos", |_| {})).unwrap();
    source.finish();
    target.finish();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(handle.model().0, "openai");
    assert!(handle.prewalk_target().is_some());
    assert!(target.requests.lock().unwrap().is_empty());
}

#[test]
#[allow(clippy::too_many_lines)]
fn real_tool_work_survives_retry_once_and_terminal_end_follows_persistence() {
    let dir = tempdir().unwrap();
    let mut server = RecoveryHttpFixture::new(vec![
        write_tool_response(),
        capacity_response(),
        completion_response(),
    ]);
    let mut entry = crate::models::ad_hoc_model_entry("openai", "fallback-b").unwrap();
    entry.model.api = "openai-completions".to_string();
    entry.model.base_url = server.url.clone();
    let provider = crate::providers::create_provider(&entry, None).unwrap();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&["write"], dir.path(), None),
        AgentConfig {
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            },
            ..AgentConfig::default()
        },
    );
    let mut stored = Session::create_with_dir(Some(dir.path().join("sessions")));
    stored.header.cwd = dir.path().display().to_string();
    stored.header.provider = Some("openai".to_string());
    stored.header.model_id = Some("fallback-b".to_string());
    let session = AgentSession::new(
        agent,
        Arc::new(AsyncMutex::new(stored)),
        true,
        ResolvedCompactionSettings {
            enabled: false,
            ..Default::default()
        },
    );
    let mut handle =
        AgentSessionHandle::from_session_with_listeners(session, EventListeners::new())
            .with_retry(Some(fast_retry_policy(1)));
    let (events, callback) = event_log();
    let store = handle.session_store();
    let result = run_async(handle.prompt("write result.txt once", move |event| {
        if matches!(event, AgentEvent::AgentEnd { .. }) {
            let stored = store
                .try_lock()
                .expect("terminal callback must not hold session lock");
            let bytes = std::fs::read_to_string(stored.path.as_ref().unwrap()).unwrap();
            assert!(
                bytes.contains("Recovered"),
                "AgentEnd preceded final persistence"
            );
        }
        callback(event);
    }))
    .expect("recovered tool turn");
    server.finish();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "saved exactly once"
    );
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "tool_execution_start")
            .count(),
        1
    );
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "retry_start:1",
            "retry_end:1:true",
            "agent_end"
        ]
    );
    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let resumed = requests[2]["messages"].as_array().unwrap();
    assert_eq!(
        resumed
            .iter()
            .filter(|message| message["role"] == "tool")
            .count(),
        1
    );
    assert_eq!(
        resumed
            .iter()
            .filter(|message| message["role"] == "user")
            .count(),
        1
    );
    let path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .unwrap();
    let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
    assert_eq!(
        events.last().unwrap()["messages"],
        serde_json::to_value(reopened.to_messages_for_current_path()).unwrap()
    );
}

struct OverflowRecoveryProvider {
    calls: AtomicUsize,
    summaries: AtomicUsize,
    large_read: std::sync::atomic::AtomicBool,
    observed: Mutex<Vec<Vec<Message>>>,
    write_first: bool,
    repeat_overflow: bool,
    summary: String,
    abort_summary: Option<AbortHandle>,
}

#[async_trait::async_trait]
impl Provider for OverflowRecoveryProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "overflow-fixture"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn api(&self) -> &str {
        "test-api"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn model_id(&self) -> &str {
        "overflow-model"
    }

    async fn stream(
        &self,
        context: &crate::provider::Context<'_>,
        _options: &StreamOptions,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        let is_summary = context
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("context summarization assistant"));
        let (content, stop_reason) = if is_summary {
            self.summaries.fetch_add(1, Ordering::SeqCst);
            if let Some(abort) = &self.abort_summary {
                abort.abort();
                return futures::future::pending().await;
            }
            (
                vec![ContentBlock::Text(TextContent::new(self.summary.clone()))],
                StopReason::Stop,
            )
        } else {
            self.observed
                .lock()
                .unwrap()
                .push(context.messages.to_vec());
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let large_read = self.write_first && self.large_read.load(Ordering::SeqCst);
            if self.write_first && call == 0 {
                (
                    vec![ContentBlock::ToolCall(crate::model::ToolCall {
                        id: "overflow-write-once".to_string(),
                        name: "write".to_string(),
                        arguments: serde_json::json!({
                            "path": "overflow-result.txt",
                            "content": "completed before overflow",
                        }),
                        thought_signature: None,
                    })],
                    StopReason::ToolUse,
                )
            } else if large_read && call == 1 {
                (
                    vec![ContentBlock::ToolCall(crate::model::ToolCall {
                        id: "overflow-read-once".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({"path": "large-result.txt"}),
                        thought_signature: None,
                    })],
                    StopReason::ToolUse,
                )
            } else if call == usize::from(self.write_first) + usize::from(large_read)
                || self.repeat_overflow
            {
                return Err(Error::api("context_length_exceeded: prompt is too long"));
            } else {
                (
                    vec![ContentBlock::Text(TextContent::new(
                        "continued after compaction",
                    ))],
                    StopReason::Stop,
                )
            }
        };
        let message = AssistantMessage {
            content,
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            stop_reason,
            ..Default::default()
        };
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::Start {
                partial: message.clone(),
            }),
            Ok(StreamEvent::Done {
                reason: stop_reason,
                message,
            }),
        ])))
    }
}

fn overflow_recovery_handle(
    dir: &Path,
    write_first: bool,
    repeat_overflow: bool,
    summary: String,
    abort_summary: Option<AbortHandle>,
) -> (AgentSessionHandle, Arc<OverflowRecoveryProvider>) {
    let provider = Arc::new(OverflowRecoveryProvider {
        calls: AtomicUsize::new(0),
        summaries: AtomicUsize::new(0),
        large_read: std::sync::atomic::AtomicBool::new(false),
        observed: Mutex::new(Vec::new()),
        write_first,
        repeat_overflow,
        summary,
        abort_summary,
    });
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(&["write", "read"], dir, None),
        AgentConfig::default(),
    );
    agent.set_model_accepts_images(true);
    let mut stored = Session::create_with_dir(Some(dir.join("sessions")));
    stored.header.provider = Some(provider.name().to_string());
    stored.header.model_id = Some(provider.model_id().to_string());
    stored.append_model_message(Message::User(UserMessage {
        content: UserContent::Text("earlier task with historical details ".repeat(500)),
        timestamp: 0,
    }));
    stored.append_model_message(Message::Assistant(Arc::new(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new(
            "earlier answer with resolved details ".repeat(500),
        ))],
        provider: provider.name().to_string(),
        model: provider.model_id().to_string(),
        stop_reason: StopReason::Stop,
        ..Default::default()
    })));
    let session = AgentSession::new(
        agent,
        Arc::new(AsyncMutex::new(stored)),
        true,
        ResolvedCompactionSettings {
            enabled: true,
            context_window_tokens: 128_000,
            keep_recent_tokens: 2_000,
            ..Default::default()
        },
    );
    (
        AgentSessionHandle::from_session_with_listeners(session, EventListeners::new()),
        provider,
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn overflow_compaction_preserves_native_input_tools_and_durable_order_without_retry_policy() {
    let dir = tempdir().unwrap();
    let (mut handle, provider) = overflow_recovery_handle(
        dir.path(),
        true,
        false,
        "The earlier task was completed.".to_string(),
        None,
    );
    assert!(handle.retry_policy().is_none());
    let content = vec![
        ContentBlock::Text(TextContent::new("write this result exactly once")),
        ContentBlock::Image(ImageContent {
            data: "aGVsbG8=".to_string(),
            mime_type: "image/png".to_string(),
        }),
    ];
    let expected_content = serde_json::to_value(&content).unwrap();
    let (events, callback) = event_log();
    let store = handle.session_store();
    let result = run_async(handle.prompt_with_content(content, move |event| {
        if let AgentEvent::AutoCompactionEnd {
            result: Some(_), ..
        } = &event
        {
            let stored = store
                .try_lock()
                .expect("compaction callback releases session lock");
            let disk = std::fs::read_to_string(stored.path.as_ref().unwrap()).unwrap();
            assert!(disk.contains("\"type\":\"compaction\""));
        }
        callback(event);
    }))
    .expect("overflow recovery");
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert!(provider.summaries.load(Ordering::SeqCst) > 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("overflow-result.txt")).unwrap(),
        "completed before overflow"
    );
    let observed = provider.observed.lock().unwrap();
    let resumed = observed.last().unwrap();
    assert_eq!(
        resumed
            .iter()
            .filter(|message| matches!(
                message, Message::ToolResult(result)
                    if result.tool_call_id == "overflow-write-once"
            ))
            .count(),
        1,
    );
    assert_eq!(
        resumed
            .iter()
            .filter(|message| matches!(
                message, Message::User(UserMessage { content: UserContent::Blocks(blocks), .. })
                    if serde_json::to_value(blocks).unwrap() == expected_content
            ))
            .count(),
        1,
        "accepted images and user input must not be replayed or flattened",
    );
    assert!(
        crate::compaction::estimate_model_context_tokens(resumed)
            < crate::compaction::estimate_model_context_tokens(&observed[1])
    );
    drop(observed);
    let events = events.lock().unwrap();
    let compaction_end = events
        .iter()
        .find(|event| event["type"] == "auto_compaction_end")
        .unwrap();
    assert_eq!(compaction_end["willRetry"], true);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "tool_execution_start")
            .count(),
        1,
    );
    assert_eq!(lifecycle_names(&events), ["agent_start", "agent_end"]);
    let path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .unwrap();
    let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
    // The overflow summary is re-stamped on every projection, so only its
    // timestamp may differ between the live context and the reopened file.
    let (live, durable) =
        reopened.current_path_comparison_values(&run_async(handle.messages()).unwrap());
    assert_eq!(durable, live);
}

#[test]
#[allow(clippy::too_many_lines)]
fn overflow_keeps_native_request_and_large_completed_tool_tail_or_stops_without_history() {
    for older_history in [false, true] {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("large-result.txt"),
            "large completed result with detailed evidence\n".repeat(1_000),
        )
        .unwrap();
        let (mut handle, provider) = overflow_recovery_handle(
            dir.path(),
            true,
            false,
            "The earlier task was completed.".to_string(),
            None,
        );
        provider.large_read.store(true, Ordering::SeqCst);
        if !older_history {
            let mut fresh = Session::create_with_dir(Some(dir.path().join("sessions")));
            fresh.header.provider = Some(provider.name().to_string());
            fresh.header.model_id = Some(provider.model_id().to_string());
            *handle.session_store().try_lock().unwrap() = fresh;
        }
        let content = native_content_fixture();
        let expected = serde_json::to_value(&content).unwrap();
        let (events, callback) = event_log();
        let result = run_async(handle.prompt_with_content(content, move |event| callback(event)));
        if older_history {
            assert_eq!(
                result.expect("compacted older prefix").stop_reason,
                StopReason::Stop
            );
            assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
            assert_eq!(provider.summaries.load(Ordering::SeqCst), 1);
        } else {
            assert!(result.is_err(), "active-only overflow must stop");
            assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
            assert_eq!(provider.summaries.load(Ordering::SeqCst), 0);
        }
        let observed = provider.observed.lock().unwrap();
        let before_overflow = &observed[2];
        let input_index = before_overflow
            .iter()
            .rposition(|message| matches!(message, Message::User(_)))
            .unwrap();
        let protected_tail = &before_overflow[input_index..];
        assert!(
            crate::compaction::estimate_model_context_tokens(protected_tail)
                > u64::from(handle.session.compaction_settings().keep_recent_tokens),
            "the active tool tail must exercise the ordinary split-turn cut",
        );
        let messages = run_async(handle.messages()).unwrap();
        let retained = if older_history {
            observed.last().unwrap()
        } else {
            &messages
        };
        let kept_input_index = retained
            .iter()
            .rposition(|message| {
                matches!(
                    message, Message::User(UserMessage { content: UserContent::Blocks(blocks), .. })
                        if serde_json::to_value(blocks).unwrap() == expected
                )
            })
            .expect("exact native user blocks retained");
        assert_eq!(
            serde_json::to_value(&retained[kept_input_index..]).unwrap(),
            serde_json::to_value(protected_tail).unwrap(),
            "current media and both completed tool pairs must remain byte-for-byte",
        );
        for call_id in ["overflow-write-once", "overflow-read-once"] {
            assert_eq!(
                retained
                    .iter()
                    .filter(|message| matches!(
                        message, Message::ToolResult(result) if result.tool_call_id == call_id
                    ))
                    .count(),
                1,
            );
        }
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "tool_execution_start")
                .count(),
            2,
        );
        assert_eq!(lifecycle_names(&events), ["agent_start", "agent_end"]);
        let path = handle
            .session_store()
            .try_lock()
            .unwrap()
            .path
            .clone()
            .unwrap();
        let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
        // The overflow summary is re-stamped on every projection; the retained
        // native input and tool pairs keep their authored timestamps.
        let (live, durable) = reopened.current_path_comparison_values(&messages);
        assert_eq!(durable, live);
    }
}

#[test]
#[allow(clippy::too_many_lines)]
#[allow(clippy::significant_drop_tightening)] // `end` borrows the events guard
fn overflow_rejects_extension_cuts_that_discard_current_input_or_name_unknown_entries() {
    for unknown_cut in [false, true] {
        let dir = tempdir().unwrap();
        let (mut handle, provider) = overflow_recovery_handle(
            dir.path(),
            false,
            false,
            "unused provider summary".to_string(),
            None,
        );
        let late_cut = {
            let store = handle.session_store();
            let mut stored = store.try_lock().unwrap();
            stored.append_model_message(Message::User(UserMessage {
                content: UserContent::Blocks(native_content_fixture()),
                timestamp: 0,
            }));
            let late_cut =
                stored.append_model_message(Message::Assistant(Arc::new(AssistantMessage {
                    content: vec![ContentBlock::ToolCall(crate::model::ToolCall {
                        id: "already-completed-write".to_string(),
                        name: "write".to_string(),
                        arguments: serde_json::json!({
                            "path": "result.txt",
                            "content": "already saved",
                        }),
                        thought_signature: None,
                    })],
                    provider: provider.name().to_string(),
                    model: provider.model_id().to_string(),
                    stop_reason: StopReason::ToolUse,
                    ..Default::default()
                })));
            stored.append_model_message(Message::tool_result(crate::model::ToolResultMessage {
                tool_call_id: "already-completed-write".to_string(),
                tool_name: "write".to_string(),
                content: vec![ContentBlock::Text(TextContent::new("saved"))],
                details: None,
                is_error: false,
                timestamp: 0,
            }));
            late_cut
        };
        run_async(handle.session.persist_session()).unwrap();
        let before = serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap();
        install_native_event_hooks(
            &mut handle,
            &serde_json::json!({
                "session_before_compact": {
                    "compaction": {
                        "summary": "A concise extension summary.",
                        "firstKeptEntryId": if unknown_cut { "missing-entry" } else { &late_cut },
                    },
                },
            }),
        );
        let (events, callback) = event_log();
        let error = run_async(handle.continue_turn(move |event| callback(event)))
            .expect_err("unsafe extension cut must be refused");
        assert!(
            error
                .to_string()
                .contains("discard the current user request")
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.summaries.load(Ordering::SeqCst), 0);
        assert_eq!(
            serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
            before,
            "refusal must preserve exact native user content and completed tool work",
        );
        handle.session.ensure_provider_reentry_allowed().unwrap();
        let store = handle.session_store();
        let stored = store.try_lock().unwrap();
        assert!(
            stored
                .entries
                .iter()
                .all(|entry| !matches!(entry, crate::session::SessionEntry::Compaction(_)))
        );
        let path = stored.path.clone().unwrap();
        drop(stored);
        let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
        assert_eq!(
            serde_json::to_value(reopened.to_messages_for_current_path()).unwrap(),
            before,
        );
        let events = events.lock().unwrap();
        assert_eq!(lifecycle_names(&events), ["agent_start", "agent_end"]);
        let end = events
            .iter()
            .find(|event| event["type"] == "auto_compaction_end")
            .unwrap();
        assert_eq!(end["willRetry"], false);
        assert!(end["result"].is_null());
    }
}

#[test]
fn overflow_recovery_is_bounded_and_rejects_a_summary_that_cannot_shrink_context() {
    for no_progress in [false, true] {
        let dir = tempdir().unwrap();
        let summary = if no_progress {
            "summary adds more context than it removes ".repeat(4_000)
        } else {
            "Earlier history summarized.".to_string()
        };
        let (mut handle, provider) =
            overflow_recovery_handle(dir.path(), false, true, summary, None);
        handle = handle.with_retry(Some(fast_retry_policy(3)));
        let (events, callback) = event_log();
        let result = run_async(handle.prompt("continue the current task", move |event| {
            callback(event);
        }));
        let error = result.expect_err("unrecoverable overflow");
        if no_progress {
            assert!(error.to_string().contains("made no progress"), "{error}");
        }
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            if no_progress { 1 } else { 2 }
        );
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "auto_compaction_start")
                .count(),
            1,
            "one maintenance attempt per logical turn",
        );
        assert_eq!(lifecycle_names(&events), ["agent_start", "agent_end"]);
        let store = handle.session_store();
        let stored = store.try_lock().unwrap();
        assert_eq!(
            stored
                .entries
                .iter()
                .filter(|entry| matches!(entry, crate::session::SessionEntry::Compaction(_)))
                .count(),
            usize::from(!no_progress),
            "a no-progress candidate must not replace live or durable history",
        );
    }
}

#[test]
fn disabled_compaction_and_unsplittable_input_never_reenter_the_provider() {
    for disabled in [false, true] {
        let dir = tempdir().unwrap();
        let (mut handle, provider) = overflow_recovery_handle(
            dir.path(),
            false,
            true,
            "Earlier history summarized.".to_string(),
            None,
        );
        handle = handle.with_retry(Some(fast_retry_policy(3)));
        if disabled {
            handle.session.set_auto_compaction(false);
        } else {
            // Only the upcoming authored request remains: no older prefix or
            // safe split exists, even when the provider says it is too large.
            let mut fresh = Session::create_with_dir(Some(dir.path().join("sessions")));
            fresh.header.provider = Some(provider.name().to_string());
            fresh.header.model_id = Some(provider.model_id().to_string());
            *handle.session_store().try_lock().unwrap() = fresh;
        }
        let (events, callback) = event_log();
        let result = run_async(
            handle.prompt("one unsplittable current request", move |event| {
                callback(event);
            }),
        );
        assert!(result.is_err(), "disabled={disabled}: {result:?}");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.summaries.load(Ordering::SeqCst), 0);
        let events = events.lock().unwrap();
        assert_eq!(lifecycle_names(&events), ["agent_start", "agent_end"]);
        assert!(
            events
                .iter()
                .all(|event| event["type"] != "auto_compaction_start")
        );
        drop(events);
        handle.session.ensure_provider_reentry_allowed().unwrap();
    }
}

#[test]
fn overflow_compaction_save_failure_quarantines_later_provider_entry() {
    let dir = tempdir().unwrap();
    let (mut handle, provider) = overflow_recovery_handle(
        dir.path(),
        false,
        false,
        "Earlier history summarized.".to_string(),
        None,
    );
    let blocked = dir.path().join("blocked-session");
    std::fs::create_dir(&blocked).unwrap();
    let store = handle.session_store();
    let original = Arc::new(Mutex::new(None));
    let saved_original = Arc::clone(&original);
    let result = run_async(handle.prompt("continue after overflow", move |event| {
        if matches!(event, AgentEvent::AutoCompactionStart { .. }) {
            let mut session = store.try_lock().unwrap();
            *saved_original.lock().unwrap() = session.path.clone();
            session.path = Some(blocked.clone());
        }
    }));
    assert!(
        result.as_ref().is_err_and(Error::is_session_persistence),
        "{result:?}"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    handle.session_store().try_lock().unwrap().path = original.lock().unwrap().clone();
    let result = run_async(handle.continue_turn(|_| {}));
    assert!(result.as_ref().is_err_and(Error::is_session_persistence));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn overflow_compaction_honors_cancellation_before_and_during_summary() {
    for during_summary in [false, true] {
        let dir = tempdir().unwrap();
        let (abort, signal) = AbortHandle::new();
        let (mut handle, provider) = overflow_recovery_handle(
            dir.path(),
            false,
            false,
            "Earlier history summarized.".to_string(),
            during_summary.then(|| abort.clone()),
        );
        let (events, callback) = event_log();
        let result = run_async(handle.prompt_with_abort("continue", signal, move |event| {
            if !during_summary && matches!(event, AgentEvent::AutoCompactionStart { .. }) {
                abort.abort();
            }
            callback(event);
        }));
        assert!(matches!(result, Err(Error::Aborted)), "{result:?}");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            provider.summaries.load(Ordering::SeqCst),
            usize::from(during_summary)
        );
        let events = events.lock().unwrap();
        let end = events
            .iter()
            .find(|event| event["type"] == "auto_compaction_end")
            .unwrap();
        assert_eq!(end["aborted"], true);
        assert_eq!(end["willRetry"], false);
        assert_eq!(lifecycle_names(&events), ["agent_start", "agent_end"]);
        drop(events);
        handle.session.ensure_provider_reentry_allowed().unwrap();
    }
}

struct TransportDropProvider;

#[async_trait::async_trait]
impl Provider for TransportDropProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "anthropic"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn api(&self) -> &str {
        "test-api"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn model_id(&self) -> &str {
        "claude-x"
    }

    async fn stream(
        &self,
        _context: &crate::provider::Context<'_>,
        _options: &StreamOptions,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        Err(Error::Io(Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "wire",
        ))))
    }
}

#[test]
fn a_typed_transport_failure_can_fail_over_without_transient_words_in_display() {
    let mut server = RecoveryHttpFixture::new(vec![completion_response()]);
    let (mut handle, _) = http_chain_handle(&server.url, 1);
    handle
        .session
        .agent
        .set_provider(Arc::new(TransportDropProvider));
    let (events, callback) = event_log();
    let result = run_async(handle.prompt("recover the wire", move |event| callback(event)))
        .expect("typed transport recovery");
    server.finish();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(handle.model().1, "fallback-a");
    let events = events.lock().unwrap().clone();
    let start = events
        .iter()
        .find(|event| event["type"] == "failover_start")
        .unwrap();
    assert_eq!(start["class"], "transient");
    assert_eq!(start["attempt"], 1);
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "failover_start:1:2:fallback-a",
            "failover_end:fallback-a:true",
            "agent_end",
        ]
    );
}

#[test]
fn extension_selection_of_the_current_fallback_cancels_restoration() {
    use crate::extensions::ExtensionSession as _;

    let dir = tempdir().unwrap();
    let mut handle = saving_fallback_selection_handle(dir.path());
    let selected = handle.model();
    let extension = crate::session::SessionHandle(handle.session_store());
    run_async(extension.set_model(selected.0.clone(), selected.1.clone(), None)).unwrap();
    assert!(
        handle
            .session_store()
            .try_lock()
            .unwrap()
            .active_failover_provenance_for_current_path()
            .is_none(),
        "the same-model extension selection must record explicit intent"
    );

    let (events, callback) = event_log();
    run_async(handle.maybe_restore_primary(&callback)).unwrap();
    assert_eq!(handle.model(), selected);
    assert!(handle.failover_state.primary().is_none());
    assert!(events.lock().unwrap().is_empty());
    let result = run_async(handle.prompt("keep the selected model", move |event| {
        callback(event);
    }))
    .unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop);
    let path = handle
        .session_store()
        .try_lock()
        .unwrap()
        .path
        .clone()
        .unwrap();
    let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
    assert!(
        reopened
            .active_failover_provenance_for_current_path()
            .is_none()
    );
    assert_eq!(reopened.effective_model_for_current_path(), Some(selected));
}

#[test]
fn extension_selection_starts_the_next_failover_from_its_own_primary_and_cursor() {
    use crate::extensions::ExtensionSession as _;

    let dir = tempdir().unwrap();
    let mut handle = saving_fallback_selection_handle(dir.path());
    let selected = handle.model();
    let extension = crate::session::SessionHandle(handle.session_store());
    run_async(extension.set_model(selected.0.clone(), selected.1.clone(), None)).unwrap();
    let (events, callback) = event_log();
    let failure = Err(Error::provider(&selected.0, "503 service unavailable"));
    assert!(
        run_async(handle.try_chain_failover(&failure, false, None, 1, &callback)).unwrap(),
        "the retired cycle's cursor cannot skip the new primary's first fallback"
    );
    let primary = handle.failover_state.primary().unwrap();
    assert_eq!(
        (&primary.provider, &primary.model_id),
        (&selected.0, &selected.1)
    );
    assert_eq!(handle.failover_state.chain_position(), 1);
    assert_ne!(
        handle.failover_state.lifecycle_id(),
        Some("selection-fixture")
    );
    assert_eq!(
        handle.model(),
        ("openai".to_string(), "gpt-4o-mini".to_string())
    );
    let store = handle.session_store();
    let session = store.try_lock().unwrap();
    let provenance = session
        .active_failover_provenance_for_current_path()
        .unwrap();
    assert_eq!(
        (&provenance.primary_provider, &provenance.primary_model_id),
        (&selected.0, &selected.1)
    );
    assert_eq!(events.lock().unwrap()[0]["type"], "failover_start");
}

#[test]
fn an_old_provider_error_cannot_fail_over_an_extension_model_selection() {
    use crate::extensions::ExtensionSession as _;

    let dir = tempdir().unwrap();
    let mut handle = saving_fallback_selection_handle(dir.path());
    let old_runtime = handle.model();
    let selected = ("openai".to_string(), "gpt-4.1-mini".to_string());
    let extension = crate::session::SessionHandle(handle.session_store());
    run_async(extension.set_model(selected.0.clone(), selected.1.clone(), None)).unwrap();
    let store = handle.session_store();
    let entries_before = serde_json::to_value(&store.try_lock().unwrap().entries).unwrap();
    let (events, callback) = event_log();
    let failure = Err(Error::provider(&old_runtime.0, "503 service unavailable"));
    assert!(!run_async(handle.try_chain_failover(&failure, false, None, 1, &callback)).unwrap());
    run_async(handle.maybe_restore_primary(&callback)).unwrap();
    assert_eq!(
        handle.model(),
        old_runtime,
        "normal entry will install the selection"
    );
    assert_eq!(
        store.try_lock().unwrap().effective_model_for_current_path(),
        Some(selected)
    );
    assert_eq!(
        serde_json::to_value(&store.try_lock().unwrap().entries).unwrap(),
        entries_before
    );
    assert!(handle.failover_state.primary().is_none());
    assert!(events.lock().unwrap().is_empty());
}

fn recovery_route_entry(provider: &str, model: &str) -> crate::models::ModelEntry {
    let mut entry = crate::models::ad_hoc_model_entry("openai", model).unwrap();
    entry.model.provider = provider.to_string();
    entry.model.api = "openai-completions".to_string();
    entry.model.base_url = format!("https://{provider}.example.test/v1");
    entry.api_key = Some(format!("{provider}-fixture-key"));
    entry
        .headers
        .insert("x-recovery-route".to_string(), provider.to_string());
    entry
}

#[test]
fn sdk_failover_policies_use_full_catalog_ownership_and_destination_metadata() {
    for (current, other) in [("openrouter", "open-router"), ("open-router", "openrouter")] {
        for reverse in [false, true] {
            let (mut handle, _) = flaky_handle_as(0, current, "vendor/shared");
            let mut native = recovery_route_entry("openrouter", "vendor/shared");
            let mut declared = recovery_route_entry("open-router", "vendor/shared");
            native.api_key = None;
            declared.api_key = None;
            let accepted = recovery_route_entry("policy-owner-fallback", "accepted");
            let wrong = recovery_route_entry("policy-other-fallback", "wrong");
            let mut catalog = vec![native, declared, accepted.clone(), wrong.clone()];
            if reverse {
                catalog.reverse();
            }
            let mut registry = ModelRegistry::default();
            registry
                .merge_extension_entries(catalog)
                .expect("separately declared provider catalog");
            handle.session.set_model_registry(registry);
            let store = handle.session_store();
            store.try_lock().unwrap().set_model_header(
                Some(current.to_string()),
                Some("vendor/shared".to_string()),
                None,
            );
            let before = serde_json::to_value(&store.try_lock().unwrap().entries).unwrap();
            let mut options = FailoverOptions {
                chains: HashMap::from([(
                    format!("{other}/vendor/shared"),
                    vec!["policy-other-fallback/wrong".to_string()],
                )]),
                // The primary names are intentionally absent from the admitted
                // pool. Their full-catalog declarations still own their policies.
                available_models: vec![accepted.clone(), wrong],
                auth: AuthStorage::empty_at(PathBuf::from("unused-policy-auth.json")),
                cli_api_key: None,
                cooldown_secs: 300,
            };
            handle = handle.with_failover(Some(options.clone()));
            let (events, callback) = event_log();
            let failure = Err(Error::provider(current, "503 service unavailable"));
            assert!(
                !run_async(handle.try_chain_failover(&failure, false, None, 1, &callback)).unwrap(),
                "{current} cannot use {other}'s policy (reverse={reverse})"
            );
            assert_eq!(
                handle.model(),
                (current.to_string(), "vendor/shared".to_string())
            );
            assert_eq!(
                serde_json::to_value(&store.try_lock().unwrap().entries).unwrap(),
                before
            );
            assert!(events.lock().unwrap().is_empty());

            options.chains.insert(
                format!(" {} / VENDOR/SHARED ", current.to_ascii_uppercase()),
                vec!["policy-owner-fallback/accepted".to_string()],
            );
            handle = handle.with_failover(Some(options));
            assert!(
                run_async(handle.try_chain_failover(&failure, false, None, 1, &callback)).unwrap()
            );
            assert_eq!(
                handle.model(),
                (accepted.model.provider.clone(), accepted.model.id.clone())
            );
            let installed = handle.session.current_model_entry().unwrap();
            assert_eq!(installed.model.base_url, accepted.model.base_url);
            assert_eq!(installed.headers, accepted.headers);
            assert_eq!(handle.session.agent.provider().api(), accepted.model.api);
            assert_eq!(
                handle.session.agent.stream_options().headers,
                accepted.headers
            );
            assert_eq!(
                handle.session.agent.stream_options().api_key,
                accepted.api_key
            );
            let stored = store.try_lock().unwrap();
            let provenance = stored
                .active_failover_provenance_for_current_path()
                .unwrap();
            assert_eq!(provenance.primary_provider, current);
            assert_eq!(provenance.primary_model_id, "vendor/shared");
            assert_eq!(provenance.fallback_provider, accepted.model.provider);
            assert_eq!(provenance.fallback_model_id, accepted.model.id);
            assert_eq!(events.lock().unwrap()[0]["type"], "failover_start");
        }
    }
}

fn saving_alias_recovery_handle(
    dir: &Path,
    current: &str,
    catalog: Vec<crate::models::ModelEntry>,
    fallback: &crate::models::ModelEntry,
    with_registry: bool,
) -> AgentSessionHandle {
    let mut handle = saving_handle(dir);
    handle
        .session
        .agent
        .set_provider(Arc::new(FlakyThenOkProvider {
            failures: 0,
            calls: Arc::new(AtomicUsize::new(0)),
            name: current.to_string(),
            model: "vendor/shared".to_string(),
            input_tokens: 0,
        }));
    let available_models = if with_registry {
        let mut registry = ModelRegistry::default();
        registry
            .merge_extension_entries(catalog)
            .expect("separately declared provider catalog");
        handle.session.set_model_registry(registry);
        vec![fallback.clone()]
    } else {
        catalog
    };
    handle.with_failover(Some(FailoverOptions {
        chains: HashMap::from([(
            "default".to_string(),
            vec![format!("{}/{}", fallback.model.provider, fallback.model.id)],
        )]),
        available_models,
        auth: AuthStorage::empty_at(dir.join("auth.json")),
        cli_api_key: None,
        cooldown_secs: 300,
    }))
}

#[test]
fn sdk_recovery_preserves_pending_declared_alias_selection_and_native_aliases() {
    use crate::extensions::ExtensionSession as _;

    for separately_declared in [false, true] {
        for with_registry in [false, true] {
            for reverse in [false, true] {
                for (current, selected) in
                    [("openrouter", "open-router"), ("open-router", "openrouter")]
                {
                    let dir = tempdir().unwrap();
                    let mut native = recovery_route_entry("openrouter", "vendor/shared");
                    native.api_key = None;
                    let mut catalog = vec![native];
                    if separately_declared {
                        let mut declared = recovery_route_entry("open-router", "vendor/shared");
                        declared.api_key = None;
                        catalog.push(declared);
                    }
                    let fallback = recovery_route_entry("selection-fallback", "accepted");
                    catalog.push(fallback.clone());
                    if reverse {
                        catalog.reverse();
                    }
                    let mut handle = saving_alias_recovery_handle(
                        dir.path(),
                        current,
                        catalog,
                        &fallback,
                        with_registry,
                    );
                    let original_runtime = handle.session.agent.provider();
                    let store = handle.session_store();
                    store.try_lock().unwrap().set_model_header(
                        Some(current.to_string()),
                        Some("vendor/shared".to_string()),
                        None,
                    );
                    let extension = crate::session::SessionHandle(Arc::clone(&store));
                    run_async(extension.set_model(
                        selected.to_string(),
                        "vendor/shared".to_string(),
                        None,
                    ))
                    .unwrap();
                    run_async(handle.session.persist_session()).unwrap();
                    let before = serde_json::to_value(&store.try_lock().unwrap().entries).unwrap();
                    let path = store.try_lock().unwrap().path.clone().unwrap();
                    let bytes_before = std::fs::read(&path).unwrap();
                    let (events, callback) = event_log();
                    let failure = Err(Error::provider(current, "503 service unavailable"));
                    let swapped =
                        run_async(handle.try_chain_failover(&failure, false, None, 1, &callback))
                            .unwrap();
                    assert_eq!(
                        swapped, !separately_declared,
                        "current={current}, selected={selected}, full registry={with_registry}, reverse={reverse}"
                    );
                    if separately_declared {
                        run_async(handle.maybe_restore_primary(&callback)).unwrap();
                        assert!(Arc::ptr_eq(
                            &original_runtime,
                            &handle.session.agent.provider()
                        ));
                        assert_eq!(
                            serde_json::to_value(&store.try_lock().unwrap().entries).unwrap(),
                            before
                        );
                        assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
                        let reopened =
                            run_async(Session::open(&path.display().to_string())).unwrap();
                        assert_eq!(
                            reopened.effective_model_for_current_path(),
                            Some((selected.to_string(), "vendor/shared".to_string()))
                        );
                        assert!(handle.failover_state.primary().is_none());
                        assert!(events.lock().unwrap().is_empty());
                    } else {
                        assert_eq!(
                            handle.model(),
                            (fallback.model.provider.clone(), fallback.model.id.clone())
                        );
                        assert_eq!(
                            handle.session.agent.stream_options().headers,
                            fallback.headers
                        );
                        assert_eq!(
                            handle.session.agent.stream_options().api_key,
                            fallback.api_key
                        );
                        assert_eq!(events.lock().unwrap()[0]["type"], "failover_start");
                    }
                }
            }
        }
    }
}

fn install_reconciliation_provenance(
    handle: &mut AgentSessionHandle,
    provenance: crate::session::ModelChangeFailover,
) {
    let store = handle.session_store();
    {
        let mut session = store.try_lock().unwrap();
        session.set_model_header(
            Some(provenance.fallback_provider.clone()),
            Some(provenance.fallback_model_id.clone()),
            None,
        );
        session.append_model_change_with_role_and_failover(
            provenance.fallback_provider.clone(),
            provenance.fallback_model_id.clone(),
            Some("failover".to_string()),
            Some(provenance),
        );
    }
    run_async(handle.session.persist_session()).unwrap();
}

#[test]
fn unchanged_failover_provenance_keeps_its_running_monotonic_cooldown() {
    let dir = tempdir().unwrap();
    let mut handle = saving_fallback_selection_handle(dir.path());
    let mut provenance = handle
        .session_store()
        .try_lock()
        .unwrap()
        .active_failover_provenance_for_current_path()
        .unwrap()
        .clone();
    // Older records have no wall deadline. Reconstructing one repeatedly
    // restarts the interval and can prevent restoration indefinitely.
    provenance.cooldown_deadline = None;
    provenance.cooldown_secs = Some(300);
    install_reconciliation_provenance(&mut handle, provenance);
    handle.failover_state = {
        let store = handle.session_store();
        let session = store.try_lock().unwrap();
        crate::failover::FailoverState::reconstruct_from_session(&session, 300, chrono::Utc::now())
    };
    let started = handle.failover_state.cooldown().unwrap().failed_at();
    let (events, callback) = event_log();
    for _ in 0..2 {
        run_async(handle.maybe_restore_primary(&callback)).unwrap();
        assert_eq!(
            handle.failover_state.cooldown().unwrap().failed_at(),
            started
        );
        assert!(
            !handle
                .failover_state
                .should_restore_primary(std::time::Instant::now())
        );
    }
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn changed_branch_provenance_refreshes_only_the_affected_failover_cycle() {
    for changed in ["lifecycle", "primary", "active", "cursor"] {
        let dir = tempdir().unwrap();
        let mut handle = saving_fallback_selection_handle(dir.path());
        let mut provenance = handle
            .session_store()
            .try_lock()
            .unwrap()
            .active_failover_provenance_for_current_path()
            .unwrap()
            .clone();
        provenance.cooldown_deadline = None;
        provenance.cooldown_secs = Some(300);
        match changed {
            "lifecycle" => provenance.lifecycle_id = Some("other-cycle".to_string()),
            "primary" => provenance.primary_model_id = "other-primary".to_string(),
            "active" => provenance.fallback_model_id = "other-fallback".to_string(),
            "cursor" => provenance.chain_position = Some(4),
            _ => unreachable!(),
        }
        install_reconciliation_provenance(&mut handle, provenance.clone());
        let (events, callback) = event_log();
        run_async(handle.maybe_restore_primary(&callback)).unwrap();
        let primary = handle.failover_state.primary().expect(changed);
        assert_eq!(primary.provider, provenance.primary_provider, "{changed}");
        assert_eq!(primary.model_id, provenance.primary_model_id, "{changed}");
        assert_eq!(
            handle.failover_state.active(),
            Some(&(provenance.fallback_provider, provenance.fallback_model_id)),
            "{changed}"
        );
        assert_eq!(
            handle.failover_state.lifecycle_id(),
            provenance.lifecycle_id.as_deref(),
            "{changed}"
        );
        assert_eq!(
            handle.failover_state.chain_position(),
            provenance.chain_position.unwrap(),
            "{changed}"
        );
        assert!(
            !handle
                .failover_state
                .should_restore_primary(std::time::Instant::now())
        );
        assert!(events.lock().unwrap().is_empty());
    }
}

#[test]
fn recovery_waits_for_provider_callbacks_before_claiming_session_actions() {
    use crate::extensions::ExtensionSession as _;

    for restore in [false, true] {
        let dir = tempdir().unwrap();
        let mut handle = saving_fallback_selection_handle(dir.path());
        let extension = crate::session::SessionHandle(handle.session_store());
        if !restore {
            let selected = handle.model();
            run_async(extension.set_model(selected.0, selected.1, None)).unwrap();
        }
        let provider_gate = handle.session.provider_admission_gate();
        let action_gate = handle.session.session_action_admission_gate();
        let (events, callback) = event_log();
        run_async(async {
            let cx = crate::agent_cx::AgentCx::for_current_or_request();
            let provider_call = provider_gate.acquire(cx.cx()).await.unwrap();
            let failure = Err(Error::provider("test-provider", "503 service unavailable"));
            let recovery = async {
                if restore {
                    handle.maybe_restore_primary(&callback).await.map(|()| true)
                } else {
                    handle
                        .try_chain_failover(&failure, false, None, 1, &callback)
                        .await
                }
            };
            futures::pin_mut!(recovery);
            assert!(
                futures::poll!(recovery.as_mut()).is_pending(),
                "recovery must wait for the active provider"
            );

            let callback_action = action_gate.acquire(cx.cx());
            futures::pin_mut!(callback_action);
            let std::task::Poll::Ready(Ok(action_permit)) =
                futures::poll!(callback_action.as_mut())
            else {
                panic!("a provider callback must still be able to enter Session actions");
            };
            extension
                .set_name("provider callback completed".to_string(), None)
                .await
                .unwrap();
            drop(action_permit);
            drop(provider_call);
            let outcome = asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(5),
                recovery,
            )
            .await
            .expect("recovery must not reacquire its own provider permit");
            assert!(
                outcome.unwrap(),
                "the pre-held authority must complete the swap"
            );
            provider_gate.ensure_allowed().unwrap();
        });
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]["type"],
            if restore {
                "failover_end"
            } else {
                "failover_start"
            }
        );
    }
}

#[test]
fn another_branch_deadline_cannot_reuse_an_expired_cooldown() {
    let dir = tempdir().unwrap();
    let mut handle = saving_fallback_selection_handle(dir.path());
    let store = handle.session_store();
    handle.failover_state = crate::failover::FailoverState::reconstruct_from_session(
        &store.try_lock().unwrap(),
        0,
        chrono::Utc::now(),
    );
    assert!(
        handle
            .failover_state
            .should_restore_primary(std::time::Instant::now())
    );
    let mut provenance = store
        .try_lock()
        .unwrap()
        .active_failover_provenance_for_current_path()
        .unwrap()
        .clone();
    provenance.cooldown_secs = Some(300);
    provenance.cooldown_deadline =
        Some((chrono::Utc::now() + chrono::Duration::seconds(300)).to_rfc3339());
    install_reconciliation_provenance(&mut handle, provenance);
    let selected = handle.model();
    let (events, callback) = event_log();
    run_async(handle.maybe_restore_primary(&callback)).unwrap();
    assert_eq!(handle.model(), selected);
    assert!(
        !handle
            .failover_state
            .should_restore_primary(std::time::Instant::now())
    );
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn distinct_legacy_failover_entries_have_independent_cooldowns() {
    let dir = tempdir().unwrap();
    let mut handle = saving_fallback_selection_handle(dir.path());
    let store = handle.session_store();
    let mut provenance = store
        .try_lock()
        .unwrap()
        .active_failover_provenance_for_current_path()
        .unwrap()
        .clone();
    provenance.lifecycle_id = None;
    provenance.cooldown_deadline = None;
    provenance.cooldown_secs = Some(300);
    install_reconciliation_provenance(&mut handle, provenance.clone());
    handle.failover_state = crate::failover::FailoverState::reconstruct_from_session(
        &store.try_lock().unwrap(),
        300,
        chrono::Utc::now(),
    );
    let old_started = handle.failover_state.cooldown().unwrap().failed_at();
    install_reconciliation_provenance(&mut handle, provenance);
    let (events, callback) = event_log();
    run_async(handle.maybe_restore_primary(&callback)).unwrap();
    let new_started = handle.failover_state.cooldown().unwrap().failed_at();
    assert_ne!(
        old_started, new_started,
        "the other entry starts its own interval"
    );
    run_async(handle.maybe_restore_primary(&callback)).unwrap();
    assert_eq!(
        handle.failover_state.cooldown().unwrap().failed_at(),
        new_started
    );
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn interactive_auth_adoption_updates_fallback_credentials_and_preserves_cli_pin() {
    for cli_pin in [None, Some("pinned-cli-key")] {
        let dir = tempdir().unwrap();
        let (mut handle, _) = flaky_handle_as(0, "auth-fixture", "auth-model");
        handle
            .session
            .set_api_key_override(cli_pin.map(str::to_string));
        handle = handle.with_failover(Some(FailoverOptions {
            chains: HashMap::new(),
            available_models: Vec::new(),
            auth: AuthStorage::empty_at(dir.path().join("old-auth.json")),
            cli_api_key: cli_pin.map(str::to_string),
            cooldown_secs: 0,
        }));
        let mut auth = AuthStorage::empty_at(dir.path().join("new-auth.json"));
        auth.set(
            "auth-fixture",
            crate::auth::AuthCredential::ApiKey {
                key: "new-active-key".to_string(),
            },
        );
        auth.set(
            "fallback-fixture",
            crate::auth::AuthCredential::ApiKey {
                key: "new-fallback-key".to_string(),
            },
        );
        handle.adopt_auth_storage(auth.clone());
        assert_eq!(
            handle.session.agent.stream_options().api_key.as_deref(),
            Some(cli_pin.unwrap_or("new-active-key"))
        );
        let options = handle.failover.as_ref().expect("failover options");
        assert_eq!(options.cli_api_key.as_deref(), cli_pin);
        assert_eq!(
            options.auth.api_key("fallback-fixture").as_deref(),
            Some("new-fallback-key")
        );

        assert!(auth.remove("auth-fixture"));
        assert!(auth.remove("fallback-fixture"));
        handle.adopt_auth_storage(auth);
        assert_eq!(
            handle.session.agent.stream_options().api_key.as_deref(),
            cli_pin,
            "logout must remove stale credentials while retaining an explicit CLI pin"
        );
        assert!(
            handle
                .failover
                .as_ref()
                .expect("failover options")
                .auth
                .api_key("fallback-fixture")
                .is_none()
        );
    }
}

fn credential_binding_handle(
    entries: &[crate::models::ModelEntry],
    bound_key: Option<&str>,
) -> AgentSessionHandle {
    let initial = &entries[0];
    let (mut handle, _) = flaky_handle_as(0, &initial.model.provider, &initial.model.id);
    handle
        .session
        .set_api_key_override(bound_key.map(str::to_string));
    handle
        .session
        .set_model_registry(ModelRegistry::from_entries_for_tests(entries.to_vec()));
    handle.session_store().try_lock().unwrap().set_model_header(
        Some(initial.model.provider.clone()),
        Some(initial.model.id.clone()),
        Some("off".to_string()),
    );
    handle
}

#[test]
fn bound_cli_key_cannot_admit_an_unready_foreign_fallback_and_survives_restoration() {
    let dir = tempdir().unwrap();
    let mut primary = recovery_route_entry("bound-primary", "execution");
    primary.api_key = None;
    let mut unready = recovery_route_entry("unready-other", "unready");
    unready.api_key = None;
    let fallback = recovery_route_entry("ready-other", "fallback");
    let entries = vec![primary, unready, fallback.clone()];
    let mut handle = credential_binding_handle(&entries, Some("execution-only-pin")).with_failover(
        Some(FailoverOptions {
            chains: HashMap::from([(
                "default".to_string(),
                vec![
                    "unready-other/unready".to_string(),
                    "ready-other/fallback".to_string(),
                ],
            )]),
            available_models: vec![fallback.clone()],
            auth: AuthStorage::empty_at(dir.path().join("auth.json")),
            cli_api_key: Some("execution-only-pin".to_string()),
            cooldown_secs: 0,
        }),
    );
    let (_, callback) = event_log();
    let failure = Err(Error::provider("bound-primary", "503 service unavailable"));
    assert!(run_async(handle.try_chain_failover(&failure, false, None, 1, &callback)).unwrap());
    assert_eq!(
        handle.model(),
        ("ready-other".to_string(), "fallback".to_string())
    );
    assert_eq!(
        handle.session.agent.stream_options().api_key,
        fallback.api_key
    );

    // A temporary planning role may remove the SDK's active-provider pin.
    // The native execution binding still admits its otherwise unready primary.
    Arc::make_mut(handle.failover.as_mut().unwrap()).cli_api_key = None;
    run_async(handle.maybe_restore_primary(&callback)).unwrap();
    assert_eq!(
        handle.model(),
        ("bound-primary".to_string(), "execution".to_string())
    );
    assert_eq!(
        handle.session.agent.stream_options().api_key.as_deref(),
        Some("execution-only-pin")
    );
}

#[test]
fn planner_fallback_recovers_original_pin_without_reusing_it_for_planner_restore() {
    let dir = tempdir().unwrap();
    let mut primary = recovery_route_entry("bound-executor", "execution");
    primary.api_key = None;
    let planner = recovery_route_entry("separate-planner", "planning");
    let entries = vec![primary, planner.clone()];
    let mut handle = credential_binding_handle(&entries, Some("bound-execution-pin"));
    run_async(handle.set_model("separate-planner", "planning")).unwrap();
    handle = handle.with_failover(Some(FailoverOptions {
        chains: HashMap::from([(
            "default".to_string(),
            vec!["bound-executor/execution".to_string()],
        )]),
        available_models: vec![planner.clone()],
        auth: AuthStorage::empty_at(dir.path().join("auth.json")),
        cli_api_key: None,
        cooldown_secs: 0,
    }));
    let (_, callback) = event_log();
    let failure = Err(Error::provider(
        "separate-planner",
        "503 service unavailable",
    ));
    assert!(run_async(handle.try_chain_failover(&failure, false, None, 1, &callback)).unwrap());
    assert_eq!(
        handle.model(),
        ("bound-executor".to_string(), "execution".to_string())
    );
    assert_eq!(
        handle.session.agent.stream_options().api_key.as_deref(),
        Some("bound-execution-pin")
    );

    Arc::make_mut(handle.failover.as_mut().unwrap()).cli_api_key =
        Some("bound-execution-pin".to_string());
    run_async(handle.maybe_restore_primary(&callback)).unwrap();
    assert_eq!(
        handle.model(),
        ("separate-planner".to_string(), "planning".to_string())
    );
    assert_eq!(
        handle.session.agent.stream_options().api_key,
        planner.api_key
    );
}

#[test]
fn recovery_without_a_native_key_binding_preserves_explicit_embedder_override() {
    let dir = tempdir().unwrap();
    let mut primary = recovery_route_entry("embedder-primary", "primary");
    primary.api_key = None;
    let mut fallback = recovery_route_entry("embedder-fallback", "fallback");
    fallback.api_key = None;
    let entries = vec![primary, fallback];
    let mut handle =
        credential_binding_handle(&entries, None).with_failover(Some(FailoverOptions {
            chains: HashMap::from([(
                "default".to_string(),
                vec!["embedder-fallback/fallback".to_string()],
            )]),
            available_models: Vec::new(),
            auth: AuthStorage::empty_at(dir.path().join("auth.json")),
            cli_api_key: Some("explicit-embedder-pin".to_string()),
            cooldown_secs: 0,
        }));
    let (_, callback) = event_log();
    let failure = Err(Error::provider(
        "embedder-primary",
        "503 service unavailable",
    ));
    assert!(run_async(handle.try_chain_failover(&failure, false, None, 1, &callback)).unwrap());
    assert_eq!(
        handle.model(),
        ("embedder-fallback".to_string(), "fallback".to_string())
    );
    assert_eq!(
        handle.session.agent.stream_options().api_key.as_deref(),
        Some("explicit-embedder-pin")
    );
    run_async(handle.maybe_restore_primary(&callback)).unwrap();
    assert_eq!(
        handle.model(),
        ("embedder-primary".to_string(), "primary".to_string())
    );
    assert_eq!(
        handle.session.agent.stream_options().api_key.as_deref(),
        Some("explicit-embedder-pin")
    );
}
